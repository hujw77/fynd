//! Tycho protocol system discovery.

use std::error::Error;

use anyhow::Result;
use tracing::{error, info};
use tycho_simulation::{
    tycho_client::rpc::{HttpRPCClient, HttpRPCClientOptions, ProtocolSystemsParams, RPCClient},
    tycho_common::models::Chain,
};

fn format_error_chain(err: &dyn Error) -> String {
    let mut chain = vec![err.to_string()];
    let mut current = err.source();

    while let Some(source) = current {
        chain.push(source.to_string());
        current = source.source();
    }

    chain.join(": ")
}

/// Fetches all available protocol systems from the Tycho RPC.
pub async fn fetch_protocol_systems(
    tycho_url: &str,
    auth_key: Option<&str>,
    use_tls: bool,
    chain: Chain,
) -> Result<Vec<String>> {
    info!("Fetching available protocol systems from Tycho RPC...");
    let rpc_url =
        if use_tls { format!("https://{tycho_url}") } else { format!("http://{tycho_url}") };
    let rpc_options = HttpRPCClientOptions::new().with_auth_key(auth_key.map(|s| s.to_string()));
    let rpc_client = HttpRPCClient::new(&rpc_url, rpc_options).map_err(|err| {
        error!(
            rpc_url,
            error_chain = %format_error_chain(&err),
            "failed to construct Tycho HTTP client"
        );
        err
    })?;

    let request = ProtocolSystemsParams::new(chain);
    let response = rpc_client
        .get_protocol_systems(request)
        .await
        .map_err(|err| {
            error!(
                rpc_url,
                error_chain = %format_error_chain(&err),
                "failed to fetch protocol systems from Tycho RPC"
            );
            err
        })?;
    let protocols = response
        .data()
        .protocol_systems()
        .to_vec();
    info!("Fetched {} protocol system(s) from Tycho RPC", protocols.len());
    Ok(protocols)
}
