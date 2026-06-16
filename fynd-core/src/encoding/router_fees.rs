//! Router fee configuration mirrored from the on-chain `FeeCalculator` contract.
//!
//! [`RouterFees`] holds the default router fees, the per-client rates (already resolved
//! against the defaults), and the contract's fee-unit precision scale. The encoder reads a
//! [`SharedRouterFees`] snapshot on every encode; a background
//! [`RouterFeeFetcher`](crate::encoding::fee_fetcher::RouterFeeFetcher) refreshes it from
//! chain, so swapping in a FeeCalculator with a different precision is tracked automatically.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use tycho_simulation::tycho_common::Bytes;

/// Legacy basis-points denominator: client fees on the wire use 10,000 = 100%.
///
/// This is the calldata convention between Fynd and the router (`clientFeeBps`), independent
/// of the FeeCalculator's internal precision. The contract scales `clientFeeBps` into its own
/// fee units by `max_fee_units / LEGACY_BPS_DENOMINATOR`.
pub const LEGACY_BPS_DENOMINATOR: u64 = 10_000;

/// Effective router fee rates for one client, together with the precision scale they are
/// expressed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeRates {
    on_output: u32,
    on_client_fee: u32,
    max_fee_units: u64,
}

impl FeeRates {
    /// Creates fee rates expressed in the given fee-unit scale (`max_fee_units` = 100%).
    pub fn new(on_output: u32, on_client_fee: u32, max_fee_units: u64) -> Self {
        Self { on_output, on_client_fee, max_fee_units }
    }

    /// Router fee charged on the swap output, in fee units.
    pub fn on_output(&self) -> u32 {
        self.on_output
    }

    /// Router share of the client fee, in fee units.
    pub fn on_client_fee(&self) -> u32 {
        self.on_client_fee
    }

    /// Fee units representing 100% (the contract's `MAX_FEE_BPS`).
    pub fn max_fee_units(&self) -> u64 {
        self.max_fee_units
    }

    /// Factor converting a legacy basis-point fee into fee units
    /// (`max_fee_units / LEGACY_BPS_DENOMINATOR`).
    pub fn fee_units_per_bps(&self) -> u64 {
        self.max_fee_units / LEGACY_BPS_DENOMINATOR
    }

    /// Combined denominator when two fee-unit rates are multiplied (`max_fee_units`²).
    pub fn max_fee_units_squared(&self) -> u128 {
        (self.max_fee_units as u128) * (self.max_fee_units as u128)
    }
}

/// Router fee configuration: precision scale, default rates, and per-client overrides.
///
/// Mirrors the on-chain FeeCalculator state. Rates are in fee units where
/// [`max_fee_units`](Self::max_fee_units) represents 100%.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterFees {
    max_fee_units: u64,
    default_fee_on_output: u32,
    default_fee_on_client_fee: u32,
    /// Per-client resolved `(fee_on_output, fee_on_client_fee)` in fee units. The fetcher
    /// has already applied each client's overrides over the defaults, so a lookup miss simply
    /// falls back to the defaults.
    custom_fees: HashMap<Bytes, (u32, u32)>,
}

impl RouterFees {
    /// Creates a fee configuration from on-chain values. `custom_fees` maps a client to its
    /// resolved `(fee_on_output, fee_on_client_fee)` pair in fee units.
    pub fn new(
        max_fee_units: u64,
        default_fee_on_output: u32,
        default_fee_on_client_fee: u32,
        custom_fees: HashMap<Bytes, (u32, u32)>,
    ) -> Self {
        Self { max_fee_units, default_fee_on_output, default_fee_on_client_fee, custom_fees }
    }

    /// Fee units representing 100% (the contract's `MAX_FEE_BPS`).
    pub fn max_fee_units(&self) -> u64 {
        self.max_fee_units
    }

    /// Resolves the effective fee rates for `client`: the per-client pair when present,
    /// otherwise the defaults. The fetcher has already applied `FeeCalculator._getFeeInfo`'s
    /// override-or-default logic per field, so this is a plain lookup. The contract's
    /// precision scale travels with the rates.
    pub fn fees_for(&self, client: &Bytes) -> FeeRates {
        let (on_output, on_client_fee) = self
            .custom_fees
            .get(client)
            .copied()
            .unwrap_or((self.default_fee_on_output, self.default_fee_on_client_fee));
        FeeRates::new(on_output, on_client_fee, self.max_fee_units)
    }

    /// Number of clients with at least one custom fee override.
    pub fn custom_client_count(&self) -> usize {
        self.custom_fees.len()
    }
}

/// Cloneable handle to the router fee configuration shared between the encoder (reader)
/// and the background fee fetcher (writer).
///
/// Empty until the [`RouterFeeFetcher`](crate::encoding::fee_fetcher::RouterFeeFetcher) lands
/// its first successful fetch. Callers must handle the absent case rather than fall back to
/// guessed fees.
#[derive(Debug, Clone, Default)]
pub struct SharedRouterFees(Arc<RwLock<Option<RouterFees>>>);

impl SharedRouterFees {
    /// Returns a copy of the current fee configuration, or `None` if no on-chain fetch has
    /// succeeded yet.
    pub fn snapshot(&self) -> Option<RouterFees> {
        self.0
            .read()
            .expect("router fees lock poisoned")
            .clone()
    }

    /// Replaces the fee configuration with freshly fetched on-chain values.
    pub fn set(&self, fees: RouterFees) {
        *self
            .0
            .write()
            .expect("router fees lock poisoned") = Some(fees);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCALE: u64 = 100_000_000;

    fn client(byte: u8) -> Bytes {
        Bytes::from(vec![byte; 20])
    }

    #[test]
    fn test_fess_for_unknown_client() {
        let fees = RouterFees::new(SCALE, 100_000, 20_000_000, HashMap::new());

        assert_eq!(fees.fees_for(&client(0xAA)), FeeRates::new(100_000, 20_000_000, SCALE));
    }

    #[test]
    fn test_fess_for_known_client() {
        let custom = HashMap::from([(client(0xAA), (50_000u32, 10_000_000u32))]);
        let fees = RouterFees::new(SCALE, 100_000, 20_000_000, custom);

        // Known client gets its stored pair; everyone else gets the defaults.
        assert_eq!(fees.fees_for(&client(0xAA)), FeeRates::new(50_000, 10_000_000, SCALE));
        assert_eq!(fees.fees_for(&client(0xBB)), FeeRates::new(100_000, 20_000_000, SCALE));
    }
}
