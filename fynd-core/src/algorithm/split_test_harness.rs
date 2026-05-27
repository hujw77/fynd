//! Test helpers for split-routing algorithm scenarios.

/// Returns `(fraction_for_pool_1, total_output)` — the theoretically optimal output when
/// splitting `trade_amount` between two constant-product pools with no fees.
///
/// Finds the split where both pools offer the same marginal rate on the last unit traded.
/// Negative allocations are clamped to `0`.
#[allow(dead_code)]
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
}
