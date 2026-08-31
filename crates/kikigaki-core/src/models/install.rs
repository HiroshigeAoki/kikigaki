//! Verified model installer.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context};
use bzip2::read::BzDecoder;
use sha2::{Digest, Sha256};

use super::fetch::Fetcher;
use super::{is_installed, model_dir, ok_marker, ManifestFile, Model, Payload};

const STALE_AFTER: Duration = Duration::from_secs(60 * 60);
static UNIQUE: AtomicU64 = AtomicU64::new(0);

/// Snapshot reported while a model is downloaded and installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress<'a> {
    /// Stable model identifier.
    pub id: &'static str,
    /// Cumulative bytes downloaded for the current payload.
    pub done_bytes: u64,
    /// Expected byte count for the current payload.
    pub total_bytes: u64,
    /// Optional description of the current installation step.
    pub message: Option<&'a str>,
}

/// Callback used for model installation progress.
pub type ProgressFn<'a> = &'a mut dyn FnMut(Progress<'_>);

/// Summary of models downloaded and models already present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    /// Model identifiers downloaded during this run.
    pub installed: Vec<&'static str>,
    /// Model identifiers whose valid installations were reused.
    pub reused: Vec<&'static str>,
}

#[derive(Debug)]
struct LockGuard {
    path: PathBuf,
}

impl LockGuard {
    fn acquire(models_dir: &Path, id: &str) -> anyhow::Result<Self> {
        fs::create_dir_all(models_dir)
            .with_context(|| format!("create models directory {}", models_dir.display()))?;
        let path = models_dir.join(format!(".lock-{id}"));
        for attempt in 0..=1 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())?;
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists && attempt == 0 => {
                    let stale_owner = lock_owner_is_dead_or_invalid(&path);
                    let stale_by_age = fs::metadata(&path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                        .is_some_and(|age| age > STALE_AFTER);
                    if stale_owner || stale_by_age {
                        match fs::remove_file(&path) {
                            Ok(()) => continue,
                            Err(remove_error) if remove_error.kind() == io::ErrorKind::NotFound => {
                                continue;
                            }
                            Err(remove_error) => {
                                return Err(remove_error).with_context(|| {
                                    format!("remove stale model install lock {}", path.display())
                                })
                            }
                        }
                    }
                    bail!("another install in progress for {id}");
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    bail!("another install in progress for {id}");
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("create model install lock {}", path.display()));
                }
            }
        }
        unreachable!("lock acquisition loop always returns")
    }
}

fn lock_owner_is_dead_or_invalid(path: &Path) -> bool {
    let Ok(contents) = fs::read_to_string(path) else {
        return true;
    };
    let Ok(pid) = contents.lines().next().unwrap_or_default().parse::<u32>() else {
        return true;
    };
    if pid == 0 {
        return true;
    }
    #[cfg(unix)]
    {
        let Ok(pid) = i32::try_from(pid) else {
            return true;
        };
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        // ESRCH is 3 on the supported Unix targets (macOS and Linux).
        const ESRCH: i32 = 3;
        // SAFETY: signal 0 performs only an existence/permission check and has no side effects.
        let result = unsafe { kill(pid, 0) };
        result != 0 && std::io::Error::last_os_error().raw_os_error() == Some(ESRCH)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.path) {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::warn!(%error, path = %self.path.display(), "remove model install lock");
            }
        }
    }
}

struct StagingGuard {
    path: PathBuf,
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path) {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::warn!(%error, path = %self.path.display(), "remove model staging directory");
            }
        }
    }
}

struct HashingWriter {
    file: File,
    hasher: Sha256,
    bytes: u64,
}

impl HashingWriter {
    fn new(file: File) -> Self {
        Self {
            file,
            hasher: Sha256::new(),
            bytes: 0,
        }
    }

    fn finish(mut self) -> anyhow::Result<(u64, String)> {
        self.file.flush()?;
        self.file.sync_all()?;
        Ok((self.bytes, hex::encode(self.hasher.finalize())))
    }
}

impl Write for HashingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.file.write(buffer)?;
        self.hasher.update(&buffer[..written]);
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Installs a model atomically after verifying all declared sizes and hashes.
///
/// Returns `true` when a download occurred and `false` when a valid existing
/// installation was reused.
pub fn install(
    models_dir: &Path,
    model: &Model,
    fetcher: &dyn Fetcher,
    progress: ProgressFn<'_>,
) -> anyhow::Result<bool> {
    fs::create_dir_all(models_dir)
        .with_context(|| format!("create models directory {}", models_dir.display()))?;
    sweep_stale(models_dir)?;
    if is_installed(models_dir, model) {
        return Ok(false);
    }

    let _lock = LockGuard::acquire(models_dir, model.id)?;
    if is_installed(models_dir, model) {
        return Ok(false);
    }

    let staging = create_staging(models_dir, model.id)?;
    let staging_guard = StagingGuard {
        path: staging.clone(),
    };
    let download_dir = staging.join("download");
    let payload_dir = staging.join("payload");
    fs::create_dir(&download_dir)?;
    fs::create_dir(&payload_dir)?;

    match model.payload {
        Payload::File(_) | Payload::Files(_) => {
            for file in model.files() {
                download_file(model, file, &download_dir, fetcher, progress)?;
                fs::copy(download_dir.join(file.name), payload_dir.join(file.name))
                    .with_context(|| format!("stage model file {}", file.name))?;
            }
        }
        Payload::TarBz2 {
            archive_size,
            archive_sha256,
            files,
        } => {
            let archive_path = download_dir.join("payload.tar.bz2");
            download(
                model,
                &model.source.url(""),
                &archive_path,
                archive_size,
                archive_sha256,
                fetcher,
                progress,
            )?;
            progress(Progress {
                id: model.id,
                done_bytes: archive_size,
                total_bytes: archive_size,
                message: Some("extracting"),
            });
            extract_archive(&archive_path, &payload_dir, files)?;
        }
    }

    progress(Progress {
        id: model.id,
        done_bytes: 0,
        total_bytes: model.files().iter().map(|file| file.size).sum(),
        message: Some("verifying files"),
    });
    for file in model.files() {
        verify_file(&payload_dir.join(file.name), file.size, file.sha256)
            .with_context(|| format!("verify installed file {}", file.name))?;
    }
    write_marker(&payload_dir, model)?;
    progress(Progress {
        id: model.id,
        done_bytes: model.files().iter().map(|file| file.size).sum(),
        total_bytes: model.files().iter().map(|file| file.size).sum(),
        message: Some("installing"),
    });
    publish(models_dir, model.id, &payload_dir)?;
    drop(staging_guard);
    Ok(true)
}

/// Ensures every selected model has a verified installation.
pub fn ensure_installed(
    models_dir: &Path,
    models: &[&Model],
    fetcher: &dyn Fetcher,
    progress: ProgressFn<'_>,
) -> anyhow::Result<InstallReport> {
    fs::create_dir_all(models_dir)
        .with_context(|| format!("create models directory {}", models_dir.display()))?;
    sweep_stale(models_dir)?;
    let mut report = InstallReport {
        installed: Vec::new(),
        reused: Vec::new(),
    };
    for model in models {
        if install(models_dir, model, fetcher, &mut *progress)? {
            report.installed.push(model.id);
        } else {
            report.reused.push(model.id);
        }
    }
    Ok(report)
}

/// Removes the success marker so a model is verified and fetched on the next run.
pub fn invalidate(models_dir: &Path, id: &str) {
    let path = model_dir(models_dir, id).join(".ok");
    if let Err(error) = fs::remove_file(&path) {
        if error.kind() != io::ErrorKind::NotFound {
            tracing::warn!(%error, path = %path.display(), "invalidate model installation");
        }
    }
}

fn download_file(
    model: &Model,
    file: &ManifestFile,
    download_dir: &Path,
    fetcher: &dyn Fetcher,
    progress: ProgressFn<'_>,
) -> anyhow::Result<()> {
    download(
        model,
        &model.source.url(file.name),
        &download_dir.join(file.name),
        file.size,
        file.sha256,
        fetcher,
        progress,
    )
}

#[allow(clippy::too_many_arguments)]
fn download(
    model: &Model,
    url: &str,
    path: &Path,
    expected_size: u64,
    expected_sha256: &str,
    fetcher: &dyn Fetcher,
    progress: ProgressFn<'_>,
) -> anyhow::Result<()> {
    progress(Progress {
        id: model.id,
        done_bytes: 0,
        total_bytes: expected_size,
        message: Some("downloading"),
    });
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create download file {}", path.display()))?;
    let mut writer = HashingWriter::new(file);
    let mut downloaded = |done_bytes| {
        progress(Progress {
            id: model.id,
            done_bytes,
            total_bytes: expected_size,
            message: Some("downloading"),
        });
    };
    fetcher
        .fetch(url, &mut writer, &mut downloaded)
        .with_context(|| format!("download {url}"))?;
    let (actual_size, actual_sha256) = writer.finish()?;
    verify_integrity(
        path,
        actual_size,
        &actual_sha256,
        expected_size,
        expected_sha256,
    )
}

fn verify_integrity(
    path: &Path,
    actual_size: u64,
    actual_sha256: &str,
    expected_size: u64,
    expected_sha256: &str,
) -> anyhow::Result<()> {
    if actual_size != expected_size {
        bail!(
            "size mismatch for {}: expected {expected_size}, got {actual_size}",
            path.display()
        );
    }
    if actual_sha256 != expected_sha256 {
        bail!(
            "sha256 mismatch for {}: expected {expected_sha256}, got {actual_sha256}",
            path.display()
        );
    }
    Ok(())
}

fn verify_file(path: &Path, expected_size: u64, expected_sha256: &str) -> anyhow::Result<()> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let actual_size = io::copy(&mut file, &mut hasher)?;
    let actual_sha256 = hex::encode(hasher.finalize());
    verify_integrity(
        path,
        actual_size,
        &actual_sha256,
        expected_size,
        expected_sha256,
    )
}

fn extract_archive(
    archive_path: &Path,
    payload_dir: &Path,
    files: &[ManifestFile],
) -> anyhow::Result<()> {
    let expected = files
        .iter()
        .map(|file| (file.name, file))
        .collect::<HashMap<_, _>>();
    let mut extracted = HashSet::new();
    let archive_file = File::open(archive_path)?;
    let decoder = BzDecoder::new(archive_file);
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().context("read tar entries")? {
        let mut entry = entry.context("read tar entry")?;
        let path = entry.path().context("read tar entry path")?.into_owned();
        let unsafe_path = path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            });
        if unsafe_path {
            tracing::warn!(path = %path.display(), "skip unsafe model archive entry");
            continue;
        }
        if !entry.header().entry_type().is_file() {
            tracing::warn!(path = %path.display(), "skip non-regular model archive entry");
            continue;
        }
        let Some(name) = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
        else {
            tracing::warn!(path = %path.display(), "skip model archive entry without UTF-8 basename");
            continue;
        };
        let Some(manifest) = expected.get(name.as_str()) else {
            continue;
        };
        if !extracted.insert(name.clone()) {
            bail!("duplicate manifest file {name} in archive");
        }
        let declared_size = entry.header().size()?;
        if declared_size != manifest.size {
            bail!(
                "size mismatch for archive entry {name}: expected {}, got {declared_size}",
                manifest.size
            );
        }
        let output_path = payload_dir.join(&name);
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output_path)?;
        let copied = io::copy(&mut entry, &mut output)?;
        if copied != declared_size {
            bail!("archive entry {name} ended after {copied} of {declared_size} bytes");
        }
        output.sync_all()?;
    }
    let missing = files
        .iter()
        .filter(|file| !extracted.contains(file.name))
        .map(|file| file.name)
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!("archive missing manifest files: {}", missing.join(", "));
    }
    Ok(())
}

fn write_marker(payload_dir: &Path, model: &Model) -> anyhow::Result<()> {
    let path = payload_dir.join(".ok");
    let mut marker = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    marker.write_all(ok_marker(model).as_bytes())?;
    marker.sync_all()?;
    Ok(())
}

fn create_staging(models_dir: &Path, id: &str) -> anyhow::Result<PathBuf> {
    let root = models_dir.join(".tmp");
    fs::create_dir_all(&root)?;
    loop {
        let sequence = UNIQUE.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!("{id}-{}-{sequence}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create staging directory {}", path.display()));
            }
        }
    }
}

fn publish(models_dir: &Path, id: &str, payload_dir: &Path) -> anyhow::Result<()> {
    let target = model_dir(models_dir, id);
    let backup = loop {
        let sequence = UNIQUE.fetch_add(1, Ordering::Relaxed);
        let candidate = models_dir.join(format!("{id}.old-{sequence}"));
        if !candidate.exists() {
            break candidate;
        }
    };
    let had_existing = target.exists();
    if had_existing {
        fs::rename(&target, &backup).with_context(|| {
            format!(
                "move existing model {} to {}",
                target.display(),
                backup.display()
            )
        })?;
    }
    if let Err(error) = fs::rename(payload_dir, &target) {
        if had_existing {
            if let Err(restore_error) = fs::rename(&backup, &target) {
                return Err(error).context(format!(
                    "publish model {}; also failed to restore {}: {restore_error}",
                    target.display(),
                    backup.display()
                ));
            }
        }
        return Err(error).with_context(|| format!("publish model {}", target.display()));
    }
    if had_existing {
        if let Err(error) = remove_entry(&backup) {
            tracing::warn!(%error, path = %backup.display(), "remove replaced model backup");
        }
    }
    Ok(())
}

fn sweep_stale(models_dir: &Path) -> anyhow::Result<()> {
    let tmp = models_dir.join(".tmp");
    if let Ok(entries) = fs::read_dir(&tmp) {
        for entry in entries {
            let entry = entry?;
            if is_stale(&entry.path()) {
                remove_entry(&entry.path())?;
            }
        }
    }
    for entry in fs::read_dir(models_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().contains(".old-") && is_stale(&entry.path()) {
            remove_entry(&entry.path())?;
        }
    }
    Ok(())
}

fn is_stale(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age > STALE_AFTER)
}

fn remove_entry(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.file_type().is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{File, FileTimes};
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime};

    use bzip2::write::BzEncoder;
    use bzip2::Compression;
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::models::fetch::{Fetcher, FileFetcher};
    use crate::models::{ManifestFile, Model, Payload, Requirement, Source};

    fn leak(value: String) -> &'static str {
        Box::leak(value.into_boxed_str())
    }

    fn hash(bytes: &[u8]) -> &'static str {
        leak(hex::encode(Sha256::digest(bytes)))
    }

    fn file_model(id: &'static str, name: &'static str, bytes: &[u8]) -> Model {
        Model {
            id,
            source: Source::GithubRelease {
                tag: "test",
                asset: name,
            },
            payload: Payload::File(ManifestFile {
                name,
                size: bytes.len() as u64,
                sha256: hash(bytes),
            }),
            required_for: Requirement::Asr,
        }
    }

    fn tar_model(
        id: &'static str,
        archive_name: &'static str,
        archive: &[u8],
        files: Vec<ManifestFile>,
    ) -> Model {
        Model {
            id,
            source: Source::GithubRelease {
                tag: "test",
                asset: archive_name,
            },
            payload: Payload::TarBz2 {
                archive_size: archive.len() as u64,
                archive_sha256: hash(archive),
                files: Box::leak(files.into_boxed_slice()),
            },
            required_for: Requirement::Asr,
        }
    }

    fn make_tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let encoder = BzEncoder::new(Vec::new(), Compression::best());
        let mut builder = tar::Builder::new(encoder);
        for (path, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, *contents).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    fn write_download(root: &std::path::Path, name: &str, bytes: &[u8]) {
        std::fs::write(root.join(name), bytes).unwrap();
    }

    fn no_progress(_: Progress<'_>) {}

    #[test]
    fn installs_single_file_and_writes_marker() {
        let temp = tempfile::tempdir().unwrap();
        let downloads = tempfile::tempdir().unwrap();
        write_download(downloads.path(), "model.bin", b"model-data");
        let model = file_model("single", "model.bin", b"model-data");
        let fetcher = FileFetcher {
            root: downloads.path().to_path_buf(),
        };
        let mut progress = no_progress;

        assert!(install(temp.path(), &model, &fetcher, &mut progress).unwrap());
        assert_eq!(
            std::fs::read(temp.path().join("single/model.bin")).unwrap(),
            b"model-data"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("single/.ok")).unwrap(),
            crate::models::ok_marker(&model)
        );
        assert!(crate::models::is_installed(temp.path(), &model));
    }

    #[test]
    fn wrong_hash_leaves_no_install_or_staging() {
        let temp = tempfile::tempdir().unwrap();
        let downloads = tempfile::tempdir().unwrap();
        write_download(downloads.path(), "model.bin", b"evil");
        let model = file_model("single", "model.bin", b"good");
        let fetcher = FileFetcher {
            root: downloads.path().to_path_buf(),
        };
        let mut progress = no_progress;

        let error = install(temp.path(), &model, &fetcher, &mut progress).unwrap_err();
        assert!(format!("{error:#}").contains("sha256"));
        assert!(!temp.path().join("single").exists());
        assert!(std::fs::read_dir(temp.path().join(".tmp"))
            .unwrap()
            .next()
            .is_none());
    }

    #[test]
    fn hostile_archive_installs_only_manifest_regular_file() {
        let temp = tempfile::tempdir().unwrap();
        let downloads = tempfile::tempdir().unwrap();
        let archive = include_bytes!("../../tests/fixtures/hostile.tar.bz2");
        write_download(downloads.path(), "hostile.tar.bz2", archive);
        let model = tar_model(
            "hostile",
            "hostile.tar.bz2",
            archive,
            vec![ManifestFile {
                name: "tokens.txt",
                size: 2,
                sha256: hash(b"ok"),
            }],
        );
        let fetcher = FileFetcher {
            root: downloads.path().to_path_buf(),
        };
        let mut progress = no_progress;

        assert!(install(temp.path(), &model, &fetcher, &mut progress).unwrap());
        assert_eq!(
            std::fs::read(temp.path().join("hostile/tokens.txt")).unwrap(),
            b"ok"
        );
        assert!(!temp.path().join("hostile/link").exists());
        assert!(!temp.path().join("hostile/evil.txt").exists());
    }

    #[test]
    fn archive_missing_manifest_file_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let downloads = tempfile::tempdir().unwrap();
        let archive = make_tar(&[("pkg/other.txt", b"other")]);
        write_download(downloads.path(), "missing.tar.bz2", &archive);
        let model = tar_model(
            "missing",
            "missing.tar.bz2",
            &archive,
            vec![ManifestFile {
                name: "wanted.txt",
                size: 6,
                sha256: hash(b"wanted"),
            }],
        );
        let fetcher = FileFetcher {
            root: downloads.path().to_path_buf(),
        };
        let mut progress = no_progress;

        let error = install(temp.path(), &model, &fetcher, &mut progress).unwrap_err();
        assert!(format!("{error:#}").contains("missing"));
        assert!(!temp.path().join("missing").exists());
    }

    #[test]
    fn archive_entry_size_mismatch_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let downloads = tempfile::tempdir().unwrap();
        let archive = make_tar(&[("pkg/wanted.txt", b"too long")]);
        write_download(downloads.path(), "mismatch.tar.bz2", &archive);
        let model = tar_model(
            "mismatch",
            "mismatch.tar.bz2",
            &archive,
            vec![ManifestFile {
                name: "wanted.txt",
                size: 3,
                sha256: hash(b"too"),
            }],
        );
        let fetcher = FileFetcher {
            root: downloads.path().to_path_buf(),
        };
        let mut progress = no_progress;

        let error = install(temp.path(), &model, &fetcher, &mut progress).unwrap_err();
        assert!(format!("{error:#}").contains("size"));
        assert!(!temp.path().join("mismatch").exists());
    }

    #[test]
    fn installed_check_rejects_stale_marker_and_wrong_size() {
        let temp = tempfile::tempdir().unwrap();
        let model = file_model("single", "model.bin", b"good");
        let dir = temp.path().join("single");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("model.bin"), b"good").unwrap();
        std::fs::write(dir.join(".ok"), "old\n").unwrap();
        assert!(!crate::models::is_installed(temp.path(), &model));
        std::fs::write(dir.join(".ok"), crate::models::ok_marker(&model)).unwrap();
        assert!(crate::models::is_installed(temp.path(), &model));
        std::fs::write(dir.join("model.bin"), b"longer").unwrap();
        assert!(!crate::models::is_installed(temp.path(), &model));
    }

    struct CountingFetcher<'a> {
        calls: &'a AtomicUsize,
    }

    impl Fetcher for CountingFetcher<'_> {
        fn fetch(
            &self,
            _url: &str,
            _dest: &mut dyn Write,
            _progress: &mut dyn FnMut(u64),
        ) -> anyhow::Result<()> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            anyhow::bail!("fetch should not be called")
        }
    }

    #[test]
    fn ensure_reuses_installed_models_without_fetching() {
        let temp = tempfile::tempdir().unwrap();
        let model = Box::leak(Box::new(file_model("single", "model.bin", b"good")));
        let dir = temp.path().join("single");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("model.bin"), b"good").unwrap();
        std::fs::write(dir.join(".ok"), crate::models::ok_marker(model)).unwrap();
        let calls = AtomicUsize::new(0);
        let fetcher = CountingFetcher { calls: &calls };
        let mut progress = no_progress;

        let report = ensure_installed(temp.path(), &[model], &fetcher, &mut progress).unwrap();
        assert!(report.installed.is_empty());
        assert_eq!(report.reused, ["single"]);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    fn make_old(path: &std::path::Path) {
        let file = File::open(path).unwrap();
        let old = SystemTime::now() - Duration::from_secs(3_700);
        file.set_times(FileTimes::new().set_modified(old)).unwrap();
    }

    #[test]
    fn stale_lock_is_stolen_but_fresh_lock_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".lock-model");
        std::fs::write(&path, format!("{}\n", std::process::id())).unwrap();
        make_old(&path);
        let lock = LockGuard::acquire(temp.path(), "model").unwrap();
        assert!(path.exists());
        drop(lock);
        assert!(!path.exists());

        std::fs::write(&path, format!("{}\n", std::process::id())).unwrap();
        let error = LockGuard::acquire(temp.path(), "model").unwrap_err();
        assert!(format!("{error:#}").contains("another install in progress"));
    }

    #[test]
    fn stale_lock_from_a_dead_pid_is_reclaimed_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".lock-test");
        std::fs::write(&path, "999999999\n").unwrap();
        let guard = LockGuard::acquire(dir.path(), "test");
        assert!(
            guard.is_ok(),
            "a lock held by a dead PID must be reclaimable without waiting an hour"
        );
    }

    #[test]
    fn pid_zero_is_an_invalid_lock_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".lock-test");
        std::fs::write(&path, "0\n").unwrap();

        assert!(lock_owner_is_dead_or_invalid(&path));
    }

    #[test]
    fn sweep_removes_old_staging_and_backup_entries() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join(".tmp/old");
        let backup = temp.path().join("model.old-1");
        let fresh = temp.path().join("fresh.old-2");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::create_dir(&backup).unwrap();
        std::fs::create_dir(&fresh).unwrap();
        make_old(&staging);
        make_old(&backup);

        sweep_stale(temp.path()).unwrap();
        assert!(!staging.exists());
        assert!(!backup.exists());
        assert!(fresh.exists());
    }

    #[test]
    fn ensure_sweeps_stale_entries_when_no_models_are_required() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join(".tmp/old");
        std::fs::create_dir_all(&staging).unwrap();
        make_old(&staging);
        let calls = AtomicUsize::new(0);
        let fetcher = CountingFetcher { calls: &calls };
        let mut progress = no_progress;

        let report = ensure_installed(temp.path(), &[], &fetcher, &mut progress).unwrap();
        assert!(report.installed.is_empty());
        assert!(report.reused.is_empty());
        assert!(!staging.exists());
    }
}
