# Windows-side build/test driver, invoked by scripts/win-sync-build.sh or run directly on the
# build machine. Mirrors what mac-sync-build.sh inlines into its ssh command for macOS.
#
#   powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\win-build.ps1 build
#   powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\win-build.ps1 test
# Smart App Control can block a fresh executable, including makensis.exe downloaded by
# tauri-bundler on the first build, with os error 4551; wait ~45 s and rerun.
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("build", "test")]
    [string]$Command
)
$ErrorActionPreference = "Stop"

function Invoke-Native {
    param([Parameter(Mandatory = $true)][scriptblock]$Cmd, [string]$Description = "")
    $ErrorActionPreference = "Continue"
    & $Cmd
    if ($LASTEXITCODE -ne 0) {
        throw "native command failed (exit $LASTEXITCODE): $Description"
    }
}

# $PSScriptRoot-relative (H5) - no .git on the synced tree (win-sync-build.sh's tar excludes it).
$Root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Set-Location $Root
# target\ lives one level above src\ (see win-sync-build.sh) so it survives a fresh-directory
# re-sync; when run directly (not via the sync script) this just resolves to a sibling of
# whatever checkout $Root is, which is still correct.
$env:CARGO_TARGET_DIR = Join-Path (Split-Path $Root -Parent) "target"

& (Join-Path $PSScriptRoot "fetch-sherpa-win.ps1")
# The fetch script runs under Stop and throws on every failure path.
$env:SHERPA_ONNX_LIB_DIR = Join-Path $env:LOCALAPPDATA "kikigaki-build\sherpa-v1.13.6\lib"

if ($Command -eq "test") {
    # H10: cargo test --workspace --lib already exercises punct - kikigaki's own default
    # features enable it, and cargo unifies features across a --workspace invocation, so
    # kikigaki-engine builds with punct here even though its *own* default features don't
    # include it (see Task 1's Cargo.toml split). No separate --features punct leg is needed
    # for this specific command.
    Invoke-Native { cargo test --workspace --lib } "cargo test --workspace --lib"
    Invoke-Native { cargo clippy --workspace -- -D warnings } "cargo clippy --workspace"

    if ($env:KIKIGAKI_MODELS_DIR) {
        if ($env:KIKIGAKI_TEST_WAVS_DIR) {
            $TestWavsDir = $env:KIKIGAKI_TEST_WAVS_DIR
        } else {
            $TestWavsDir = Join-Path $env:KIKIGAKI_MODELS_DIR "reazonspeech-ja-en-2025-01-17\test_wavs"
        }
        if (-not [System.IO.Path]::IsPathRooted($TestWavsDir)) {
            throw "win-build test: KIKIGAKI_TEST_WAVS_DIR / KIKIGAKI_MODELS_DIR must be absolute: $TestWavsDir"
        }
        foreach ($TestWav in @("t09.wav", "test_ja_1.wav", "test_ja_2.wav")) {
            $TestWavPath = Join-Path $TestWavsDir $TestWav
            if (-not (Test-Path -LiteralPath $TestWavPath -PathType Leaf)) {
                throw "win-build test: test WAV missing: $TestWavPath"
            }
        }

        Invoke-Native { cargo test -p kikigaki-engine --all-features --no-run } "cargo test -p kikigaki-engine --all-features --no-run"
        $ProfileDir = Join-Path $env:CARGO_TARGET_DIR "debug"
        $DepsDir = Join-Path $ProfileDir "deps"
        # Test executables run from deps\, where no runtime DLLs are staged by the build script.
        # System32 ships onnxruntime.dll and is searched ahead of PATH, so the wrong ORT loads.
        # Copying the DLLs into deps\ makes the executable-adjacent lookup win, matching the
        # installed app's layout.
        $OnnxRuntimeDll = Join-Path $ProfileDir "onnxruntime.dll"
        if (-not (Test-Path -LiteralPath $OnnxRuntimeDll -PathType Leaf)) {
            throw "win-build test: runtime DLL missing: $OnnxRuntimeDll (SHERPA_ONNX_LIB_DIR misconfigured?)"
        }
        Get-ChildItem -LiteralPath $ProfileDir -Filter "*.dll" -File |
            Copy-Item -Destination $DepsDir -Force

        # No --ignored (real_models.rs has no #[ignore] attributes - it self-skips at runtime
        # via KIKIGAKI_MODELS_DIR and reports `ok` either way, so `-- --ignored` would silently
        # select zero tests and this whole block would falsely "pass" - Task 1's Windows-side
        # verification resolved this exact invocation; reused here verbatim). --all-features
        # so the punct/ORT path is actually exercised, not skipped by an unset feature flag.
        $LogPath = Join-Path $env:TEMP "kikigaki-real-models.log"
        Invoke-Native {
            cargo test -p kikigaki-engine --all-features -- --nocapture 2>&1 |
                Tee-Object -FilePath $LogPath
        } "cargo test -p kikigaki-engine --all-features"
        if (Select-String -Path $LogPath -Pattern "SKIPPED" -Quiet) {
            throw "win-build test: real-model tests skipped (see SKIPPED lines in $LogPath)"
        }
    } else {
        Write-Host "win-build: KIKIGAKI_MODELS_DIR unset, skipping real-model tests"
    }
    exit 0
}

$TauriVersionOutput = & {
    $ErrorActionPreference = "Continue"
    cmd /c "cargo tauri --version 2>nul"
}
$TauriVersionLine = ($TauriVersionOutput | Out-String).Trim()
if ($TauriVersionLine -ne "tauri-cli 2.11.4") {
    Invoke-Native { cargo install tauri-cli --version "=2.11.4" --locked } "install tauri-cli"
}
Set-Location (Join-Path $Root "crates\kikigaki")
Invoke-Native { cargo tauri build --bundles nsis -- --locked } "cargo tauri build"
