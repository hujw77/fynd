//! Data Transfer Objects (DTOs) for the HTTP API.
//!
//! Types are defined in `fynd-rpc-types` and re-exported here. Conversions
//! between DTO types and `fynd-core` domain types are implemented in
//! `fynd-rpc-types` via `From`/`Into` (enabled by the `core` feature).

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

pub use fynd_rpc_types::{
    BlockInfo, EncodingOptions, ErrorResponse, HealthStatus, InstanceInfo, Order, OrderQuote,
    OrderSide, PermitDetails, PermitSingle, PriceGuardConfig, Quote, QuoteOptions, QuoteRequest,
    QuoteStatus, Route, Swap, Transaction, UserTransferType,
};

/// Query parameters for `GET /v1/debug/components`.
#[derive(Debug, Deserialize, IntoParams)]
pub struct DebugComponentsQuery {
    /// Maximum number of components to return.
    #[param(default = 100, minimum = 1, maximum = 1000)]
    pub limit: Option<usize>,
    /// Number of sorted results to skip before returning rows.
    #[param(default = 0, minimum = 0)]
    pub offset: Option<usize>,
    /// Optional token address filter. Only components containing this token are returned.
    pub token: Option<String>,
}

/// A single market component currently tracked by Fynd.
#[derive(Debug, Serialize, ToSchema)]
pub struct DebugComponentEntry {
    /// Component identifier.
    pub id: String,
    /// Protocol system name (for example `uniswap_v3`).
    pub protocol_system: String,
    /// Protocol type name reported by Tycho.
    pub protocol_type_name: String,
    /// Chain name for this component.
    pub chain: String,
    /// Token addresses connected by this component.
    pub tokens: Vec<String>,
}

/// Sync status for a protocol synchronizer inside Fynd's market feed.
#[derive(Debug, Serialize, ToSchema)]
pub struct DebugProtocolSyncStatus {
    /// Protocol system name (for example `uniswap_v3`).
    pub protocol_system: String,
    /// Human-readable synchronizer state.
    pub state: String,
}

/// Response body for `GET /v1/debug/components`.
#[derive(Debug, Serialize, ToSchema)]
pub struct DebugComponentsResponse {
    /// Total number of tracked components before filtering.
    pub total_components: usize,
    /// Number of components matching the optional token filter.
    pub filtered_components: usize,
    /// Number of components returned in this page.
    pub returned_components: usize,
    /// Effective page size used for this response.
    pub limit: usize,
    /// Number of matching rows skipped before this page.
    pub offset: usize,
    /// Token address filter applied to the results, if any.
    pub token_filter: Option<String>,
    /// Last market-data update timestamp in seconds since epoch, if any.
    pub last_updated_timestamp: Option<u64>,
    /// Per-protocol synchronizer states tracked by the feed.
    pub protocol_sync_statuses: Vec<DebugProtocolSyncStatus>,
    /// Returned component rows.
    pub components: Vec<DebugComponentEntry>,
}
