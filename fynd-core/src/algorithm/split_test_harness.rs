//! Test helpers for split-routing algorithm split_scenarios.

use num_bigint::{BigInt, BigUint};
use tycho_simulation::tycho_core::{models::token::Token, simulation::protocol_sim::ProtocolSim};

use crate::{
    algorithm::test_utils::setup_market_unweighted, feed::market_data::MarketData,
    graph::petgraph::PetgraphStableDiGraphManager,
};

/// Returns `(fraction_for_pool_1, total_output)` — the theoretically optimal output when
/// splitting `trade_amount` between two constant-product pools with no fees.
///
/// Finds the split where both pools offer the same marginal rate on the last unit traded.
/// Negative allocations are clamped to `0` — a clamped value means the full trade routes through
/// the other pool.
pub(crate) fn optimal_two_pool_output(
    reserve_in_1: f64,
    reserve_out_1: f64,
    reserve_in_2: f64,
    reserve_out_2: f64,
    trade_amount: f64,
) -> (f64, f64) {
    let d = ((reserve_in_1 * reserve_out_1) / (reserve_in_2 * reserve_out_2)).sqrt();
    let a2 =
        ((trade_amount + reserve_in_1 - d * reserve_in_2) / (d + 1.0)).clamp(0.0, trade_amount);
    let a1 = trade_amount - a2;

    let fraction_1 = a1 / trade_amount;
    let out_1 = a1 * reserve_out_1 / (reserve_in_1 + a1);
    let out_2 = a2 * reserve_out_2 / (reserve_in_2 + a2);

    (fraction_1, out_1 + out_2)
}

// ==================== Scenario harness ====================

/// A pool entry in a `TestScenario`.
pub(crate) struct ScenarioPool {
    pub id: &'static str,
    pub token_1: Token,
    pub token_2: Token,
    pub sim: Box<dyn ProtocolSim>,
}

/// A self-contained algorithm test case with pre-computed bounds.
///
/// Both bounds are net output amounts (gross minus gas cost), hardcoded from the scenario's fixed
/// reserves. Gas cost uses the test market's fixed assumptions: 100 wei/gas, 1 output-token = 1
/// ETH.
#[allow(dead_code)]
pub(crate) struct TestScenario {
    pub name: &'static str,
    pub description: &'static str,
    pub pools: Vec<ScenarioPool>,
    pub token_in: Token,
    pub token_out: Token,
    pub trade_amount: BigUint,
    /// Floor: the algorithm must produce at least this much net output.
    pub lower_bound: BigInt,
    /// Target: the best net output achievable under the scenario's simplified pool model. A
    /// quality ceiling to measure against, not a hard constraint.
    pub analytical_optimum: BigInt,
}

impl TestScenario {
    /// Builds a `MarketData` + graph manager from this scenario's pool definitions.
    ///
    /// Consumes `self` because pool simulators are moved into the market. Clone any fields you
    /// need before calling this.
    pub(crate) fn build_market(self) -> (MarketData, PetgraphStableDiGraphManager<()>) {
        // Clone ids and tokens first so they can be borrowed while `pools` is consumed for sims.
        let ids: Vec<&'static str> = self
            .pools
            .iter()
            .map(|p| p.id)
            .collect();
        let tokens_a: Vec<Token> = self
            .pools
            .iter()
            .map(|p| p.token_1.clone())
            .collect();
        let tokens_b: Vec<Token> = self
            .pools
            .iter()
            .map(|p| p.token_2.clone())
            .collect();
        let sims: Vec<Box<dyn ProtocolSim>> = self
            .pools
            .into_iter()
            .map(|p| p.sim)
            .collect();
        setup_market_unweighted(
            ids.into_iter()
                .zip(tokens_a.iter())
                .zip(tokens_b.iter())
                .zip(sims)
                .map(|(((id, ta), tb), sim)| (id, ta, tb, sim))
                .collect(),
        )
    }
}

// ==================== Named split_scenarios ====================

pub(crate) mod split_scenarios {
    use num_bigint::{BigInt, BigUint};

    use super::{ScenarioPool, TestScenario};
    use crate::algorithm::test_utils::{token, ConstantProductSim, ONE_ETH};

    /// S1: two identical A→B pools; 50/50 split is optimal.
    ///
    /// `analytical_optimum`: `optimal_two_pool_output` with equal reserves — exact mathematical
    /// optimum (50/50).
    pub(crate) fn symmetric_split() -> TestScenario {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let r = BigUint::from(1_000_000u64) * BigUint::from(ONE_ETH);

        TestScenario {
            name: "SYMMETRIC_SPLIT",
            description: "Two identical A→B pools. 50/50 split is optimal.",
            pools: vec![
                ScenarioPool {
                    id: "pool_1",
                    token_1: token_a.clone(),
                    token_2: token_b.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r.clone(),
                        reserve_1: r.clone(),
                        gas: 50_000,
                    }),
                },
                ScenarioPool {
                    id: "pool_2",
                    token_1: token_a.clone(),
                    token_2: token_b.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r.clone(),
                        reserve_1: r.clone(),
                        gas: 50_000,
                    }),
                },
            ],
            token_in: token_a,
            token_out: token_b,
            trade_amount: BigUint::from(100_000u64) * BigUint::from(ONE_ETH),
            // gross 90_909_090_909_090_909_090_909 − 1 pool × 50_000 gas × 100 wei/gas
            lower_bound: BigInt::from(90_909_090_909_090_904_090_909u128),
            // gross 95_238_095_238_095_236_709_344 − 2 pools × 50_000 gas × 100 wei/gas
            analytical_optimum: BigInt::from(95_238_095_238_095_226_709_344u128),
        }
    }

    /// S2: two A→B pools with reserves 1_000_000 and 500_000; optimal split favours the larger
    /// pool.
    ///
    /// `analytical_optimum`: `optimal_two_pool_output` with the asymmetric reserves — exact
    /// mathematical optimum.
    pub(crate) fn asymmetric_split() -> TestScenario {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let one_eth = BigUint::from(ONE_ETH);
        let r1 = BigUint::from(1_000_000u64) * &one_eth;
        let r2 = BigUint::from(500_000u64) * &one_eth;

        TestScenario {
            name: "ASYMMETRIC_SPLIT",
            description: "Two A→B pools of unequal size. Optimal split favours the larger pool.",
            pools: vec![
                ScenarioPool {
                    id: "pool_1",
                    token_1: token_a.clone(),
                    token_2: token_b.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r1.clone(),
                        reserve_1: r1.clone(),
                        gas: 50_000,
                    }),
                },
                ScenarioPool {
                    id: "pool_2",
                    token_1: token_a.clone(),
                    token_2: token_b.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r2.clone(),
                        reserve_1: r2.clone(),
                        gas: 50_000,
                    }),
                },
            ],
            token_in: token_a,
            token_out: token_b,
            trade_amount: BigUint::from(200_000u64) * &one_eth,
            // gross 166_666_666_666_666_666_666_666 − 1 pool × 50_000 gas × 100 wei/gas
            lower_bound: BigInt::from(166_666_666_666_666_661_666_666u128),
            // gross 176_470_588_235_294_097_103_232 − 2 pools × 50_000 gas × 100 wei/gas
            analytical_optimum: BigInt::from(176_470_588_235_294_087_103_232u128),
        }
    }

    /// S3: split has a real gross benefit, but the extra-hop gas cost exceeds it.
    ///
    /// `analytical_optimum`: equals `lower_bound`. Gas overhead makes splitting strictly worse than
    /// single-route — the best achievable output is the BF single-route result.
    pub(crate) fn gas_kills_split() -> TestScenario {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let r = BigUint::from(20_000_000u64);

        TestScenario {
            name: "GAS_KILLS_SPLIT",
            description: "Split has a real gross benefit but the extra-hop gas exceeds it, making the split net-negative.",
            pools: vec![
                ScenarioPool {
                    id: "pool_1",
                    token_1: token_a.clone(),
                    token_2: token_b.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r.clone(),
                        reserve_1: r.clone(),
                        gas: 50_000,
                    }),
                },
                ScenarioPool {
                    id: "pool_2",
                    token_1: token_a.clone(),
                    token_2: token_b.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r.clone(),
                        reserve_1: r.clone(),
                        gas: 50_000,
                    }),
                },
            ],
            token_in: token_a,
            token_out: token_b,
            trade_amount: BigUint::from(10_000_000u64),
            // gross 6_666_666 − 1 pool × 50_000 gas × 100 wei/gas
            lower_bound: BigInt::from(1_666_666i64),
            // optimal net strategy is single route; gross split output (8M) loses on net
            analytical_optimum: BigInt::from(1_666_666i64),
        }
    }

    /// S4: single A→B pool only; no alternative path to split into.
    ///
    /// `analytical_optimum`: equals `lower_bound`. With only one pool there is nothing to split
    /// across; the analytical optimum is simply the single-route output.
    pub(crate) fn no_alternative_path() -> TestScenario {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let r = BigUint::from(1_000_000u64) * BigUint::from(ONE_ETH);

        TestScenario {
            name: "NO_ALTERNATIVE_PATH",
            description:
                "Single A→B pool. No pool to split into; algorithm must return single-route result.",
            pools: vec![ScenarioPool {
                id: "pool_1",
                token_1: token_a.clone(),
                token_2: token_b.clone(),
                sim: Box::new(ConstantProductSim {
                    reserve_0: r.clone(),
                    reserve_1: r.clone(),
                    gas: 50_000,
                }),
            }],
            token_in: token_a,
            token_out: token_b,
            trade_amount: BigUint::from(100_000u64) * BigUint::from(ONE_ETH),
            // gross 90_909_090_909_090_909_090_909 − 1 pool × 50_000 gas × 100 wei/gas
            lower_bound: BigInt::from(90_909_090_909_090_904_090_909u128),
            // single pool only — no split possible; net optimum equals lower_bound
            analytical_optimum: BigInt::from(90_909_090_909_090_904_090_909u128),
        }
    }

    /// S5: A→B (one pool) → C (two parallel pools); bottleneck is at the B→C hop.
    ///
    /// PathFrankWolfe discovers two complete paths [P_AB, P_BC1] and [P_AB, P_BC2]. Because both
    /// share P_AB, `build_split_route` emits one combined A→B swap followed by split B→C swaps,
    /// with P_AB's gas counted once.
    ///
    /// `lower_bound`: best single 2-hop route A→B→C through the larger B→C pool.
    /// `analytical_optimum`: only one A→B pool (P_AB) exists so the B amount is fixed;
    /// `optimal_two_pool_output` gives the exact optimum for splitting that B across the two B→C
    /// pools.
    pub(crate) fn multi_hop_bottleneck() -> TestScenario {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let one_eth = BigUint::from(ONE_ETH);
        let r_ab = BigUint::from(10_000_000u64) * &one_eth;
        let r_bc_main = BigUint::from(1_000_000u64) * &one_eth;
        let r_bc_par = BigUint::from(500_000u64) * &one_eth;

        TestScenario {
            name: "MULTI_HOP_BOTTLENECK",
            description: "A→B→C with two parallel B→C pools. PathFrankWolfe discovers both paths sharing P_AB.",
            pools: vec![
                ScenarioPool {
                    id: "pool_ab",
                    token_1: token_a.clone(),
                    token_2: token_b.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r_ab.clone(),
                        reserve_1: r_ab,
                        gas: 50_000,
                    }),
                },
                ScenarioPool {
                    id: "pool_bc_main",
                    token_1: token_b.clone(),
                    token_2: token_c.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r_bc_main.clone(),
                        reserve_1: r_bc_main,
                        gas: 50_000,
                    }),
                },
                ScenarioPool {
                    id: "pool_bc_par",
                    token_1: token_b.clone(),
                    token_2: token_c.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r_bc_par.clone(),
                        reserve_1: r_bc_par,
                        gas: 50_000,
                    }),
                },
            ],
            token_in: token_a,
            token_out: token_c,
            trade_amount: BigUint::from(200_000u64) * &one_eth,
            // gross 163_934_426_229_508_196_721_311 − 2 pools × 50_000 gas × 100 wei/gas
            lower_bound: BigInt::from(163_934_426_229_508_186_721_311u128),
            // gross 173_410_404_624_277_463_881_280 − 3 pools × 50_000 gas × 100 wei/gas
            analytical_optimum: BigInt::from(173_410_404_624_277_448_881_280u128),
        }
    }

    /// S6: two A→B pools then two B→C pools; pool sizes mismatched between hops so the optimal
    /// A→B and B→C splits differ. An algorithm that routes independent per-path hops without
    /// pooling B first will use the wrong cross-allocations and miss the optimum.
    ///
    /// `lower_bound`: best single 2-hop path (larger pool at each hop).
    /// `analytical_optimum`: chained `optimal_two_pool_output` across both hops.
    pub(crate) fn double_split() -> TestScenario {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let one_eth = BigUint::from(ONE_ETH);
        let r_ab1 = BigUint::from(1_000_000u64) * &one_eth;
        let r_ab2 = BigUint::from(500_000u64) * &one_eth;
        let r_bc1 = BigUint::from(500_000u64) * &one_eth;
        let r_bc2 = BigUint::from(1_500_000u64) * &one_eth;

        TestScenario {
            name: "DOUBLE_SPLIT",
            description: "Two A→B pools (1M and 500k ETH) then two B→C pools (500k and 1.5M \
                          ETH). Optimal splits differ at each hop, forcing B to be pooled before \
                          re-splitting.",
            pools: vec![
                ScenarioPool {
                    id: "pool_ab_1",
                    token_1: token_a.clone(),
                    token_2: token_b.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r_ab1.clone(),
                        reserve_1: r_ab1,
                        gas: 50_000,
                    }),
                },
                ScenarioPool {
                    id: "pool_ab_2",
                    token_1: token_a.clone(),
                    token_2: token_b.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r_ab2.clone(),
                        reserve_1: r_ab2,
                        gas: 50_000,
                    }),
                },
                ScenarioPool {
                    id: "pool_bc_1",
                    token_1: token_b.clone(),
                    token_2: token_c.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r_bc1.clone(),
                        reserve_1: r_bc1,
                        gas: 50_000,
                    }),
                },
                ScenarioPool {
                    id: "pool_bc_2",
                    token_1: token_b.clone(),
                    token_2: token_c.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r_bc2.clone(),
                        reserve_1: r_bc2,
                        gas: 50_000,
                    }),
                },
            ],
            token_in: token_a,
            token_out: token_c,
            trade_amount: BigUint::from(500_000u64) * &one_eth,
            // gross floor(3×10²⁴/11) = 272_727_272_727_272_727_272_727 − 2 pools × 50_000 gas × 100
            // wei/gas
            lower_bound: BigInt::from(272_727_272_727_272_717_272_727u128),
            // gross floor(6×10²⁴/19) = 315_789_473_684_210_526_315_789 − 4 pools × 50_000 gas × 100
            // wei/gas
            analytical_optimum: BigInt::from(315_789_473_684_210_526_295_789u128),
        }
    }

    /// Returns all 6 named split_scenarios.
    pub(crate) fn all() -> Vec<TestScenario> {
        vec![
            symmetric_split(),
            asymmetric_split(),
            gas_kills_split(),
            no_alternative_path(),
            multi_hop_bottleneck(),
            double_split(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f64_eq(x: f64, y: f64) -> bool {
        (x - y).abs() < 1e-9
    }

    #[test]
    fn test_optimal_two_pool_output_symmetric() {
        // Identical pools → 50/50 split is always optimal
        let (fraction, _) =
            optimal_two_pool_output(10_000.0, 10_000.0, 10_000.0, 10_000.0, 1_000.0);
        assert!(f64_eq(fraction, 0.5), "symmetric pools: expected fraction 0.5, got {fraction}");
    }

    #[test]
    fn test_optimal_two_pool_output_asymmetric() {
        // Pool 1: reserve_in=100, reserve_out=400
        // Pool 2: reserve_in=100, reserve_out=100
        // swap amount: 400
        let (fraction, split_out) = optimal_two_pool_output(100.0, 400.0, 100.0, 100.0, 400.0);

        // Verify the split is correct
        assert!(f64_eq(fraction, 0.75), "expected fraction 0.75, got {fraction}");
        assert!(f64_eq(split_out, 350.0), "expected split output 350.0, got {split_out}");

        // Verify marginal prices are equal at the optimal split.
        let pool_1_amount = fraction * 400.0;
        let pool_2_amount = 400.0 - pool_1_amount;
        let marginal_1 = (100.0 * 400.0) / (100.0 + pool_1_amount).powi(2);
        let marginal_2 = (100.0 * 100.0) / (100.0 + pool_2_amount).powi(2);
        assert!(
            f64_eq(marginal_1, marginal_2),
            "marginal prices should equalise at the optimum: {marginal_1} vs {marginal_2}"
        );
    }

    #[test]
    fn test_scenario_market_builds_without_panic() {
        for scenario in split_scenarios::all() {
            assert!(!scenario.name.is_empty());
            assert!(!scenario.description.is_empty());
            assert!(scenario.analytical_optimum >= scenario.lower_bound);
            let _ = scenario.build_market();
        }
    }
}
