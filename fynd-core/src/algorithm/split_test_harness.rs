//! Test helpers for split-routing algorithm scenarios.

/// Returns `(fraction_for_pool_1, total_output)` — the theoretically optimal output when
/// splitting `trade_amount` between two constant-product pools.
///
/// Negative allocations are clamped to `0` — a clamped value means the full trade routes through
/// the other pool.
#[allow(dead_code)]
pub(crate) fn optimal_two_pool_output(
    reserve_in_1: f64,
    reserve_out_1: f64,
    reserve_in_2: f64,
    reserve_out_2: f64,
    trade_amount: f64,
) -> (f64, f64) {
    // The optimal split equates marginal prices across pools. Let `d = √(k1/k2)` where
    // `k_i = reserve_in_i · reserve_out_i`. Solving the marginal-price-equality condition with
    // `a_1 + a_2 = trade_amount` gives:
    //
    // ```text
    // d   = sqrt((reserve_in_1 * reserve_out_1) / (reserve_in_2 * reserve_out_2))
    // a_2 = (trade_amount + reserve_in_1 - d * reserve_in_2) / (d + 1)
    // a_1 = trade_amount - a_2
    // ```
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

    #[test]
    fn test_optimal_two_pool_output_symmetric() {
        // Identical pools → d=1 → 50/50 split is always optimal.
        let (fraction, _) =
            optimal_two_pool_output(10_000.0, 10_000.0, 10_000.0, 10_000.0, 1_000.0);
        assert!(
            (fraction - 0.5).abs() < 1e-9,
            "symmetric pools: expected fraction 0.5, got {fraction}"
        );
    }

    #[test]
    fn test_optimal_two_pool_output_asymmetric() {
        // Pool 1: reserve_in=100, reserve_out=400  →  k1=40_000, K1=200
        // Pool 2: reserve_in=100, reserve_out=100  →  k2=10_000, K2=100
        // d = sqrt(40_000/10_000) = 2
        // a_2 = (400 + 100 - 2·100) / (2+1) = 300/3 = 100
        // a_1 = 300  →  fraction = 300/400 = 0.75
        //
        // Verify optimality: marginal prices after split must be equal.
        // Pool 1: k1 / (100+300)² = 40_000/160_000 = 0.25
        // Pool 2: k2 / (100+100)² = 10_000/40_000  = 0.25  ✓
        let (fraction, total_out) = optimal_two_pool_output(100.0, 400.0, 100.0, 100.0, 400.0);
        assert!(
            (fraction - 0.75).abs() < 1e-9,
            "asymmetric pools: expected fraction 0.75, got {fraction}"
        );
        // out_1 = 300·400/(100+300) = 300, out_2 = 100·100/(100+100) = 50
        assert!(
            (total_out - 350.0).abs() < 1e-9,
            "asymmetric pools: expected total output 350.0, got {total_out}"
        );
    }
}
