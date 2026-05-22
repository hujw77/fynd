use std::collections::HashMap;

use num_bigint::BigUint;
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
    /// last simulated. Used to compute per-path price impact:
    /// `PI = 1 - amount_out / (amount_in * marginal_price_product)`.
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
