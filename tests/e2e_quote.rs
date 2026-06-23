use std::{
    collections::HashMap,
    env,
    net::{TcpListener, TcpStream},
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
use serde::Deserialize;
use tokio::time::sleep;
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
}

const DEFAULT_TYCHO_URL: &str = "127.0.0.1:4242";
const DEFAULT_RPC_URL: &str = "https://rpc.mevblocker.io";
const DEFAULT_PROTOCOLS: &str = "uniswap_v3";
const DEFAULT_TOKEN_IN: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
const DEFAULT_TOKEN_OUT: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
const DEFAULT_AMOUNT_IN: &str = "1000000000000000";
const DEFAULT_SENDER: &str = "0x000000000000000000000000000000000000beef";
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_FEE_PER_GAS_WEI: u128 = 1_000_000_000_000;

#[derive(Debug)]
struct TestConfig {
    tycho_url: String,
    rpc_url: String,
    protocols: Vec<String>,
    token_in: Bytes,
    token_out: Bytes,
    amount_in: BigUint,
    sender: Address,
    health_timeout: Duration,
    component_timeout: Duration,
    quote_timeout: Duration,
}

#[derive(Debug, Deserialize)]
struct DebugComponentsResponse {
    total_components: usize,
}

#[tokio::test]
#[ignore = "requires local Tycho plus a live RPC endpoint"]
async fn quote_settles_within_encoded_bounds_at_quote_block() -> Result<()> {
    let config = load_test_config()?;
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
    .worker_router_timeout(Duration::from_secs(5))
    .build()
    .context("build fynd RPC server")?;

    let handle = fynd.server_handle();
    let server_task = tokio::spawn(fynd.run());

    let result = async {
        let client = FyndClientBuilder::new(base_url.clone())
            .with_rpc_url(config.rpc_url.clone())
            .with_sender(config.sender)
            .build()
            .await
            .context("build e2e client")?;

        wait_for_health(&client, config.health_timeout).await?;
        wait_for_components(&base_url, config.component_timeout).await?;

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
            "quote must include calldata for dry-run execution"
        );

        let provider: RootProvider<Ethereum> = ProviderBuilder::default().connect_http(
            config
                .rpc_url
                .parse()
                .with_context(|| format!("invalid RPC URL: {}", config.rpc_url))?,
        );
        if let Some(first_swap) = route.swaps().first() {
            print_uniswap_v3_chain_state(&provider, first_swap, quote.block().number()).await?;
        }
        let raw_amount_out =
            dry_run_amount_out_at_quote_block(&provider, &client, &config, &quote).await?;
        let fee_breakdown = quote
            .fee_breakdown()
            .context("quote missing fee breakdown despite encoding options")?;
        let expected_post_fee_amount =
            quote.amount_out() - fee_breakdown.router_fee() - fee_breakdown.client_fee();
        let min_received = fee_breakdown.min_amount_received();
        let actual_post_fee_amount = raw_amount_out.clone();
        let post_fee_delta = if actual_post_fee_amount >= expected_post_fee_amount {
            &actual_post_fee_amount - &expected_post_fee_amount
        } else {
            &expected_post_fee_amount - &actual_post_fee_amount
        };

        println!(
            "quote_block={} quote_amount_out={} raw_amount_out={} expected_post_fee={} actual_post_fee={} min_received={} post_fee_delta={}",
            quote.block().number(),
            quote.amount_out(),
            raw_amount_out,
            expected_post_fee_amount,
            actual_post_fee_amount,
            min_received,
            post_fee_delta,
        );

        // The router's return value is already post-fee settled output. The quote's amount_out
        // is solver-side pre-fee output, so small same-block differences should be judged against
        // the encoded min_amount_out rather than exact equality.
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
    let health_timeout = load_timeout("FYND_E2E_HEALTH_TIMEOUT_SECS", 90)?;
    let component_timeout = load_timeout("FYND_E2E_COMPONENT_TIMEOUT_SECS", 120)?;
    let quote_timeout = load_timeout("FYND_E2E_QUOTE_TIMEOUT_SECS", 120)?;

    Ok(TestConfig {
        tycho_url,
        rpc_url,
        protocols,
        token_in,
        token_out,
        amount_in,
        sender,
        health_timeout,
        component_timeout,
        quote_timeout,
    })
}

fn load_timeout(env_key: &str, default_secs: u64) -> Result<Duration> {
    let secs = match env::var(env_key) {
        Ok(value) => value
            .parse::<u64>()
            .with_context(|| format!("invalid {env_key}"))?,
        Err(_) => default_secs,
    };
    Ok(Duration::from_secs(secs))
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
    loop {
        match client.health().await {
            Ok(health) if health.healthy() => return Ok(()),
            Ok(_) | Err(_) if Instant::now() >= deadline => {
                bail!("solver did not become healthy within {}s", timeout.as_secs());
            }
            Ok(_) | Err(_) => sleep(POLL_INTERVAL).await,
        }
    }
}

async fn wait_for_components(base_url: &str, timeout: Duration) -> Result<()> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build debug HTTP client")?;
    let deadline = Instant::now() + timeout;
    let url = format!("{base_url}/v1/debug/components?limit=1");

    loop {
        match http.get(&url).send().await {
            Ok(response) if response.status().is_success() => {
                let payload: DebugComponentsResponse = response
                    .json()
                    .await
                    .context("decode debug components response")?;
                if payload.total_components > 1 {
                    return Ok(());
                }
            }
            Ok(_) | Err(_) if Instant::now() >= deadline => {
                bail!("solver did not load market components within {}s", timeout.as_secs());
            }
            Ok(_) | Err(_) => {}
        }

        sleep(POLL_INTERVAL).await;
    }
}

async fn wait_for_quote(client: &FyndClient, config: &TestConfig, timeout: Duration) -> Result<fynd_client::Quote> {
    let deadline = Instant::now() + timeout;

    loop {
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
            Ok(quote) if quote.transaction().is_some() => return Ok(quote),
            Ok(quote) => format!("quote returned status {:?} without transaction", quote.status()),
            Err(err) => err.to_string(),
        };

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

    // Keep gas estimation on the quote block too, even though this test currently
    // asserts only the settled amount. This catches block-specific execution issues.
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

fn biguint_to_u256(value: &BigUint) -> Result<U256> {
    let bytes = value.to_bytes_be();
    if bytes.len() > 32 {
        bail!("value does not fit into U256");
    }
    Ok(U256::from_be_slice(&bytes))
}
