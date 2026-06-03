//! Path-based Frank-Wolfe split-routing algorithm.
//!
//! Wraps [`BellmanFordAlgorithm`] to find multiple candidate paths and then
//! optimally split the input amount across them. The inner BF instance handles
//! single-path discovery; this module layers on the Frank-Wolfe optimisation
//! loop to determine the best split fractions.

use std::time::Duration;

use num_bigint::BigUint;
use num_traits::ToPrimitive;

use super::{
    split_primitives::{split_amount, PathAllocation},
    Algorithm, AlgorithmConfig, AlgorithmError, BellmanFordAlgorithm,
};
use crate::{
    derived::{computation::ComputationRequirements, SharedDerivedDataRef},
    feed::market_data::{MarketData, StateLabel},
    graph::{petgraph::StableDiGraph, PetgraphStableDiGraphManager},
    types::{quote::Order, RouteResult},
};

/// Tuning parameters for the path-based Frank-Wolfe split-routing loop.
#[derive(Debug, Clone)]
pub struct PathFrankWolfeConfig {
    /// Maximum number of distinct paths to split across.
    pub max_paths: usize,
    /// Cap beyond which no split is attempted (e.g. 0.25 = 25%).
    pub max_probe: f64,
    /// Minimum share of total input allocated to any single path (e.g. 0.05 = 5%).
    pub min_split: f64,
    /// Number of function evaluations for the golden-section line search.
    pub line_search_evals: usize,
}

impl Default for PathFrankWolfeConfig {
    fn default() -> Self {
        Self { max_paths: 4, max_probe: 0.25, min_split: 0.05, line_search_evals: 12 }
    }
}

/// Split-routing algorithm that discovers multiple paths via Bellman-Ford
/// and optimises the input split across them using a Frank-Wolfe loop.
pub struct PathFrankWolfeAlgorithm {
    inner: BellmanFordAlgorithm,
    // Used by the split-routing loop (not yet implemented).
    #[allow(dead_code)]
    config: PathFrankWolfeConfig,
}

impl PathFrankWolfeAlgorithm {
    /// Creates a new `PathFrankWolfeAlgorithm`.
    pub(crate) fn new(algorithm_config: AlgorithmConfig, config: PathFrankWolfeConfig) -> Self {
        let inner = BellmanFordAlgorithm::with_config(algorithm_config);
        Self { inner, config }
    }
}

impl Default for PathFrankWolfeAlgorithm {
    fn default() -> Self {
        Self::new(AlgorithmConfig::default(), PathFrankWolfeConfig::default())
    }
}

impl Algorithm for PathFrankWolfeAlgorithm {
    type GraphType = StableDiGraph<()>;
    type GraphManager = PetgraphStableDiGraphManager<()>;

    fn name(&self) -> &str {
        "path_frank_wolfe"
    }

    async fn find_best_route(
        &self,
        _graph: &Self::GraphType,
        _market: MarketData,
        _label: Option<StateLabel>,
        _derived: Option<SharedDerivedDataRef>,
        _order: &Order,
    ) -> Result<RouteResult, AlgorithmError> {
        unimplemented!("PathFrankWolfe split-routing loop not yet implemented")
    }

    fn computation_requirements(&self) -> ComputationRequirements {
        ComputationRequirements::none()
            .allow_stale("token_prices")
            .expect("token_prices requirement conflicts (bug)")
            .allow_stale("spot_prices")
            .expect("spot_prices requirement conflicts (bug)")
    }

    fn timeout(&self) -> Duration {
        self.inner.timeout()
    }
}

    /// Computes the minimum probe amount from the initial route's price impact.
    ///
    /// Returns `None` when the probe exceeds `config.max_probe × total_amount`,
    /// signalling that splitting is not worthwhile.
    #[allow(dead_code)]
    fn compute_probe_amount(
        &self,
        total_amount: &BigUint,
        price_impact: f64,
        gas_cost_output_tokens: f64,
    ) -> Option<BigUint> {
        if price_impact <= 0.0 {
            return None;
        }
        let gas_floor = gas_cost_output_tokens / price_impact;

        let probe_amount = BigUint::from(gas_floor.ceil() as u128);
        let (max_probe_amount, _remainder) = split_amount(total_amount, self.config.max_probe);
        if probe_amount > max_probe_amount {
            return None;
        }

        Some(probe_amount)
    }

    /// Flow-fraction-weighted average price impact across all active paths.
    ///
    /// Per-path price impact measures how much the realized output falls short
    /// of the ideal (marginal-price) output. Paths are weighted by their share of
    /// total flow, not averaged equally — a 95/5 split means the big path
    /// dominates the result and the small path barely matters.
    #[allow(dead_code)]
    fn compute_average_price_impact(paths: &[PathAllocation]) -> Result<f64, AlgorithmError> {
        let mut weighted_price_impact = 0.0;
        for path in paths {
            let amount_in = path.amount_in.to_f64().ok_or_else(|| {
                AlgorithmError::Other(format!("amount_in too large for f64: {}", path.amount_in))
            })?;
            let amount_out = path
                .amount_out
                .to_f64()
                .ok_or_else(|| {
                    AlgorithmError::Other(format!(
                        "amount_out too large for f64: {}",
                        path.amount_out
                    ))
                })?;
            let ideal_out = amount_in * path.marginal_price_product;
            if ideal_out <= 0.0 {
                return Err(AlgorithmError::Other(format!(
                    "non-positive ideal output ({ideal_out}) from \
                     amount_in={amount_in}, \
                     marginal_price_product={}",
                    path.marginal_price_product
                )));
            }

            let price_impact = 1.0 - amount_out / ideal_out;
            weighted_price_impact += path.flow_fraction * price_impact;
        }
        Ok(weighted_price_impact)
    }
}

impl Algorithm for PathFrankWolfeAlgorithm {
    type GraphType = StableDiGraph<()>;
    type GraphManager = PetgraphStableDiGraphManager<()>;

    fn name(&self) -> &str {
        "path_frank_wolfe"
    }

    async fn find_best_route(
        &self,
        graph: &Self::GraphType,
        market: MarketData,
        label: Option<StateLabel>,
        derived: Option<SharedDerivedDataRef>,
        order: &Order,
    ) -> Result<RouteResult, AlgorithmError> {
        // Delegate to inner BF until the split-routing loop is implemented.
        self.inner
            .find_best_route(graph, market, label, derived, order)
            .await
    }

    fn computation_requirements(&self) -> ComputationRequirements {
        ComputationRequirements::none()
            .allow_stale("token_prices")
            .expect("token_prices requirement conflicts (bug)")
            .allow_stale("spot_prices")
            .expect("spot_prices requirement conflicts (bug)")
    }

    fn timeout(&self) -> Duration {
        self.inner.timeout()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithm::AlgorithmConfig;

    impl PathFrankWolfeAlgorithm {
        /// Returns a reference to the PFW-specific tuning config.
        fn pfw_config(&self) -> &PathFrankWolfeConfig {
            &self.config
        }
    }

    #[test]
    fn test_with_pfw_config_override() {
        let pfw_config = PathFrankWolfeConfig {
            max_paths: 8,
            max_probe: 0.5,
            min_split: 0.1,
            line_search_evals: 24,
        };
        let algo = PathFrankWolfeAlgorithm::new(AlgorithmConfig::default(), pfw_config);

        assert_eq!(algo.pfw_config().max_paths, 8);
        assert!((algo.pfw_config().max_probe - 0.5).abs() < f64::EPSILON);
        assert!((algo.pfw_config().min_split - 0.1).abs() < f64::EPSILON);
        assert_eq!(algo.pfw_config().line_search_evals, 24);
    }

    // ==================== compute_probe_amount ====================

    #[test]
    fn test_probe_amount_low_impact() {
        // Very small price impact makes gas_floor huge, exceeding max_probe cap → None.
        //   gas_floor = 100_000 / 0.001 = 100_000_000
        //   max_probe = 1_000_000 * 0.25 = 250_000
        let total = BigUint::from(1_000_000u64);
        let algo = PathFrankWolfeAlgorithm::new_with_defaults().unwrap();

        let result = algo.compute_probe_amount(&total, 0.001, 100_000.0);
        assert!(result.is_none());
    }

    #[test]
    fn test_probe_amount_scaling() {
        // Higher price impact → lower probe floor (inversely proportional).
        //   probe = gas_cost / price_impact, so doubling price impact halves
        //   the probe.
        let total = BigUint::from(10_000_000u64);
        let algo = PathFrankWolfeAlgorithm::new_with_defaults().unwrap();
        let gas_cost = 1000.0;

        let probe_high_pi = algo
            .compute_probe_amount(&total, 0.10, gas_cost)
            .unwrap();
        let probe_low_pi = algo
            .compute_probe_amount(&total, 0.05, gas_cost)
            .unwrap();

        assert!(probe_high_pi < probe_low_pi);

        // price_impact ratio is 0.10/0.05 = 2×, so the probe ratio should be
        // the inverse: 0.5×. Verify within 1% tolerance.
        let ratio = probe_high_pi.to_f64().unwrap() / probe_low_pi.to_f64().unwrap();
        assert!(
            (ratio - 0.5).abs() < 0.01,
            "expected ratio ~0.5 (inverse proportionality), got {ratio}"
        );
    }

    #[test]
    fn test_probe_amount_within_cap() {
        // Moderate price impact where probe fits within max_probe cap → Some.
        //   gas_floor = 1000 / 0.10 = 10_000
        //   max_probe = 1_000_000 * 0.25 = 250_000
        let total = BigUint::from(1_000_000u64);
        let algo = PathFrankWolfeAlgorithm::new_with_defaults().unwrap();

        let probe_amount = algo
            .compute_probe_amount(&total, 0.10, 1000.0)
            .unwrap();
        assert_eq!(probe_amount, BigUint::from(10_000u64));
    }

    #[test]
    fn test_probe_amount_zero_price_impact() {
        let total = BigUint::from(1_000_000u64);
        let algo = PathFrankWolfeAlgorithm::new_with_defaults().unwrap();

        assert!(algo
            .compute_probe_amount(&total, 0.0, 1000.0)
            .is_none());
    }

    // ==================== compute_average_price_impact ====================

    #[test]
    fn test_average_price_impact_redistribution() {
        // Splitting flow across more paths should reduce average price impact.
        // Uses constant-product pool outputs (reserve_in=1M, reserve_out=2M) to construct
        // allocations at 1, 2, and 3 paths.
        let iter_0 = [PathAllocation {
            hops: vec![],
            flow_fraction: 1.0,
            amount_in: BigUint::from(100_000u64),
            amount_out: BigUint::from(181_818u64),
            marginal_price_product: 2.0,
        }];

        let iter_1 = [
            PathAllocation {
                hops: vec![],
                flow_fraction: 0.5,
                amount_in: BigUint::from(50_000u64),
                amount_out: BigUint::from(95_238u64),
                marginal_price_product: 2.0,
            },
            PathAllocation {
                hops: vec![],
                flow_fraction: 0.5,
                amount_in: BigUint::from(50_000u64),
                amount_out: BigUint::from(95_238u64),
                marginal_price_product: 2.0,
            },
        ];

        let third = 1.0 / 3.0;
        let iter_2 = [
            PathAllocation {
                hops: vec![],
                flow_fraction: third,
                amount_in: BigUint::from(33_333u64),
                amount_out: BigUint::from(64_514u64),
                marginal_price_product: 2.0,
            },
            PathAllocation {
                hops: vec![],
                flow_fraction: third,
                amount_in: BigUint::from(33_333u64),
                amount_out: BigUint::from(64_514u64),
                marginal_price_product: 2.0,
            },
            PathAllocation {
                hops: vec![],
                flow_fraction: third,
                amount_in: BigUint::from(33_334u64),
                amount_out: BigUint::from(64_516u64),
                marginal_price_product: 2.0,
            },
        ];

        let pi_0 = PathFrankWolfeAlgorithm::compute_average_price_impact(&iter_0).unwrap();
        let pi_1 = PathFrankWolfeAlgorithm::compute_average_price_impact(&iter_1).unwrap();
        let pi_2 = PathFrankWolfeAlgorithm::compute_average_price_impact(&iter_2).unwrap();

        assert!(pi_1 < pi_0, "price impact should decrease after first split: {pi_1} >= {pi_0}");
        assert!(pi_2 < pi_1, "price impact should decrease after second split: {pi_2} >= {pi_1}");

        assert!((pi_0 - 0.09091).abs() < 1e-5, "expected ~0.0909, got {pi_0}");
        assert!((pi_1 - 0.04762).abs() < 1e-5, "expected ~0.0476, got {pi_1}");
        assert!((pi_2 - 0.03228).abs() < 1e-5, "expected ~0.0323, got {pi_2}");
    }

    #[test]
    fn test_average_price_impact_weighting() {
        // Weighted average: 90% of flow with 10% price impact + 10% of flow
        // with 50% price impact = 0.14, not the simple mean of 0.30.
        //   Path 1: flow=0.9, price_impact = 1 − 900/1000 = 0.10
        //   Path 2: flow=0.1, price_impact = 1 − 50/100  = 0.50
        //   Weighted = 0.9 × 0.10 + 0.1 × 0.50 = 0.14
        let allocations = [
            PathAllocation {
                hops: vec![],
                flow_fraction: 0.9,
                amount_in: BigUint::from(1000u64),
                amount_out: BigUint::from(900u64),
                marginal_price_product: 1.0,
            },
            PathAllocation {
                hops: vec![],
                flow_fraction: 0.1,
                amount_in: BigUint::from(100u64),
                amount_out: BigUint::from(50u64),
                marginal_price_product: 1.0,
            },
        ];

        let pi = PathFrankWolfeAlgorithm::compute_average_price_impact(&allocations).unwrap();
        assert!((pi - 0.14).abs() < 1e-10, "expected 0.14, got {pi}");
    }

    // TODO(ENG-5856): requires the main loop to verify early exit behavior.
    #[test]
    #[ignore]
    fn test_pi_exit_criterion_stops_loop_early() {}
}
