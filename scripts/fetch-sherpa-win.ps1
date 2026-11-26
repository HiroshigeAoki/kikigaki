# fetch-sherpa-win.ps1 -- Windows PowerShell 5.1 script (M11 -- no `pwsh`); run via powershell.exe,
# not by executing this file directly, as shown below.
# Fetches, sha256-verifies, and extracts the Windows sherpa-onnx no-tts shared archive into a
# persistent cache directory, then stages the three *runtime* DLLs kikigaki ships (not the
# .lib import libraries, which stay in the cache and are referenced via SHERPA_ONNX_LIB_DIR)
# into crates/kikigaki/vendor/win/. Idempotent and hash-verified on every run -- a corrupted or
# tampered cache must fail loudly, not link silently against the wrong bytes.
#
# Requires Windows 11 24H2 or later: extraction needs the in-box bsdtar built with bz2lib
# support to read this archive's .tar.bz2 compression. Windows 10's tar.exe lacks bz2lib; the
# pre-extraction check below fails with a clear message if it is missing.
#
#   powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\fetch-sherpa-win.ps1
#
# Mirrors scripts/fetch-onnxruntime.sh's shape; see
# docs/plans/2026-09-07-windows-support-design.md section 1 for the archive provenance.
$ErrorActionPreference = "Stop"

# $PSScriptRoot-relative, not `git rev-parse` (H5) -- the synced tree on the build machine has no
# .git directory (win-sync-build.sh's tar transfer excludes it), so `git rev-parse
# --show-toplevel` fails on line 1 of every script that used it. `scripts\` is always one level
# below the repo root, on every checkout shape this repo supports.
$Root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$CacheDir = Join-Path $env:LOCALAPPDATA "kikigaki-build\sherpa-v1.13.6"
$VendorDir = Join-Path $Root "crates\kikigaki\vendor\win"
$ArchiveUrl = "https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.13.6/sherpa-onnx-v1.13.6-win-x64-shared-MT-Release-no-tts-lib.tar.bz2"

# Every file this script produces is pinned -- re-verified on every run, cache included.
$Pins = [ordered]@{
    "archive"                            = "323d80e7e83e862bbdf204260c77a8f4e9da71df0ee1ba11d2db6cb12414c34e"
    "lib\onnxruntime.dll"                = "b9f6713c3602a4742680a7e6a77e3f9ac4a676ad9447bce609e53efcda795d7e"
    "lib\onnxruntime_providers_shared.dll" = "1f8b8ebfb07aad2c692669ba77c45b5a24e72b9c708bc3f2be4103ff29370c60"
    "lib\sherpa-onnx-c-api.dll"           = "36fc4bed08fde511c51e84db4c0fdf2fcde5c7a34c25a17c3d2f4d3911a05a07"
    "lib\sherpa-onnx-c-api.lib"           = "aa04e30cd0d9386fa2446b5b8cc8dff6dcf1c15d1263be1bcfc86cf92f6663d2"
    "lib\onnxruntime.lib"                 = "b9fc3cd678257d88a111b0773ede4bfceaf0fe95daab4379f2b2b37348a68781"
}
# (Re-derive these six hashes from the design doc's "Verified facts" table before implementing --
# copy them character-for-character; a transcription error here fails closed, which is the
# correct failure mode, but wastes a build cycle finding out.)
$RuntimeDlls = @("sherpa-onnx-c-api.dll", "onnxruntime.dll", "onnxruntime_providers_shared.dll")

function Assert-Sha256 {
    param([string]$Path, [string]$Expected, [string]$Label)
    if (-not (Test-Path $Path)) {
        throw "fetch-sherpa-win: $Label missing: $Path"
    }
    $Actual = (Get-FileHash -Algorithm SHA256 -Path $Path).Hash.ToLower()
    if ($Actual -ne $Expected) {
        throw "fetch-sherpa-win: $Label hash mismatch: got $Actual, expected $Expected -- delete $CacheDir and re-run"
    }
}

$LibDir = Join-Path $CacheDir "lib"
$NeedsExtract = $true
if (Test-Path $LibDir) {
    $AllPresent = $true
    foreach ($rel in $Pins.Keys) {
        if ($rel -eq "archive") { continue }
        if (-not (Test-Path (Join-Path $CacheDir $rel))) { $AllPresent = $false }
    }
    if ($AllPresent) { $NeedsExtract = $false }
}

if ($NeedsExtract) {
    New-Item -ItemType Directory -Force -Path $CacheDir | Out-Null
    $ArchivePath = Join-Path $CacheDir "sherpa-onnx-win.tar.bz2"
    # Windows PowerShell 5.1 defaults to weaker TLS and renders a progress bar that throttles
    # large downloads badly; -UseBasicParsing skips the IE first-run engine (often absent on
    # build images), which would otherwise fail the request outright.
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $ProgressPreference = 'SilentlyContinue'
    Invoke-WebRequest -Uri $ArchiveUrl -OutFile $ArchivePath -UseBasicParsing
    Assert-Sha256 -Path $ArchivePath -Expected $Pins["archive"] -Label "archive"
    # Extracted flat into $CacheDir (Windows 11 24H2+ ships bsdtar with bz2lib as `tar`); the
    # archive's own top-level layout is
    # `sherpa-onnx-v1.13.6-win-x64-shared-MT-Release-no-tts-lib/lib/...`, so re-home that one
    # known subdirectory rather than searching for it.
    $ExtractRoot = Join-Path $CacheDir "sherpa-onnx-v1.13.6-win-x64-shared-MT-Release-no-tts-lib"
    if (Test-Path $ExtractRoot) { Remove-Item -Recurse -Force $ExtractRoot }
    if (-not ((& tar --version) -match 'bz2lib')) {
        throw "fetch-sherpa-win: tar.exe lacks bz2lib support; this script requires Windows 11 24H2 or later's in-box bsdtar to extract the .tar.bz2 archive"
    }
    tar -xjf $ArchivePath -C $CacheDir
    if ($LASTEXITCODE -ne 0) {
        throw "fetch-sherpa-win: tar extraction failed (exit $LASTEXITCODE)"
    }
    if (-not (Test-Path (Join-Path $ExtractRoot "lib"))) {
        throw "fetch-sherpa-win: expected $ExtractRoot\lib not found after extraction -- archive layout changed?"
    }
    if (Test-Path $LibDir) { Remove-Item -Recurse -Force $LibDir }
    Move-Item -Path (Join-Path $ExtractRoot "lib") -Destination $LibDir
    Remove-Item -Recurse -Force $ExtractRoot
    Remove-Item -Force $ArchivePath
}

foreach ($rel in $Pins.Keys) {
    if ($rel -eq "archive") { continue }
    Assert-Sha256 -Path (Join-Path $CacheDir $rel) -Expected $Pins[$rel] -Label $rel
}

New-Item -ItemType Directory -Force -Path $VendorDir | Out-Null
foreach ($dll in $RuntimeDlls) {
    Copy-Item -Path (Join-Path $LibDir $dll) -Destination (Join-Path $VendorDir $dll) -Force
}

Write-Host "fetch-sherpa-win: SHERPA_ONNX_LIB_DIR=$LibDir"
Write-Host "fetch-sherpa-win: staged runtime DLLs to $VendorDir"
# Callers (win-build.ps1, release-win.ps1) do not capture this script's output; they recompute
# the same $CacheDir\lib path themselves after running it, since it's a pure function of
# $PSScriptRoot and the pinned version string.
