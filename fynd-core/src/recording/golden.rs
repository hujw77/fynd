//! Golden output types shared between the `record-market` tool and integration tests.

use std::path::PathBuf;

use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_simulation::tycho_common::models::Address;

use crate::types::{Order, OrderSide, QuoteStatus};

/// A test scenario: a single token swap to quote.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestScenario {
    /// Input token address.
    pub token_in: Address,
    /// Output token address.
    pub token_out: Address,
    /// Swap amount in token's smallest unit.
    pub amount: BigUint,
    /// Buy or sell.
    pub side: OrderSide,
    /// Human-readable scenario name (e.g. `"WETH_to_USDC_500"`).
    pub name: String,
}

impl TestScenario {
    /// Convert to an [`Order`] for quoting.
    pub fn to_order(&self) -> Order {
        Order::new(
            self.token_in.clone(),
            self.token_out.clone(),
            self.amount.clone(),
            self.side,
            Address::zero(20),
        )
    }
}

/// Expected output for a scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoldenOutput {
    /// Quote status (success, no_route_found, etc.).
    pub status: QuoteStatus,
    /// Output amount after gas cost deduction.
    pub amount_out_net_gas: BigUint,
    /// Estimated gas cost in wei.
    pub gas_estimate: BigUint,
    /// Number of swaps (hops) in the route.
    pub num_swaps: usize,
    /// Wall-clock solve time in milliseconds.
    pub solve_time_ms: u64,
}

/// A scenario paired with its expected output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoldenScenario {
    /// The input scenario.
    pub scenario: TestScenario,
    /// Expected quote result.
    pub expected: GoldenOutput,
}

/// Top-level golden file: metadata + scenarios.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoldenFile {
    /// Recording and pipeline metadata.
    pub metadata: GoldenMetadata,
    /// Scenario results.
    pub scenarios: Vec<GoldenScenario>,
}

/// Metadata about the golden output generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoldenMetadata {
    /// Block number of the last recorded update.
    pub block_number: u64,
    /// Total registered pool count.
    pub num_pools: usize,
    /// Total registered token count.
    pub num_tokens: usize,
    /// Fynd crate version at generation time.
    pub fynd_version: String,
    /// Derived data metrics captured at golden generation time.
    /// Integration tests assert exact equality against these.
    #[serde(default)]
    pub derived_data: Option<DerivedDataMetrics>,
}

/// Snapshot of derived data counts for deterministic replay assertions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DerivedDataMetrics {
    /// Number of unique pools with at least one spot price.
    pub spot_price_pools: usize,
    /// Number of unique pools with at least one pool depth.
    pub pool_depth_pools: usize,
    /// Number of tokens with gas price conversions.
    pub token_prices: usize,
}

/// Standard path for golden outputs in the fixtures directory.
pub fn golden_file_path() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures/integration/golden_outputs.json")
}

/// Load the golden outputs file.
/// Returns `None` if the file doesn't exist (first run before recording).
pub fn load_golden_file() -> Option<GoldenFile> {
    let path = golden_file_path();
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&path).expect("failed to read golden_outputs.json");
    Some(serde_json::from_str(&content).expect("failed to parse golden_outputs.json"))
}

/// Load test scenarios from `pairs.json` (the canonical source of trading pairs).
pub fn load_test_scenarios() -> Vec<TestScenario> {
    let content = include_str!("../../../tools/benchmark/src/pairs.json");
    let raw: serde_json::Value =
        serde_json::from_str(content).expect("failed to parse pairs.json");

    let tokens: std::collections::HashMap<String, (Address, u32)> = raw["tokens"]
        .as_array()
        .expect("tokens should be an array")
        .iter()
        .map(|t| {
            let symbol = t["symbol"].as_str().expect("symbol").to_string();
            let address: Address = t["address"]
                .as_str()
                .expect("address")
                .parse()
                .expect("valid address");
            let decimals = t["decimals"].as_u64().expect("decimals") as u32;
            (symbol, (address, decimals))
        })
        .collect();

    raw["pairs"]
        .as_array()
        .expect("pairs should be an array")
        .iter()
        .map(|pair| {
            let token_in_sym = pair["token_in"].as_str().expect("token_in");
            let token_out_sym = pair["token_out"].as_str().expect("token_out");
            let (token_in, decimals_in) = tokens
                .get(token_in_sym)
                .unwrap_or_else(|| panic!("unknown token: {token_in_sym}"));
            let (token_out, _) = tokens
                .get(token_out_sym)
                .unwrap_or_else(|| panic!("unknown token: {token_out_sym}"));

            let human_amount = pair["amounts"][0].as_f64().expect("amount");
            let raw_amount = human_amount * 10_f64.powi(*decimals_in as i32);
            let amount = BigUint::from(raw_amount as u128);

            TestScenario {
                name: format!("{token_in_sym}_to_{token_out_sym}_{human_amount}"),
                token_in: token_in.clone(),
                token_out: token_out.clone(),
                amount,
                side: OrderSide::Sell,
            }
        })
        .collect()
}
