use std::collections::{HashMap, HashSet};

use num_bigint::BigUint;
use num_traits::{ToPrimitive, Zero};
use tycho_simulation::tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use crate::{algorithm::AlgorithmError, feed::market_data::MarketState, types::ComponentId};

pub(crate) struct HopDescriptor {
    pub(crate) component_id: ComponentId,
    pub(crate) token_in: Token,
    pub(crate) token_out: Token,
}

/// A fully-simulated path allocation.
///
/// One path in the current split solution, with the fraction of total `amount_in`
/// currently allocated to it. All fractions across allocations sum to 1.0.
pub(crate) struct PathAllocation {
    pub(crate) hops: Vec<HopDescriptor>,
    /// Fraction of total input on this path (0 < f <= 1).
    pub(crate) flow_fraction: f64,
    pub(crate) amount_in: BigUint,
    pub(crate) amount_out: BigUint,
    /// Product of marginal prices along all hops at the time this allocation was
    /// last simulated.
    pub(crate) marginal_price_product: f64,
}

/// Output of simulating one path at a given input amount.
pub(crate) struct SimResult {
    pub(crate) amount_out: BigUint,
    /// Raw per-hop sum; use only via `evaluate_total_output`.
    pub(crate) gas: u64,
    pub(crate) marginal_price_product: f64,
}

/// Pool state overrides for reused pools in subsequent simulation/route searches.
#[derive(Default)]
pub(crate) struct MarketOverrides(HashMap<ComponentId, Box<dyn ProtocolSim>>);

impl MarketOverrides {
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    /// Insert a degraded pool state as an override.
    pub(crate) fn with_override(mut self, id: ComponentId, sim: Box<dyn ProtocolSim>) -> Self {
        self.0.insert(id, sim);
        self
    }

    /// Insert a zero-gas wrapper around an existing sim. The underlying pool still
    /// produces correct amounts; only `get_amount_out().gas` is zeroed. Use for pools
    /// already present in `current_allocations` — their gas is paid once in the
    /// combined transaction.
    pub(crate) fn with_zero_gas(mut self, id: ComponentId, sim: Box<dyn ProtocolSim>) -> Self {
        self.0
            .insert(id, Box::new(ZeroGasSim(sim)));
        self
    }

    pub(crate) fn get(&self, id: &ComponentId) -> Option<&dyn ProtocolSim> {
        self.0.get(id).map(|b| b.as_ref())
    }
}

/// Wrapper that delegates all [`ProtocolSim`] calls unchanged except
/// [`get_amount_out`](ProtocolSim::get_amount_out), where it zeroes the returned gas.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ZeroGasSim(Box<dyn ProtocolSim>);

#[typetag::serde]
impl ProtocolSim for ZeroGasSim {
    fn fee(&self) -> f64 {
        self.0.fee()
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        self.0.spot_price(base, quote)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let mut result = self
            .0
            .get_amount_out(amount_in, token_in, token_out)?;
        result.gas = BigUint::ZERO;
        result.new_state = Box::new(ZeroGasSim(result.new_state));
        Ok(result)
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        self.0.get_limits(sell_token, buy_token)
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        tokens: &HashMap<Bytes, Token>,
        balances: &Balances,
    ) -> Result<(), TransitionError> {
        self.0
            .delta_transition(delta, tokens, balances)
    }

    fn clone_box(&self) -> Box<dyn ProtocolSim> {
        Box::new(ZeroGasSim(self.0.clone_box()))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn eq(&self, other: &dyn ProtocolSim) -> bool {
        other
            .as_any()
            .downcast_ref::<Self>()
            .map(|o| self.0.eq(&*o.0))
            .unwrap_or(false)
    }
}

/// Find the `x` in `[lo, hi]` that maximises `f(x)` using golden-section search.
///
/// Assumes `f` is roughly unimodal (has one maximum). `max_evals` controls the
/// number of function evaluations (higher = more precise but slower).
pub(crate) fn golden_section_search(
    f: impl Fn(f64) -> f64,
    mut lo: f64,
    mut hi: f64,
    max_evals: usize,
) -> f64 {
    let inv_phi = (5_f64.sqrt() - 1.0) / 2.0;

    let mut x1 = hi - inv_phi * (hi - lo);
    let mut x2 = lo + inv_phi * (hi - lo);
    let mut f1 = f(x1);
    let mut f2 = f(x2);
    // Two evaluations consumed so far.
    let remaining = max_evals.saturating_sub(2);

    for _ in 0..remaining {
        if f1 < f2 {
            lo = x1;
            x1 = x2;
            f1 = f2;
            x2 = lo + inv_phi * (hi - lo);
            f2 = f(x2);
        } else {
            hi = x2;
            x2 = x1;
            f2 = f1;
            x1 = hi - inv_phi * (hi - lo);
            f1 = f(x1);
        }
    }

    if f1 >= f2 {
        x1
    } else {
        x2
    }
}

/// Split `total` into `(part, remainder)` where `part ≈ total * fraction`.
///
/// Both values always sum exactly to `total` — no tokens lost to rounding.
/// `fraction` is clamped to `[0.0, 1.0]` before use.
pub(crate) fn split_amount(total: &BigUint, fraction: f64) -> (BigUint, BigUint) {
    let clamped = fraction.clamp(0.0, 1.0);
    // Scale fraction to fixed-point with 18 decimal digits of precision.
    let scale: u64 = 1_000_000_000_000_000_000;
    let numerator = (clamped * scale as f64) as u64;
    let part = (total * BigUint::from(numerator)) / BigUint::from(scale);
    let remainder = total - &part;
    (part, remainder)
}

/// Errors from split-routing math utilities.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub(crate) enum SplitMathError {
    #[error("fractions slice must not be empty")]
    EmptyFractions,
    #[error("all fractions are zero, cannot normalize")]
    AllZeroFractions,
    #[error("fractions must not be negative")]
    NegativeFraction,
}

/// Normalize a slice of fractions so they sum to 1.0.
///
/// # Errors
///
/// Returns [`SplitMathError::EmptyFractions`] if the slice is empty, or
/// [`SplitMathError::AllZeroFractions`] if every element is zero.
pub(crate) fn normalize_fractions(fractions: &mut [f64]) -> Result<(), SplitMathError> {
    if fractions.is_empty() {
        return Err(SplitMathError::EmptyFractions);
    }
    if fractions.iter().any(|&f| f < 0.0) {
        return Err(SplitMathError::NegativeFraction);
    }
    let sum: f64 = fractions.iter().sum();
    if sum == 0.0 {
        return Err(SplitMathError::AllZeroFractions);
    }
    for f in fractions.iter_mut() {
        *f /= sum;
    }
    Ok(())
}

/// Convert fractions (summing to 1.0) into `BigUint` amounts summing exactly
/// to `total`.
///
/// The last element absorbs any rounding remainder so the sum is exact.
///
/// # Errors
///
/// Returns [`SplitMathError::EmptyFractions`] if `fractions` is empty.
pub(crate) fn fractions_to_amounts(
    total: &BigUint,
    fractions: &[f64],
) -> Result<Vec<BigUint>, SplitMathError> {
    if fractions.is_empty() {
        return Err(SplitMathError::EmptyFractions);
    }
    let n = fractions.len();
    let mut amounts = Vec::with_capacity(n);
    let mut running_sum = BigUint::zero();

    for &frac in &fractions[..n - 1] {
        let (part, _) = split_amount(total, frac);
        running_sum += &part;
        amounts.push(part);
    }

    // Last element gets the remainder to guarantee exact sum.
    amounts.push(total - &running_sum);
    Ok(amounts)
}

/// Product of spot prices along a path — approximates the exchange rate at
/// near-zero input.
pub(crate) fn compute_marginal_price_product(
    hops: &[HopDescriptor],
    market: &MarketState,
    overrides: &MarketOverrides,
) -> Result<f64, AlgorithmError> {
    let mut product = 1.0;
    for hop in hops {
        let sim = overrides
            .get(&hop.component_id)
            .or_else(|| market.get_simulation_state(&hop.component_id))
            .ok_or_else(|| AlgorithmError::DataNotFound {
                kind: "simulation state",
                id: Some(hop.component_id.clone()),
            })?;
        let price = sim
            .spot_price(&hop.token_in, &hop.token_out)
            .map_err(|e| AlgorithmError::SimulationFailed {
                component_id: hop.component_id.clone(),
                error: e.to_string(),
            })?;
        product *= price;
    }
    Ok(product)
}

/// Simulates a path hop-by-hop, threading output of each hop as input to the
/// next.
///
/// Checks `overrides` before falling back to the live market state for each
/// hop. Returns the final output amount, raw gas sum, and marginal price
/// product.
pub(crate) fn simulate_path(
    hops: &[HopDescriptor],
    amount_in: &BigUint,
    market: &MarketState,
    overrides: &MarketOverrides,
) -> Result<SimResult, AlgorithmError> {
    let mut current_amount = amount_in.clone();
    let mut total_gas: u64 = 0;

    for hop in hops {
        let sim = overrides
            .get(&hop.component_id)
            .or_else(|| market.get_simulation_state(&hop.component_id))
            .ok_or_else(|| AlgorithmError::DataNotFound {
                kind: "simulation state",
                id: Some(hop.component_id.clone()),
            })?;

        let result = sim
            .get_amount_out(current_amount, &hop.token_in, &hop.token_out)
            .map_err(|e| AlgorithmError::SimulationFailed {
                component_id: hop.component_id.clone(),
                error: e.to_string(),
            })?;

        // Cap at u64::MAX instead of panicking on overflow.
        total_gas = total_gas.saturating_add(result.gas.to_u64().unwrap_or(u64::MAX));
        current_amount = result.amount;
    }

    let marginal_price_product = compute_marginal_price_product(hops, market, overrides)?;

    Ok(SimResult { amount_out: current_amount, gas: total_gas, marginal_price_product })
}

/// Simulates all paths at their current fractions and returns
/// `(total_amount_out, total_gas)`. `paths[i]` corresponds to `fractions[i]`.
pub(crate) fn evaluate_total_output(
    paths: &[&[HopDescriptor]],
    fractions: &[f64],
    total_amount: &BigUint,
    market: &MarketState,
    overrides: &MarketOverrides,
) -> Result<(BigUint, u64), AlgorithmError> {
    let amounts = fractions_to_amounts(total_amount, fractions)
        .map_err(|e| AlgorithmError::Other(e.to_string()))?;

    let mut total_out = BigUint::zero();
    let mut total_gas: u64 = 0;
    let mut seen_hops: HashSet<(ComponentId, Bytes, Bytes)> = HashSet::new();

    for (path, amount) in paths.iter().zip(amounts.iter()) {
        if amount.is_zero() {
            continue;
        }

        let mut current_amount = amount.clone();

        for hop in path.iter() {
            let sim = overrides
                .get(&hop.component_id)
                .or_else(|| market.get_simulation_state(&hop.component_id))
                .ok_or_else(|| AlgorithmError::DataNotFound {
                    kind: "simulation state",
                    id: Some(hop.component_id.clone()),
                })?;

            let result = sim
                .get_amount_out(current_amount, &hop.token_in, &hop.token_out)
                .map_err(|e| AlgorithmError::SimulationFailed {
                    component_id: hop.component_id.clone(),
                    error: e.to_string(),
                })?;

            // Shared pre-split hops appear in multiple paths but are
            // executed once on-chain — count gas only once per unique
            // (pool, token_in, token_out).
            let hop_key = (
                hop.component_id.clone(),
                hop.token_in.address.clone(),
                hop.token_out.address.clone(),
            );
            if seen_hops.insert(hop_key) {
                total_gas = total_gas.saturating_add(result.gas.to_u64().unwrap_or(u64::MAX));
            }
            current_amount = result.amount;
        }

        total_out += current_amount;
    }

    Ok((total_out, total_gas))
}

/// Builds a post-swap view of the market after all paths in a single
/// split-route solution have been executed.
///
/// For example, if the current solution splits 1000 USDC→ETH across:
///   - Path 1: USDC→WETH via Uniswap (600 USDC)
///   - Path 2: USDC→WBTC→WETH via Curve+Balancer (400 USDC)
///
/// this function simulates both swaps and returns overrides where Uniswap,
/// Curve, and Balancer all reflect their post-swap reserves.
///
/// Paths are processed in order so shared pools accumulate the effects of
/// all prior paths.
pub(crate) fn build_degraded_market(
    paths: &[&PathAllocation],
    market: &MarketState,
) -> MarketOverrides {
    let mut states: HashMap<ComponentId, Box<dyn ProtocolSim>> = HashMap::new();

    for path in paths {
        let mut current_amount = path.amount_in.clone();

        for hop in &path.hops {
            let sim = states
                .get(&hop.component_id)
                .map(|b| b.as_ref())
                .or_else(|| market.get_simulation_state(&hop.component_id));

            let Some(sim) = sim else { break };

            let Ok(result) = sim.get_amount_out(current_amount, &hop.token_in, &hop.token_out)
            else {
                break;
            };

            current_amount = result.amount;
            states.insert(hop.component_id.clone(), result.new_state);
        }
    }

    let mut overrides = MarketOverrides::empty();
    for (id, sim) in states {
        overrides = overrides.with_override(id, sim);
    }
    overrides
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[test]
    fn test_split_amount_exact_sum() {
        let total = BigUint::from(1_000_000_000_000_000_000_u64);
        for fraction in [0.1, 0.5, 0.9, 0.999] {
            let (part, remainder) = split_amount(&total, fraction);
            assert_eq!(
                &part + &remainder,
                total,
                "part + remainder must equal total for fraction={fraction}"
            );
        }
    }

    #[test]
    fn test_split_amount_edge_fraction_zero() {
        let total = BigUint::from(1_000_000_000_000_000_000_u64);
        let (part, remainder) = split_amount(&total, 0.0);
        assert!(part.is_zero());
        assert_eq!(remainder, total);
    }

    #[test]
    fn test_split_amount_clamps_above_one() {
        let total = BigUint::from(1_000_000_000_000_000_000_u64);
        let (part, remainder) = split_amount(&total, 1.5);
        assert_eq!(part, total);
        assert!(remainder.is_zero());
    }

    #[test]
    fn test_split_amount_clamps_negative() {
        let total = BigUint::from(1_000_000_000_000_000_000_u64);
        let (part, remainder) = split_amount(&total, -0.5);
        assert!(part.is_zero());
        assert_eq!(remainder, total);
    }

    #[test]
    fn test_fractions_to_amounts_exact_sum() {
        let total = BigUint::from(999_999_999_999_999_999_u64);
        let fractions = [0.3, 0.5, 0.2];
        let amounts = fractions_to_amounts(&total, &fractions).unwrap();
        assert_eq!(amounts.len(), 3);
        let sum: BigUint = amounts.iter().sum();
        assert_eq!(sum, total, "amounts must sum exactly to total");
    }

    #[test]
    fn test_fractions_to_amounts_empty() {
        let total = BigUint::from(1_000_u64);
        let err = fractions_to_amounts(&total, &[]).unwrap_err();
        assert_eq!(err, SplitMathError::EmptyFractions);
    }

    #[rstest]
    #[case::already_normalized(&[0.3, 0.5, 0.2])]
    #[case::drift(&[0.33, 0.33, 0.33])]
    fn test_normalize_fractions(#[case] input: &[f64]) {
        let mut fractions = input.to_vec();
        normalize_fractions(&mut fractions).unwrap();
        let sum: f64 = fractions.iter().sum();
        assert!((sum - 1.0).abs() < f64::EPSILON);
    }

    #[rstest]
    #[case::empty(&[], SplitMathError::EmptyFractions)]
    #[case::all_zeros(&[0.0, 0.0, 0.0], SplitMathError::AllZeroFractions)]
    #[case::negative(&[-0.5, 0.5], SplitMathError::NegativeFraction)]
    fn test_normalize_fractions_invalid(#[case] input: &[f64], #[case] expected: SplitMathError) {
        let mut fractions = input.to_vec();
        let err = normalize_fractions(&mut fractions).unwrap_err();
        assert_eq!(err, expected);
    }

    #[test]
    fn test_golden_section_finds_maximum() {
        // Maximize -(x - 0.3)^2; true maximum at x = 0.3.
        let f = |x: f64| -(x - 0.3) * (x - 0.3);
        let result = golden_section_search(f, 0.0, 1.0, 100);
        assert!((result - 0.3).abs() < 1e-4, "expected ~0.3, got {result}");
    }

    // ==================== Simulation Utility Tests ====================

    use crate::{
        algorithm::test_utils::{component, token, ConstantProductSim, MockProtocolSim},
        feed::market_data::MarketState,
    };

    fn make_market(pools: Vec<(&str, Vec<Token>, Box<dyn ProtocolSim>)>) -> MarketState {
        let mut market = MarketState::new();
        for (pool_id, tokens, sim) in pools {
            market.upsert_components(std::iter::once(component(pool_id, &tokens)));
            market.update_states([(pool_id.to_string(), sim)]);
            market.upsert_tokens(tokens);
        }
        market
    }

    #[test]
    fn test_compute_marginal_price_product_single_hop() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let market = make_market(vec![(
            "pool_ab",
            vec![token_a.clone(), token_b.clone()],
            Box::new(MockProtocolSim::new(3.0)),
        )]);

        let hops = [HopDescriptor {
            component_id: "pool_ab".to_string(),
            token_in: token_a,
            token_out: token_b,
        }];

        let product =
            compute_marginal_price_product(&hops, &market, &MarketOverrides::empty()).unwrap();
        assert!((product - 3.0).abs() < f64::EPSILON, "expected 3.0, got {product}");
    }

    #[test]
    fn test_compute_marginal_price_product_multi_hop() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let market = make_market(vec![
            (
                "pool_ab",
                vec![token_a.clone(), token_b.clone()],
                Box::new(MockProtocolSim::new(2.0)),
            ),
            (
                "pool_bc",
                vec![token_b.clone(), token_c.clone()],
                Box::new(MockProtocolSim::new(4.0)),
            ),
        ]);

        let hops = [
            HopDescriptor {
                component_id: "pool_ab".to_string(),
                token_in: token_a,
                token_out: token_b.clone(),
            },
            HopDescriptor {
                component_id: "pool_bc".to_string(),
                token_in: token_b,
                token_out: token_c,
            },
        ];

        let product =
            compute_marginal_price_product(&hops, &market, &MarketOverrides::empty()).unwrap();
        // 2.0 * 4.0 = 8.0
        assert!((product - 8.0).abs() < f64::EPSILON, "expected 8.0, got {product}");
    }

    #[test]
    fn test_compute_marginal_price_product_uses_overrides() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let market = make_market(vec![(
            "pool_ab",
            vec![token_a.clone(), token_b.clone()],
            Box::new(MockProtocolSim::new(3.0)),
        )]);

        let hops = [HopDescriptor {
            component_id: "pool_ab".to_string(),
            token_in: token_a,
            token_out: token_b,
        }];

        // Override pool_ab with a different spot price.
        let overrides = MarketOverrides::empty()
            .with_override("pool_ab".to_string(), Box::new(MockProtocolSim::new(7.0)));

        let product = compute_marginal_price_product(&hops, &market, &overrides).unwrap();
        assert!((product - 7.0).abs() < f64::EPSILON, "expected 7.0, got {product}");
    }

    #[test]
    fn test_simulate_path_correct_output() {
        // 2-hop path A→B→C with spot prices 2.0 and 3.0.
        // Input 1000 should thread through: 1000*2=2000, 2000*3=6000.
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let market = make_market(vec![
            (
                "pool_ab",
                vec![token_a.clone(), token_b.clone()],
                Box::new(MockProtocolSim::new(2.0)),
            ),
            (
                "pool_bc",
                vec![token_b.clone(), token_c.clone()],
                Box::new(MockProtocolSim::new(3.0)),
            ),
        ]);

        let hops = [
            HopDescriptor {
                component_id: "pool_ab".to_string(),
                token_in: token_a,
                token_out: token_b.clone(),
            },
            HopDescriptor {
                component_id: "pool_bc".to_string(),
                token_in: token_b,
                token_out: token_c,
            },
        ];

        let amount_in = BigUint::from(1000u64);
        let overrides = MarketOverrides::empty();
        let result = simulate_path(&hops, &amount_in, &market, &overrides).unwrap();

        assert_eq!(result.amount_out, BigUint::from(6000u64));

        // spot_price(A→B) = 2.0, spot_price(B→C) = 3.0 → product = 6.0
        assert!(
            (result.marginal_price_product - 6.0).abs() < f64::EPSILON,
            "expected marginal_price_product 6.0, got {}",
            result.marginal_price_product
        );
    }

    #[test]
    fn test_market_overrides_with_zero_gas() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let sim_ab = MockProtocolSim::new(2.0).with_gas(100_000);
        let sim_bc = MockProtocolSim::new(3.0).with_gas(70_000);
        let market = make_market(vec![
            ("pool_ab", vec![token_a.clone(), token_b.clone()], Box::new(sim_ab.clone())),
            ("pool_bc", vec![token_b.clone(), token_c.clone()], Box::new(sim_bc.clone())),
        ]);

        // Zero gas on pool_ab, leave pool_bc as a normal override.
        let overrides = MarketOverrides::empty()
            .with_zero_gas("pool_ab".to_string(), Box::new(sim_ab))
            .with_override("pool_bc".to_string(), Box::new(sim_bc));

        let hops_ab = [HopDescriptor {
            component_id: "pool_ab".to_string(),
            token_in: token_a.clone(),
            token_out: token_b.clone(),
        }];
        let hops_bc = [HopDescriptor {
            component_id: "pool_bc".to_string(),
            token_in: token_b,
            token_out: token_c,
        }];
        let amount_in = BigUint::from(1000u64);

        let normal_ab =
            simulate_path(&hops_ab, &amount_in, &market, &MarketOverrides::empty()).unwrap();
        let zero_gas_ab = simulate_path(&hops_ab, &amount_in, &market, &overrides).unwrap();

        assert_eq!(normal_ab.amount_out, zero_gas_ab.amount_out);
        assert!(normal_ab.gas > 0, "normal gas should be non-zero");
        assert_eq!(zero_gas_ab.gas, 0, "zero-gas override should report gas=0");

        // pool_bc is a normal override — its gas should be unaffected.
        let result_bc = simulate_path(&hops_bc, &amount_in, &market, &overrides).unwrap();
        assert_eq!(result_bc.gas, 70_000, "non-zero-gas override should keep its gas");
    }

    #[test]
    fn test_evaluate_total_output_two_paths() {
        // 50/50 split of 1000 across two parallel 1-hop paths:
        //
        //       500 -- pool_1 (price=2.0) --> 1000
        //      /                                   \
        //  1000                                     2500
        //      \                                   /
        //       500 -- pool_2 (price=3.0) --> 1500
        //
        // total_gas = 50k + 60k = 110k
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let market = make_market(vec![
            (
                "pool_1",
                vec![token_a.clone(), token_b.clone()],
                Box::new(MockProtocolSim::new(2.0).with_gas(50_000)),
            ),
            (
                "pool_2",
                vec![token_a.clone(), token_b.clone()],
                Box::new(MockProtocolSim::new(3.0).with_gas(60_000)),
            ),
        ]);

        let hops_1 = [HopDescriptor {
            component_id: "pool_1".to_string(),
            token_in: token_a.clone(),
            token_out: token_b.clone(),
        }];
        let hops_2 = [HopDescriptor {
            component_id: "pool_2".to_string(),
            token_in: token_a,
            token_out: token_b,
        }];

        let paths: Vec<&[HopDescriptor]> = vec![&hops_1, &hops_2];
        let fractions = [0.5, 0.5];
        let total_amount = BigUint::from(1000u64);
        let overrides = MarketOverrides::empty();

        let (total_out, total_gas) =
            evaluate_total_output(&paths, &fractions, &total_amount, &market, &overrides).unwrap();

        assert_eq!(total_out, BigUint::from(2500u64));
        assert_eq!(total_gas, 110_000);
    }

    #[test]
    fn test_evaluate_total_output_gas_deduplication() {
        // Two paths share pool P1 (pre-split hop). P1's gas should be
        // counted once, not twice.
        //
        //              P2 (50k gas) --> C
        //             /
        //  A -- P1 --+
        //             \
        //              P3 (70k gas) --> D
        //
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let token_d = token(0x0D, "D");
        let market = make_market(vec![
            (
                "P1",
                vec![token_a.clone(), token_b.clone()],
                Box::new(MockProtocolSim::new(2.0).with_gas(100_000)),
            ),
            (
                "P2",
                vec![token_b.clone(), token_c.clone()],
                Box::new(MockProtocolSim::new(1.5).with_gas(50_000)),
            ),
            (
                "P3",
                vec![token_b.clone(), token_d.clone()],
                Box::new(MockProtocolSim::new(3.0).with_gas(70_000)),
            ),
        ]);

        // Path 1: A -> P1 -> B -> P2 -> C (uses P1 and P2)
        let hops_1 = [
            HopDescriptor {
                component_id: "P1".to_string(),
                token_in: token_a.clone(),
                token_out: token_b.clone(),
            },
            HopDescriptor {
                component_id: "P2".to_string(),
                token_in: token_b.clone(),
                token_out: token_c,
            },
        ];
        // Path 2: A -> P1 -> B -> P3 -> D (uses P1 and P3)
        let hops_2 = [
            HopDescriptor {
                component_id: "P1".to_string(),
                token_in: token_a,
                token_out: token_b.clone(),
            },
            HopDescriptor { component_id: "P3".to_string(), token_in: token_b, token_out: token_d },
        ];

        let paths: Vec<&[HopDescriptor]> = vec![&hops_1, &hops_2];
        let fractions = [0.5, 0.5];
        let total_amount = BigUint::from(1000u64);
        let overrides = MarketOverrides::empty();

        let (_, total_gas) =
            evaluate_total_output(&paths, &fractions, &total_amount, &market, &overrides).unwrap();

        // P1 counted once: 100k + 50k + 70k = 220k
        assert_eq!(total_gas, 220_000);
    }

    #[test]
    fn test_gas_dedup_different_tokens() {
        // A single 3-token pool used for two different token pairs is two
        // distinct hops — gas must be counted for each.
        //
        //  A -- TRIPOOL (A→B) --> B    (path 1)
        //  B -- TRIPOOL (B→C) --> C    (path 2)
        //
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let market = make_market(vec![(
            "tripool",
            vec![token_a.clone(), token_b.clone(), token_c.clone()],
            Box::new(MockProtocolSim::new(1.0).with_gas(80_000)),
        )]);

        let hops_1 = [HopDescriptor {
            component_id: "tripool".to_string(),
            token_in: token_a,
            token_out: token_b.clone(),
        }];
        let hops_2 = [HopDescriptor {
            component_id: "tripool".to_string(),
            token_in: token_b,
            token_out: token_c,
        }];

        let paths: Vec<&[HopDescriptor]> = vec![&hops_1, &hops_2];
        let fractions = [0.5, 0.5];
        let total_amount = BigUint::from(1000u64);
        let overrides = MarketOverrides::empty();

        let (_, total_gas) =
            evaluate_total_output(&paths, &fractions, &total_amount, &market, &overrides).unwrap();

        // Different token pairs on the same pool: 80k + 80k = 160k
        assert_eq!(total_gas, 160_000);
    }

    #[test]
    fn test_build_degraded_market_degrades_used_pools() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let market = make_market(vec![(
            "pool_ab",
            vec![token_a.clone(), token_b.clone()],
            Box::new(ConstantProductSim {
                reserve_0: BigUint::from(10_000u64),
                reserve_1: BigUint::from(20_000u64),
                gas: 50_000,
            }),
        )]);

        let allocation = PathAllocation {
            hops: vec![HopDescriptor {
                component_id: "pool_ab".to_string(),
                token_in: token_a.clone(),
                token_out: token_b.clone(),
            }],
            flow_fraction: 1.0,
            amount_in: BigUint::from(1000u64),
            amount_out: BigUint::from(1818u64),
            marginal_price_product: 2.0,
        };

        let degraded = build_degraded_market(&[&allocation], &market);

        // xy=k: amount_out = amount_in * reserve_out / (reserve_in + amount_in)
        // Fresh pool (10000/20000): 100 * 20000 / (10000 + 100) = 198
        let probe = BigUint::from(100u64);
        let fresh_out = market
            .get_simulation_state("pool_ab")
            .unwrap()
            .get_amount_out(probe.clone(), &token_a, &token_b)
            .unwrap()
            .amount;
        assert_eq!(fresh_out, BigUint::from(198u64));

        // The 1000-in allocation produces 1000*20000/(10000+1000) = 1818 out,
        // shifting reserves to (10000+1000, 20000-1818) = (11000, 18182).
        // Degraded pool: 100 * 18182 / (11000 + 100) = 163
        let degraded_out = degraded
            .get(&"pool_ab".to_string())
            .unwrap()
            .get_amount_out(probe, &token_a, &token_b)
            .unwrap()
            .amount;
        assert_eq!(degraded_out, BigUint::from(163u64));
    }
}
