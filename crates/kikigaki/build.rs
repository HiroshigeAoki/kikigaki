fn main() {
    // tauri-build tracks tauri.conf.json but not `frontendDist`; without this a JS/HTML-only
    // edit leaves the previously embedded assets in the binary.
    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed=capabilities");
    // tauri-build's default manifest, including the Common Controls v6 dependency, is a
    // bin-only resource (embed-resource directs its linker arguments to binaries). Cargo's
    // lib unit-test harness therefore had no manifest and bound System32's comctl32 v5. It
    // died at load with STATUS_ENTRYPOINT_NOT_FOUND on TaskDialogIndirect (imported via muda /
    // tauri-runtime-wry). Cargo's tests-scoped linker instruction cannot help because it
    // applies only to integration-test targets, so give every executable of this crate a
    // linker-synthesized embedded manifest with the same dependency and disable tauri-build's
    // own manifest to avoid a duplicate RT_MANIFEST. The generated manifest also carries the
    // default requestedExecutionLevel asInvoker, which is already the effective default.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg=/MANIFESTDEPENDENCY:type='win32' \
             name='Microsoft.Windows.Common-Controls' version='6.0.0.0' \
             processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
        );
    }
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest())
            .app_manifest(tauri_build::AppManifest::new().commands(&[
                "get_snapshot",
                "apply_settings",
                "begin_hotkey_capture",
                "end_hotkey_capture",
                "set_launch_at_login",
                "retry_bootstrap",
                "open_config",
                "open_transcript_dir",
                "open_settings_pane",
                "request_microphone_access",
                "start_download",
                "retry_download",
                "list_history",
                "clear_history",
                "quit",
            ])),
    )
    .expect("tauri_build");
}
