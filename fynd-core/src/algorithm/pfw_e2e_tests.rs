//! End-to-end tests for PathFrankWolfeAlgorithm.
//!
//! Each test runs the full pipeline: order → algorithm → RouteResult → Solution → encode →
//! calldata. Comparison benchmarks validate that split routing beats single-path routing on
//! sufficiently large trades.

use std::{sync::Arc, time::Duration};

use num_bigint::BigUint;
use num_traits::ToPrimitive;
use tokio::sync::RwLock;
use tycho_execution::encoding::{
    evm::swap_encoder::swap_encoder_registry::SwapEncoderRegistry, models::Solution,
};
use tycho_simulation::tycho_common::{
    models::{token::Token, Chain},
    simulation::protocol_sim::{Price, ProtocolSim},
    Bytes,
};

use super::{
    path_frank_wolfe::{PathFrankWolfeAlgorithm, PathFrankWolfeConfig},
    test_utils::{setup_market_unweighted, token, ConstantProductSim},
    Algorithm, AlgorithmConfig, BellmanFordAlgorithm,
};
use crate::{
    derived::{types::TokenGasPrices, DerivedData, SharedDerivedDataRef},
    encoding::encoder::Encoder,
    graph::GraphManager,
    types::{
        quote::{OrderQuote, RouteResult},
        BlockInfo, OrderSide, QuoteStatus,
    },
};

fn derived_with_token_prices(tokens: &[&Token]) -> SharedDerivedDataRef {
    let mut prices = TokenGasPrices::new();
    let price = Price::new(BigUint::from(1u64), BigUint::from(1_000_000u64));
    for token in tokens {
        prices.insert(token.address.clone(), price.clone());
    }
    let mut derived = DerivedData::new();
    derived.set_token_prices(prices, vec![], 1, true);
    Arc::new(RwLock::new(derived))
}

fn pfw_algo(max_hops: usize, pfw_config: PathFrankWolfeConfig) -> PathFrankWolfeAlgorithm {
    PathFrankWolfeAlgorithm::new(
        AlgorithmConfig::new(1, max_hops, Duration::from_millis(5000), None).unwrap(),
        pfw_config,
    )
}

fn bf_algo(max_hops: usize) -> BellmanFordAlgorithm {
    BellmanFordAlgorithm::with_config(
        AlgorithmConfig::new(1, max_hops, Duration::from_millis(5000), None).unwrap(),
    )
}

fn order(token_in: &Token, token_out: &Token, amount: u128, side: OrderSide) -> crate::Order {
    let sender = tycho_simulation::tycho_common::models::Address::from([0xAAu8; 20]);
    crate::Order::new(
        token_in.address.clone(),
        token_out.address.clone(),
        BigUint::from(amount),
        side,
        sender,
    )
    .with_id("e2e-test-order".to_string())
}

fn cp(reserve: u64) -> Box<dyn ProtocolSim> {
    Box::new(ConstantProductSim {
        reserve_0: BigUint::from(reserve),
        reserve_1: BigUint::from(reserve),
        gas: 50_000,
    })
}

/// Converts a `RouteResult` into an `OrderQuote` with the route attached, mirroring the worker's
/// conversion logic.
fn route_result_to_order_quote(
    result: RouteResult,
    order: &crate::Order,
    algo_name: &str,
) -> OrderQuote {
    use num_traits::Zero;
    let amount_out_net_gas = result
        .net_amount_out()
        .to_biguint()
        .unwrap_or(BigUint::ZERO);
    let gas_price = result.gas_price().clone();
    let route = result.into_route();
    let gas_estimate = route.total_gas();
    let amount_in = order.amount().clone();
    let amount_out = route
        .swaps()
        .last()
        .map(|s: &crate::Swap| s.amount_out().clone())
        .unwrap_or_else(BigUint::zero);

    OrderQuote::new(
        order.id().to_string(),
        QuoteStatus::Success,
        amount_in,
        amount_out,
        gas_estimate,
        amount_out_net_gas,
        BlockInfo::new(1, "0x00".to_string(), 0),
        algo_name.to_string(),
        Bytes::from(order.sender().as_ref()),
        Bytes::from(order.effective_receiver().as_ref()),
        "1".to_string(),
    )
    .with_route(route)
    .with_gas_price(gas_price)
}

// ==================== 1. Correctness Test ====================

#[tokio::test]
async fn test_path_frank_wolfe_e2e_produces_valid_calldata() {
    // Diamond graph: A→B direct + A→X→B via intermediate, large trade with clear price impact.
    //
    //        ┌────[pool_ab: 100k]──────────────────┐
    //   A ───┤                                     ├─── B
    //        └─[pool_ax: 200k]─ X ─[pool_xb: 200k]─┘
    //
    // Component IDs must be valid Ethereum addresses for the USV2 swap encoder.
    let token_a = token(0x01, "A");
    let token_b = token(0x02, "B");
    let token_x = token(0x03, "X");
    let pool_ab = "0xaB00000000000000000000000000000000000001";
    let pool_ax = "0xaB00000000000000000000000000000000000002";
    let pool_xb = "0xaB00000000000000000000000000000000000003";

    let cp_low_gas = |reserve: u64| -> Box<dyn ProtocolSim> {
        Box::new(ConstantProductSim {
            reserve_0: BigUint::from(reserve),
            reserve_1: BigUint::from(reserve),
            gas: 20_000,
        })
    };
    let (market, graph_manager) = setup_market_unweighted(vec![
        (pool_ab, &token_a, &token_b, cp(100_000)),
        (pool_ax, &token_a, &token_x, cp_low_gas(200_000)),
        (pool_xb, &token_x, &token_b, cp_low_gas(200_000)),
    ]);

    let algo = pfw_algo(
        3,
        PathFrankWolfeConfig {
            max_paths: 4,
            max_probe: 0.25,
            min_split: 0.01,
            ..Default::default()
        },
    );
    let derived = derived_with_token_prices(&[&token_a, &token_b, &token_x]);
    let ord = order(&token_a, &token_b, 50_000, OrderSide::Sell);

    // Step 1: Algorithm produces a RouteResult.
    let result = algo
        .find_best_route(graph_manager.graph(), market, None, Some(derived), &ord)
        .await
        .expect("algorithm should find a route");

    // Step 2: Route validates.
    result
        .route()
        .validate()
        .expect("route must pass validation");
    assert!(!result.route().swaps().is_empty(), "route must have at least one swap");

    // Step 3: Convert to OrderQuote (mirrors worker logic).
    let order_quote = route_result_to_order_quote(result, &ord, "path_frank_wolfe");

    // Step 4: Convert to Solution (tycho-execution).
    let solution =
        Solution::try_from(&order_quote).expect("OrderQuote → Solution conversion must succeed");
    assert!(!solution.swaps().is_empty(), "solution must have swaps");
    assert_eq!(
        *solution.token_in(),
        Bytes::from(token_a.address.as_ref()),
        "solution token_in must match order"
    );
    assert_eq!(
        *solution.token_out(),
        Bytes::from(token_b.address.as_ref()),
        "solution token_out must match order"
    );

    // Step 5: Encode into calldata.
    let registry = SwapEncoderRegistry::new(Chain::Ethereum)
        .add_default_encoders(None)
        .expect("swap encoder registry should build");
    let encoder = Encoder::new(Chain::Ethereum, registry).expect("encoder should build");
    let encoding_options = crate::EncodingOptions::new(0.01);

    let encoded = encoder
        .encode(vec![order_quote], encoding_options)
        .await
        .expect("encoding must succeed");

    assert_eq!(encoded.len(), 1);
    let tx = encoded[0]
        .transaction()
        .expect("encoded quote must have a transaction");
    assert!(!tx.data().is_empty(), "encode_tycho_router_call must produce non-empty bytes");

    // Step 6: Decode calldata and verify token_in, token_out, amount_in.
    // ABI layout after 4-byte selector: amount_in (U256), token_in (address), token_out (address).
    let calldata = tx.data();
    assert!(
        calldata.len() >= 100,
        "calldata too short to contain amount_in + token_in + token_out"
    );

    let encoded_amount_in = BigUint::from_bytes_be(&calldata[4..36]);
    assert_eq!(
        encoded_amount_in,
        BigUint::from(50_000u64),
        "calldata amount_in must match order amount"
    );

    let encoded_token_in = &calldata[48..68];
    assert_eq!(
        encoded_token_in,
        token_a.address.as_ref(),
        "calldata token_in must match order token_in"
    );

    let encoded_token_out = &calldata[80..100];
    assert_eq!(
        encoded_token_out,
        token_b.address.as_ref(),
        "calldata token_out must match order token_out"
    );
}

// ==================== 2. Comparison Benchmark ====================

#[tokio::test]
async fn test_split_vs_single_route_comparison() {
    // Four parallel pools of varying depths:
    //
    //        ┌──[P1: 200k]──┐
    //        ├──[P2: 150k]──┤
    //   A ───┼──[P3: 100k]──┼─── B
    //        └──[P4:  50k]──┘
    let token_a = token(0x01, "A");
    let token_b = token(0x02, "B");

    let pools = [
        ("P1", &token_a, &token_b, cp(200_000)),
        ("P2", &token_a, &token_b, cp(150_000)),
        ("P3", &token_a, &token_b, cp(100_000)),
        ("P4", &token_a, &token_b, cp(50_000)),
    ];

    let pfw = pfw_algo(
        2,
        PathFrankWolfeConfig {
            max_paths: 6,
            max_probe: 0.5,
            min_split: 0.01,
            line_search_evals: 16,
        },
    );
    let bf = bf_algo(2);

    // Test at 3%, 10%, and 20% of total pool liquidity (500k).
    let trade_sizes: [(u128, &str); 3] = [
        (15_000, "3% of pool liquidity"),
        (50_000, "10% of pool liquidity"),
        (100_000, "20% of pool liquidity"),
    ];

    for (trade_amount, description) in trade_sizes {
        let (market_pfw, gm_pfw) = setup_market_unweighted(
            pools
                .iter()
                .map(|(id, t_in, t_out, sim)| (*id, *t_in, *t_out, sim.clone_box()))
                .collect(),
        );
        let (market_bf, gm_bf) = setup_market_unweighted(
            pools
                .iter()
                .map(|(id, t_in, t_out, sim)| (*id, *t_in, *t_out, sim.clone_box()))
                .collect(),
        );

        let derived_pfw = derived_with_token_prices(&[&token_a, &token_b]);
        let derived_bf = derived_with_token_prices(&[&token_a, &token_b]);
        let ord = order(&token_a, &token_b, trade_amount, OrderSide::Sell);

        let pfw_start = std::time::Instant::now();
        let pfw_result = pfw
            .find_best_route(gm_pfw.graph(), market_pfw, None, Some(derived_pfw), &ord)
            .await
            .expect("PFW should find a route");
        let pfw_time = pfw_start.elapsed();

        let bf_start = std::time::Instant::now();
        let bf_result = bf
            .find_best_route(gm_bf.graph(), market_bf, None, Some(derived_bf), &ord)
            .await
            .expect("BF should find a route");
        let bf_time = bf_start.elapsed();

        let pfw_output = pfw_result.net_amount_out().clone();
        let bf_output = bf_result.net_amount_out().clone();

        // Core property: PFW must produce output >= BF.
        assert!(
            pfw_output >= bf_output,
            "[{description}] PFW output ({pfw_output}) must be >= BF output ({bf_output})"
        );

        // Log for manual review.
        let improvement_pct =
            if let (Some(pfw_f), Some(bf_f)) = (pfw_output.to_f64(), bf_output.to_f64()) {
                if bf_f > 0.0 {
                    (pfw_f - bf_f) / bf_f * 100.0
                } else {
                    0.0
                }
            } else {
                0.0
            };

        eprintln!(
            "[{description}] trade={trade_amount}, \
             PFW={pfw_output} ({} swaps, {pfw_time:?}), \
             BF={bf_output} ({} swaps, {bf_time:?}), \
             improvement={improvement_pct:.2}%",
            pfw_result.route().swaps().len(),
            bf_result.route().swaps().len(),
        );
    }
}

// ==================== 3. Property Test ====================

mod proptest_tests {
    use proptest::prelude::*;

    use super::*;

    /// Runs a single comparison at the given trade fraction and returns (pfw_output, bf_output).
    ///
    /// Diamond graph: A→B direct (shallow) + A→Y→B via deep intermediate.
    /// The indirect path has 4x the reserves, making it competitive despite the double hop.
    ///
    ///        ┌─────[P_ab: 500k]──────────────┐
    ///   A ───┤                                ├─── B
    ///        └─[P_ay: 2M]─ Y ─[P_yb: 2M]────┘
    async fn compare_at_fraction(trade_fraction: f64) -> (i128, i128) {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_y = token(0x04, "Y");

        let direct_reserve: u64 = 500_000;
        let indirect_reserve: u64 = 2_000_000;
        let trade_amount = (direct_reserve as f64 * trade_fraction) as u128;
        if trade_amount == 0 {
            return (0, 0);
        }

        let pools = [
            ("P_ab", &token_a, &token_b, cp(direct_reserve)),
            ("P_ay", &token_a, &token_y, cp(indirect_reserve)),
            ("P_yb", &token_y, &token_b, cp(indirect_reserve)),
        ];

        let (market_pfw, gm_pfw) = setup_market_unweighted(
            pools
                .iter()
                .map(|(id, t_in, t_out, sim)| (*id, *t_in, *t_out, sim.clone_box()))
                .collect(),
        );
        let (market_bf, gm_bf) = setup_market_unweighted(
            pools
                .iter()
                .map(|(id, t_in, t_out, sim)| (*id, *t_in, *t_out, sim.clone_box()))
                .collect(),
        );

        let derived_pfw = derived_with_token_prices(&[&token_a, &token_b, &token_y]);
        let derived_bf = derived_with_token_prices(&[&token_a, &token_b, &token_y]);

        let ord = order(&token_a, &token_b, trade_amount, OrderSide::Sell);

        let pfw = pfw_algo(
            3,
            PathFrankWolfeConfig {
                max_paths: 4,
                max_probe: 0.5,
                min_split: 0.01,
                ..Default::default()
            },
        );
        let bf = bf_algo(3);

        let pfw_result = pfw
            .find_best_route(gm_pfw.graph(), market_pfw, None, Some(derived_pfw), &ord)
            .await;
        let bf_result = bf
            .find_best_route(gm_bf.graph(), market_bf, None, Some(derived_bf), &ord)
            .await;

        let pfw_out = pfw_result
            .map(|r| {
                r.net_amount_out()
                    .to_i128()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let bf_out = bf_result
            .map(|r| {
                r.net_amount_out()
                    .to_i128()
                    .unwrap_or(0)
            })
            .unwrap_or(0);

        (pfw_out, bf_out)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(20))]

        #[test]
        fn test_split_at_least_as_good_as_single_route(
            // Trade fraction in [0.1%, 50%] of pool liquidity.
            trade_fraction in 0.001f64..0.5f64,
        ) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let (pfw_out, bf_out) = rt.block_on(compare_at_fraction(trade_fraction));

            prop_assert!(
                pfw_out >= bf_out,
                "PFW output ({}) must be >= BF output ({}) at trade_fraction={:.4}",
                pfw_out,
                bf_out,
                trade_fraction,
            );
        }
    }
}
