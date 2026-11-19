use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The operating system a path/hotkey default is computed for. A real enum (not a bare
/// `cfg!(target_os = "windows")` check spread across every call site) so both platform policies
/// can be exercised deterministically from a single-OS test run — see `config_dir_from` /
/// `logs_dir_from` / `default_hotkey_for` below. The `platform` module of the `kikigaki` app
/// crate reuses this enum for the same reason (e.g. the paste-modifier choice).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    /// macOS, including the app's macOS-style policy on other non-Windows hosts.
    MacOs,
    /// Windows.
    Windows,
}

impl Os {
    /// Returns the operating-system policy for the current compilation target. Linux (the
    /// dev/CI host; the app never ships there) maps to `MacOs` so Linux test runs exercise the
    /// macOS layout, exactly as they did before Windows support existed.
    pub const fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::MacOs
        }
    }
}

/// Runtime settings for the kikigaki application.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Transcription engine implementation to use.
    pub engine: EngineKind,
    /// Push-to-talk global hotkey.
    pub hotkey: String,
    /// Method used to insert transcribed text.
    pub paste_method: PasteMethod,
    /// Whether one trailing Japanese or ASCII period is removed.
    pub strip_trailing_period: bool,
    /// Silence appended after hotkey release, in milliseconds.
    pub silence_pad_ms: u64,
    /// Maximum wait for a final transcription, in milliseconds.
    pub final_timeout_ms: u64,
    /// Enable the built-in katakana→English replacement dictionary tier. Default false (opt-in).
    pub builtin_replace_dict: bool,
    /// Persist successful transcriptions as local JSONL records. Default false (opt-in).
    pub transcript_log: bool,
    /// Directory containing date-partitioned transcript logs.
    pub transcript_dir: PathBuf,
    /// Replacement dictionary used during transcription post-processing.
    pub replace_file: PathBuf,
    /// Destination JSONL file for latency records.
    pub metrics_path: PathBuf,
    /// Directory containing the installed model files.
    pub models_dir: PathBuf,
    /// Offline speech recognition settings.
    pub asr: AsrConfig,
    /// Voice activity detection settings.
    pub vad: VadConfig,
    /// Punctuation settings.
    pub punct: PunctConfig,
    /// Remote hayamimi engine settings.
    pub remote: RemoteConfig,
    /// Hand-edited personal replacement rules, merged over `replace_file` with priority (§5).
    pub learned_file: PathBuf,
}

/// Transcription engine implementation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EngineKind {
    /// Run speech recognition in this process.
    #[serde(alias = "whisper")]
    Local,
    /// Connect to a hayamimi WebSocket server.
    Remote,
}

impl EngineKind {
    /// Returns the serialized snake_case engine name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }
}

/// Method used to insert transcribed text into the frontmost application.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PasteMethod {
    /// Paste through the system clipboard.
    Clipboard,
    /// Type text as synthetic keyboard input.
    Type,
}

/// Local speech recognition model selected at launch.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum AsrModel {
    /// ReazonSpeech Japanese-English transducer.
    #[serde(rename = "reazonspeech")]
    ReazonSpeech,
    /// Haas Japanese transducer with built-in punctuation.
    #[default]
    #[serde(rename = "haas")]
    Haas,
}

/// Resolves a keyless configuration without replacing an explicit model choice.
pub fn resolve_asr_model(
    explicit: Option<AsrModel>,
    reazon_installed: bool,
    haas_installed: bool,
) -> AsrModel {
    explicit.unwrap_or(if reazon_installed && !haas_installed {
        AsrModel::ReazonSpeech
    } else {
        AsrModel::Haas
    })
}

/// Offline speech recognition settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AsrConfig {
    /// Local model selected for the next launch.
    pub model: AsrModel,
    /// Score used to boost Haas hotwords.
    pub haas_hotwords_score: f32,
    /// Number of threads used by the recognizer.
    pub num_threads: u32,
    /// Search algorithm used to decode recognizer output.
    pub decoding_method: DecodingMethod,
    /// Score used to boost hotwords during recognition.
    pub hotwords_score: f32,
}

impl AsrConfig {
    /// Returns the hotword score for the selected local model.
    pub fn active_hotwords_score(&self) -> f32 {
        match self.model {
            AsrModel::Haas => self.haas_hotwords_score,
            AsrModel::ReazonSpeech => self.hotwords_score,
        }
    }
}

impl Default for AsrConfig {
    fn default() -> Self {
        Self {
            model: AsrModel::Haas,
            haas_hotwords_score: 1.5,
            num_threads: 4,
            decoding_method: DecodingMethod::ModifiedBeamSearch,
            hotwords_score: 3.0,
        }
    }
}

/// Search algorithm used by the offline recognizer.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DecodingMethod {
    /// Select the most likely token at each decoding step.
    GreedySearch,
    /// Use sherpa-onnx modified beam search.
    ModifiedBeamSearch,
}

impl DecodingMethod {
    /// Returns the decoding method name expected by sherpa-onnx.
    pub fn as_sherpa_str(&self) -> &'static str {
        match self {
            Self::GreedySearch => "greedy_search",
            Self::ModifiedBeamSearch => "modified_beam_search",
        }
    }
}

/// Voice activity detection settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct VadConfig {
    /// Silence duration that closes an utterance, in milliseconds.
    pub min_silence_ms: u64,
    /// Minimum accepted speech duration, in milliseconds.
    pub min_speech_ms: u64,
    /// Maximum speech segment duration, in seconds.
    pub max_speech_s: f32,
    /// Speech probability threshold.
    pub threshold: f32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            min_silence_ms: 350,
            min_speech_ms: 250,
            max_speech_s: 12.0,
            threshold: 0.5,
        }
    }
}

/// Punctuation settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PunctConfig {
    /// Whether punctuation is enabled.
    pub enabled: PunctEnabled,
    /// Minimum confidence for inserting a comma.
    pub comma_threshold: f32,
    /// Minimum confidence for inserting a period.
    pub period_threshold: f32,
}

impl Default for PunctConfig {
    fn default() -> Self {
        Self {
            enabled: PunctEnabled::Auto,
            comma_threshold: 0.5,
            period_threshold: 0.5,
        }
    }
}

impl PunctConfig {
    /// Returns whether punctuation should run for the compiled capabilities.
    pub fn effective(&self, caps: Capabilities) -> bool {
        match self.enabled {
            PunctEnabled::Auto => caps.punct,
            PunctEnabled::On => true,
            PunctEnabled::Off => false,
        }
    }
}

/// User selection for punctuation support.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PunctEnabled {
    /// Enable punctuation when it was compiled into the binary.
    Auto,
    /// Always enable punctuation.
    On,
    /// Disable punctuation.
    Off,
}

impl Serialize for PunctEnabled {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::On => serializer.serialize_bool(true),
            Self::Off => serializer.serialize_bool(false),
        }
    }
}

impl<'de> Deserialize<'de> for PunctEnabled {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PunctEnabledVisitor;

        impl Visitor<'_> for PunctEnabledVisitor {
            type Value = PunctEnabled;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("\"auto\" or a boolean")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(if value {
                    PunctEnabled::On
                } else {
                    PunctEnabled::Off
                })
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value == "auto" {
                    Ok(PunctEnabled::Auto)
                } else {
                    Err(E::invalid_value(de::Unexpected::Str(value), &self))
                }
            }
        }

        deserializer.deserialize_any(PunctEnabledVisitor)
    }
}

/// Settings for the remote hayamimi engine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RemoteConfig {
    /// Hayamimi WebSocket ingest URL.
    pub ws_url: String,
    /// Directory containing the hayamimi checkout.
    pub hayamimi_dir: PathBuf,
    /// Absolute path to the Python executable used by hayamimi.
    pub python: PathBuf,
    /// Whether the application starts the hayamimi sidecar.
    pub spawn_sidecar: bool,
    /// Extra command-line arguments passed to hayamimi.
    pub extra_args: Vec<String>,
    /// Time allowed for the sidecar WebSocket connection, in milliseconds.
    pub connect_timeout_ms: u64,
    /// Whether the remote server already punctuates its output.
    pub server_punctuates: bool,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        let home = home_dir();
        let hayamimi_dir = home.join("dev/voice-engine/hayamimi");
        Self {
            ws_url: "ws://127.0.0.1:8766/ingest".into(),
            python: hayamimi_dir.join(".venv/bin/python"),
            hayamimi_dir,
            spawn_sidecar: true,
            extra_args: vec!["--serve".into(), "--no-refine".into()],
            connect_timeout_ms: 30_000,
            server_punctuates: true,
        }
    }
}

/// Optional features compiled into the running application.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Capabilities {
    /// Whether local punctuation support is available.
    pub punct: bool,
    /// Whether the remote WebSocket engine is available.
    pub remote_engine: bool,
}

impl Default for Config {
    fn default() -> Self {
        let home = home_dir();
        Self {
            engine: EngineKind::Local,
            hotkey: default_hotkey().into(),
            paste_method: PasteMethod::Clipboard,
            strip_trailing_period: true,
            silence_pad_ms: 500,
            final_timeout_ms: 3_000,
            builtin_replace_dict: false,
            transcript_log: false,
            transcript_dir: config_dir(&home).join("transcripts"),
            replace_file: config_dir(&home).join("replace.toml"),
            metrics_path: logs_dir(&home).join("latency.jsonl"),
            models_dir: dirs::data_local_dir()
                .unwrap_or_else(|| home.join(".local/share"))
                .join("kikigaki")
                .join("models"),
            asr: AsrConfig::default(),
            vad: VadConfig::default(),
            punct: PunctConfig::default(),
            remote: RemoteConfig::default(),
            learned_file: config_dir(&home).join("learned.toml"),
        }
    }
}

/// Compares chords with case-insensitive keys and unordered, aliased modifier sets.
/// Modifier aliases follow global-hotkey 0.7's parser, including platform-specific CmdOrCtrl.
pub fn chords_match(a: &str, b: &str) -> bool {
    fn canonical(chord: &str) -> (Vec<String>, Vec<String>) {
        let mut modifiers = Vec::new();
        let mut keys = Vec::new();
        for part in chord.split('+') {
            let part = part.trim().to_ascii_lowercase();
            let modifier = match part.as_str() {
                "option" | "alt" => Some("alt"),
                "control" | "ctrl" => Some("ctrl"),
                "command" | "cmd" | "super" => Some("super"),
                "shift" => Some("shift"),
                "commandorcontrol" | "commandorctrl" | "cmdorctrl" | "cmdorcontrol" => {
                    Some(if cfg!(target_os = "macos") {
                        "super"
                    } else {
                        "ctrl"
                    })
                }
                _ => None,
            };
            if let Some(modifier) = modifier {
                modifiers.push(modifier.to_owned());
            } else {
                keys.push(part);
            }
        }
        modifiers.sort_unstable();
        modifiers.dedup();
        (modifiers, keys)
    }
    canonical(a) == canonical(b)
}

impl Config {
    /// Loads TOML configuration, returning defaults when the file is absent.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        Self::load_with_warnings(path).map(|(config, _)| config)
    }

    /// Loads configuration and reports ignored keys without modifying the file or logging.
    pub fn load_with_warnings(path: Option<&Path>) -> anyhow::Result<(Self, Vec<String>)> {
        let path = path.map(Path::to_path_buf).unwrap_or_else(default_path);
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok((Self::default(), Vec::new()))
            }
            Err(error) => {
                return Err(error).with_context(|| format!("read config {}", path.display()));
            }
        };
        let table: toml::Table = toml::from_str(&contents)
            .with_context(|| format!("parse config {}", path.display()))?;
        if LEGACY_REMOTE_KEYS
            .iter()
            .any(|key| table.contains_key(*key))
        {
            bail!("parse config {}: these keys moved to [remote] in Phase 2 — see docs/config-migration.md", path.display());
        }
        let mut config: Self = table
            .clone()
            .try_into()
            .with_context(|| format!("parse config {}", path.display()))?;
        // All current fields serialize, including PunctEnabled::Auto as "auto".
        // Keep this schema fixture populated if optional or skipped fields are added.
        let known = toml::Table::try_from(Self::default())?;
        let mut warnings = Vec::new();
        fn diff(
            input: &toml::Table,
            known: &toml::Table,
            prefix: &str,
            warnings: &mut Vec<String>,
        ) {
            for (key, value) in input {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                match known.get(key) {
                    None => warnings.push(
                        if matches!(
                            path.as_str(),
                            "long_key" | "long_hotkey" | "auto_long_ms" | "asr.blank_penalty"
                        ) {
                            format!("config key {path} is retired and ignored")
                        } else {
                            format!("unknown config key {path} is ignored")
                        },
                    ),
                    Some(toml::Value::Table(expected)) => {
                        if let toml::Value::Table(actual) = value {
                            diff(actual, expected, &path, warnings);
                        }
                    }
                    _ => {}
                }
            }
        }
        diff(&table, &known, "", &mut warnings);
        if table.get("engine").and_then(toml::Value::as_str) == Some("whisper") {
            warnings.push("Whisper was removed; the local engine is used".into());
        }
        config.expand_paths();
        Ok((config, warnings))
    }

    /// Validates configuration values against supported ranges and capabilities.
    pub fn validate(&self, caps: Capabilities) -> anyhow::Result<()> {
        if !(1..=16).contains(&self.asr.num_threads) {
            bail!("asr.num_threads must be in 1..=16");
        }
        validate_positive("asr.hotwords_score", self.asr.hotwords_score)?;
        validate_positive("asr.haas_hotwords_score", self.asr.haas_hotwords_score)?;
        validate_threshold("vad.threshold", self.vad.threshold)?;
        validate_threshold("punct.comma_threshold", self.punct.comma_threshold)?;
        validate_threshold("punct.period_threshold", self.punct.period_threshold)?;
        if self.vad.min_silence_ms == 0 {
            bail!("vad.min_silence_ms must be greater than zero");
        }
        if self.vad.min_speech_ms == 0 {
            bail!("vad.min_speech_ms must be greater than zero");
        }
        if !self.vad.max_speech_s.is_finite() || !(1.0..=30.0).contains(&self.vad.max_speech_s) {
            bail!("vad.max_speech_s must be in 1..=30");
        }
        if !(1..=5_000).contains(&self.silence_pad_ms) {
            bail!("silence_pad_ms must be in 1..=5000");
        }
        if self.final_timeout_ms == 0 {
            bail!("final_timeout_ms must be greater than zero");
        }
        if self.remote.connect_timeout_ms == 0 {
            bail!("remote.connect_timeout_ms must be greater than zero");
        }
        if self.punct.enabled == PunctEnabled::On && !caps.punct {
            bail!("punct.enabled is true, but punctuation support is not compiled in");
        }
        if self.engine == EngineKind::Remote && !caps.remote_engine {
            bail!("engine = \"remote\" requires a build with the remote-engine feature");
        }
        if self.remote.spawn_sidecar {
            let (host, _) = sidecar_endpoint(&self.remote.ws_url)?;
            let loopback = host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback());
            if !loopback {
                bail!("remote.ws_url host must be loopback when remote.spawn_sidecar is true");
            }
        }
        Ok(())
    }

    /// Returns whether engine punctuation is retained instead of running local punctuation.
    pub fn engine_punctuates(&self, engine: EngineKind, applied_asr_model: AsrModel) -> bool {
        match engine {
            EngineKind::Remote => self.remote.server_punctuates,
            EngineKind::Local => applied_asr_model == AsrModel::Haas,
        }
    }

    fn expand_paths(&mut self) {
        self.replace_file = expand_tilde(&self.replace_file);
        self.metrics_path = expand_tilde(&self.metrics_path);
        self.models_dir = expand_tilde(&self.models_dir);
        self.learned_file = expand_tilde(&self.learned_file);
        self.transcript_dir = expand_tilde(&self.transcript_dir);
        self.remote.hayamimi_dir = expand_tilde(&self.remote.hayamimi_dir);
        self.remote.python = expand_tilde(&self.remote.python);
    }
}

/// Returns whether this process should run punctuation for the selected engine.
pub fn punct_effective(
    config: &Config,
    caps: Capabilities,
    engine: EngineKind,
    applied_asr_model: AsrModel,
) -> bool {
    config.punct.effective(caps) && !config.engine_punctuates(engine, applied_asr_model)
}

/// Resolves text processing choices independently of the configured engine.
pub fn postprocess_settings(
    config: &Config,
    caps: Capabilities,
    engine: EngineKind,
    applied_asr_model: AsrModel,
) -> crate::postprocess::PostprocessSettings {
    crate::postprocess::PostprocessSettings {
        local_punct: punct_effective(config, caps, engine, applied_asr_model),
        strip_trailing_period: config.strip_trailing_period,
        strip_engine_punct: engine == EngineKind::Local
            && applied_asr_model == AsrModel::Haas
            && !config.punct.effective(caps),
    }
}

const LEGACY_REMOTE_KEYS: &[&str] = &[
    "ws_url",
    "hayamimi_dir",
    "python",
    "spawn_sidecar",
    "extra_args",
    "connect_timeout_ms",
];

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"))
}

/// Windows: `Ctrl+Alt+Space` (`Alt+Space` opens the system menu / PowerToys Run there).
/// macOS: `Alt+Space` (unchanged).
fn default_hotkey_for(os: Os) -> &'static str {
    match os {
        Os::Windows => "Ctrl+Alt+Space",
        Os::MacOs => "Alt+Space",
    }
}

fn default_hotkey() -> &'static str {
    default_hotkey_for(Os::current())
}

/// Per-OS config-file directory, given the already-resolved home directory and (for Windows)
/// the `dirs::config_dir()` answer if one was available. Pure — no `dirs::*` call inside —
/// so both policies are testable with injected roots on any host.
fn config_dir_from(os: Os, home: &Path, os_config_dir: Option<&Path>) -> PathBuf {
    match os {
        Os::Windows => os_config_dir
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join("AppData").join("Roaming"))
            .join("kikigaki"),
        Os::MacOs => home.join(".config/kikigaki"),
    }
}

fn config_dir(home: &Path) -> PathBuf {
    config_dir_from(Os::current(), home, dirs::config_dir().as_deref())
}

/// Per-OS log/metrics directory, same shape as `config_dir_from`.
fn logs_dir_from(os: Os, home: &Path, os_data_local_dir: Option<&Path>) -> PathBuf {
    match os {
        Os::Windows => os_data_local_dir
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join("AppData").join("Local"))
            .join("kikigaki")
            .join("logs"),
        Os::MacOs => home.join("Library/Logs/kikigaki"),
    }
}

fn logs_dir(home: &Path) -> PathBuf {
    logs_dir_from(Os::current(), home, dirs::data_local_dir().as_deref())
}

fn validate_threshold(name: &str, value: f32) -> anyhow::Result<()> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        bail!("{name} must be in 0..=1");
    }
    Ok(())
}

fn validate_positive(name: &str, value: f32) -> anyhow::Result<()> {
    if !value.is_finite() || value <= 0.0 {
        bail!("{name} must be finite and greater than zero");
    }
    Ok(())
}

/// Expands a leading `~` path component to the user's home directory.
pub fn expand_tilde(path: &Path) -> PathBuf {
    let mut components = path.components();
    if components
        .next()
        .is_some_and(|component| component.as_os_str() == "~")
    {
        if let Some(home) = dirs::home_dir() {
            return components.fold(home, |expanded, component| expanded.join(component));
        }
    }
    path.to_path_buf()
}

/// Returns the default per-user configuration path.
pub fn default_path() -> PathBuf {
    config_dir(&home_dir()).join("config.toml")
}

/// The per-OS log/metrics directory (`%LOCALAPPDATA%\kikigaki\logs` on Windows,
/// `~/Library/Logs/kikigaki` on macOS). Intended for `shell.rs::init_logging` and used by
/// `Config::default()`'s `metrics_path`.
pub fn default_log_dir() -> PathBuf {
    logs_dir(&home_dir())
}

/// Extracts the host and port used to launch a sidecar from a `ws://` URL.
pub fn sidecar_endpoint(ws_url: &str) -> anyhow::Result<(String, u16)> {
    let rest = ws_url.strip_prefix("ws://").with_context(|| {
        format!("remote.ws_url must begin with ws:// when spawning a sidecar: {ws_url}")
    })?;
    let authority = rest.split('/').next().unwrap_or_default();
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let end = bracketed
            .find(']')
            .with_context(|| format!("invalid IPv6 remote.ws_url authority: {authority}"))?;
        let host = &bracketed[..end];
        let port = bracketed[end + 1..]
            .strip_prefix(':')
            .with_context(|| format!("remote.ws_url needs an explicit port: {ws_url}"))?;
        (host, port)
    } else {
        authority
            .rsplit_once(':')
            .with_context(|| format!("remote.ws_url needs an explicit port: {ws_url}"))?
    };
    if host.is_empty() {
        bail!("remote.ws_url host is empty");
    }
    let port = port
        .parse::<u16>()
        .with_context(|| format!("invalid remote.ws_url port in {ws_url}"))?;
    Ok((host.to_owned(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_hotkey_for_matches_os_policy() {
        assert_eq!(default_hotkey_for(Os::Windows), "Ctrl+Alt+Space");
        assert_eq!(default_hotkey_for(Os::MacOs), "Alt+Space");
    }

    #[test]
    fn config_dir_from_matches_os_policy_with_injected_roots() {
        let home = Path::new("/Users/aoki");
        assert_eq!(
            config_dir_from(Os::MacOs, home, None),
            Path::new("/Users/aoki/.config/kikigaki")
        );
        let appdata = Path::new(r"C:\Users\aoki\AppData\Roaming");
        // Built with join(): a `\` literal is a single component on Linux, so a spelled-out
        // expected path would only match on Windows.
        assert_eq!(
            config_dir_from(Os::Windows, home, Some(appdata)),
            appdata.join("kikigaki")
        );
        // No injected root falls back to home-relative, matching how dirs::config_dir() behaves
        // when it can't resolve %APPDATA% (extremely rare, but must not panic).
        assert_eq!(
            config_dir_from(Os::Windows, home, None),
            Path::new("/Users/aoki/AppData/Roaming/kikigaki")
        );
    }

    #[test]
    fn logs_dir_from_matches_os_policy_with_injected_roots() {
        let home = Path::new("/Users/aoki");
        assert_eq!(
            logs_dir_from(Os::MacOs, home, None),
            Path::new("/Users/aoki/Library/Logs/kikigaki")
        );
        let local_appdata = Path::new(r"C:\Users\aoki\AppData\Local");
        // Built with join(): a `\` literal is a single component on Linux, so a spelled-out
        // expected path would only match on Windows.
        assert_eq!(
            logs_dir_from(Os::Windows, home, Some(local_appdata)),
            local_appdata.join("kikigaki").join("logs")
        );
    }

    // Host-policy assertion: the injected-root tests above prove the *policy* for each
    // `Os` in isolation; this proves `config_dir`/`logs_dir` land on the right one of those
    // policies for whatever OS this test actually runs on, without restating their bodies.
    #[test]
    fn config_dir_and_logs_dir_follow_the_host_policy() {
        let home = home_dir();
        match Os::current() {
            Os::Windows => {
                assert_eq!(
                    config_dir(&home),
                    dirs::config_dir().unwrap().join("kikigaki")
                );
                assert_eq!(
                    logs_dir(&home),
                    dirs::data_local_dir()
                        .unwrap()
                        .join("kikigaki")
                        .join("logs")
                );
            }
            Os::MacOs => {
                // Deliberately NOT dirs::config_dir(): on macOS that is ~/Library/Application
                // Support, which this app has never used.
                assert_eq!(config_dir(&home), home.join(".config/kikigaki"));
                assert_eq!(logs_dir(&home), home.join("Library/Logs/kikigaki"));
            }
        }
    }

    #[test]
    fn keyless_model_resolution_matrix() {
        assert_eq!(AsrModel::default(), AsrModel::Haas);
        assert_eq!(Config::default().asr.model, AsrModel::Haas);
        for reazon in [false, true] {
            for haas in [false, true] {
                let expected = if reazon && !haas {
                    AsrModel::ReazonSpeech
                } else {
                    AsrModel::Haas
                };
                assert_eq!(resolve_asr_model(None, reazon, haas), expected);
                for model in [AsrModel::Haas, AsrModel::ReazonSpeech] {
                    assert_eq!(resolve_asr_model(Some(model), reazon, haas), model);
                }
            }
        }
    }

    #[test]
    fn active_hotwords_score_follows_model_and_custom_scores() {
        let mut config = AsrConfig {
            model: AsrModel::ReazonSpeech,
            ..AsrConfig::default()
        };
        assert_eq!(config.active_hotwords_score(), 3.0);
        config.hotwords_score = 4.5;
        config.haas_hotwords_score = 2.5;
        assert_eq!(config.active_hotwords_score(), 4.5);
        config.model = AsrModel::Haas;
        assert_eq!(config.active_hotwords_score(), 2.5);
        assert_eq!(
            AsrConfig {
                model: AsrModel::Haas,
                ..AsrConfig::default()
            }
            .active_hotwords_score(),
            1.5
        );
    }

    #[test]
    fn haas_punctuation_uses_applied_model_and_effective_choice() {
        for (choice, capability, strip, strip_engine) in [
            (PunctEnabled::On, true, false, false),
            (PunctEnabled::On, true, true, false),
            (PunctEnabled::Off, true, false, true),
            (PunctEnabled::Off, true, true, true),
            (PunctEnabled::Auto, true, false, false),
            (PunctEnabled::Auto, false, false, true),
            (PunctEnabled::On, false, false, false),
        ] {
            let mut config = Config::default();
            // The saved model deliberately differs from the running model.
            config.asr.model = AsrModel::ReazonSpeech;
            config.punct.enabled = choice;
            config.strip_trailing_period = strip;
            let caps = Capabilities {
                punct: capability,
                remote_engine: true,
            };
            assert!(config.engine_punctuates(EngineKind::Local, AsrModel::Haas));
            assert!(!punct_effective(
                &config,
                caps,
                EngineKind::Local,
                AsrModel::Haas
            ));
            let actual = postprocess_settings(&config, caps, EngineKind::Local, AsrModel::Haas);
            assert!(!actual.local_punct);
            assert_eq!(actual.strip_engine_punct, strip_engine);
            assert_eq!(actual.strip_trailing_period, strip);
            config.asr.model = AsrModel::Haas;
            let expected = postprocess_settings(
                &Config {
                    asr: AsrConfig::default(),
                    ..config.clone()
                },
                caps,
                EngineKind::Local,
                AsrModel::ReazonSpeech,
            );
            assert_eq!(
                postprocess_settings(&config, caps, EngineKind::Local, AsrModel::ReazonSpeech),
                expected
            );
        }
    }

    #[test]
    fn asr_model_and_haas_score_config_contract() {
        let legacy: Config = toml::from_str("[asr]\nnum_threads = 4").unwrap();
        assert_eq!(legacy.asr.model, AsrModel::Haas);
        assert_eq!(legacy.asr.haas_hotwords_score, 1.5);
        let defaults = toml::Value::try_from(Config::default()).unwrap();
        assert_eq!(defaults["asr"]["model"].as_str(), Some("haas"));
        assert_eq!(defaults["asr"]["haas_hotwords_score"].as_float(), Some(1.5));
        for model in ["haas", "reazonspeech"] {
            let config: Config = toml::from_str(&format!("[asr]\nmodel = \"{model}\"")).unwrap();
            let serialized = toml::to_string(&config).unwrap();
            assert_eq!(toml::from_str::<Config>(&serialized).unwrap(), config);
        }
        assert!(toml::from_str::<Config>("[asr]\nmodel = \"unknown\"").is_err());
        for score in ["0", "-1", "inf", "nan"] {
            let config: Config =
                toml::from_str(&format!("[asr]\nhaas_hotwords_score = {score}")).unwrap();
            assert_eq!(
                config
                    .validate(Capabilities {
                        punct: true,
                        remote_engine: true,
                    })
                    .unwrap_err()
                    .to_string(),
                "asr.haas_hotwords_score must be finite and greater than zero"
            );
        }
    }

    #[test]
    fn postprocess_settings_per_engine_matches_shared_punctuation() {
        use crate::postprocess::PostprocessSettings;
        let caps = Capabilities {
            punct: true,
            remote_engine: true,
        };
        for (choice, strip) in [
            (PunctEnabled::Off, false),
            (PunctEnabled::On, false),
            (PunctEnabled::On, true),
        ] {
            let mut cfg = Config {
                strip_trailing_period: strip,
                ..Config::default()
            };
            cfg.punct.enabled = choice;
            for engine in [EngineKind::Local, EngineKind::Remote] {
                assert_eq!(
                    postprocess_settings(&cfg, caps, engine, AsrModel::ReazonSpeech),
                    PostprocessSettings {
                        local_punct: engine != EngineKind::Remote && choice != PunctEnabled::Off,
                        strip_trailing_period: strip,
                        strip_engine_punct: false,
                    }
                );
            }
            cfg.remote.server_punctuates = false;
            assert_eq!(
                postprocess_settings(&cfg, caps, EngineKind::Remote, AsrModel::ReazonSpeech)
                    .local_punct,
                choice != PunctEnabled::Off
            );
        }
    }

    fn write(temp: &tempfile::TempDir, body: &str) -> PathBuf {
        let path = temp.path().join("config.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn chords_match_normalizes_modifiers() {
        for (a, b) in [
            (" Shift + Alt + Space ", "alt+shift+SPACE"),
            ("Option+Space", "Alt+Space"),
            ("Control+Space", "Ctrl+Space"),
            ("Command+Space", "Cmd+Space"),
            ("Super+Space", "Cmd+Space"),
            ("Alt+Alt+Space", "Alt+Space"),
        ] {
            assert!(chords_match(a, b), "{a} vs {b}");
        }
        let platform_modifier = if cfg!(target_os = "macos") {
            "Super"
        } else {
            "Ctrl"
        };
        for alias in [
            "CommandOrControl",
            "CommandOrCtrl",
            "CmdOrCtrl",
            "CmdOrControl",
        ] {
            assert!(chords_match(
                &format!("{alias}+Space"),
                &format!("{platform_modifier}+Space")
            ));
        }
        assert!(!chords_match("Alt+Space", "Alt+Shift+Space"));
    }

    #[test]
    fn legacy_whisper_maps_to_local() {
        let temp = tempfile::tempdir().unwrap();
        let raw = "engine = \"whisper\"\n";
        let path = write(&temp, raw);
        let cfg = Config::load(Some(&path)).unwrap();
        assert_eq!(cfg.engine, EngineKind::Local);
        assert!(toml::to_string(&cfg)
            .unwrap()
            .contains("engine = \"local\""));
        assert_eq!(std::fs::read_to_string(path).unwrap(), raw);
    }

    #[test]
    fn default_round_trips_through_toml() {
        let expected = Config::default();
        assert!(!expected.builtin_replace_dict);
        assert!(!expected.transcript_log);
        let actual: Config = toml::from_str(&toml::to_string(&expected).unwrap()).unwrap();
        assert_eq!(actual, expected);
        assert!(!actual.builtin_replace_dict);
        assert!(!actual.transcript_log);
        assert_eq!(actual.engine, EngineKind::Local);
        assert_eq!(actual.asr.num_threads, 4);
        assert_eq!(actual.asr.hotwords_score, 3.0);
        assert_eq!(
            actual.asr.decoding_method,
            DecodingMethod::ModifiedBeamSearch
        );
        assert_eq!(actual.vad.min_silence_ms, 350);
        assert_eq!(actual.vad.min_speech_ms, 250);
        assert_eq!(actual.vad.max_speech_s, 12.0);
        assert_eq!(actual.punct.enabled, PunctEnabled::Auto);
        assert_eq!(actual.remote.extra_args, ["--serve", "--no-refine"]);
        assert!(actual.remote.server_punctuates);
        assert!(actual.models_dir.ends_with("kikigaki/models"));
        assert_eq!(actual.hotkey, default_hotkey_for(Os::current()));
        if cfg!(target_os = "windows") {
            assert!(actual
                .metrics_path
                .ends_with(r"kikigaki\logs\latency.jsonl"));
            assert!(actual.replace_file.ends_with(r"kikigaki\replace.toml"));
            assert!(actual.learned_file.ends_with(r"kikigaki\learned.toml"));
            assert!(actual.transcript_dir.ends_with(r"kikigaki\transcripts"));
            assert!(default_path().ends_with(r"kikigaki\config.toml"));
        } else {
            assert!(actual
                .metrics_path
                .ends_with("Library/Logs/kikigaki/latency.jsonl"));
            assert!(actual
                .learned_file
                .ends_with(".config/kikigaki/learned.toml"));
            assert!(actual
                .transcript_dir
                .ends_with(".config/kikigaki/transcripts"));
            assert!(default_path().ends_with(".config/kikigaki/config.toml"));
            assert!(actual
                .replace_file
                .ends_with(".config/kikigaki/replace.toml"));
        }
        let config_root = config_dir(&home_dir());
        assert_eq!(actual.transcript_dir, config_root.join("transcripts"));
        assert_eq!(actual.replace_file, config_root.join("replace.toml"));
        assert_eq!(actual.learned_file, config_root.join("learned.toml"));
        assert_eq!(actual.metrics_path, default_log_dir().join("latency.jsonl"));
    }

    #[test]
    fn top_level_partial_override_keeps_other_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let path = write(&temp, "builtin_replace_dict = true\n");
        let actual = Config::load(Some(&path)).unwrap();
        assert!(actual.builtin_replace_dict);
        assert_eq!(actual.engine, Config::default().engine);
    }

    #[test]
    fn transcript_log_settings_load_and_expand_tilde() {
        let temp = tempfile::tempdir().unwrap();
        let path = write(
            &temp,
            "transcript_log = true\ntranscript_dir = \"~/private-transcripts\"\n",
        );
        let actual = Config::load(Some(&path)).unwrap();
        assert!(actual.transcript_log);
        assert_eq!(
            actual.transcript_dir,
            home_dir().join("private-transcripts")
        );
    }

    #[test]
    fn nested_partial_override_keeps_other_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let path = write(&temp, "[vad]\nmin_silence_ms = 700\n");
        let actual = Config::load(Some(&path)).unwrap();
        assert_eq!(actual.vad.min_silence_ms, 700);
        assert_eq!(actual.vad.min_speech_ms, 250);
        assert_eq!(actual.asr, AsrConfig::default());
    }

    #[test]
    fn asr_hotwords_score_deserialization_defaults_when_missing() {
        let actual: AsrConfig = toml::from_str("num_threads = 2\n").unwrap();
        assert_eq!(actual.hotwords_score, 3.0);
    }

    #[test]
    fn asr_hotwords_score_round_trips_without_validation() {
        for hotwords_score in [0.0, -1.0, f32::INFINITY, f32::NAN] {
            let expected = AsrConfig {
                hotwords_score,
                ..AsrConfig::default()
            };
            let actual: AsrConfig = toml::from_str(&toml::to_string(&expected).unwrap()).unwrap();
            if hotwords_score.is_nan() {
                assert!(actual.hotwords_score.is_nan());
            } else {
                assert_eq!(actual.hotwords_score, hotwords_score);
            }
        }
    }

    #[test]
    fn validate_hotwords_score_requires_a_finite_positive_value() {
        let capabilities = Capabilities {
            punct: true,
            remote_engine: true,
        };
        for hotwords_score in [0.0, f32::INFINITY, f32::NAN, -1.0] {
            let mut config = Config::default();
            config.asr.hotwords_score = hotwords_score;
            let error = config.validate(capabilities).unwrap_err();
            assert_eq!(
                error.to_string(),
                "asr.hotwords_score must be finite and greater than zero"
            );
        }

        assert!(Config::default().validate(capabilities).is_ok());
    }

    #[test]
    fn punct_enabled_accepts_auto_true_false() {
        for (body, expected) in [
            ("[punct]\nenabled = \"auto\"\n", PunctEnabled::Auto),
            ("[punct]\nenabled = true\n", PunctEnabled::On),
            ("[punct]\nenabled = false\n", PunctEnabled::Off),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let actual = Config::load(Some(&write(&temp, body))).unwrap();
            assert_eq!(actual.punct.enabled, expected, "{body}");
        }
    }

    #[test]
    fn tilde_paths_are_expanded_at_load() {
        let temp = tempfile::tempdir().unwrap();
        let path = write(
            &temp,
            "replace_file = \"~/r.toml\"\nmodels_dir = \"~/m\"\n[remote]\nhayamimi_dir = \"~/h\"\n",
        );
        let actual = Config::load(Some(&path)).unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(actual.replace_file, home.join("r.toml"));
        assert_eq!(actual.models_dir, home.join("m"));
        assert_eq!(actual.remote.hayamimi_dir, home.join("h"));
    }

    #[test]
    fn expand_tilde_only_touches_leading_component() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde(Path::new("~/x")), home.join("x"));
        assert_eq!(expand_tilde(Path::new("~")), home);
        assert_eq!(expand_tilde(Path::new("/a/~/b")), PathBuf::from("/a/~/b"));
        assert_eq!(expand_tilde(Path::new("~user/x")), PathBuf::from("~user/x"));
    }

    #[test]
    fn validate_rejects_out_of_range_values() {
        let mut cfg = Config::default();
        cfg.asr.num_threads = 0;
        assert!(cfg
            .validate(Capabilities {
                punct: true,
                remote_engine: true,
            })
            .is_err());
        let mut cfg = Config::default();
        cfg.vad.threshold = 1.5;
        assert!(cfg
            .validate(Capabilities {
                punct: true,
                remote_engine: true,
            })
            .is_err());
        let mut cfg = Config::default();
        cfg.vad.max_speech_s = 45.0;
        assert!(cfg
            .validate(Capabilities {
                punct: true,
                remote_engine: true,
            })
            .is_err());
        let cfg = Config {
            silence_pad_ms: 6000,
            ..Config::default()
        };
        assert!(cfg
            .validate(Capabilities {
                punct: true,
                remote_engine: true,
            })
            .is_err());
        let mut cfg = Config::default();
        cfg.vad.min_silence_ms = 0;
        assert!(cfg
            .validate(Capabilities {
                punct: true,
                remote_engine: true,
            })
            .is_err());
        assert!(Config::default()
            .validate(Capabilities {
                punct: false,
                remote_engine: true,
            })
            .is_ok());
    }

    #[test]
    fn validate_rejects_explicit_punct_on_binary_without_punct() {
        let mut cfg = Config::default();
        cfg.punct.enabled = PunctEnabled::On;
        let error = cfg
            .validate(Capabilities {
                punct: false,
                remote_engine: true,
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains("punct"));
        assert!(cfg
            .validate(Capabilities {
                punct: true,
                remote_engine: true,
            })
            .is_ok());
    }

    #[test]
    fn validate_rejects_remote_engine_without_the_feature_when_capabilities_say_so() {
        let cfg = Config {
            engine: EngineKind::Remote,
            ..Config::default()
        };
        let error = cfg
            .validate(Capabilities {
                punct: true,
                remote_engine: false,
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains("remote-engine"));
        assert!(cfg
            .validate(Capabilities {
                punct: true,
                remote_engine: true,
            })
            .is_ok());
    }

    #[test]
    fn sidecar_requires_a_loopback_websocket_host() {
        let mut cfg = Config::default();
        cfg.remote.ws_url = "ws://192.0.2.1:8766/ingest".into();
        let error = cfg
            .validate(Capabilities {
                punct: true,
                remote_engine: true,
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains("loopback"));

        for url in [
            "ws://127.0.0.1:8766/ingest",
            "ws://localhost:8766/ingest",
            "ws://[::1]:8766/ingest",
        ] {
            cfg.remote.ws_url = url.into();
            assert!(
                cfg.validate(Capabilities {
                    punct: true,
                    remote_engine: true,
                })
                .is_ok(),
                "{url}"
            );
        }

        cfg.remote.spawn_sidecar = false;
        cfg.remote.ws_url = "wss://example.com/ingest".into();
        assert!(cfg
            .validate(Capabilities {
                punct: true,
                remote_engine: true,
            })
            .is_ok());
    }

    #[test]
    fn punct_effective_follows_auto_rule() {
        let mut cfg = Config::default();
        assert!(cfg.punct.effective(Capabilities {
            punct: true,
            remote_engine: true,
        }));
        assert!(!cfg.punct.effective(Capabilities {
            punct: false,
            remote_engine: true,
        }));
        cfg.punct.enabled = PunctEnabled::Off;
        assert!(!cfg.punct.effective(Capabilities {
            punct: true,
            remote_engine: true,
        }));

        cfg.punct.enabled = PunctEnabled::On;
        cfg.engine = EngineKind::Remote;
        cfg.remote.server_punctuates = true;
        assert!(!punct_effective(
            &cfg,
            Capabilities {
                punct: true,
                remote_engine: true,
            },
            cfg.engine,
            AsrModel::ReazonSpeech
        ));
        cfg.remote.server_punctuates = false;
        assert!(punct_effective(
            &cfg,
            Capabilities {
                punct: true,
                remote_engine: true,
            },
            cfg.engine,
            AsrModel::ReazonSpeech
        ));
    }

    #[test]
    fn legacy_top_level_remote_keys_get_migration_hint() {
        let temp = tempfile::tempdir().unwrap();
        let path = write(&temp, "ws_url = \"ws://x\"\nspawn_sidecar = false\n");
        let error = Config::load(Some(&path)).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("[remote]"), "{text}");
        assert!(text.contains("docs/config-migration.md"), "{text}");
    }

    #[test]
    fn unknown_field_without_legacy_keys_has_no_hint() {
        let temp = tempfile::tempdir().unwrap();
        let path = write(&temp, "stip_trailing_period = false\n[vad]\nfoo = 1\n");
        let (_, warnings) = Config::load_with_warnings(Some(&path)).unwrap();
        assert_eq!(
            warnings,
            [
                "unknown config key stip_trailing_period is ignored",
                "unknown config key vad.foo is ignored"
            ]
        );
    }

    #[test]
    fn missing_file_returns_default() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(
            Config::load(Some(&temp.path().join("nope.toml"))).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn compatibility_warnings() {
        let temp = tempfile::tempdir().unwrap();
        let path = write(&temp, "hotkey = \"Shift+Space\"\nstrip_trailing_period = true\nbuiltin_replace_dict = true\nengine = \"local\"\ntranscript_log = true\nlong_key = true\n[punct]\nenabled = true\n");
        let (cfg, warnings) = Config::load_with_warnings(Some(&path)).unwrap();
        assert_eq!(cfg.engine, EngineKind::Local);
        assert_eq!(warnings, ["config key long_key is retired and ignored"]);
        let path = write(&temp, "engine = \"whisper\"\nlong_key = true\nlong_hotkey = \"Ctrl+Alt+Space\"\nauto_long_ms = 900\n[asr]\nblank_penalty = 1.5\n");
        let (cfg, warnings) = Config::load_with_warnings(Some(&path)).unwrap();
        assert_eq!(cfg.engine, EngineKind::Local);
        assert_eq!(warnings.len(), 5);
        for key in [
            "long_key",
            "long_hotkey",
            "auto_long_ms",
            "asr.blank_penalty",
        ] {
            assert!(warnings.contains(&format!("config key {key} is retired and ignored")));
        }
        assert!(warnings.iter().any(|w| w.contains("Whisper was removed")));
        let path = write(&temp, "hotkey = 3");
        assert!(Config::load_with_warnings(Some(&path)).is_err());
    }

    #[test]
    fn documented_keys_have_no_warnings() {
        let doc = include_str!("../../../docs/configuration.md");
        let sample = doc
            .split("```toml\n")
            .nth(1)
            .unwrap()
            .split("```")
            .next()
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let path = write(&temp, sample);
        assert!(Config::load_with_warnings(Some(&path))
            .unwrap()
            .1
            .is_empty());
    }
}
