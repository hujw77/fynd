use std::collections::HashMap;

use num_bigint::BigUint;
use num_traits::Zero;
use tycho_simulation::tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use crate::types::ComponentId;

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
/// Assumes `f` is roughly unimodal. `max_evals` controls the number of function
/// evaluations (higher = more precise but slower).
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
pub(crate) fn split_amount(total: &BigUint, fraction: f64) -> (BigUint, BigUint) {
    // Scale fraction to fixed-point with 18 decimal digits of precision.
    let scale: u64 = 1_000_000_000_000_000_000;
    let numerator = (fraction * scale as f64) as u64;
    let part = (total * BigUint::from(numerator)) / BigUint::from(scale);
    let remainder = total - &part;
    (part, remainder)
}

/// Errors from split-routing math utilities.
#[derive(Debug, Clone, thiserror::Error)]
pub(crate) enum SplitMathError {
    #[error("fractions slice must not be empty")]
    EmptyFractions,
    #[error("all fractions are zero, cannot normalize")]
    AllZeroFractions,
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
    let sum: f64 = fractions.iter().sum();
    if sum == 0.0 {
        return Err(SplitMathError::AllZeroFractions);
    }
    for f in fractions.iter_mut() {
        *f /= sum;
    }
    Ok(())
}

/// Convert fractions (summing to 1.0) into `BigUint` amounts summing exactly to `total`.
///
/// The last element absorbs any rounding remainder so the sum is exact.
pub(crate) fn fractions_to_amounts(total: &BigUint, fractions: &[f64]) -> Vec<BigUint> {
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
    amounts
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
    fn test_fractions_to_amounts_exact_sum() {
        let total = BigUint::from(999_999_999_999_999_999_u64);
        let fractions = [0.3, 0.5, 0.2];
        let amounts = fractions_to_amounts(&total, &fractions);
        assert_eq!(amounts.len(), 3);
        let sum: BigUint = amounts.iter().sum();
        assert_eq!(sum, total, "amounts must sum exactly to total");
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
    fn test_normalize_fractions_invalid(#[case] input: &[f64], #[case] expected: SplitMathError) {
        let mut fractions = input.to_vec();
        let err = normalize_fractions(&mut fractions).unwrap_err();
        assert_eq!(err.to_string(), expected.to_string());
    }

    #[test]
    fn test_golden_section_finds_maximum() {
        // Maximize -(x - 0.3)^2; true maximum at x = 0.3.
        let f = |x: f64| -(x - 0.3) * (x - 0.3);
        let result = golden_section_search(f, 0.0, 1.0, 100);
        assert!((result - 0.3).abs() < 1e-4, "expected ~0.3, got {result}");
    }
}
