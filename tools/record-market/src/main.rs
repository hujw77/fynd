use std::path::PathBuf;

use clap::Parser;
use tracing_subscriber::EnvFilter;

mod golden;
mod recorder;

#[derive(Parser)]
#[command(name = "record-market", about = "Capture Tycho market state for integration testing")]
struct Cli {
    /// Tycho WebSocket URL.
    #[arg(long, env = "TYCHO_URL")]
    tycho_url: String,

    /// Tycho API key.
    #[arg(long, env = "TYCHO_API_KEY")]
    tycho_api_key: String,

    /// Ethereum RPC URL for gas price capture.
    #[arg(long, env = "RPC_URL")]
    rpc_url: Option<String>,

    /// Duration to record stream updates (seconds).
    #[arg(long, default_value = "600")]
    duration_secs: u64,

    /// Output directory for fixtures.
    #[arg(long, default_value = "fynd-core/tests/fixtures")]
    output_dir: PathBuf,

    /// Protocol systems to record (comma-delimited).
    #[arg(long, value_delimiter = ',')]
    protocols: Option<Vec<String>>,

    /// Minimum TVL in ETH for component filtering.
    #[arg(long, default_value = "10.0")]
    min_tvl: f64,

    /// Minimum token quality score.
    #[arg(long, default_value = "100")]
    min_token_quality: i32,

    /// Only include tokens traded within this many days.
    #[arg(long, default_value = "3")]
    traded_n_days_ago: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    tracing::info!("connecting to Tycho at {}", cli.tycho_url);

    let recording_opts = recorder::RecordingOptions {
        tycho_url: cli.tycho_url,
        tycho_api_key: cli.tycho_api_key,
        duration_secs: cli.duration_secs,
        protocols: cli.protocols,
        min_tvl: cli.min_tvl,
        min_token_quality: cli.min_token_quality,
        traded_n_days_ago: cli.traded_n_days_ago,
        rpc_url: cli.rpc_url,
    };

    let recording = recorder::record_market(&recording_opts).await?;

    tracing::info!(
        updates = recording.updates.len(),
        duration_s = recording
            .metadata
            .recording_duration_secs,
        "market recording captured"
    );

    std::fs::create_dir_all(&cli.output_dir)?;
    let recording_path = cli
        .output_dir
        .join("market_recording.json.zst");
    fynd_test_fixtures::write_recording(&recording, &recording_path)?;
    tracing::info!(path = %recording_path.display(), "recording written");

    // Read back the recording from disk so golden generation uses the same
    // deserialized data that integration tests will see (VM states filtered
    // during serialization won't be present in the deserialized version).
    let recording = fynd_test_fixtures::read_recording(&recording_path)?;

    let pools_toml = include_str!("../../../worker_pools.toml");
    let expected = golden::generate_expected_outputs(recording, pools_toml).await?;
    let expected_path = cli
        .output_dir
        .join("expected_outputs.json");
    let json = serde_json::to_string_pretty(&expected)?;
    std::fs::write(&expected_path, json)?;
    tracing::info!(
        scenarios = expected.scenarios.len(),
        path = %expected_path.display(),
        "expected outputs written"
    );

    Ok(())
}
