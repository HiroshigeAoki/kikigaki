#![cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]

//! Build-time capability flags. Phase 3a has no CLI surface; model downloads happen through the
//! onboarding window (design §6).

use kikigaki_core::config::Capabilities;

/// Returns the capabilities compiled into this binary.
pub fn capabilities() -> Capabilities {
    Capabilities {
        punct: cfg!(feature = "punct"),
        remote_engine: cfg!(all(feature = "remote-engine", target_os = "macos")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kikigaki_core::config::Config;

    #[test]
    fn remote_engine_capability_is_false_off_macos() {
        if !cfg!(target_os = "macos") {
            assert!(!capabilities().remote_engine);
        }
    }

    #[test]
    fn legacy_config_loads_with_warnings_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "long_key = true\nlong_hotkey = \"Ctrl+Alt+Space\"\nengine = \"whisper\"\n",
        )
        .unwrap();
        let (config, warnings) = Config::load_with_warnings(Some(&path)).unwrap();
        assert_eq!(warnings.len(), 3);
        config.validate(capabilities()).unwrap();
        assert_eq!(config.engine, kikigaki_core::config::EngineKind::Local);
    }
}
