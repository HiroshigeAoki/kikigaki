//! Serialized configuration updates that preserve unrelated TOML and external edits.

use std::path::{Path, PathBuf};

use kikigaki_core::config::{Capabilities, Config, PunctEnabled};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPatch {
    pub hotkey: Option<String>,
    pub punctuation: Option<PunctSetting>,
    /// JSON uses `builtinReplaceDict`; snapshots and persisted TOML stay snake_case.
    pub builtin_replace_dict: Option<bool>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PunctSetting {
    Off,
    On,
    OnStrip,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SettingsSnapshot {
    pub hotkey: String,
    pub punct_enabled: bool,
    pub strip_trailing_period: bool,
    pub builtin_replace_dict: bool,
    pub paste_method: kikigaki_core::config::PasteMethod,
}

pub struct SettingsCoordinator {
    path: PathBuf,
    config: Config,
    capabilities: Capabilities,
    last_written: Option<String>,
}

impl SettingsCoordinator {
    pub fn load(path: PathBuf, capabilities: Capabilities) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(&path).unwrap_or_default();
        let config = Config::load(Some(&path))?;
        Ok(Self {
            path,
            config,
            capabilities,
            last_written: Some(raw),
        })
    }

    pub fn snapshot(&self) -> SettingsSnapshot {
        SettingsSnapshot {
            hotkey: self.config.hotkey.clone(),
            punct_enabled: kikigaki_core::config::punct_effective(&self.config, self.capabilities),
            strip_trailing_period: self.config.strip_trailing_period,
            builtin_replace_dict: self.config.builtin_replace_dict,
            paste_method: self.config.paste_method,
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn apply(
        &mut self,
        patch: SettingsPatch,
    ) -> Result<SettingsSnapshot, crate::status::UiError> {
        let on_disk = std::fs::read_to_string(&self.path).unwrap_or_default();
        if Some(&on_disk) != self.last_written.as_ref() {
            return Err(crate::status::UiError {
                code: "external_change",
                message: "設定ファイルが外部で変更されました".into(),
            });
        }
        let mut document: toml_edit::DocumentMut =
            on_disk.parse().map_err(|error| crate::status::UiError {
                code: "parse_error",
                message: format!("設定ファイルを読み込めませんでした: {error}"),
            })?;
        let mut candidate = self.config.clone();
        if let Some(chord) = &patch.hotkey {
            parse_chord(chord).map_err(|message| crate::status::UiError {
                code: "invalid_hotkey",
                message,
            })?;
            document["hotkey"] = toml_edit::value(chord.as_str());
            candidate.hotkey = chord.clone();
        }
        if let Some(setting) = patch.punctuation {
            let (enabled, strip) = match setting {
                PunctSetting::Off => (false, false),
                PunctSetting::On => (true, false),
                PunctSetting::OnStrip => (true, true),
            };
            if !document.contains_key("punct") {
                document["punct"] = toml_edit::table();
            }
            document["punct"]["enabled"] = toml_edit::value(enabled);
            document["strip_trailing_period"] = toml_edit::value(strip);
            candidate.punct.enabled = if enabled {
                PunctEnabled::On
            } else {
                PunctEnabled::Off
            };
            candidate.strip_trailing_period = strip;
        }
        if let Some(enabled) = patch.builtin_replace_dict {
            document["builtin_replace_dict"] = toml_edit::value(enabled);
            candidate.builtin_replace_dict = enabled;
        }

        let body = document.to_string();
        atomic_write(&self.path, body.as_bytes()).map_err(|error| crate::status::UiError {
            code: "write_failed",
            message: format!("設定を保存できませんでした: {error:#}"),
        })?;
        self.config = candidate;
        self.last_written = Some(body);
        Ok(self.snapshot())
    }
}

pub fn atomic_write(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::io::Write;

    let directory = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent directory"))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("path has no file name"))?;
    // A fresh install has no ~/.config/kikigaki yet; the first settings change must create it.
    std::fs::create_dir_all(directory)
        .with_context(|| format!("create {}", directory.display()))?;
    let mut temp = tempfile::Builder::new()
        .prefix(&format!(".{}.tmp", file_name.to_string_lossy()))
        .tempfile_in(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    temp.write_all(contents)?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    std::fs::File::open(directory)?.sync_all()?;
    Ok(())
}

pub fn parse_chord(chord: &str) -> Result<(), String> {
    const MAX_LEN: usize = 40;
    const MODIFIERS: &[&str] = &["Cmd", "Alt", "Ctrl", "Shift"];
    if chord.len() > MAX_LEN {
        return Err("キーの組み合わせが長すぎます".into());
    }
    let parts: Vec<&str> = chord.split('+').collect();
    let Some((key, modifiers)) = parts.split_last() else {
        return Err("キーの組み合わせを入力してください".into());
    };
    if modifiers.is_empty() {
        return Err("修飾キーを 1 つ以上含めてください".into());
    }
    if !modifiers
        .iter()
        .all(|modifier| MODIFIERS.contains(modifier))
    {
        return Err(format!("未対応の修飾キーです: {}", modifiers.join("+")));
    }
    if key.is_empty() {
        return Err("キーを指定してください".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kikigaki_core::config::Capabilities;

    #[test]
    fn builtin_replace_dict_patch_uses_camel_case_json_wire_key() {
        let camel_case: SettingsPatch =
            serde_json::from_str(r#"{"builtinReplaceDict": true}"#).unwrap();
        assert_eq!(camel_case.builtin_replace_dict, Some(true));

        let snake_case: SettingsPatch =
            serde_json::from_str(r#"{"builtin_replace_dict": true}"#).unwrap();
        assert_eq!(snake_case.builtin_replace_dict, None);
    }

    #[test]
    fn atomic_write_creates_missing_parent_directories() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp
            .path()
            .join("fresh")
            .join("kikigaki")
            .join("config.toml");
        atomic_write(&path, b"a = 1\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"a = 1\n");
    }

    #[test]
    fn atomic_write_creates_the_file_with_owner_only_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        atomic_write(&path, b"hotkey = \"Alt+Space\"\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "hotkey = \"Alt+Space\"\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn atomic_write_reports_an_unwritable_directory_instead_of_reporting_success() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let locked = temp.path().join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = atomic_write(&locked.join("config.toml"), b"a = 1\n");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn apply_hotkey_patch_preserves_unrelated_toml_keys_and_comments() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(
            &path,
            "# a comment\nhotkey = \"Alt+Space\"\n[vad]\nthreshold = 0.6\n",
        )
        .unwrap();
        let mut coordinator = SettingsCoordinator::load(
            path.clone(),
            Capabilities {
                punct: true,
                remote_engine: true,
            },
        )
        .unwrap();
        coordinator
            .apply(SettingsPatch {
                hotkey: Some("Cmd+Shift+Space".into()),
                punctuation: None,
                builtin_replace_dict: None,
            })
            .unwrap();
        let on_disk = std::fs::read_to_string(path).unwrap();
        assert!(on_disk.contains("# a comment"));
        assert!(on_disk.contains("threshold = 0.6"));
        assert!(on_disk.contains("hotkey = \"Cmd+Shift+Space\""));
    }

    #[test]
    fn builtin_replace_dict_patch_persists_and_updates_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(
            &path,
            "# a comment\nhotkey = \"Alt+Space\"\n[vad]\nthreshold = 0.6\n",
        )
        .unwrap();
        let mut coordinator = SettingsCoordinator::load(
            path.clone(),
            Capabilities {
                punct: true,
                remote_engine: true,
            },
        )
        .unwrap();

        let snapshot = coordinator
            .apply(SettingsPatch {
                hotkey: None,
                punctuation: None,
                builtin_replace_dict: Some(true),
            })
            .unwrap();

        let on_disk = std::fs::read_to_string(path).unwrap();
        assert!(on_disk.contains("# a comment"));
        assert!(on_disk.contains("threshold = 0.6"));
        assert!(on_disk.contains("builtin_replace_dict = true"));
        assert!(snapshot.builtin_replace_dict);
        assert!(coordinator.snapshot().builtin_replace_dict);
    }

    #[test]
    fn absent_builtin_replace_dict_patch_leaves_file_and_config_value_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(&path, "hotkey = \"Alt+Space\"\n").unwrap();
        let mut coordinator = SettingsCoordinator::load(
            path.clone(),
            Capabilities {
                punct: true,
                remote_engine: true,
            },
        )
        .unwrap();
        let config_value = coordinator.config().builtin_replace_dict;

        let snapshot = coordinator
            .apply(SettingsPatch {
                hotkey: None,
                punctuation: None,
                builtin_replace_dict: None,
            })
            .unwrap();

        let on_disk = std::fs::read_to_string(path).unwrap();
        assert!(!on_disk.contains("builtin_replace_dict"));
        assert_eq!(snapshot.builtin_replace_dict, config_value);
        assert_eq!(coordinator.config().builtin_replace_dict, config_value);
    }

    #[test]
    fn punct_patch_sets_both_enabled_and_strip_trailing_period() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        let mut coordinator = SettingsCoordinator::load(
            path,
            Capabilities {
                punct: true,
                remote_engine: true,
            },
        )
        .unwrap();
        let snapshot = coordinator
            .apply(SettingsPatch {
                hotkey: None,
                punctuation: Some(PunctSetting::OnStrip),
                builtin_replace_dict: None,
            })
            .unwrap();
        assert!(snapshot.punct_enabled && snapshot.strip_trailing_period);
    }

    #[test]
    fn remote_server_punctuation_suppresses_local_punctuation_in_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(
            &path,
            "engine = \"remote\"\n[remote]\nserver_punctuates = true\n",
        )
        .unwrap();
        let coordinator = SettingsCoordinator::load(
            path,
            Capabilities {
                punct: true,
                remote_engine: true,
            },
        )
        .unwrap();

        assert!(!coordinator.snapshot().punct_enabled);
    }

    #[test]
    fn external_edit_between_load_and_apply_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(&path, "hotkey = \"Alt+Space\"\n").unwrap();
        let mut coordinator = SettingsCoordinator::load(
            path.clone(),
            Capabilities {
                punct: true,
                remote_engine: true,
            },
        )
        .unwrap();
        std::fs::write(&path, "hotkey = \"Cmd+Space\"\n").unwrap();
        let error = coordinator
            .apply(SettingsPatch {
                hotkey: Some("Ctrl+Space".into()),
                punctuation: None,
                builtin_replace_dict: None,
            })
            .unwrap_err();
        assert_eq!(error.code, "external_change");
    }

    #[test]
    fn external_edit_rejects_builtin_replace_dict_patch_without_partial_effects() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(&path, "hotkey = \"Alt+Space\"\n").unwrap();
        let mut coordinator = SettingsCoordinator::load(
            path.clone(),
            Capabilities {
                punct: true,
                remote_engine: true,
            },
        )
        .unwrap();
        let externally_edited = "# external edit\nhotkey = \"Cmd+Space\"\n";
        std::fs::write(&path, externally_edited).unwrap();

        let error = coordinator
            .apply(SettingsPatch {
                hotkey: None,
                punctuation: None,
                builtin_replace_dict: Some(true),
            })
            .unwrap_err();

        assert_eq!(error.code, "external_change");
        assert_eq!(std::fs::read_to_string(path).unwrap(), externally_edited);
        assert!(!coordinator.config().builtin_replace_dict);
        assert!(!coordinator.snapshot().builtin_replace_dict);
    }

    #[test]
    fn a_failed_apply_never_mutates_in_memory_state() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("cfgdir").join("config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "hotkey = \"Alt+Space\"\n").unwrap();
        let mut coordinator = SettingsCoordinator::load(
            path.clone(),
            Capabilities {
                punct: true,
                remote_engine: true,
            },
        )
        .unwrap();
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        assert!(coordinator
            .apply(SettingsPatch {
                hotkey: Some("Cmd+Space".into()),
                punctuation: None,
                builtin_replace_dict: None,
            })
            .is_err());
        assert_eq!(coordinator.snapshot().hotkey, "Alt+Space");
    }

    #[test]
    fn hotkey_chord_needs_at_least_one_modifier_and_is_length_capped() {
        assert!(parse_chord("Space").is_err());
        assert!(parse_chord("Alt+Space").is_ok());
        assert!(parse_chord(&"Alt+".repeat(20)).is_err());
    }
}
