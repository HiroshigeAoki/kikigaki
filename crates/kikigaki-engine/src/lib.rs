//! In-process speech engine for kikigaki: silero VAD + ReazonSpeech transducer on `sherpa-onnx`,
//! and (feature `punct`) mojicast punctuation on `ort`.
//!
//! Implemented in Phase 2 Task 4 (`LocalEngine`) and Task 6 (`MojicastPunctuator`).

#![deny(missing_docs)]

/// WAV decoding and sample-rate conversion helpers.
pub mod audio;
mod framer;
/// Hotword artifact materialization and model-derived BPE vocabulary synthesis.
pub mod hotwords;
/// Local in-process engine and its testable worker interfaces.
pub mod local;
/// Mojicast punctuation inference through dynamically loaded ONNX Runtime.
#[cfg(feature = "punct")]
pub mod punct;
/// Thin sherpa-onnx model construction and inference wrappers.
pub mod sherpa;
