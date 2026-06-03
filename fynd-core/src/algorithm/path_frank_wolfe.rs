//! Path-based Frank-Wolfe split-routing algorithm.
//!
//! Wraps [`BellmanFordAlgorithm`] to find multiple candidate paths and then
//! optimally split the input amount across them. The inner BF instance handles
//! single-path discovery; this module layers on the Frank-Wolfe optimisation
//! loop to determine the best split fractions.

use std::time::Duration;

use super::{Algorithm, AlgorithmConfig, AlgorithmError, BellmanFordAlgorithm};
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
}
