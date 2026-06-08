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
    bellman_ford::{BellmanFordContext, FindRouteOptions},
    split_primitives::{
        build_post_swap_overrides, split_amount, HopDescriptor, PathAllocation, SimulatedHop,
    },
    Algorithm, AlgorithmConfig, AlgorithmError, BellmanFordAlgorithm,
};
use crate::{
    derived::{computation::ComputationRequirements, SharedDerivedDataRef},
    feed::market_data::{MarketData, StateLabel},
    graph::{petgraph::StableDiGraph, PetgraphStableDiGraphManager},
    types::{quote::Order, OrderSide, RouteResult},
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

impl PathFrankWolfeAlgorithm {
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
            if amount_in <= 0.0 {
                return Err(AlgorithmError::Other(format!("non-positive amount_in ({amount_in})")));
            }
            if path.marginal_price_product <= 0.0 {
                return Err(AlgorithmError::Other(format!(
                    "non-positive marginal_price_product ({})",
                    path.marginal_price_product
                )));
            }

            let ideal_out = amount_in * path.marginal_price_product;
            let price_impact = 1.0 - amount_out / ideal_out;
            weighted_price_impact += path.flow_fraction * price_impact;
        }
        Ok(weighted_price_impact)
    }

    /// Finds the next candidate routing path for the Frank-Wolfe algorithm.
    ///
    /// Builds a post-swap market state that reflects `current_allocations` (applying each
    /// allocation's simulated pool outputs as overrides), then runs Bellman-Ford at
    /// `probe_amount` to discover the best remaining route.
    ///
    /// Pools already present in `current_allocations` are promoted to zero-gas so their
    /// committed gas cost is not counted again as marginal cost for the new path.
    ///
    /// Returns an ordered sequence of [`SimulatedHop`]s representing the discovered path,
    /// or an error if no route exists.
    #[allow(dead_code)]
    pub(crate) fn find_candidate_path(
        &self,
        ctx: &BellmanFordContext,
        current_allocations: &[PathAllocation],
        probe_amount: &BigUint,
    ) -> Result<Vec<SimulatedHop>, AlgorithmError> {
        let mut overrides = build_post_swap_overrides(current_allocations, &ctx.market_data);

        // Pools committed in the current solution are executed once on-chain — their gas is
        // already priced into the combined transaction. Zero out protocol gas so BF doesn't
        // double-charge them when evaluating extensions. We track by (component_id, token_in,
        // token_out) because different token pairs through the same pool are separate on-chain
        // swaps with independent gas costs.
        for alloc in current_allocations {
            for hop in &alloc.hops {
                overrides = overrides.with_zero_gas(
                    hop.descriptor.component_id.clone(),
                    hop.descriptor.token_in.address.clone(),
                    hop.descriptor.token_out.address.clone(),
                );
            }
        }

        let token_in = ctx
            .node_address
            .get(&ctx.token_in_node)
            .cloned()
            .ok_or_else(|| AlgorithmError::DataNotFound {
                kind: "token_in node index",
                id: Some(format!("{:?}", ctx.token_in_node)),
            })?;
        let token_out = ctx
            .node_address
            .get(&ctx.token_out_node)
            .cloned()
            .ok_or_else(|| AlgorithmError::DataNotFound {
                kind: "token_out node index",
                id: Some(format!("{:?}", ctx.token_out_node)),
            })?;
        let probe_order = Order::new(
            token_in,
            token_out,
            probe_amount.clone(),
            OrderSide::Sell,
            Default::default(),
        );

        let result =
            self.inner
                .find_single_route(ctx, &probe_order, FindRouteOptions { overrides })?;

        let route = result.route();
        let tokens = route.tokens();
        route
            .swaps()
            .iter()
            .map(|swap| {
                let token_in = tokens
                    .get(swap.token_in())
                    .cloned()
                    .ok_or_else(|| AlgorithmError::DataNotFound {
                        kind: "token",
                        id: Some(format!("{:?}", swap.token_in())),
                    })?;
                let token_out = tokens
                    .get(swap.token_out())
                    .cloned()
                    .ok_or_else(|| AlgorithmError::DataNotFound {
                        kind: "token",
                        id: Some(format!("{:?}", swap.token_out())),
                    })?;
                Ok(SimulatedHop {
                    descriptor: HopDescriptor::new(
                        swap.component_id().to_string(),
                        token_in,
                        token_out,
                    ),
                    amount_out: swap.amount_out().clone(),
                    gas: swap.gas_estimate().clone(),
                })
            })
            .collect()
    }
    /// Golden-section search over the split fraction `gamma ∈ [0, 1]`.
    ///
    /// At each probe point, builds trial fractions (existing paths scaled by
    /// `1 − gamma`, candidate at `gamma`) and evaluates the combined output via
    /// `evaluate_total_output`.
    fn optimize_step_size(
        &self,
        current_allocations: &[PathAllocation],
        candidate: &[SimulatedHop],
        total_amount: &BigUint,
        ctx: &BellmanFordContext,
    ) -> f64 {
        let existing_descriptors: Vec<Vec<HopDescriptor>> = current_allocations
            .iter()
            .map(|a| {
                a.hops
                    .iter()
                    .map(|h| h.descriptor.clone())
                    .collect()
            })
            .collect();
        let candidate_descriptors: Vec<HopDescriptor> = candidate
            .iter()
            .map(|h| h.descriptor.clone())
            .collect();
        let overrides = MarketOverrides::empty();

        let evaluate_split = |gamma: f64| -> f64 {
            let mut trial_fractions: Vec<f64> = current_allocations
                .iter()
                .map(|a| a.flow_fraction * (1.0 - gamma))
                .collect();
            trial_fractions.push(gamma);

            let mut trial_paths: Vec<&[HopDescriptor]> = existing_descriptors
                .iter()
                .map(|v| v.as_slice())
                .collect();
            trial_paths.push(&candidate_descriptors);

            match evaluate_total_output(
                &trial_paths,
                &trial_fractions,
                total_amount,
                &ctx.market_data,
                &overrides,
            ) {
                Ok((total_output, _gas)) => total_output.to_f64().unwrap_or(0.0),
                Err(_) => 0.0,
            }
        };

        golden_section_search(evaluate_split, 0.0, 1.0, self.config.line_search_evals)
    }

    /// Applies a Frank-Wolfe step: shifts `gamma` fraction of flow to the
    /// candidate path, re-simulates all paths, and prunes negligible allocations.
    fn apply_step(
        &self,
        allocations: &mut Vec<PathAllocation>,
        candidate: &[SimulatedHop],
        gamma: f64,
        total_amount: &BigUint,
        ctx: &BellmanFordContext,
    ) -> Result<(), AlgorithmError> {
        for alloc in allocations.iter_mut() {
            alloc.flow_fraction *= 1.0 - gamma;
        }

        allocations.push(PathAllocation {
            hops: candidate.to_vec(),
            flow_fraction: gamma,
            amount_in: BigUint::zero(),
            amount_out: BigUint::zero(),
            marginal_price_product: 0.0,
        });

        allocations.retain(|a| a.flow_fraction >= self.config.min_split);

        let mut remaining_fractions: Vec<f64> = allocations
            .iter()
            .map(|a| a.flow_fraction)
            .collect();
        normalize_fractions(&mut remaining_fractions)
            .map_err(|e| AlgorithmError::Other(e.to_string()))?;
        let overrides = MarketOverrides::empty();
        for (alloc, &frac) in allocations
            .iter_mut()
            .zip(remaining_fractions.iter())
        {
            alloc.flow_fraction = frac;
            let (alloc_amount_in, _) = split_amount(total_amount, frac);
            let hop_descriptors: Vec<HopDescriptor> = alloc
                .hops
                .iter()
                .map(|h| h.descriptor.clone())
                .collect();
            let sim = simulate_path(
                &hop_descriptors,
                &alloc_amount_in,
                &ctx.market_data,
                &overrides,
            )?;
            alloc.amount_in = alloc_amount_in;
            alloc.amount_out = sim.amount_out;
            alloc.marginal_price_product = sim.marginal_price_product;
        }

        Ok(())
    }


    /// Returns `true` if `candidate` has the same ordered sequence of
    /// `(component_id, token_in, token_out)` as any existing allocation.
    ///
    /// Both the pool and the token pair must match at every hop — the same pool used with
    /// different tokens (e.g. in a multi-token pool) is a distinct path.
    ///
    /// Paths that share only a prefix but diverge at a later hop are **not** duplicates — the
    /// shared hops are handled by `build_split_route`, which emits a single combined swap for
    /// the common segment.
    #[allow(dead_code)]
    pub(crate) fn is_duplicate_path(
        candidate: &[SimulatedHop],
        existing: &[PathAllocation],
    ) -> bool {
        existing.iter().any(|alloc| {
            alloc.hops.len() == candidate.len() &&
                alloc
                    .hops
                    .iter()
                    .zip(candidate.iter())
                    .all(|(a, b)| {
                        a.descriptor.component_id == b.descriptor.component_id &&
                            a.descriptor.token_in.address == b.descriptor.token_in.address &&
                            a.descriptor.token_out.address == b.descriptor.token_out.address
                    })
        })
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
    use std::time::Duration as StdDuration;

    use tycho_simulation::tycho_common::simulation::protocol_sim::ProtocolSim;

    use super::*;
    use crate::{
        algorithm::{
            split_primitives::{build_split_route, MarketOverrides},
            test_utils::{
                order, setup_market_unweighted, token, ConstantProductSim, MockProtocolSim,
            },
            AlgorithmConfig,
        },
        graph::GraphManager,
        types::OrderSide,
    };

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
        let algo = PathFrankWolfeAlgorithm::default();

        let result = algo.compute_probe_amount(&total, 0.001, 100_000.0);
        assert!(result.is_none());
    }

    #[test]
    fn test_probe_amount_scaling() {
        // Higher price impact → lower probe floor (inversely proportional).
        //   probe = gas_cost / price_impact, so doubling price impact halves
        //   the probe.
        let total = BigUint::from(10_000_000u64);
        let algo = PathFrankWolfeAlgorithm::default();
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
        let algo = PathFrankWolfeAlgorithm::default();

        let probe_amount = algo
            .compute_probe_amount(&total, 0.10, 1000.0)
            .unwrap();
        assert_eq!(probe_amount, BigUint::from(10_000u64));
    }

    #[test]
    fn test_probe_amount_zero_price_impact() {
        let total = BigUint::from(1_000_000u64);
        let algo = PathFrankWolfeAlgorithm::default();

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

    // ==================== find_candidate_path / is_duplicate_path ====================

    fn pfw_algo(max_hops: usize) -> PathFrankWolfeAlgorithm {
        PathFrankWolfeAlgorithm::new(
            AlgorithmConfig::new(1, max_hops, StdDuration::from_millis(1000), None).unwrap(),
            PathFrankWolfeConfig::default(),
        )
    }

    #[test]
    fn test_is_duplicate_path_exact_match() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let candidate =
            vec![HopDescriptor::new("P1".to_string(), token_a.clone(), token_b.clone())
                .with_amounts(BigUint::from(200u64), BigUint::from(50_000u64))];
        let alloc = PathAllocation {
            hops: vec![HopDescriptor::new("P1".to_string(), token_a, token_b)
                .with_amounts(BigUint::from(200u64), BigUint::from(50_000u64))],
            flow_fraction: 1.0,
            amount_in: BigUint::from(100u64),
            amount_out: BigUint::from(200u64),
            marginal_price_product: 2.0,
        };
        assert!(PathFrankWolfeAlgorithm::is_duplicate_path(&candidate, &[alloc]));
    }

    #[test]
    fn test_is_duplicate_path_shared_prefix() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let zero = BigUint::from(0u64);
        let alloc = PathAllocation {
            hops: vec![
                HopDescriptor::new("P1".to_string(), token_a.clone(), token_b.clone())
                    .with_amounts(zero.clone(), zero.clone()),
                HopDescriptor::new("P2".to_string(), token_b.clone(), token_c.clone())
                    .with_amounts(zero.clone(), zero.clone()),
            ],
            flow_fraction: 1.0,
            amount_in: BigUint::from(100u64),
            amount_out: BigUint::from(200u64),
            marginal_price_product: 1.0,
        };

        // [P1, P3] shares first hop with [P1, P2] but diverges at hop 2
        let candidate = vec![
            HopDescriptor::new("P1".to_string(), token_a, token_b.clone())
                .with_amounts(zero.clone(), zero.clone()),
            HopDescriptor::new("P3".to_string(), token_b, token_c)
                .with_amounts(zero.clone(), zero.clone()),
        ];
        assert!(!PathFrankWolfeAlgorithm::is_duplicate_path(&candidate, &[alloc]));
    }

    #[test]
    fn test_is_duplicate_path_same_pool_different_tokens() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        // Same pool "P1" but with different token pairs — not a duplicate.
        let zero = BigUint::from(0u64);
        let alloc = PathAllocation {
            hops: vec![HopDescriptor::new("P1".to_string(), token_a.clone(), token_b.clone())
                .with_amounts(zero.clone(), zero.clone())],
            flow_fraction: 1.0,
            amount_in: BigUint::from(100u64),
            amount_out: BigUint::from(200u64),
            marginal_price_product: 2.0,
        };
        let candidate = vec![HopDescriptor::new("P1".to_string(), token_a, token_c)
            .with_amounts(zero.clone(), zero.clone())];
        assert!(!PathFrankWolfeAlgorithm::is_duplicate_path(&candidate, &[alloc]));
    }

    #[tokio::test]
    async fn test_shared_first_pool_two_outputs() {
        // Two paths share pool P1 (A→B) and diverge at B→C via P2 vs P3.
        //
        // P2 has higher initial rate but degrades after one allocation; BF then discovers P3.
        // Verifies: `is_duplicate_path` returns false, `build_split_route` emits 3 swaps,
        // P1 gas counted once.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, graph_manager) = setup_market_unweighted(vec![
            (
                "P1",
                &token_a,
                &token_b,
                Box::new(ConstantProductSim {
                    reserve_0: BigUint::from(10_000u64),
                    reserve_1: BigUint::from(10_000u64),
                    gas: 50_000,
                }) as Box<dyn ProtocolSim>,
            ),
            (
                "P2",
                &token_b,
                &token_c,
                Box::new(ConstantProductSim {
                    reserve_0: BigUint::from(1_000u64),
                    reserve_1: BigUint::from(1_500u64),
                    gas: 50_000,
                }) as Box<dyn ProtocolSim>,
            ),
            (
                "P3",
                &token_b,
                &token_c,
                Box::new(ConstantProductSim {
                    reserve_0: BigUint::from(1_000u64),
                    reserve_1: BigUint::from(1_000u64),
                    gas: 50_000,
                }) as Box<dyn ProtocolSim>,
            ),
        ]);

        let algo = pfw_algo(3);
        let probe_amount = BigUint::from(1_000u64);
        let ord = order(&token_a, &token_c, 1_000, OrderSide::Sell);

        let ctx = algo
            .inner
            .build_context(graph_manager.graph(), market, None, None, &ord)
            .await
            .unwrap();

        // First candidate: P2 has 1.5x rate vs P3's 1.0x → finds [P1, P2].
        let first_path = algo
            .find_candidate_path(&ctx, &[], &probe_amount)
            .unwrap();
        assert_eq!(first_path[0].descriptor.component_id, "P1");
        assert_eq!(first_path[1].descriptor.component_id, "P2");

        let first_amount_out = first_path[1].amount_out.clone();
        let first_alloc = PathAllocation {
            hops: first_path,
            flow_fraction: 0.5,
            amount_in: probe_amount.clone(),
            amount_out: first_amount_out,
            marginal_price_product: 1.5,
        };

        // After allocating 1000 A on [P1, P2], P2 degrades enough that BF finds [P1, P3].
        let second_path = algo
            .find_candidate_path(&ctx, std::slice::from_ref(&first_alloc), &probe_amount)
            .unwrap();
        assert_eq!(second_path[0].descriptor.component_id, "P1");
        assert_eq!(second_path[1].descriptor.component_id, "P3");

        // Shared prefix [P1] does not make these duplicates.
        assert!(!PathFrankWolfeAlgorithm::is_duplicate_path(
            &second_path,
            std::slice::from_ref(&first_alloc)
        ));

        let second_amount_out = second_path[1].amount_out.clone();
        let second_alloc = PathAllocation {
            hops: second_path,
            flow_fraction: 0.5,
            amount_in: probe_amount.clone(),
            amount_out: second_amount_out,
            marginal_price_product: 1.0,
        };

        // build_split_route must emit 3 swaps: one combined A→B (P1), two B→C (P2, P3).
        let all_allocs = [first_alloc, second_alloc];
        let route = build_split_route(&all_allocs, &ctx.market_data, &ord).unwrap();
        let swaps = route.swaps();
        assert_eq!(swaps.len(), 3, "expected P1 + P2 + P3 = 3 swaps");
        let ids: Vec<&str> = swaps
            .iter()
            .map(|s| s.component_id())
            .collect();
        assert_eq!(
            ids.iter()
                .filter(|&&id| id == "P1")
                .count(),
            1,
            "P1 deduplicated"
        );
        assert!(ids.contains(&"P2"));
        assert!(ids.contains(&"P3"));
        // P1 gas counted once: P1(50k) + P2(50k) + P3(50k) = 150k.
        assert_eq!(route.total_gas(), BigUint::from(150_000u64));
    }

    #[tokio::test]
    async fn test_duplicate_path_stops_iteration() {
        // When BF repeatedly returns the same path, `is_duplicate_path` detects it so the
        // Frank-Wolfe loop can stop.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, graph_manager) = setup_market_unweighted(vec![(
            "P1",
            &token_a,
            &token_b,
            Box::new(MockProtocolSim::new(2.0)) as Box<dyn ProtocolSim>,
        )]);

        let algo = pfw_algo(2);
        let probe_amount = BigUint::from(100u64);
        let ord = order(&token_a, &token_b, 100, OrderSide::Sell);

        let ctx = algo
            .inner
            .build_context(graph_manager.graph(), market, None, None, &ord)
            .await
            .unwrap();

        let first_path = algo
            .find_candidate_path(&ctx, &[], &probe_amount)
            .unwrap();
        assert_eq!(first_path[0].descriptor.component_id, "P1");

        let first_alloc = PathAllocation {
            hops: first_path,
            flow_fraction: 1.0,
            amount_in: probe_amount.clone(),
            amount_out: BigUint::from(200u64),
            marginal_price_product: 2.0,
        };

        // P1 is the only pool — BF returns it again.
        let second_path = algo
            .find_candidate_path(&ctx, std::slice::from_ref(&first_alloc), &probe_amount)
            .unwrap();
        assert!(PathFrankWolfeAlgorithm::is_duplicate_path(
            &second_path,
            std::slice::from_ref(&first_alloc)
        ));
    }

    #[test]
    fn test_with_zero_gas_zeroes_gas_keeps_amounts() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let sim = MockProtocolSim::new(2.0).with_gas(50_000);

        let overrides = MarketOverrides::empty()
            .with_override("P1".to_string(), Box::new(sim.clone()))
            .with_zero_gas("P1".to_string(), token_a.address.clone(), token_b.address.clone());

        let result = overrides
            .get(&"P1".to_string())
            .unwrap()
            .get_amount_out(BigUint::from(100u64), &token_a, &token_b)
            .unwrap();

        assert_eq!(result.amount, BigUint::from(200u64), "amount unaffected");
        assert_eq!(result.gas, BigUint::ZERO, "gas zeroed by with_zero_gas");
    }
}
