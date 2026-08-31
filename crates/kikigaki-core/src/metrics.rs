use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

/// Latency and transcript metadata for one utterance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Utterance {
    /// RFC3339 timestamp filled by the application.
    pub ts: String,
    /// Captured audio duration in milliseconds.
    pub audio_ms: u64,
    /// Time from hotkey press to a final received while still recording.
    pub since_press_ms: Option<u64>,
    /// Time from hotkey release to final transcription.
    pub release_to_final_ms: Option<u64>,
    /// Time from final transcription to completed paste.
    pub final_to_paste_ms: Option<u64>,
    /// Processing latency reported by the active engine.
    #[serde(alias = "hayamimi_latency_ms", default)]
    pub engine_latency_ms: Option<f64>,
    /// Time between VAD close and ASR completion in milliseconds.
    #[serde(default)]
    pub vad_close_to_asr_end_ms: Option<u64>,
    /// Time spent post-processing the transcription in milliseconds.
    #[serde(default)]
    pub postprocess_ms: Option<u64>,
    /// Audio chunks dropped for this generation.
    #[serde(default)]
    pub dropped_chunks: u64,
    /// Session generation assigned to the utterance.
    #[serde(default)]
    pub gen: u64,
    /// Number of characters in normalized output text.
    pub chars: usize,
    /// Language code reported by hayamimi.
    pub lang: String,
    /// Whether finalization timed out or failed.
    pub timeout: bool,
    /// Whether pasting the normalized transcription failed.
    #[serde(default)]
    pub paste_failed: bool,
}

/// Startup timing and model-installation metadata for one process launch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Startup {
    /// RFC3339 timestamp filled by the application.
    pub ts: String,
    /// Time from process entry to the first engine-ready event in milliseconds.
    pub process_to_ready_ms: u64,
    /// Whether any models were downloaded during this launch.
    pub cold: bool,
}

/// One typed JSONL metrics record.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind")]
pub enum MetricRow {
    /// Per-utterance timing and transcript metadata.
    #[serde(rename = "utterance")]
    Utterance(Utterance),
    /// First-ready timing for a process launch.
    #[serde(rename = "startup")]
    Startup(Startup),
}

impl<'de> Deserialize<'de> for MetricRow {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut value = serde_json::Value::deserialize(deserializer)?;
        let kind = value
            .as_object_mut()
            .ok_or_else(|| D::Error::custom("metric row must be an object"))?
            .remove("kind");
        match kind {
            None => serde_json::from_value(value)
                .map(Self::Utterance)
                .map_err(D::Error::custom),
            Some(serde_json::Value::String(ref kind)) if kind == "utterance" => {
                serde_json::from_value(value)
                    .map(Self::Utterance)
                    .map_err(D::Error::custom)
            }
            Some(serde_json::Value::String(ref kind)) if kind == "startup" => {
                serde_json::from_value(value)
                    .map(Self::Startup)
                    .map_err(D::Error::custom)
            }
            Some(serde_json::Value::String(kind)) => {
                Err(D::Error::custom(format!("unknown metric kind {kind:?}")))
            }
            Some(other) => Err(D::Error::custom(format!(
                "metric kind must be a string, got {other}"
            ))),
        }
    }
}

/// Serializes a metric row as one newline-terminated JSON record.
pub fn to_line(row: &MetricRow) -> String {
    match serde_json::to_string(row) {
        Ok(mut line) => {
            line.push('\n');
            line
        }
        Err(error) => unreachable!("MetricRow serialization cannot fail: {error}"),
    }
}

/// Creates parent directories and appends a line to a metrics file.
pub fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utterance() -> Utterance {
        Utterance {
            ts: "2026-08-27T12:34:56Z".into(),
            audio_ms: 1_250,
            since_press_ms: Some(1_370),
            release_to_final_ms: Some(120),
            final_to_paste_ms: Some(14),
            engine_latency_ms: Some(98.4),
            vad_close_to_asr_end_ms: Some(7),
            postprocess_ms: Some(3),
            dropped_chunks: 2,
            gen: 4,
            chars: 5,
            lang: "ja".into(),
            timeout: false,
            paste_failed: true,
        }
    }

    #[test]
    fn line_round_trips_as_json_and_ends_with_newline() {
        let expected = MetricRow::Utterance(utterance());
        let line = to_line(&expected);
        assert!(line.ends_with('\n'));
        assert!(line.contains(r#""kind":"utterance""#));
        let actual: MetricRow = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn legacy_utterance_without_kind_uses_latency_alias_and_defaults() {
        let line = r#"{"ts":"2026-08-27T12:34:56Z","audio_ms":1250,"since_press_ms":1370,"release_to_final_ms":120,"final_to_paste_ms":14,"hayamimi_latency_ms":98.4,"chars":5,"lang":"ja","timeout":false}"#;
        let row: MetricRow = serde_json::from_str(line).unwrap();
        let MetricRow::Utterance(row) = row else {
            panic!("legacy row was not an utterance")
        };
        assert_eq!(row.engine_latency_ms, Some(98.4));
        assert_eq!(row.vad_close_to_asr_end_ms, None);
        assert_eq!(row.postprocess_ms, None);
        assert_eq!(row.dropped_chunks, 0);
        assert_eq!(row.gen, 0);
    }

    #[test]
    fn startup_row_round_trips_with_kind() {
        let expected = MetricRow::Startup(Startup {
            ts: "2026-08-27T12:34:56Z".into(),
            process_to_ready_ms: 321,
            cold: true,
        });
        let line = to_line(&expected);
        assert!(line.contains(r#""kind":"startup""#));
        assert_eq!(
            serde_json::from_str::<MetricRow>(line.trim()).unwrap(),
            expected
        );
    }

    #[test]
    fn append_line_creates_parent_and_appends_two_lines() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nested/latency.jsonl");
        append_line(&path, &to_line(&MetricRow::Utterance(utterance()))).unwrap();
        append_line(&path, &to_line(&MetricRow::Utterance(utterance()))).unwrap();

        let contents = std::fs::read_to_string(path).unwrap();
        assert_eq!(contents.lines().count(), 2);
    }
}
