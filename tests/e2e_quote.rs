use std::{
    collections::HashMap,
    env,
    net::{TcpListener, TcpStream},
    sync::OnceLock,
    str::FromStr,
    time::{Duration, Instant},
};

use alloy::{
    consensus::TypedTransaction,
    eips::BlockId,
    network::Ethereum,
    primitives::{Address, B256, Bytes as AlloyBytes, TxKind, U256},
    providers::{Provider, ProviderBuilder, RootProvider},
    rpc::types::{state::{AccountOverride, StateOverride}, TransactionInput, TransactionRequest},
    sol,
    sol_types::SolCall,
};
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use erc20_overrides::{allowance_slot_at, balance_slot_at, find_allowance_slot, find_balance_slot};
use fynd::{
    core::PoolConfig,
    rpc::builder::FyndRPCBuilder,
};
use fynd_client::{
    EncodingOptions, FyndClient, FyndClientBuilder, Order, OrderSide, QuoteOptions, QuoteParams,
    SigningHints, SwapPayload,
};
use num_bigint::BigUint;
use reqwest::Client as HttpClient;
use serde::Deserialize;
use tokio::time::sleep;
use tracing_subscriber::{fmt, EnvFilter};
use tycho_simulation::{
    tycho_common::models::Chain,
};

sol! {
    interface IUniswapV3PoolStateProbe {
        function slot0()
            external
            view
            returns (
                uint160 sqrtPriceX96,
                int24 tick,
                uint16 observationIndex,
                uint16 observationCardinality,
                uint16 observationCardinalityNext,
                uint8 feeProtocol,
                bool unlocked
            );
        function liquidity() external view returns (uint128);
    }

    interface IUniswapV2PoolStateProbe {
        function getReserves()
            external
            view
            returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
        function token0() external view returns (address);
        function token1() external view returns (address);
    }
}

const DEFAULT_TYCHO_URL: &str = "127.0.0.1:4242";
const DEFAULT_RPC_URL: &str = "https://rpc.mevblocker.io";
const DEFAULT_PROTOCOLS: &str = "uniswap_v3";
const DEFAULT_TOKEN_IN: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
const DEFAULT_TOKEN_OUT: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
const DEFAULT_AMOUNT_IN: &str = "1000000000000000";
const DEFAULT_SENDER: &str = "0x000000000000000000000000000000000000beef";
const DEFAULT_TRADED_N_DAYS_AGO: u64 = 42;
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_FEE_PER_GAS_WEI: u128 = 1_000_000_000_000;
static TRACING_INIT: OnceLock<()> = OnceLock::new();

#[derive(Debug)]
struct TestConfig {
    tycho_url: String,
    rpc_url: String,
    protocols: Vec<String>,
    token_in: Bytes,
    token_out: Bytes,
    amount_in: BigUint,
    sender: Address,
    traded_n_days_ago: u64,
    health_timeout: Duration,
    quote_timeout: Duration,
}

#[derive(Debug, Deserialize)]
struct TychoProtocolStateResponse {
    states: Vec<TychoProtocolState>,
}

#[derive(Debug, Deserialize)]
struct TychoProtocolState {
    component_id: String,
    attributes: HashMap<String, String>,
}

#[tokio::test]
#[ignore = "requires local Tycho plus a live RPC endpoint"]
async fn quote_returns_route() -> Result<()> {
    init_test_tracing();
    let config = load_test_config()?;
    println!(
        "e2e_config tycho_url={} rpc_url={} protocols={} token_in=0x{} token_out=0x{} amount_in={} traded_n_days_ago={} health_timeout_secs={} quote_timeout_secs={}",
        config.tycho_url,
        config.rpc_url,
        config.protocols.join(","),
        alloy::primitives::hex::encode(config.token_in.as_ref()),
        alloy::primitives::hex::encode(config.token_out.as_ref()),
        config.amount_in,
        config.traded_n_days_ago,
        config.health_timeout.as_secs(),
        config.quote_timeout.as_secs(),
    );
    ensure_tycho_reachable(&config.tycho_url)?;
    let listener = TcpListener::bind("127.0.0.1:0").context("bind ephemeral HTTP listener")?;
    listener
        .set_nonblocking(true)
        .context("set listener nonblocking")?;
    let addr = listener.local_addr().context("read listener addr")?;
    let base_url = format!("http://{addr}");

    let mut pools = HashMap::new();
    pools.insert(
        "e2e_quote".to_string(),
        PoolConfig::new("bellman_ford")
            .with_num_workers(1)
            .with_min_hops(1)
            .with_max_hops(3)
            .with_timeout_ms(2_000),
    );

    let fynd = FyndRPCBuilder::new(
        Chain::Ethereum,
        pools,
        config.tycho_url.clone(),
        config.rpc_url.clone(),
        config.protocols.clone(),
    )?
    .http_listener(listener)
    .disable_tls()
    .min_tvl(0.0)
    .min_token_quality(0)
    .traded_n_days_ago(config.traded_n_days_ago)
    .worker_router_timeout(Duration::from_secs(5))
    .build()
    .context("build fynd RPC server")?;

    let handle = fynd.server_handle();
    let server_task = tokio::spawn(fynd.run());

    let result = async {
        println!("e2e_progress stage=client_build");
        let client = FyndClientBuilder::new(base_url.clone())
            .with_rpc_url(config.rpc_url.clone())
            .with_sender(config.sender)
            .build()
            .await
            .context("build e2e client")?;

        println!("e2e_progress stage=wait_for_health");
        wait_for_health(&client, config.health_timeout).await?;
        println!("e2e_progress stage=wait_for_quote");
        let quote = wait_for_quote(&client, &config, config.quote_timeout).await?;
        let route = quote.route().context("quote missing route")?;
        assert!(
            !route.swaps().is_empty(),
            "quote returned success without any swaps"
        );
        for (idx, swap) in route.swaps().iter().enumerate() {
            println!(
                "route_swap[{}] protocol={} component_id={} token_in=0x{} token_out=0x{} amount_in={} amount_out={} gas_estimate={}",
                idx,
                swap.protocol(),
                swap.component_id(),
                alloy::primitives::hex::encode(swap.token_in().as_ref()),
                alloy::primitives::hex::encode(swap.token_out().as_ref()),
                swap.amount_in(),
                swap.amount_out(),
                swap.gas_estimate(),
            );
        }
        assert!(
            quote.transaction().is_some(),
            "quote must include transaction calldata"
        );

        Ok(())
    }
    .await;

    handle.stop(true).await;
    let _ = server_task.await;

    result
}

#[tokio::test]
#[ignore = "requires local Tycho plus a live RPC endpoint"]
async fn quote_settles_within_encoded_bounds_at_quote_block() -> Result<()> {
    init_test_tracing();
    let config = load_test_config()?;
    println!(
        "e2e_exec_config tycho_url={} rpc_url={} protocols={} token_in=0x{} token_out=0x{} amount_in={} traded_n_days_ago={} health_timeout_secs={} quote_timeout_secs={}",
        config.tycho_url,
        config.rpc_url,
        config.protocols.join(","),
        alloy::primitives::hex::encode(config.token_in.as_ref()),
        alloy::primitives::hex::encode(config.token_out.as_ref()),
        config.amount_in,
        config.traded_n_days_ago,
        config.health_timeout.as_secs(),
        config.quote_timeout.as_secs(),
    );
    ensure_tycho_reachable(&config.tycho_url)?;
    let listener = TcpListener::bind("127.0.0.1:0").context("bind ephemeral HTTP listener")?;
    listener
        .set_nonblocking(true)
        .context("set listener nonblocking")?;
    let addr = listener.local_addr().context("read listener addr")?;
    let base_url = format!("http://{addr}");

    let mut pools = HashMap::new();
    pools.insert(
        "e2e_quote".to_string(),
        PoolConfig::new("bellman_ford")
            .with_num_workers(1)
            .with_min_hops(1)
            .with_max_hops(3)
            .with_timeout_ms(2_000),
    );

    let fynd = FyndRPCBuilder::new(
        Chain::Ethereum,
        pools,
        config.tycho_url.clone(),
        config.rpc_url.clone(),
        config.protocols.clone(),
    )?
    .http_listener(listener)
    .disable_tls()
    .min_tvl(0.0)
    .min_token_quality(0)
    .traded_n_days_ago(config.traded_n_days_ago)
    .worker_router_timeout(Duration::from_secs(5))
    .build()
    .context("build fynd RPC server")?;

    let handle = fynd.server_handle();
    let server_task = tokio::spawn(fynd.run());

    let result = async {
        println!("e2e_exec_progress stage=client_build");
        let client = FyndClientBuilder::new(base_url.clone())
            .with_rpc_url(config.rpc_url.clone())
            .with_sender(config.sender)
            .build()
            .await
            .context("build e2e client")?;

        println!("e2e_exec_progress stage=wait_for_health");
        wait_for_health(&client, config.health_timeout).await?;
        println!("e2e_exec_progress stage=wait_for_quote");
        let quote = wait_for_quote(&client, &config, config.quote_timeout).await?;
        let route = quote.route().context("quote missing route")?;
        assert!(
            !route.swaps().is_empty(),
            "quote returned success without any swaps"
        );

        let provider: RootProvider<Ethereum> = ProviderBuilder::default().connect_http(
            config
                .rpc_url
                .parse()
                .with_context(|| format!("invalid RPC URL: {}", config.rpc_url))?,
        );
        for (idx, swap) in route.swaps().iter().enumerate() {
            println!(
                "e2e_exec_route_swap[{}] protocol={} component_id={} token_in=0x{} token_out=0x{} amount_in={} amount_out={} gas_estimate={}",
                idx,
                swap.protocol(),
                swap.component_id(),
                alloy::primitives::hex::encode(swap.token_in().as_ref()),
                alloy::primitives::hex::encode(swap.token_out().as_ref()),
                swap.amount_in(),
                swap.amount_out(),
                swap.gas_estimate(),
            );

            print_uniswap_v3_chain_state(&provider, swap, quote.block().number()).await?;
            print_uniswap_v2_chain_state(&provider, swap, quote.block().number()).await?;
            print_uniswap_v2_tycho_state(&config.tycho_url, swap, quote.block().number()).await?;
        }

        let fee_breakdown = quote
            .fee_breakdown()
            .context("quote missing fee breakdown despite encoding options")?;
        let expected_post_fee_amount =
            quote.amount_out() - fee_breakdown.router_fee() - fee_breakdown.client_fee();
        let min_received = fee_breakdown.min_amount_received();

        println!(
            "e2e_exec_fee_breakdown quote_block={} quote_amount_out={} router_fee={} client_fee={} max_slippage={} min_received={}",
            quote.block().number(),
            quote.amount_out(),
            fee_breakdown.router_fee(),
            fee_breakdown.client_fee(),
            fee_breakdown.max_slippage(),
            min_received,
        );

        let raw_amount_out =
            dry_run_amount_out_at_quote_block(&provider, &client, &config, &quote).await?;
        let actual_post_fee_amount = raw_amount_out.clone();
        let post_fee_delta = if actual_post_fee_amount >= expected_post_fee_amount {
            &actual_post_fee_amount - &expected_post_fee_amount
        } else {
            &expected_post_fee_amount - &actual_post_fee_amount
        };

        println!(
            "e2e_exec_result quote_block={} quote_amount_out={} raw_amount_out={} expected_post_fee={} actual_post_fee={} min_received={} post_fee_delta={}",
            quote.block().number(),
            quote.amount_out(),
            raw_amount_out,
            expected_post_fee_amount,
            actual_post_fee_amount,
            min_received,
            post_fee_delta,
        );

        assert!(
            &actual_post_fee_amount >= min_received,
            "same-block post-fee output must satisfy encoded min_amount_out: actual_post_fee={}, min_received={}, expected_post_fee={}, raw_amount_out={}, quote_block={}",
            actual_post_fee_amount,
            min_received,
            expected_post_fee_amount,
            raw_amount_out,
            quote.block().number(),
        );

        Ok(())
    }
    .await;

    handle.stop(true).await;
    let _ = server_task.await;

    result
}

fn init_test_tracing() {
    TRACING_INIT.get_or_init(|| {
        ensure_localhost_bypasses_proxy();
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("info"));
        let _ = fmt()
            .with_env_filter(filter)
            .with_test_writer()
            .try_init();
    });
}


fn load_test_config() -> Result<TestConfig> {
    let tycho_url = env::var("FYND_E2E_TYCHO_URL").unwrap_or_else(|_| DEFAULT_TYCHO_URL.to_string());
    let rpc_url = env::var("FYND_E2E_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC_URL.to_string());
    let protocols = env::var("FYND_E2E_PROTOCOLS")
        .unwrap_or_else(|_| DEFAULT_PROTOCOLS.to_string())
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if protocols.is_empty() {
        bail!("FYND_E2E_PROTOCOLS resolved to an empty protocol list");
    }

    let token_in = parse_hex_bytes(
        &env::var("FYND_E2E_TOKEN_IN").unwrap_or_else(|_| DEFAULT_TOKEN_IN.to_string()),
    )?;
    let token_out = parse_hex_bytes(
        &env::var("FYND_E2E_TOKEN_OUT").unwrap_or_else(|_| DEFAULT_TOKEN_OUT.to_string()),
    )?;
    let amount_in = BigUint::from_str(
        &env::var("FYND_E2E_AMOUNT_IN").unwrap_or_else(|_| DEFAULT_AMOUNT_IN.to_string()),
    )
    .context("invalid FYND_E2E_AMOUNT_IN")?;
    let sender = Address::from_str(
        &env::var("FYND_E2E_SENDER").unwrap_or_else(|_| DEFAULT_SENDER.to_string()),
    )
    .context("invalid FYND_E2E_SENDER")?;
    let traded_n_days_ago = load_u64(
        "FYND_E2E_TRADED_N_DAYS_AGO",
        DEFAULT_TRADED_N_DAYS_AGO,
    )?;
    let health_timeout = load_timeout("FYND_E2E_HEALTH_TIMEOUT_SECS", 90)?;
    let quote_timeout = load_timeout("FYND_E2E_QUOTE_TIMEOUT_SECS", 120)?;

    Ok(TestConfig {
        tycho_url,
        rpc_url,
        protocols,
        token_in,
        token_out,
        amount_in,
        sender,
        traded_n_days_ago,
        health_timeout,
        quote_timeout,
    })
}

fn load_timeout(env_key: &str, default_secs: u64) -> Result<Duration> {
    Ok(Duration::from_secs(load_u64(env_key, default_secs)?))
}

fn load_u64(env_key: &str, default_value: u64) -> Result<u64> {
    let secs = match env::var(env_key) {
        Ok(value) => value
            .parse::<u64>()
            .with_context(|| format!("invalid {env_key}"))?,
        Err(_) => default_value,
    };
    Ok(secs)
}

fn ensure_tycho_reachable(tycho_url: &str) -> Result<()> {
    let address = tycho_url
        .strip_prefix("http://")
        .or_else(|| tycho_url.strip_prefix("https://"))
        .unwrap_or(tycho_url);
    TcpStream::connect(address)
        .with_context(|| format!("failed to connect to Tycho at {address}"))?;
    Ok(())
}

async fn wait_for_health(client: &FyndClient, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let started = Instant::now();
    let mut attempt: u64 = 0;
    loop {
        attempt += 1;
        match client.health().await {
            Ok(health) if health.healthy() => {
                println!(
                    "e2e_health attempt={} elapsed_secs={} healthy={} last_update_ms={} num_solver_pools={} derived_data_ready={} gas_price_age_ms={:?}",
                    attempt,
                    started.elapsed().as_secs(),
                    health.healthy(),
                    health.last_update_ms(),
                    health.num_solver_pools(),
                    health.derived_data_ready(),
                    health.gas_price_age_ms(),
                );
                return Ok(());
            }
            Ok(health) if Instant::now() >= deadline => {
                println!(
                    "e2e_health_timeout attempt={} elapsed_secs={} healthy={} last_update_ms={} num_solver_pools={} derived_data_ready={} gas_price_age_ms={:?}",
                    attempt,
                    started.elapsed().as_secs(),
                    health.healthy(),
                    health.last_update_ms(),
                    health.num_solver_pools(),
                    health.derived_data_ready(),
                    health.gas_price_age_ms(),
                );
                bail!("solver did not become healthy within {}s", timeout.as_secs());
            }
            Ok(health) => {
                println!(
                    "e2e_health attempt={} elapsed_secs={} healthy={} last_update_ms={} num_solver_pools={} derived_data_ready={} gas_price_age_ms={:?}",
                    attempt,
                    started.elapsed().as_secs(),
                    health.healthy(),
                    health.last_update_ms(),
                    health.num_solver_pools(),
                    health.derived_data_ready(),
                    health.gas_price_age_ms(),
                );
                sleep(POLL_INTERVAL).await;
            }
            Err(err) if Instant::now() >= deadline => {
                println!(
                    "e2e_health_error_timeout attempt={} elapsed_secs={} error={}",
                    attempt,
                    started.elapsed().as_secs(),
                    err,
                );
                bail!("solver did not become healthy within {}s", timeout.as_secs());
            }
            Err(err) => {
                println!(
                    "e2e_health_error attempt={} elapsed_secs={} error={}",
                    attempt,
                    started.elapsed().as_secs(),
                    err,
                );
                sleep(POLL_INTERVAL).await;
            }
        }
    }
}

async fn wait_for_quote(client: &FyndClient, config: &TestConfig, timeout: Duration) -> Result<fynd_client::Quote> {
    let deadline = Instant::now() + timeout;
    let started = Instant::now();
    let mut attempt: u64 = 0;

    loop {
        attempt += 1;
        let order = Order::new(
            config.token_in.clone(),
            config.token_out.clone(),
            config.amount_in.clone(),
            OrderSide::Sell,
            Bytes::copy_from_slice(config.sender.as_slice()),
            None,
        );
        let options = QuoteOptions::default()
            .with_timeout_ms(10_000)
            .with_encoding_options(EncodingOptions::new(0.005));

        let last_error = match client.quote(QuoteParams::new(order, options)).await {
            Ok(quote) if quote.transaction().is_some() => {
                println!(
                    "e2e_quote_ready attempt={} elapsed_secs={} status={:?} quote_block={} amount_out={}",
                    attempt,
                    started.elapsed().as_secs(),
                    quote.status(),
                    quote.block().number(),
                    quote.amount_out(),
                );
                return Ok(quote);
            }
            Ok(quote) => {
                format!(
                    "quote returned status {:?} without transaction",
                    quote.status()
                )
            }
            Err(err) => err.to_string(),
        };

        println!(
            "e2e_quote_wait attempt={} elapsed_secs={} error={}",
            attempt,
            started.elapsed().as_secs(),
            last_error,
        );

        if Instant::now() >= deadline {
            bail!("quote never succeeded within {}s: {}", timeout.as_secs(), last_error);
        }
        sleep(POLL_INTERVAL).await;
    }
}

async fn dry_run_amount_out_at_quote_block(
    provider: &RootProvider<Ethereum>,
    client: &FyndClient,
    config: &TestConfig,
    quote: &fynd_client::Quote,
) -> Result<BigUint> {
    let block = BlockId::number(quote.block().number());
    let tx = quote.transaction().context("quote has no transaction")?;
    let router = Address::try_from(tx.to().as_ref()).context("invalid router address")?;
    let token_in = Address::try_from(config.token_in.as_ref()).context("invalid token_in")?;

    let overrides = build_state_overrides(provider, config.sender, token_in, router).await?;
    let hints = SigningHints::default()
        .with_sender(config.sender)
        .with_max_fee_per_gas(MAX_FEE_PER_GAS_WEI)
        .with_max_priority_fee_per_gas(MAX_FEE_PER_GAS_WEI)
        .with_gas_limit(30_000_000);

    let payload = client
        .swap_payload(quote.clone(), &hints)
        .await
        .context("build swap payload for dry-run")?;
    let tx = match payload {
        SwapPayload::Fynd(payload) => payload.tx().clone(),
        SwapPayload::Turbine(_) => bail!("turbine execution is not supported in this e2e"),
    };
    let TypedTransaction::Eip1559(tx_eip1559) = tx else {
        bail!("quote payload used unsupported transaction type");
    };

    let mut req: alloy::rpc::types::TransactionRequest = tx_eip1559.clone().into();
    req.from = Some(config.sender);

    let return_data = provider
        .call(req.clone())
        .block(block)
        .overrides_opt(Some(overrides.clone()))
        .await
        .context("run same-block eth_call for quote dry-run")?;

    let _gas_used = provider
        .estimate_gas(req)
        .block(block)
        .overrides_opt(Some(overrides))
        .await
        .context("run same-block eth_estimateGas for quote dry-run")?;

    if return_data.len() < 32 {
        bail!("same-block dry-run returned less than 32 bytes");
    }
    Ok(BigUint::from_bytes_be(&return_data[0..32]))
}

async fn print_uniswap_v3_chain_state(
    provider: &RootProvider<Ethereum>,
    swap: &fynd_client::Swap,
    block_number: u64,
) -> Result<()> {
    if swap.protocol() != "uniswap_v3" {
        return Ok(());
    }

    let pool = Address::from_str(swap.component_id()).context("invalid uniswap_v3 component id")?;
    let block = BlockId::number(block_number);

    let slot0_result = provider
        .call(
            TransactionRequest {
                to: Some(TxKind::Call(pool)),
                input: TransactionInput::both(AlloyBytes::from(
                    IUniswapV3PoolStateProbe::slot0Call {}.abi_encode(),
                )),
                ..Default::default()
            },
        )
        .block(block)
        .await
        .context("read uniswap_v3 slot0 at quote block")?;
    let slot0 = IUniswapV3PoolStateProbe::slot0Call::abi_decode_returns(&slot0_result)
        .context("decode uniswap_v3 slot0 return data")?;

    let liquidity_result = provider
        .call(
            TransactionRequest {
                to: Some(TxKind::Call(pool)),
                input: TransactionInput::both(AlloyBytes::from(
                    IUniswapV3PoolStateProbe::liquidityCall {}.abi_encode(),
                )),
                ..Default::default()
            },
        )
        .block(block)
        .await
        .context("read uniswap_v3 liquidity at quote block")?;
    let liquidity = IUniswapV3PoolStateProbe::liquidityCall::abi_decode_returns(&liquidity_result)
        .context("decode uniswap_v3 liquidity return data")?;

    println!(
        "route_swap_usv3_chain_state pool={} block={} sqrt_price_x96={} tick={} liquidity={}",
        swap.component_id(),
        block_number,
        slot0.sqrtPriceX96,
        slot0.tick,
        liquidity,
    );

    Ok(())
}

async fn print_uniswap_v2_chain_state(
    provider: &RootProvider<Ethereum>,
    swap: &fynd_client::Swap,
    block_number: u64,
) -> Result<()> {
    if swap.protocol() != "uniswap_v2" {
        return Ok(());
    }

    let pool = Address::from_str(swap.component_id()).context("invalid uniswap_v2 component id")?;
    let block = BlockId::number(block_number);

    let token0_result = provider
        .call(
            TransactionRequest {
                to: Some(TxKind::Call(pool)),
                input: TransactionInput::both(AlloyBytes::from(
                    IUniswapV2PoolStateProbe::token0Call {}.abi_encode(),
                )),
                ..Default::default()
            },
        )
        .block(block)
        .await
        .context("read uniswap_v2 token0 at quote block")?;
    let token0 = IUniswapV2PoolStateProbe::token0Call::abi_decode_returns(&token0_result)
        .context("decode uniswap_v2 token0 return data")?;

    let token1_result = provider
        .call(
            TransactionRequest {
                to: Some(TxKind::Call(pool)),
                input: TransactionInput::both(AlloyBytes::from(
                    IUniswapV2PoolStateProbe::token1Call {}.abi_encode(),
                )),
                ..Default::default()
            },
        )
        .block(block)
        .await
        .context("read uniswap_v2 token1 at quote block")?;
    let token1 = IUniswapV2PoolStateProbe::token1Call::abi_decode_returns(&token1_result)
        .context("decode uniswap_v2 token1 return data")?;

    let reserves_result = provider
        .call(
            TransactionRequest {
                to: Some(TxKind::Call(pool)),
                input: TransactionInput::both(AlloyBytes::from(
                    IUniswapV2PoolStateProbe::getReservesCall {}.abi_encode(),
                )),
                ..Default::default()
            },
        )
        .block(block)
        .await
        .context("read uniswap_v2 reserves at quote block")?;
    let reserves = IUniswapV2PoolStateProbe::getReservesCall::abi_decode_returns(&reserves_result)
        .context("decode uniswap_v2 getReserves return data")?;

    println!(
        "route_swap_usv2_chain_state pool={} block={} token0={} token1={} reserve0={} reserve1={} block_timestamp_last={}",
        swap.component_id(),
        block_number,
        token0,
        token1,
        reserves.reserve0,
        reserves.reserve1,
        reserves.blockTimestampLast,
    );

    Ok(())
}

async fn print_uniswap_v2_tycho_state(
    tycho_url: &str,
    swap: &fynd_client::Swap,
    block_number: u64,
) -> Result<()> {
    if swap.protocol() != "uniswap_v2" {
        return Ok(());
    }

    let request = serde_json::json!({
        "chain": "ethereum",
        "protocol_system": "uniswap_v2",
        "protocol_ids": [swap.component_id()],
        "include_balances": false,
        "version": {
            "timestamp": null,
            "block": {
                "number": block_number as i64,
                "chain": "ethereum",
                "hash": null
            }
        },
        "pagination": {
            "page": 0,
            "page_size": 1
        }
    });

    let response = HttpClient::new()
        .post(format!("{}/v1/protocol_state", normalize_tycho_http_url(tycho_url)))
        .json(&request)
        .send()
        .await
        .with_context(|| format!("request tycho protocol_state for {}", swap.component_id()))?
        .error_for_status()
        .with_context(|| format!("tycho protocol_state returned error for {}", swap.component_id()))?;

    let body: TychoProtocolStateResponse = response
        .json()
        .await
        .with_context(|| format!("decode tycho protocol_state response for {}", swap.component_id()))?;

    let Some(state) = body.states.first() else {
        println!(
            "route_swap_usv2_tycho_state pool={} block={} missing=true",
            swap.component_id(),
            block_number,
        );
        return Ok(());
    };

    let reserve0 = parse_tycho_hex_u256(
        state
            .attributes
            .get("reserve0")
            .context("tycho protocol_state missing reserve0")?,
    )?;
    let reserve1 = parse_tycho_hex_u256(
        state
            .attributes
            .get("reserve1")
            .context("tycho protocol_state missing reserve1")?,
    )?;

    println!(
        "route_swap_usv2_tycho_state pool={} block={} component_id={} reserve0={} reserve1={}",
        swap.component_id(),
        block_number,
        state.component_id,
        reserve0,
        reserve1,
    );

    Ok(())
}

async fn build_state_overrides(
    provider: &RootProvider<Ethereum>,
    sender: Address,
    token_in: Address,
    router: Address,
) -> Result<StateOverride> {
    let (balance_pos, allowance_pos) = tokio::join!(
        find_balance_slot(provider, token_in, sender),
        find_allowance_slot(provider, token_in, sender, router),
    );
    let balance_pos = balance_pos.context("find token balance slot")?;
    let allowance_pos = allowance_pos.context("find token allowance slot")?;

    let huge = huge_balance();
    let balance_slot = balance_slot_at(sender, balance_pos);
    let allowance_slot = allowance_slot_at(sender, router, allowance_pos);

    let mut token_state_diff = alloy::primitives::map::B256HashMap::default();
    token_state_diff.insert(balance_slot, huge);
    token_state_diff.insert(allowance_slot, huge);

    let mut overrides = StateOverride::default();
    overrides.insert(
        token_in,
        AccountOverride { state_diff: Some(token_state_diff), ..Default::default() },
    );
    overrides.insert(
        sender,
        AccountOverride {
            balance: Some(biguint_to_u256(&BigUint::from_bytes_be(huge.as_slice()))?),
            ..Default::default()
        },
    );
    Ok(overrides)
}

fn parse_hex_bytes(value: &str) -> Result<Bytes> {
    let stripped = value.strip_prefix("0x").unwrap_or(value);
    let raw = alloy::primitives::hex::decode(stripped)
        .with_context(|| format!("invalid hex address '{value}'"))?;
    Ok(Bytes::from(raw))
}

fn huge_balance() -> B256 {
    B256::from(U256::MAX >> 1)
}

fn ensure_localhost_bypasses_proxy() {
    const LOCAL_BYPASS: &str = "127.0.0.1,localhost";

    for key in ["NO_PROXY", "no_proxy"] {
        let current = env::var(key).unwrap_or_default();
        if current
            .split(',')
            .map(str::trim)
            .any(|entry| entry == "127.0.0.1" || entry == "localhost")
        {
            continue;
        }

        let next = if current.trim().is_empty() {
            LOCAL_BYPASS.to_string()
        } else {
            format!("{current},{LOCAL_BYPASS}")
        };
        // Test-only setup before reqwest clients are created.
        unsafe { env::set_var(key, next) };
    }
}

fn normalize_tycho_http_url(tycho_url: &str) -> String {
    if tycho_url.starts_with("http://") || tycho_url.starts_with("https://") {
        tycho_url.trim_end_matches('/').to_string()
    } else {
        format!("http://{}", tycho_url.trim_end_matches('/'))
    }
}

fn parse_tycho_hex_u256(value: &str) -> Result<U256> {
    let stripped = value.strip_prefix("0x").unwrap_or(value);
    let raw = alloy::primitives::hex::decode(stripped)
        .with_context(|| format!("invalid tycho hex value '{value}'"))?;
    Ok(U256::from_be_slice(&raw))
}

fn biguint_to_u256(value: &BigUint) -> Result<U256> {
    let bytes = value.to_bytes_be();
    if bytes.len() > 32 {
        bail!("value does not fit into U256");
    }
    Ok(U256::from_be_slice(&bytes))
}
