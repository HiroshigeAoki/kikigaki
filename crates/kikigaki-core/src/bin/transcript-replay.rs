use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;

use kikigaki_core::punct::nfkc;
use kikigaki_core::replace::{builtin_rules, Rules};
use serde::{Deserialize, Serialize};

const USAGE: &str = "transcript-replay --transcripts <dir> --user-rules <replace.toml> [--no-builtin] --out <file.jsonl>";

struct Args {
    transcripts: PathBuf,
    user_rules: PathBuf,
    no_builtin: bool,
    out: PathBuf,
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut args = std::env::args().skip(1);
        let (mut transcripts, mut user_rules, mut out) = (None, None, None);
        let mut no_builtin = false;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--no-builtin" => no_builtin = true,
                "--transcripts" | "--user-rules" | "--out" => {
                    let value = PathBuf::from(
                        args.next()
                            .ok_or_else(|| anyhow::anyhow!("missing value: {arg}; {USAGE}"))?,
                    );
                    match arg.as_str() {
                        "--transcripts" => transcripts = Some(value),
                        "--user-rules" => user_rules = Some(value),
                        _ => out = Some(value),
                    }
                }
                _ => anyhow::bail!("unknown argument: {arg}; {USAGE}"),
            }
        }
        Ok(Self {
            transcripts: transcripts.ok_or_else(|| anyhow::anyhow!(USAGE))?,
            user_rules: user_rules.ok_or_else(|| anyhow::anyhow!(USAGE))?,
            out: out.ok_or_else(|| anyhow::anyhow!(USAGE))?,
            no_builtin,
        })
    }
}

#[derive(Deserialize)]
struct Record {
    at: String,
    engine: String,
    raw: String,
}

#[derive(Serialize)]
struct UserHit {
    from: String,
    to: String,
    left: String,
    right: String,
}

#[derive(Serialize)]
struct Replay {
    at: String,
    engine: String,
    changed: bool,
    baseline: String,
    proposal: String,
    user_hits: Vec<UserHit>,
}

fn replay_record(record: Record, base: &Rules, proposed: &Rules, user: &Rules) -> Replay {
    let normalized = nfkc(record.raw.trim());
    let baseline = base.apply(&normalized);
    let (proposal, applied) = proposed.apply_traced(&normalized);
    let user_hits = applied
        .into_iter()
        .filter(|hit| user.contains_source(&hit.from))
        .map(|hit| {
            let left: Vec<char> = normalized[..hit.start].chars().rev().take(10).collect();
            UserHit {
                from: hit.from,
                to: hit.to,
                left: left.into_iter().rev().collect(),
                right: normalized[hit.end..].chars().take(10).collect(),
            }
        })
        .collect();
    Replay {
        at: record.at,
        engine: record.engine,
        changed: baseline != proposal,
        baseline,
        proposal,
        user_hits,
    }
}

#[derive(Default)]
struct Counts {
    records: usize,
    changed: usize,
    user_hits: usize,
    malformed: usize,
}

fn run(args: Args) -> anyhow::Result<Counts> {
    let user = Rules::parse(&fs::read_to_string(&args.user_rules)?)?;
    let base = if args.no_builtin {
        Rules::default()
    } else {
        builtin_rules()?
    };
    // Same exact-source override and longest-match merge as Pipeline::rebuild_effective.
    let proposed = Rules::merge(&base, &user);
    let mut files = fs::read_dir(&args.transcripts)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    files.retain(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "jsonl"));
    files.sort();
    // Avoid truncating an input file when the output path is inside the input directory.
    let output = if args.out.exists() {
        Some(fs::canonicalize(&args.out)?)
    } else {
        None
    };
    for path in &files {
        anyhow::ensure!(
            output.as_ref() != Some(&fs::canonicalize(path)?),
            "output must not be an input file"
        );
    }
    #[cfg(unix)]
    let output = {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(args.out)?
    };
    #[cfg(not(unix))]
    let output = File::create(args.out)?;
    let mut writer = BufWriter::new(output);
    let mut counts = Counts::default();
    for path in files {
        for line in BufReader::new(File::open(path)?).lines() {
            let line = line?;
            let record = match serde_json::from_str::<Record>(&line) {
                Ok(record) => record,
                Err(_) => {
                    counts.malformed += 1;
                    continue;
                }
            };
            let replay = replay_record(record, &base, &proposed, &user);
            counts.records += 1;
            counts.changed += usize::from(replay.changed);
            counts.user_hits += replay.user_hits.len();
            serde_json::to_writer(&mut writer, &replay)?;
            writeln!(writer)?;
        }
    }
    writer.flush()?;
    Ok(counts)
}

fn main() -> anyhow::Result<()> {
    let counts = run(Args::parse()?)?;
    eprintln!(
        "records={} changed={} user_hits={} malformed={}",
        counts.records, counts.changed, counts.user_hits, counts.malformed
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn replay_output_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let transcripts = temp.path().join("input");
        fs::create_dir(&transcripts).unwrap();
        let user_rules = temp.path().join("rules.toml");
        fs::write(&user_rules, "").unwrap();
        let out = temp.path().join("replay.jsonl");
        run(Args {
            transcripts,
            user_rules,
            no_builtin: true,
            out: out.clone(),
        })
        .unwrap();
        assert_eq!(
            fs::metadata(out).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn replay_normalizes_overrides_and_limits_unicode_context() {
        let base = Rules::parse("[[rule]]\nfrom = [\"uat\"]\nto = \"base\"\n[[rule]]\nfrom = [\"内蔵\"]\nto = \"builtin\"\n").unwrap();
        let user =
            Rules::parse("[[rule]]\nfrom = [\"uat\"]\nto = \"UAT\"\nwhole_word = true\n").unwrap();
        let proposal = Rules::merge(&base, &user);
        let record = Record {
            at: "now".into(),
            engine: "test".into(),
            raw: "  一二三四五六七八九十甲ｕａｔ乙一二三四五六七八九十 内蔵  ".into(),
        };
        let result = replay_record(record, &base, &proposal, &user);
        assert!(result.changed);
        assert!(result.baseline.contains("base"));
        assert!(result.proposal.ends_with("builtin"));
        assert_eq!(result.user_hits.len(), 1);
        assert_eq!(result.user_hits[0].left, "二三四五六七八九十甲");
        assert_eq!(result.user_hits[0].right, "乙一二三四五六七八九");
        let result = replay_record(
            Record {
                at: "".into(),
                engine: "".into(),
                raw: "uatx".into(),
            },
            &Rules::default(),
            &user,
            &user,
        );
        assert!(!result.changed);
        assert!(result.user_hits.is_empty());
    }

    #[test]
    fn replay_directory_sorts_files_counts_malformed_and_supports_no_builtin() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("input");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("b.jsonl"),
            "{\"at\":\"b\",\"engine\":\"e\",\"raw\":\"クバネティス\"}\n{}\n",
        )
        .unwrap();
        std::fs::write(dir.join("a.jsonl"), "bad\n{\"at\":\"a\",\"engine\":\"e\",\"raw\":\"ハウスを使う\",\"extra\":1}\n{\"at\":1,\"engine\":\"e\",\"raw\":\"x\"}\n").unwrap();
        std::fs::write(dir.join("ignored.txt"), "bad").unwrap();
        let user = temp.path().join("replace.toml");
        std::fs::write(
            &user,
            "[[rule]]\nfrom = [\"ハウス\"]\nto = \"Haas\"\nwhole_word = true\n",
        )
        .unwrap();
        for no_builtin in [false, true] {
            let out = temp.path().join("out.jsonl");
            let counts = run(Args {
                transcripts: dir.clone(),
                user_rules: user.clone(),
                no_builtin,
                out: out.clone(),
            })
            .unwrap();
            assert_eq!(
                (
                    counts.records,
                    counts.changed,
                    counts.user_hits,
                    counts.malformed
                ),
                (2, 1, 1, 3)
            );
            let rows: Vec<serde_json::Value> = std::fs::read_to_string(out)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(rows[0]["at"], "a");
            assert_eq!(rows[0]["proposal"], "Haasを使う");
            assert_eq!(
                rows[1]["baseline"],
                if no_builtin {
                    "クバネティス"
                } else {
                    "Kubernetes"
                }
            );
        }
    }
}
