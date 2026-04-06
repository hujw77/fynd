//! Expected output types for golden baseline comparison.

use std::path::Path;

use fynd_core::types::QuoteStatus;
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};

use crate::scenarios::TestScenario;

/// Expected quote result for a scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedOutput {
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
pub struct ExpectedScenario {
    /// The input scenario.
    pub scenario: TestScenario,
    /// Expected quote result.
    pub expected: ExpectedOutput,
}

/// Top-level expected output file: metadata + scenarios.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedFile {
    /// Recording and pipeline metadata.
    pub metadata: ExpectedMetadata,
    /// Scenario results.
    pub scenarios: Vec<ExpectedScenario>,
}

/// Metadata about the expected output generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedMetadata {
    /// Block number of the last recorded update.
    pub block_number: u64,
    /// Total registered pool count.
    pub num_pools: usize,
    /// Total registered token count.
    pub num_tokens: usize,
    /// Fynd crate version at generation time.
    pub fynd_version: String,
    /// Derived data metrics for deterministic replay assertions.
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

/// Load an expected output file from disk.
/// Returns `Ok(None)` if the file does not exist.
pub fn load_expected_file(path: &Path) -> anyhow::Result<Option<ExpectedFile>> {
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)?;
    let file = serde_json::from_str(&content)?;
    Ok(Some(file))
}

/// Standard path for expected outputs relative to fynd-core.
pub fn expected_file_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fynd-core/tests/fixtures/expected_outputs.json")
}
