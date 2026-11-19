#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

//! Build-time capability flags. Phase 3a has no CLI surface; model downloads happen through the
//! onboarding window (design §6).

use kikigaki_core::config::Capabilities;

/// Returns the capabilities compiled into this binary.
pub fn capabilities() -> Capabilities {
    Capabilities {
        punct: cfg!(feature = "punct"),
        remote_engine: cfg!(feature = "remote-engine"),
        long_key: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kikigaki_core::config::Config;

    #[test]
    fn build_has_no_long_key_capability() {
        assert!(!capabilities().long_key);
    }

    #[test]
    fn legacy_long_key_config_loads_but_is_ineffective() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "long_key = true\nlong_hotkey = \"Ctrl+Alt+Space\"\nengine = \"whisper\"\n",
        )
        .unwrap();
        let config = Config::load(Some(&path)).unwrap();
        assert!(config.long_key);
        assert_eq!(config.long_hotkey, "Ctrl+Alt+Space");
        // Loading a legacy engine selection retains the existing migration to Local.
        assert_eq!(config.engine, kikigaki_core::config::EngineKind::Local);
        assert!(!config.long_key_effective(capabilities()));
    }
}
