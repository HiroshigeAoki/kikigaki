fn main() {
    // tauri-build tracks tauri.conf.json but not `frontendDist`; without this a JS/HTML-only
    // edit leaves the previously embedded assets in the binary.
    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed=capabilities");
    #[cfg(target_os = "macos")]
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "get_snapshot",
            "apply_settings",
            "begin_hotkey_capture",
            "end_hotkey_capture",
            "set_launch_at_login",
            "retry_bootstrap",
            "open_config",
            "open_settings_pane",
            "request_microphone_access",
            "start_download",
            "retry_download",
            "list_history",
            "preview_correction",
            "remember_correction",
            "delete_learned_rule",
            "clear_history",
            "quit",
        ]),
    ))
    .expect("tauri_build");
}
