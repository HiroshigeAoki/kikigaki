use serde::Deserialize;

/// Audio sample rate expected by hayamimi's ingest endpoint.
pub const SAMPLE_RATE: u32 = 16_000;

/// A final transcription returned by hayamimi.
#[derive(Debug, Clone, PartialEq)]
pub struct Final {
    /// Session generation assigned by the engine adapter.
    pub gen: u64,
    /// Transcribed text.
    pub text: String,
    /// Detected language code.
    pub lang: String,
    /// Engine-reported processing latency in milliseconds.
    pub engine_latency_ms: Option<f64>,
    /// Audio chunks dropped for this generation.
    pub dropped_chunks: u64,
    /// Time between VAD close and ASR completion in milliseconds.
    pub vad_close_to_asr_end_ms: Option<u64>,
    /// Time spent post-processing the transcription in milliseconds.
    pub postprocess_ms: Option<u64>,
}

/// An event received from hayamimi's ingest endpoint.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The ingest endpoint is ready for audio at the given sample rate.
    Ready {
        /// Sample rate accepted by the ingest endpoint.
        sr: u32,
    },
    /// An interim transcription.
    Partial(String),
    /// A completed transcription.
    Final(Final),
    /// A refined transcription.
    Refine(String),
    /// An engine error message.
    Error(String),
    /// An event type not otherwise recognized by kikigaki-core.
    Other(String),
}

#[derive(Deserialize)]
struct Raw {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    lang: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    latency_ms: Option<f64>,
    #[serde(default)]
    sr: Option<u32>,
}

impl Event {
    /// Parses one JSON text frame from hayamimi.
    pub fn parse(s: &str) -> Result<Self, serde_json::Error> {
        let raw: Raw = serde_json::from_str(s)?;
        Ok(match raw.kind.as_str() {
            "ready" => Self::Ready {
                sr: raw.sr.unwrap_or(SAMPLE_RATE),
            },
            "partial" => Self::Partial(raw.text),
            "final" => Self::Final(Final {
                gen: 0,
                text: raw.text,
                lang: raw.lang,
                engine_latency_ms: raw.latency_ms,
                dropped_chunks: 0,
                vad_close_to_asr_end_ms: None,
                postprocess_ms: None,
            }),
            "refine" => Self::Refine(raw.text),
            "error" => Self::Error(raw.message),
            other => Self::Other(other.to_owned()),
        })
    }
}

/// Returns the exact hayamimi ingest handshake frame.
pub fn hello_frame() -> &'static str {
    r#"{"sr":16000,"format":"pcm_s16le","channels":1}"#
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ready() {
        let e = Event::parse(r#"{"type":"ready","sr":16000}"#).unwrap();
        assert_eq!(e, Event::Ready { sr: 16000 });
    }

    #[test]
    fn parses_final_with_all_fields() {
        let e = Event::parse(r#"{"type":"final","text":"こんにちは。","lang":"ja","speaker":"","latency_ms":98.4,"tier":"reazon"}"#).unwrap();
        assert_eq!(
            e,
            Event::Final(Final {
                gen: 0,
                text: "こんにちは。".into(),
                lang: "ja".into(),
                engine_latency_ms: Some(98.4),
                dropped_chunks: 0,
                vad_close_to_asr_end_ms: None,
                postprocess_ms: None,
            })
        );
    }

    #[test]
    fn parses_final_with_null_latency() {
        let e = Event::parse(
            r#"{"type":"final","text":"x","lang":"","speaker":"","latency_ms":null,"tier":""}"#,
        )
        .unwrap();
        assert!(matches!(
            e,
            Event::Final(Final {
                gen: 0,
                engine_latency_ms: None,
                dropped_chunks: 0,
                vad_close_to_asr_end_ms: None,
                postprocess_ms: None,
                ..
            })
        ));
    }

    #[test]
    fn parses_partial_refine_error_and_unknown() {
        assert_eq!(
            Event::parse(r#"{"type":"partial","text":"こん"}"#).unwrap(),
            Event::Partial("こん".into())
        );
        assert_eq!(
            Event::parse(r#"{"type":"refine","text":"a","lang":"ja"}"#).unwrap(),
            Event::Refine("a".into())
        );
        assert_eq!(
            Event::parse(r#"{"type":"error","message":"busy"}"#).unwrap(),
            Event::Error("busy".into())
        );
        assert_eq!(
            Event::parse(r#"{"type":"session_start"}"#).unwrap(),
            Event::Other("session_start".into())
        );
    }

    #[test]
    fn rejects_non_json() {
        assert!(Event::parse("nope").is_err());
    }

    #[test]
    fn hello_frame_is_exact() {
        assert_eq!(
            hello_frame(),
            r#"{"sr":16000,"format":"pcm_s16le","channels":1}"#
        );
    }
}
