use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{anyhow, bail, ensure, Context};
use kikigaki_core::config::AsrConfig;
use kikigaki_core::models::ASR_MODEL_ID;
use kikigaki_engine::audio::load_wav_16k;
use kikigaki_engine::local::Recognizer;
use kikigaki_engine::sherpa::{build_recognizer, build_recognizer_with_hotwords};
use serde::Serialize;

const DEFAULT_HOTWORDS_SCORE: f32 = 1.5;
const USAGE: &str = "usage: hotword-eval --models-dir D --manifest M.tsv [--manifest M.tsv ...] --wavs-dir W --out R.jsonl [--hotwords-file F --bpe-vocab V] [--hotwords-score S] [--num-threads N]";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct Args {
    models_dir: PathBuf,
    manifests: Vec<PathBuf>,
    wavs_dir: PathBuf,
    out: PathBuf,
    hotwords_file: Option<PathBuf>,
    bpe_vocab: Option<PathBuf>,
    hotwords_score: Option<f32>,
    num_threads: u32,
}

#[derive(Debug)]
struct ManifestRow {
    id: String,
    spoken_text: String,
    ref_text: String,
    expected_surfaces: Vec<String>,
    manifest_path: PathBuf,
    line_number: usize,
}

impl ManifestRow {
    fn context(&self) -> String {
        row_context(&self.manifest_path, self.line_number, &self.id)
    }
}

#[derive(Clone, Debug)]
struct SourceLocation {
    path: PathBuf,
    line_number: usize,
}

#[derive(Serialize)]
struct RunMetadata {
    condition: &'static str,
    hotwords_file_sha256: Option<String>,
    hotwords_line_count: Option<usize>,
    hotwords_score: Option<f32>,
    bpe_vocab_sha256: Option<String>,
    bpe_vocab_line_count: Option<usize>,
    num_threads: u32,
    manifest_sha256: Vec<String>,
    git_head: String,
    model_dir: String,
}

#[derive(Serialize)]
struct ResultRecord<'a> {
    id: &'a str,
    #[serde(rename = "ref")]
    reference: &'a str,
    #[serde(rename = "hyp")]
    hypothesis: &'a str,
    decode_ms: f64,
}

struct PendingOutput {
    path: PathBuf,
    published: bool,
}

impl Drop for PendingOutput {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("hotword-eval: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let args = parse_args(env::args_os().skip(1))?;
    let (rows, manifest_sha256) = load_manifests(&args.manifests)?;
    let hotwords = hotwords_metadata(&args)?;
    let git_head = git_head()?;

    let asr_config = AsrConfig {
        num_threads: args.num_threads,
        ..AsrConfig::default()
    };
    let mut recognizer = match &hotwords {
        Some(metadata) => build_recognizer_with_hotwords(
            &args.models_dir,
            &asr_config,
            metadata.path.as_path(),
            metadata.score,
            metadata.bpe_vocab_path.as_path(),
        )
        .context("build recognizer with hotwords")?,
        None => build_recognizer(&args.models_dir, &asr_config).context("build recognizer")?,
    };

    let (mut pending, file) = create_pending_output(&args.out)?;
    let mut writer = BufWriter::new(file);
    let header = RunMetadata {
        condition: if hotwords.is_some() { "D" } else { "B" },
        hotwords_file_sha256: hotwords.as_ref().map(|metadata| metadata.sha256.clone()),
        hotwords_line_count: hotwords.as_ref().map(|metadata| metadata.line_count),
        hotwords_score: hotwords.as_ref().map(|metadata| metadata.score),
        bpe_vocab_sha256: hotwords
            .as_ref()
            .map(|metadata| metadata.bpe_vocab_sha256.clone()),
        bpe_vocab_line_count: hotwords
            .as_ref()
            .map(|metadata| metadata.bpe_vocab_line_count),
        num_threads: args.num_threads,
        manifest_sha256,
        git_head,
        model_dir: ASR_MODEL_ID.to_owned(),
    };
    writer
        .write_all(json_line(&header)?.as_bytes())
        .with_context(|| format!("write run metadata to {}", pending.path.display()))?;

    let mut last_context = String::from("run metadata header");
    for row in &rows {
        last_context = row.context();
        let wav_path = args.wavs_dir.join(format!("{}.wav", row.id));
        let samples = load_wav_16k(&wav_path)
            .with_context(|| format!("{}: load {}", row.context(), wav_path.display()))?;

        let started = Instant::now();
        let hypothesis = recognizer
            .transcribe(&samples)
            .ok_or_else(|| anyhow!("{}: transcribe returned no result", row.context()))?;
        let decode_ms = started.elapsed().as_secs_f64() * 1_000.0;
        let record = ResultRecord {
            id: &row.id,
            reference: &row.ref_text,
            hypothesis: &hypothesis,
            decode_ms,
        };
        writer
            .write_all(json_line(&record)?.as_bytes())
            .with_context(|| {
                format!(
                    "{}: write result to {}",
                    row.context(),
                    pending.path.display()
                )
            })?;

        // These columns are canonical manifest input even though decoding uses neither one.
        let _ = (&row.spoken_text, &row.expected_surfaces);
    }
    writer.flush().with_context(|| {
        format!(
            "after {last_context}: flush temporary output {}",
            pending.path.display()
        )
    })?;
    drop(writer);
    fs::rename(&pending.path, &args.out).with_context(|| {
        format!(
            "after {last_context}: rename {} to {}",
            pending.path.display(),
            args.out.display()
        )
    })?;
    pending.published = true;
    Ok(())
}

struct HotwordsMetadata {
    path: PathBuf,
    score: f32,
    sha256: String,
    line_count: usize,
    bpe_vocab_path: PathBuf,
    bpe_vocab_sha256: String,
    bpe_vocab_line_count: usize,
}

fn hotwords_metadata(args: &Args) -> anyhow::Result<Option<HotwordsMetadata>> {
    let Some(path) = &args.hotwords_file else {
        ensure!(
            args.hotwords_score.is_none(),
            "--hotwords-score requires --hotwords-file"
        );
        return Ok(None);
    };
    let score = args.hotwords_score.unwrap_or(DEFAULT_HOTWORDS_SCORE);
    ensure!(
        score.is_finite() && score > 0.0,
        "--hotwords-score must be finite and greater than zero, got {score}"
    );
    let bytes = fs::read(path).with_context(|| format!("read hotwords file {}", path.display()))?;
    let text = std::str::from_utf8(&bytes)
        .with_context(|| format!("hotwords file is not UTF-8: {}", path.display()))?;
    let bpe_vocab_path = args
        .bpe_vocab
        .as_ref()
        .context("--hotwords-file requires --bpe-vocab")?;
    let bpe_vocab_bytes = fs::read(bpe_vocab_path)
        .with_context(|| format!("read BPE vocabulary file {}", bpe_vocab_path.display()))?;
    let bpe_vocab_text = std::str::from_utf8(&bpe_vocab_bytes).with_context(|| {
        format!(
            "BPE vocabulary file is not UTF-8: {}",
            bpe_vocab_path.display()
        )
    })?;
    Ok(Some(HotwordsMetadata {
        path: path.clone(),
        score,
        sha256: sha256_hex(&bytes),
        line_count: text.lines().count(),
        bpe_vocab_path: bpe_vocab_path.clone(),
        bpe_vocab_sha256: sha256_hex(&bpe_vocab_bytes),
        bpe_vocab_line_count: bpe_vocab_text.lines().count(),
    }))
}

fn load_manifests(paths: &[PathBuf]) -> anyhow::Result<(Vec<ManifestRow>, Vec<String>)> {
    let mut rows = Vec::new();
    let mut hashes = Vec::with_capacity(paths.len());
    let mut seen = HashMap::new();
    for path in paths {
        let bytes = fs::read(path).with_context(|| format!("read manifest {}", path.display()))?;
        let text = std::str::from_utf8(&bytes)
            .with_context(|| format!("manifest is not UTF-8: {}", path.display()))?;
        rows.extend(parse_manifest_text(path, text, &mut seen)?);
        hashes.push(sha256_hex(&bytes));
    }
    Ok((rows, hashes))
}

fn parse_manifest_text(
    path: &Path,
    text: &str,
    seen: &mut HashMap<String, SourceLocation>,
) -> anyhow::Result<Vec<ManifestRow>> {
    let mut rows = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if line.starts_with('#') {
            continue;
        }
        let columns: Vec<_> = line.split('\t').collect();
        ensure!(
            columns.len() == 4,
            "{}: expected 4 tab-separated columns, got {}",
            row_context(
                path,
                line_number,
                columns.first().copied().unwrap_or("<missing>")
            ),
            columns.len()
        );
        let id = columns[0];
        ensure!(
            valid_id(id),
            "{}: malformed ID; expected [A-Za-z0-9][A-Za-z0-9_-]*",
            row_context(path, line_number, id)
        );
        if let Some(first) = seen.get(id) {
            bail!(
                "{}: duplicate ID; first seen at {}:{}",
                row_context(path, line_number, id),
                first.path.display(),
                first.line_number
            );
        }
        seen.insert(
            id.to_owned(),
            SourceLocation {
                path: path.to_owned(),
                line_number,
            },
        );
        let expected_surfaces = if columns[3].is_empty() {
            Vec::new()
        } else {
            columns[3].split(',').map(str::to_owned).collect()
        };
        rows.push(ManifestRow {
            id: id.to_owned(),
            spoken_text: columns[1].to_owned(),
            ref_text: columns[2].to_owned(),
            expected_surfaces,
            manifest_path: path.to_owned(),
            line_number,
        });
    }
    Ok(rows)
}

fn valid_id(id: &str) -> bool {
    let mut bytes = id.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn row_context(path: &Path, line_number: usize, id: &str) -> String {
    format!("{}:{line_number} (id={id:?})", path.display())
}

fn json_line(value: &impl Serialize) -> anyhow::Result<String> {
    let mut line = serde_json::to_string(value).context("serialize JSONL record")?;
    line.push('\n');
    Ok(line)
}

fn git_head() -> anyhow::Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .context("run git rev-parse HEAD")?;
    ensure!(
        output.status.success(),
        "git rev-parse HEAD failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let head = String::from_utf8(output.stdout).context("git HEAD is not UTF-8")?;
    let head = head.trim();
    ensure!(
        !head.is_empty() && head.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "git rev-parse HEAD returned invalid object ID {head:?}"
    );
    Ok(head.to_owned())
}

fn create_pending_output(out: &Path) -> anyhow::Result<(PendingOutput, File)> {
    let parent = out.parent().unwrap_or_else(|| Path::new("."));
    let name = out
        .file_name()
        .ok_or_else(|| anyhow!("--out must name a file: {}", out.display()))?
        .to_string_lossy();
    for _ in 0..100 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".{name}.hotword-eval.{}.{sequence}.tmp",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                return Ok((
                    PendingOutput {
                        path,
                        published: false,
                    },
                    file,
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create temporary output next to {}", out.display()));
            }
        }
    }
    bail!(
        "could not allocate temporary output next to {}",
        out.display()
    )
}

fn parse_args(arguments: impl IntoIterator<Item = OsString>) -> anyhow::Result<Args> {
    let mut models_dir = None;
    let mut manifests = Vec::new();
    let mut wavs_dir = None;
    let mut out = None;
    let mut hotwords_file = None;
    let mut bpe_vocab = None;
    let mut hotwords_score = None;
    let mut num_threads = 1;
    let mut arguments = arguments.into_iter();

    while let Some(argument) = arguments.next() {
        let flag = argument
            .to_str()
            .ok_or_else(|| anyhow!("option name is not UTF-8"))?;
        match flag {
            "--models-dir" => set_once(&mut models_dir, take_path(&mut arguments, flag)?, flag)?,
            "--manifest" => manifests.push(take_path(&mut arguments, flag)?),
            "--wavs-dir" => set_once(&mut wavs_dir, take_path(&mut arguments, flag)?, flag)?,
            "--out" => set_once(&mut out, take_path(&mut arguments, flag)?, flag)?,
            "--hotwords-file" => {
                set_once(&mut hotwords_file, take_path(&mut arguments, flag)?, flag)?
            }
            "--bpe-vocab" => set_once(&mut bpe_vocab, take_path(&mut arguments, flag)?, flag)?,
            "--hotwords-score" => {
                let value = take_utf8(&mut arguments, flag)?;
                let score = value
                    .parse::<f32>()
                    .with_context(|| format!("invalid {flag} value {value:?}"))?;
                set_once(&mut hotwords_score, score, flag)?;
            }
            "--num-threads" => {
                let value = take_utf8(&mut arguments, flag)?;
                num_threads = value
                    .parse::<u32>()
                    .with_context(|| format!("invalid {flag} value {value:?}"))?;
                ensure!(num_threads > 0, "--num-threads must be greater than zero");
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            _ => bail!("unknown argument {flag:?}\n{USAGE}"),
        }
    }
    ensure!(
        !manifests.is_empty(),
        "at least one --manifest is required\n{USAGE}"
    );
    match (&hotwords_file, &bpe_vocab) {
        (Some(_), None) => bail!("--hotwords-file requires --bpe-vocab\n{USAGE}"),
        (None, Some(_)) => bail!("--bpe-vocab requires --hotwords-file\n{USAGE}"),
        (None, None) if hotwords_score.is_some() => {
            bail!("--hotwords-score requires --hotwords-file\n{USAGE}")
        }
        _ => {}
    }
    Ok(Args {
        models_dir: models_dir.ok_or_else(|| anyhow!("--models-dir is required\n{USAGE}"))?,
        manifests,
        wavs_dir: wavs_dir.ok_or_else(|| anyhow!("--wavs-dir is required\n{USAGE}"))?,
        out: out.ok_or_else(|| anyhow!("--out is required\n{USAGE}"))?,
        hotwords_file,
        bpe_vocab,
        hotwords_score,
        num_threads,
    })
}

fn take_path(
    arguments: &mut impl Iterator<Item = OsString>,
    flag: &str,
) -> anyhow::Result<PathBuf> {
    arguments
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("{flag} requires a value"))
}

fn take_utf8(arguments: &mut impl Iterator<Item = OsString>, flag: &str) -> anyhow::Result<String> {
    arguments
        .next()
        .ok_or_else(|| anyhow!("{flag} requires a value"))?
        .into_string()
        .map_err(|_| anyhow!("{flag} value is not UTF-8"))
}

fn set_once<T>(slot: &mut Option<T>, value: T, flag: &str) -> anyhow::Result<()> {
    ensure!(slot.is_none(), "{flag} may only be supplied once");
    *slot = Some(value);
    Ok(())
}

fn sha256_hex(input: &[u8]) -> String {
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const ROUND: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut padded = input.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let mut hash = INITIAL;
    for chunk in padded.chunks_exact(64) {
        let mut words = [0_u32; 64];
        for (index, bytes) in chunk.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes(bytes.try_into().expect("four-byte chunk"));
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = hash;
        for index in 0..64 {
            let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(sum1)
                .wrapping_add(choose)
                .wrapping_add(ROUND[index])
                .wrapping_add(words[index]);
            let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = sum0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (value, addition) in hash.iter_mut().zip([a, b, c, d, e, f, g, h].into_iter()) {
            *value = value.wrapping_add(addition);
        }
    }
    hash.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_manifest_and_preserves_order() {
        let mut seen = HashMap::new();
        let rows = parse_manifest_text(
            Path::new("target.tsv"),
            "# id\tspoken_text\tref_text\texpected_surfaces\n\
             t01\tクバネティスを使う\tクバネティスを使う\tKubernetes\n\
             t02\t二つ使う\t二つ使う\tMySQL,PostgreSQL\n",
            &mut seen,
        )
        .unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "t01");
        assert_eq!(rows[0].spoken_text, "クバネティスを使う");
        assert_eq!(rows[0].ref_text, "クバネティスを使う");
        assert_eq!(rows[0].expected_surfaces, ["Kubernetes"]);
        assert_eq!(rows[0].line_number, 2);
        assert_eq!(rows[1].id, "t02");
        assert_eq!(rows[1].expected_surfaces, ["MySQL", "PostgreSQL"]);
    }

    #[test]
    fn accepts_empty_expected_surfaces_in_fourth_column() {
        let mut seen = HashMap::new();
        let rows = parse_manifest_text(Path::new("negative.tsv"), "n01\t話す\t話す\t\n", &mut seen)
            .unwrap();

        assert!(rows[0].expected_surfaces.is_empty());
    }

    #[test]
    fn rejects_non_canonical_column_count() {
        let mut seen = HashMap::new();
        let error = parse_manifest_text(Path::new("negative.tsv"), "n01\t話す\t話す\n", &mut seen)
            .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("negative.tsv:1"));
        assert!(message.contains("expected 4 tab-separated columns"));
    }

    #[test]
    fn rejects_malformed_ids_with_row_context() {
        for id in ["_bad", "bad.id", "日本語", "bad id"] {
            let mut seen = HashMap::new();
            let text = format!("{id}\tspoken\tref\tSurface\n");
            let error = parse_manifest_text(Path::new("bad.tsv"), &text, &mut seen).unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains("bad.tsv:1"), "{message}");
            assert!(message.contains(id), "{message}");
            assert!(message.contains("malformed ID"), "{message}");
        }
    }

    #[test]
    fn rejects_duplicates_across_manifests_with_both_locations() {
        let mut seen = HashMap::new();
        parse_manifest_text(
            Path::new("target.tsv"),
            "same-id\tspoken\tref\tSurface\n",
            &mut seen,
        )
        .unwrap();
        let error = parse_manifest_text(
            Path::new("negative.tsv"),
            "# comment\nsame-id\tspoken\tref\t\n",
            &mut seen,
        )
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("negative.tsv:2"));
        assert!(message.contains("same-id"));
        assert!(message.contains("target.tsv:1"));
    }

    #[test]
    fn serializes_baseline_header_as_one_json_line() {
        let header = RunMetadata {
            condition: "B",
            hotwords_file_sha256: None,
            hotwords_line_count: None,
            hotwords_score: None,
            bpe_vocab_sha256: None,
            bpe_vocab_line_count: None,
            num_threads: 1,
            manifest_sha256: vec!["abc".into(), "def".into()],
            git_head: "012345".into(),
            model_dir: "reazonspeech-ja-en-2025-01-17".into(),
        };

        assert_eq!(
            json_line(&header).unwrap(),
            "{\"condition\":\"B\",\"hotwords_file_sha256\":null,\"hotwords_line_count\":null,\"hotwords_score\":null,\"bpe_vocab_sha256\":null,\"bpe_vocab_line_count\":null,\"num_threads\":1,\"manifest_sha256\":[\"abc\",\"def\"],\"git_head\":\"012345\",\"model_dir\":\"reazonspeech-ja-en-2025-01-17\"}\n"
        );
    }

    #[test]
    fn serializes_hotword_header_values() {
        let header = RunMetadata {
            condition: "D",
            hotwords_file_sha256: Some("hotwords-hash".into()),
            hotwords_line_count: Some(242),
            hotwords_score: Some(2.0),
            bpe_vocab_sha256: Some("vocab-hash".into()),
            bpe_vocab_line_count: Some(2000),
            num_threads: 3,
            manifest_sha256: vec!["manifest-hash".into()],
            git_head: "012345".into(),
            model_dir: "model-id".into(),
        };

        assert_eq!(
            json_line(&header).unwrap(),
            "{\"condition\":\"D\",\"hotwords_file_sha256\":\"hotwords-hash\",\"hotwords_line_count\":242,\"hotwords_score\":2.0,\"bpe_vocab_sha256\":\"vocab-hash\",\"bpe_vocab_line_count\":2000,\"num_threads\":3,\"manifest_sha256\":[\"manifest-hash\"],\"git_head\":\"012345\",\"model_dir\":\"model-id\"}\n"
        );
    }

    #[test]
    fn hotwords_and_bpe_vocab_must_be_supplied_together() {
        let required = [
            "--models-dir",
            "models",
            "--manifest",
            "manifest.tsv",
            "--wavs-dir",
            "wavs",
            "--out",
            "result.jsonl",
        ];

        let only_hotwords = parse_args(
            required
                .into_iter()
                .chain(["--hotwords-file", "hotwords.txt"])
                .map(OsString::from),
        )
        .unwrap_err();
        assert!(only_hotwords.to_string().contains("--bpe-vocab"));

        let only_vocab = parse_args(
            required
                .into_iter()
                .chain(["--bpe-vocab", "bpe.vocab"])
                .map(OsString::from),
        )
        .unwrap_err();
        assert!(only_vocab.to_string().contains("--hotwords-file"));

        let paired = parse_args(
            required
                .into_iter()
                .chain([
                    "--hotwords-file",
                    "hotwords.txt",
                    "--bpe-vocab",
                    "bpe.vocab",
                ])
                .map(OsString::from),
        )
        .unwrap();
        assert_eq!(paired.bpe_vocab.as_deref(), Some(Path::new("bpe.vocab")));
    }

    #[test]
    fn num_threads_defaults_to_one_and_can_be_overridden() {
        let required = [
            "--models-dir",
            "models",
            "--manifest",
            "manifest.tsv",
            "--wavs-dir",
            "wavs",
            "--out",
            "result.jsonl",
        ];
        let defaults = parse_args(required.map(OsString::from)).unwrap();
        assert_eq!(defaults.num_threads, 1);

        let overridden = parse_args(
            required
                .into_iter()
                .chain(["--num-threads", "6"])
                .map(OsString::from),
        )
        .unwrap();
        assert_eq!(overridden.num_threads, 6);
    }

    #[test]
    fn serializes_result_row_as_one_json_line() {
        let row = ResultRecord {
            id: "t01",
            reference: "参照",
            hypothesis: "仮説",
            decode_ms: 12.5,
        };

        assert_eq!(
            json_line(&row).unwrap(),
            "{\"id\":\"t01\",\"ref\":\"参照\",\"hyp\":\"仮説\",\"decode_ms\":12.5}\n"
        );
    }

    #[test]
    fn computes_standard_sha256_vector() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn pending_output_does_not_truncate_until_rename() {
        let directory = tempfile::tempdir().unwrap();
        let out = directory.path().join("result.jsonl");
        fs::write(&out, "old\n").unwrap();

        let (mut pending, mut file) = create_pending_output(&out).unwrap();
        assert_eq!(pending.path.parent(), out.parent());
        file.write_all(b"new\n").unwrap();
        file.flush().unwrap();
        drop(file);
        assert_eq!(fs::read_to_string(&out).unwrap(), "old\n");

        fs::rename(&pending.path, &out).unwrap();
        pending.published = true;
        assert_eq!(fs::read_to_string(&out).unwrap(), "new\n");
    }
}
