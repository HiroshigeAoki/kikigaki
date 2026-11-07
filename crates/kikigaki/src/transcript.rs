//! Best-effort, opt-in JSONL transcript logging.

use std::io::Write;
use std::path::Path;

use chrono::{DateTime, Local, SecondsFormat, Utc};
use serde::Serialize;

#[derive(Serialize)]
struct Transcript<'a> {
    at: String,
    engine: &'a str,
    raw: &'a str,
    text: &'a str,
}

fn append(dir: &Path, engine: &str, raw: &str, text: &str, at: DateTime<Utc>) {
    if let Err(error) = append_inner(dir, engine, raw, text, at) {
        tracing::warn!(%error, path = %dir.display(), "write transcript log");
    }
}

pub fn enqueue(
    enabled: bool,
    dir: std::path::PathBuf,
    engine: &'static str,
    raw: String,
    text: String,
    at: DateTime<Utc>,
) {
    if enabled {
        std::thread::spawn(move || append(&dir, engine, &raw, &text, at));
    }
}

fn append_inner(
    dir: &Path,
    engine: &str,
    raw: &str,
    text: &str,
    at: DateTime<Utc>,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;

    let date = at.with_timezone(&Local).format("%Y-%m-%d");
    let path = dir.join(format!("{date}.jsonl"));
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    serde_json::to_writer(
        &mut file,
        &Transcript {
            at: at.to_rfc3339_opts(SecondsFormat::Millis, true),
            engine,
            raw,
            text,
        },
    )?;
    file.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_jsonl_and_creates_date_named_file() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("transcripts");
        let at = "2026-10-11T01:02:03Z".parse().unwrap();
        append(&dir, "whisper", "raw", "final", at);
        append(&dir, "whisper", "raw2", "final2", at);
        let path = dir.join(
            at.with_timezone(&Local)
                .format("%Y-%m-%d.jsonl")
                .to_string(),
        );
        let lines = std::fs::read_to_string(path).unwrap();
        assert_eq!(lines.lines().count(), 2);
        let row: serde_json::Value = serde_json::from_str(lines.lines().next().unwrap()).unwrap();
        assert_eq!(row["engine"], "whisper");
        assert_eq!(row["raw"], "raw");
        assert_eq!(row["text"], "final");
        assert_eq!(row["at"], "2026-10-11T01:02:03.000Z");
    }

    #[cfg(unix)]
    #[test]
    fn creates_private_directory_and_file() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("transcripts");
        let at = Utc::now();
        append(&dir, "local", "raw", "text", at);
        let path = dir.join(
            at.with_timezone(&Local)
                .format("%Y-%m-%d.jsonl")
                .to_string(),
        );
        assert_eq!(
            std::fs::metadata(dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn write_errors_do_not_propagate_or_panic() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("not-a-directory");
        std::fs::write(&file, "occupied").unwrap();
        append(&file, "local", "raw", "text", Utc::now());
    }

    #[test]
    fn disabled_logging_creates_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("transcripts");
        enqueue(
            false,
            dir.clone(),
            "local",
            "raw".into(),
            "text".into(),
            Utc::now(),
        );
        assert!(!dir.exists());
    }
}
