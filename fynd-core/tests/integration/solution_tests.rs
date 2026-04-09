use fynd_core::types::QuoteStatus;
use fynd_test_fixtures::{expected::load_expected_file, load_test_scenarios};

use crate::harness::TestHarness;

fn expected_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/expected_outputs.json")
}

fn scenarios() -> Vec<fynd_test_fixtures::TestScenario> {
    let pairs_json = include_str!("../../../tools/benchmark/src/pairs.json");
    load_test_scenarios(pairs_json).expect("failed to load test scenarios")
}

/// Scenarios that succeeded in expected baseline should also succeed in replay.
#[tokio::test]
async fn test_all_expected_pairs_return_solutions() {
    let harness = TestHarness::from_fixture().await;
    let expected_file = load_expected_file(&expected_path())
        .expect("I/O error")
        .expect("expected_outputs.json required");
    let expected_map: std::collections::HashMap<_, _> = expected_file
        .scenarios
        .iter()
        .map(|es| (es.scenario.name.clone(), &es.expected))
        .collect();

    let scenarios = scenarios();
    let mut failures = Vec::new();
    for scenario in &scenarios {
        let Some(expected) = expected_map.get(&scenario.name) else {
            continue;
        };
        if expected.status != QuoteStatus::Success {
            continue;
        }

        let order = scenario.to_order();
        let result = harness.quote(vec![order]).await;

        match result {
            Ok(quote) => {
                let oq = &quote.orders()[0];
                if oq.status() != QuoteStatus::Success {
                    failures.push(format!(
                        "{}: expected Success, got {:?}",
                        scenario.name,
                        oq.status()
                    ));
                }
            }
            Err(e) => {
                failures.push(format!("{}: solver error: {}", scenario.name, e));
            }
        }
    }

    assert!(failures.is_empty(), "solution availability failures:\n{}", failures.join("\n"));
}

/// Unknown tokens should return an error, not panic.
#[tokio::test]
async fn test_unknown_token_returns_error() {
    let harness = TestHarness::from_fixture().await;

    let fake_token: tycho_simulation::tycho_common::models::Address =
        "0x0000000000000000000000000000000000000BAD"
            .parse()
            .unwrap();
    let expected_file = load_expected_file(&expected_path())
        .expect("I/O error")
        .expect("expected_outputs.json required");
    let known_token = expected_file.scenarios[0]
        .scenario
        .token_in
        .clone();

    let order = fynd_core::types::Order::new(
        fake_token,
        known_token,
        num_bigint::BigUint::from(1_000_000_000_000_000_000u64),
        fynd_core::types::OrderSide::Sell,
        tycho_simulation::tycho_common::models::Address::zero(20),
    );

    let result = harness.quote(vec![order]).await;
    match result {
        Ok(quote) => {
            assert_ne!(
                quote.orders()[0].status(),
                QuoteStatus::Success,
                "unknown token should not produce a successful quote"
            );
        }
        Err(_) => { /* Expected */ }
    }
}

/// Quality: each pair's amount_out_net_gas should be within 20% of expected baseline.
#[tokio::test]
async fn test_quality_within_expected_baseline() {
    let harness = TestHarness::from_fixture().await;
    let expected_file = load_expected_file(&expected_path())
        .expect("I/O error")
        .expect("expected_outputs.json required");
    let expected_map: std::collections::HashMap<_, _> = expected_file
        .scenarios
        .iter()
        .map(|es| (es.scenario.name.clone(), &es.expected))
        .collect();
    let scenarios = scenarios();

    let mut regressions = Vec::new();
    for scenario in &scenarios {
        let Some(expected_output) = expected_map.get(&scenario.name) else {
            continue;
        };
        let order = scenario.to_order();
        let result = harness.quote(vec![order]).await;

        if let Ok(quote) = result {
            let oq = &quote.orders()[0];
            if oq.status() == QuoteStatus::Success {
                let actual = oq.amount_out_net_gas();
                let expected = &expected_output.amount_out_net_gas;

                // Use BigUint arithmetic to avoid f64 precision loss on
                // large wei-denominated values (>2^53).
                // Allow 20% variance: the solver is non-deterministic due to
                // async derived-data computation timing. This threshold catches
                // real algorithmic regressions while tolerating run-to-run jitter.
                if expected.gt(&num_bigint::BigUint::ZERO) {
                    let actual_scaled = actual * &num_bigint::BigUint::from(100u32);
                    let threshold = expected * &num_bigint::BigUint::from(80u32);

                    if actual_scaled < threshold {
                        regressions.push(format!(
                            "{}: degraded >20% (expected {}, got {})",
                            scenario.name, expected, actual,
                        ));
                    }
                }
            }
        }
    }

    assert!(
        regressions.is_empty(),
        "quality regressions (>20% degradation):\n{}",
        regressions.join("\n")
    );
}

/// Quality invariant: all successful quotes should have positive net output.
#[tokio::test]
async fn test_quality_invariants() {
    let harness = TestHarness::from_fixture().await;
    let scenarios = scenarios();

    for scenario in &scenarios {
        let order = scenario.to_order();
        if let Ok(quote) = harness.quote(vec![order]).await {
            let oq = &quote.orders()[0];
            if oq.status() == QuoteStatus::Success {
                assert!(
                    oq.amount_out_net_gas() > &num_bigint::BigUint::ZERO,
                    "{}: amount_out_net_gas should be positive",
                    scenario.name
                );
                assert!(
                    oq.gas_estimate() > &num_bigint::BigUint::ZERO,
                    "{}: gas_estimate should be positive",
                    scenario.name
                );
                assert!(
                    oq.route().is_some(),
                    "{}: successful quote should have a route",
                    scenario.name
                );
            }
        }
    }
}
