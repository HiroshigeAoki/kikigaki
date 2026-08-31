use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Runtime settings for the kikigaki application.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
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
    /// Learned correction rules, merged over `replace_file` with priority (§5).
    pub learned_file: PathBuf,
}

/// Transcription engine implementation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EngineKind {
    /// Run speech recognition in this process.
    Local,
    /// Connect to a hayamimi WebSocket server.
    Remote,
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

/// Offline speech recognition settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AsrConfig {
    /// Number of threads used by the recognizer.
    pub num_threads: u32,
    /// Search algorithm used to decode recognizer output.
    pub decoding_method: DecodingMethod,
}

impl Default for AsrConfig {
    fn default() -> Self {
        Self {
            num_threads: 4,
            decoding_method: DecodingMethod::ModifiedBeamSearch,
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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
            hotkey: "Alt+Space".into(),
            paste_method: PasteMethod::Clipboard,
            strip_trailing_period: true,
            silence_pad_ms: 500,
            final_timeout_ms: 3_000,
            builtin_replace_dict: false,
            replace_file: home.join(".config/kikigaki/replace.toml"),
            metrics_path: home.join("Library/Logs/kikigaki/latency.jsonl"),
            models_dir: dirs::data_local_dir()
                .unwrap_or_else(|| home.join(".local/share"))
                .join("kikigaki/models"),
            asr: AsrConfig::default(),
            vad: VadConfig::default(),
            punct: PunctConfig::default(),
            remote: RemoteConfig::default(),
            learned_file: home.join(".config/kikigaki/learned.toml"),
        }
    }
}

impl Config {
    /// Loads TOML configuration, returning defaults when the file is absent.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let path = path.map(Path::to_path_buf).unwrap_or_else(default_path);
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => {
                return Err(error).with_context(|| format!("read config {}", path.display()));
            }
        };
        let mut config: Self = match toml::from_str(&contents) {
            Ok(config) => config,
            Err(error) => {
                let has_legacy_remote_key = error.to_string().contains("unknown field")
                    && toml::from_str::<toml::Table>(&contents).is_ok_and(|table| {
                        LEGACY_REMOTE_KEYS
                            .iter()
                            .any(|key| table.contains_key(*key))
                    });
                if has_legacy_remote_key {
                    return Err(anyhow::Error::new(error).context(
                        "these keys moved to [remote] in Phase 2 — see docs/config-migration.md",
                    ))
                    .with_context(|| format!("parse config {}", path.display()));
                }
                return Err(error).with_context(|| format!("parse config {}", path.display()));
            }
        };
        config.expand_paths();
        Ok(config)
    }

    /// Validates configuration values against supported ranges and capabilities.
    pub fn validate(&self, caps: Capabilities) -> anyhow::Result<()> {
        if !(1..=16).contains(&self.asr.num_threads) {
            bail!("asr.num_threads must be in 1..=16");
        }
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

    fn expand_paths(&mut self) {
        self.replace_file = expand_tilde(&self.replace_file);
        self.metrics_path = expand_tilde(&self.metrics_path);
        self.models_dir = expand_tilde(&self.models_dir);
        self.learned_file = expand_tilde(&self.learned_file);
        self.remote.hayamimi_dir = expand_tilde(&self.remote.hayamimi_dir);
        self.remote.python = expand_tilde(&self.remote.python);
    }
}

/// Returns whether this process should run punctuation for the selected engine.
///
/// A remote server that already punctuates its output suppresses local punctuation even when
/// punctuation support is otherwise enabled.
pub fn punct_effective(config: &Config, caps: Capabilities) -> bool {
    config.punct.effective(caps)
        && !(config.engine == EngineKind::Remote && config.remote.server_punctuates)
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

fn validate_threshold(name: &str, value: f32) -> anyhow::Result<()> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        bail!("{name} must be in 0..=1");
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
    home_dir().join(".config/kikigaki/config.toml")
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

    fn write(temp: &tempfile::TempDir, body: &str) -> PathBuf {
        let path = temp.path().join("config.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn default_round_trips_through_toml() {
        let expected = Config::default();
        assert!(!expected.builtin_replace_dict);
        let actual: Config = toml::from_str(&toml::to_string(&expected).unwrap()).unwrap();
        assert_eq!(actual, expected);
        assert!(!actual.builtin_replace_dict);
        assert_eq!(actual.engine, EngineKind::Local);
        assert_eq!(actual.asr.num_threads, 4);
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
        assert!(actual
            .metrics_path
            .ends_with("Library/Logs/kikigaki/latency.jsonl"));
        assert!(actual
            .learned_file
            .ends_with(".config/kikigaki/learned.toml"));
        assert!(default_path().ends_with(".config/kikigaki/config.toml"));
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
    fn nested_partial_override_keeps_other_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let path = write(&temp, "[vad]\nmin_silence_ms = 700\n");
        let actual = Config::load(Some(&path)).unwrap();
        assert_eq!(actual.vad.min_silence_ms, 700);
        assert_eq!(actual.vad.min_speech_ms, 250);
        assert_eq!(actual.asr, AsrConfig::default());
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
            }
        ));
        cfg.remote.server_punctuates = false;
        assert!(punct_effective(
            &cfg,
            Capabilities {
                punct: true,
                remote_engine: true,
            }
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
        let path = write(&temp, "stip_trailing_period = false\n");
        let text = format!("{:#}", Config::load(Some(&path)).unwrap_err());
        assert!(text.contains("stip_trailing_period"));
        assert!(!text.contains("[remote]"));
    }

    #[test]
    fn missing_file_returns_default() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(
            Config::load(Some(&temp.path().join("nope.toml"))).unwrap(),
            Config::default()
        );
    }
}
