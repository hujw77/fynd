use std::collections::HashMap;

use fynd_core::PoolConfig;
use fynd_test_fixtures::{expected::load_expected_file, load_test_scenarios};

use crate::harness::TestHarness;

fn expected_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/expected_outputs.json")
}

fn scenarios() -> Vec<fynd_test_fixtures::TestScenario> {
    let pairs_json = include_str!("../../../tools/benchmark/src/pairs.json");
    load_test_scenarios(pairs_json).expect("failed to load test scenarios")
}

fn max_pool_timeout_ms() -> u64 {
    let toml_content = include_str!("../../../worker_pools.toml");

    #[derive(serde::Deserialize)]
    struct PoolsFile {
        pools: HashMap<String, PoolConfig>,
    }

    let config: PoolsFile =
        toml::from_str(toml_content).expect("failed to parse worker_pools.toml");
    config
        .pools
        .values()
        .map(|p| p.timeout_ms())
        .max()
        .unwrap_or(5000)
}

/// P95 solve time should stay within a reasonable multiple of the expected baseline.
#[tokio::test]
async fn test_solve_time_p95_within_threshold() {
    let harness = TestHarness::from_fixture().await;
    let expected_file = load_expected_file(&expected_path())
        .expect("I/O error")
        .expect("expected_outputs.json required");
    let scenarios = scenarios();

    let mut solve_times_ms: Vec<u64> = Vec::new();

    for scenario in &scenarios {
        let order = scenario.to_order();
        if let Ok(quote) = harness.quote(vec![order]).await {
            solve_times_ms.push(quote.solve_time_ms());
        }
    }

    assert!(!solve_times_ms.is_empty(), "no successful solves to measure");

    solve_times_ms.sort_unstable();
    let p95_idx = (solve_times_ms.len() as f64 * 0.95).ceil() as usize - 1;
    let p95 = solve_times_ms[p95_idx.min(solve_times_ms.len() - 1)];

    let mut expected_times: Vec<u64> = expected_file
        .scenarios
        .iter()
        .map(|es| es.expected.solve_time_ms)
        .collect();
    expected_times.sort_unstable();
    let expected_p95_idx = (expected_times.len() as f64 * 0.95).ceil() as usize - 1;
    let expected_p95 = expected_times[expected_p95_idx.min(expected_times.len() - 1)];

    let relative_threshold = expected_p95.saturating_mul(4);
    let absolute_threshold = max_pool_timeout_ms();
    let threshold = relative_threshold.max(absolute_threshold);

    assert!(
        p95 <= threshold,
        "P95 solve time {}ms exceeds threshold {}ms \
         (expected P95: {}ms, 4x: {}ms, absolute cap: {}ms)",
        p95,
        threshold,
        expected_p95,
        relative_threshold,
        absolute_threshold
    );
}

/// No individual solve should exceed the router timeout (max pool timeout + margin).
#[tokio::test]
async fn test_no_solve_exceeds_absolute_cap() {
    let harness = TestHarness::from_fixture().await;
    let scenarios = scenarios();
    let absolute_cap_ms = max_pool_timeout_ms() + 1000;

    let mut violations = Vec::new();
    for scenario in &scenarios {
        let order = scenario.to_order();
        if let Ok(quote) = harness.quote(vec![order]).await {
            if quote.solve_time_ms() > absolute_cap_ms {
                violations.push(format!(
                    "{}: {}ms exceeds {}ms cap",
                    scenario.name,
                    quote.solve_time_ms(),
                    absolute_cap_ms
                ));
            }
        }
    }

    assert!(violations.is_empty(), "solve time violations:\n{}", violations.join("\n"));
}
