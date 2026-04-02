//! Market recording and golden output types for integration testing.

/// Golden output types shared between the `record-market` tool and integration tests.
pub mod golden;
/// Compressed JSON I/O for market recordings.
pub mod io;
/// Core recording types: [`MarketRecording`], [`RecordedUpdate`], [`RecordingMetadata`].
pub mod types;

pub use golden::{
    DerivedDataMetrics, GoldenFile, GoldenMetadata, GoldenOutput, GoldenScenario, TestScenario,
};
pub use io::{read_recording, write_recording};
pub use types::{MarketRecording, RecordedUpdate, RecordingMetadata};
