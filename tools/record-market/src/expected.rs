use std::time::Duration;

use fynd_core::{QuoteOptions, QuoteRequest, QuoteStatus, Solver};
use fynd_test_fixtures::{
    DerivedDataMetrics, ExpectedFile, ExpectedMetadata, ExpectedOutput, ExpectedScenario,
    MarketRecording,
};
use num_bigint::BigUint;
use tycho_simulation::tycho_common::models::Chain;

/// Generate expected outputs by replaying a recording through the full pipeline.
pub async fn generate_expected_outputs(
    recording: MarketRecording,
    pools_toml: &str,
) -> anyhow::Result<ExpectedFile> {
    let gas_price = recording
        .metadata
        .gas_price_as_biguint();
    let pools = fynd_test_fixtures::parse_pools_toml(pools_toml)?;

    let solver = Solver::from_recording(Chain::Ethereum, recording.updates, pools, gas_price)
        .await
        .map_err(|e| anyhow::anyhow!("failed to build solver from recording: {e}"))?;

    solver
        .wait_until_ready(Duration::from_secs(120))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    tracing::info!("pipeline ready, running scenarios...");

    let pairs_json = include_str!("../../../tools/benchmark/src/pairs.json");
    let scenarios = fynd_test_fixtures::load_test_scenarios(pairs_json)?;
    let mut expected_scenarios = Vec::new();

    for scenario in &scenarios {
        let order = scenario.to_order();
        let request = QuoteRequest::new(vec![order], QuoteOptions::default());
        let result = solver.quote(request).await;

        let expected = match result {
            Ok(quote) => {
                let oq = &quote.orders()[0];
                ExpectedOutput {
                    status: oq.status(),
                    amount_out_net_gas: oq.amount_out_net_gas().clone(),
                    gas_estimate: oq.gas_estimate().clone(),
                    num_swaps: oq
                        .route()
                        .map(|r| r.hop_count())
                        .unwrap_or(0),
                    solve_time_ms: quote.solve_time_ms(),
                }
            }
            Err(_e) => ExpectedOutput {
                status: QuoteStatus::NoRouteFound,
                amount_out_net_gas: BigUint::ZERO,
                gas_estimate: BigUint::ZERO,
                num_swaps: 0,
                solve_time_ms: 0,
            },
        };

        tracing::info!(
            name = %scenario.name,
            status = ?expected.status,
            "scenario complete"
        );

        expected_scenarios.push(ExpectedScenario { scenario: scenario.clone(), expected });
    }

    let successful = expected_scenarios
        .iter()
        .filter(|s| s.expected.status == QuoteStatus::Success)
        .count();
    tracing::info!(total = expected_scenarios.len(), successful, "generation complete");

    // Capture derived data metrics
    let derived_metrics = {
        let derived_ref = solver.derived_data();
        let d = derived_ref.read().await;
        let spot_price_pools = d
            .spot_prices()
            .map(|sp| {
                sp.keys()
                    .map(|(id, _, _)| id.clone())
                    .collect::<std::collections::HashSet<_>>()
                    .len()
            })
            .unwrap_or(0);
        let pool_depth_pools = d
            .pool_depths()
            .map(|pd| {
                pd.keys()
                    .map(|(id, _, _)| id.clone())
                    .collect::<std::collections::HashSet<_>>()
                    .len()
            })
            .unwrap_or(0);
        let token_prices = d
            .token_prices()
            .map(|tp| tp.len())
            .unwrap_or(0);
        DerivedDataMetrics { spot_price_pools, pool_depth_pools, token_prices }
    };

    let market_ref = solver.market_data();
    let market = market_ref.read().await;
    // Single source of truth for the block number: the replayed market state,
    // matching what Solver::from_recording injects into the gas price.
    let block_number = market
        .last_updated()
        .map(|block| block.number())
        .unwrap_or(0);
    let num_pools = market.component_topology().len();
    let num_tokens = market.token_registry_ref().len();
    drop(market);

    solver.shutdown();

    Ok(ExpectedFile {
        metadata: ExpectedMetadata {
            block_number,
            num_pools,
            num_tokens,
            fynd_version: env!("CARGO_PKG_VERSION").to_string(),
            derived_data: Some(derived_metrics),
        },
        scenarios: expected_scenarios,
    })
}
