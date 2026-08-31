//! Platform-independent audio, protocol, session, and engine support for kikigaki.

#![deny(missing_docs)]

/// PCM conversion, resampling, and channel helpers.
pub mod audio;
/// Runtime configuration loading and defaults.
pub mod config;
/// Asynchronous hayamimi WebSocket client.
pub mod engine;
/// JSONL utterance and startup metrics.
pub mod metrics;
/// Pinned model manifest, installation, and verification.
pub mod models;
/// Ordered transcription post-processing and its dedicated worker.
pub mod postprocess;
/// Hayamimi wire protocol types and parsing.
pub mod protocol;
/// Mojicast punctuation tokenization and punctuation decision rules.
pub mod punct;
/// Remote hayamimi engine adapter and sidecar process abstractions.
#[cfg(feature = "remote-engine")]
pub mod remote;
/// Hot-reloaded transcription replacement rules.
pub mod replace;
/// Time-injected push-to-talk state machine.
pub mod session;
/// Model installation and engine-startup coordination.
pub mod startup;
/// Transcription output normalization.
pub mod text;
