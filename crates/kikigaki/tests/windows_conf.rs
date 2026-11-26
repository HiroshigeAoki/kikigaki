//! Sanity-checks `tauri.windows.conf.json` on every OS (it's just JSON) — the config itself is
//! only exercised for real by a Windows `cargo tauri build`, but a malformed or drifted file
//! should fail fast on Linux CI too.

#[test]
fn windows_conf_declares_nsis_and_exactly_the_three_shipped_dlls() {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tauri.windows.conf.json"
    ))
    .expect("tauri.windows.conf.json must exist");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("must be valid JSON");

    let targets = value["bundle"]["targets"]
        .as_array()
        .expect("bundle.targets must be an array");
    assert_eq!(targets, &[serde_json::json!("nsis")]);

    let install_mode = value["bundle"]["windows"]["nsis"]["installMode"]
        .as_str()
        .expect("nsis.installMode must be a string");
    assert_eq!(install_mode, "currentUser");

    let webview_mode = value["bundle"]["windows"]["webviewInstallMode"]["type"]
        .as_str()
        .expect("webviewInstallMode.type must be a string");
    assert_eq!(webview_mode, "downloadBootstrapper");

    let resources = value["bundle"]["resources"]
        .as_object()
        .expect("bundle.resources must be an object");
    let dll_sources: std::collections::BTreeSet<&str> =
        resources.keys().map(String::as_str).collect();
    let expected: std::collections::BTreeSet<&str> = [
        "vendor/win/sherpa-onnx-c-api.dll",
        "vendor/win/onnxruntime.dll",
        "vendor/win/onnxruntime_providers_shared.dll",
    ]
    .into_iter()
    .collect();
    assert_eq!(
        dll_sources, expected,
        "bundle.resources must map exactly the three shipped DLLs, no more, no fewer"
    );
    // Design §1: DLLs land next to the exe, not under a resources\ subdirectory.
    for destination in resources.values() {
        let destination = destination.as_str().unwrap();
        assert!(
            !destination.contains('/') && !destination.contains('\\'),
            "DLL destination {destination:?} must be a bare file name at $INSTDIR root, not nested"
        );
    }

    // N1: tauri-build 2.6.3 requires a .ico; verify both that this config declares one and
    // that the file it points at actually exists (a config-only assertion could pass while the
    // file itself is missing, which is exactly the failure mode this test exists to catch).
    let icons = value["bundle"]["icon"]
        .as_array()
        .expect("bundle.icon must be an array");
    let ico_path = icons
        .iter()
        .filter_map(|entry| entry.as_str())
        .find(|entry| entry.ends_with(".ico"))
        .expect("bundle.icon must list a .ico file for the Windows build");
    let full_ico_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(ico_path);
    assert!(
        full_ico_path.is_file(),
        "bundle.icon points at {ico_path:?}, which does not exist at {full_ico_path:?}"
    );
}
