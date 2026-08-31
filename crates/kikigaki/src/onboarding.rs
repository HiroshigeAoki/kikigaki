//! First-launch permission and required-model scanning.

#[cfg(target_os = "macos")]
pub fn scan(
    settings: &crate::settings::SettingsCoordinator,
    startup: &dyn crate::controller::StartupPort,
) -> Option<crate::status::OnboardingState> {
    let microphone = mic_status_str(crate::permissions::microphone_permission());
    let accessibility_trusted = accessibility_trusted();
    let models_installed = required_models_installed(&settings.config().models_dir, startup);
    if microphone == "authorized" && accessibility_trusted && models_installed {
        return None;
    }
    Some(crate::status::OnboardingState {
        microphone,
        accessibility_trusted,
        models_installed,
        download: None,
        consent_copy: consent_copy(startup.required()),
    })
}

fn required_models_installed(
    models_dir: &std::path::Path,
    startup: &dyn crate::controller::StartupPort,
) -> bool {
    startup
        .required()
        .iter()
        .all(|model| kikigaki_core::models::is_installed(models_dir, model))
}

fn consent_copy(required: &[&'static kikigaki_core::models::Model]) -> String {
    use kikigaki_core::models::Payload;

    let (mut transfer_bytes, mut installed_bytes) = (0_u64, 0_u64);
    for model in required {
        match model.payload {
            Payload::File(file) => {
                transfer_bytes = transfer_bytes.saturating_add(file.size);
                installed_bytes = installed_bytes.saturating_add(file.size);
            }
            Payload::Files(files) => {
                let size = files.iter().map(|file| file.size).sum::<u64>();
                transfer_bytes = transfer_bytes.saturating_add(size);
                installed_bytes = installed_bytes.saturating_add(size);
            }
            Payload::TarBz2 {
                archive_size,
                files,
                ..
            } => {
                transfer_bytes = transfer_bytes.saturating_add(archive_size);
                installed_bytes =
                    installed_bytes.saturating_add(files.iter().map(|file| file.size).sum::<u64>());
            }
        }
    }
    format!(
        "{} MB のダウンロード（インストール後 {} MB）",
        transfer_bytes / 1_000_000,
        installed_bytes / 1_000_000,
    )
}

#[cfg(target_os = "macos")]
fn accessibility_trusted() -> bool {
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }

    unsafe { AXIsProcessTrusted() }
}

#[cfg(target_os = "macos")]
fn mic_status_str(status: crate::permissions::MicPermission) -> &'static str {
    use crate::permissions::MicPermission;

    match status {
        MicPermission::NotDetermined => "not_determined",
        MicPermission::Restricted => "restricted",
        MicPermission::Denied => "denied",
        MicPermission::Authorized => "authorized",
        MicPermission::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::time::Duration;

    use kikigaki_core::models::{ok_marker, Model, MODELS};

    use super::*;

    struct RequiredStartup {
        required: Vec<&'static Model>,
        events: VecDeque<kikigaki_core::startup::StartupEvent>,
    }

    impl crate::controller::StartupPort for RequiredStartup {
        fn start(&mut self) -> anyhow::Result<bool> {
            Ok(false)
        }

        fn try_recv(&mut self) -> Option<kikigaki_core::startup::StartupEvent> {
            self.events.pop_front()
        }

        fn required(&self) -> &[&'static Model] {
            &self.required
        }

        fn invalidate_model_load_failure(&mut self, _failed_model: Option<&'static str>) -> bool {
            false
        }

        fn join(&mut self, _timeout: Duration) -> bool {
            true
        }

        fn rescan_onboarding(
            &mut self,
            _settings: &crate::settings::SettingsCoordinator,
        ) -> Option<crate::status::OnboardingState> {
            None
        }
    }

    #[test]
    fn readiness_checks_only_startup_ports_required_models() {
        let temp = tempfile::tempdir().unwrap();
        let none = RequiredStartup {
            required: Vec::new(),
            events: VecDeque::new(),
        };
        assert!(required_models_installed(temp.path(), &none));

        let model = &MODELS[1];
        let model_dir = temp.path().join(model.id);
        std::fs::create_dir_all(&model_dir).unwrap();
        let kikigaki_core::models::Payload::File(file) = model.payload else {
            panic!("test fixture must be a single file")
        };
        let installed = std::fs::File::create(model_dir.join(file.name)).unwrap();
        installed.set_len(file.size).unwrap();
        std::fs::write(model_dir.join(".ok"), ok_marker(model)).unwrap();

        let one = RequiredStartup {
            required: vec![model],
            events: VecDeque::new(),
        };
        assert!(required_models_installed(temp.path(), &one));
        let one_plus_missing = RequiredStartup {
            required: vec![model, &MODELS[0]],
            events: VecDeque::new(),
        };
        assert!(!required_models_installed(temp.path(), &one_plus_missing));
    }

    #[test]
    fn consent_copy_sums_transfer_and_installed_bytes_from_required_payloads() {
        assert_eq!(
            consent_copy(&[&MODELS[0], &MODELS[1]]),
            "438 MB のダウンロード（インストール後 73 MB）"
        );
        assert_eq!(
            consent_copy(&[]),
            "0 MB のダウンロード（インストール後 0 MB）"
        );
    }
}
