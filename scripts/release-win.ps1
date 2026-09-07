# Windows release build: installs the produced NSIS installer into a fresh scratch directory
# and gates the *installed* payload (H6) - GPL-symbol scan (ASCII + UTF-16LE, M13), DLL
# allowlist (vswhere-located dumpbin, H7), ORT provenance, NOTICE/license bundling, no
# executables under models\, release metadata (M14) - then launches the installed exe for a
# smoke check, exercises the uninstall hook, uninstalls, and verifies the registry is clean.
# Produces kikigaki-<ver>-windows-x64-setup.exe + .sha256 in a versioned output directory beside
# target\, outside the src\ tree that win-sync-build.sh replaces.
# Do not run this gate on an account with a real per-user kikigaki install: both installations
# share the HKCU Uninstall and manufacturer keys, and uninstalling the gate copy (plus its hook)
# removes that user's real autostart entry.
# NSIS requires unquoted /D= and _?= paths, so %TEMP% must not contain spaces (true on the build
# machine); otherwise set $InstallDir to a space-free path.
$ErrorActionPreference = "Stop"

function Invoke-Native {
    param([Parameter(Mandatory = $true)][scriptblock]$Cmd, [string]$Description = "")
    $ErrorActionPreference = "Continue"
    & $Cmd
    if ($LASTEXITCODE -ne 0) {
        throw "native command failed (exit $LASTEXITCODE): $Description"
    }
}

# $PSScriptRoot-relative (H5), not `git rev-parse --show-toplevel`.
$Root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
# target\ lives one level above src\ (see win-sync-build.sh) so it survives a fresh-directory
# re-sync; when run directly (not via the sync script) this just resolves to a sibling of
# whatever checkout $Root is, which is still correct.
$env:CARGO_TARGET_DIR = Join-Path (Split-Path $Root -Parent) "target"

Write-Host "== release metadata gate (M14) =="
$CargoVersion = (Select-String -Path (Join-Path $Root "Cargo.toml") `
    -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches.Groups[1].Value
if (-not $CargoVersion) {
    throw "release-win: could not read [workspace.package] version from Cargo.toml"
}
$TauriConf = Get-Content (Join-Path $Root "crates\kikigaki\tauri.conf.json") -Raw |
    ConvertFrom-Json
if ($TauriConf.version -ne $CargoVersion) {
    throw "release-win: tauri.conf.json version ($($TauriConf.version)) != Cargo.toml version ($CargoVersion)"
}
$Version = $CargoVersion

& (Join-Path $PSScriptRoot "fetch-sherpa-win.ps1")
# The fetch script runs under Stop and throws on every failure path.
$env:SHERPA_ONNX_LIB_DIR = Join-Path $env:LOCALAPPDATA "kikigaki-build\sherpa-v1.13.6\lib"

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

$TargetDir = (Invoke-Native { cargo metadata --format-version=1 --no-deps } "cargo metadata" | ConvertFrom-Json).target_directory
$NsisDir = Join-Path $TargetDir "release\bundle\nsis"
$Installers = @(Get-ChildItem -Path $NsisDir -Filter "*-setup.exe")
if ($Installers.Count -ne 1) {
    throw "release-win: expected exactly one *-setup.exe in $NsisDir, found $($Installers.Count)"
}
$Installer = $Installers[0]

Write-Host "== silent install into a fresh scratch directory (H6) =="
$InstallDir = Join-Path $env:TEMP "kikigaki-release-gate-$([System.Guid]::NewGuid())"
$ManufacturerKey = "HKCU:\Software\aoki\kikigaki"
$ManufacturerKeyExisted = Test-Path $ManufacturerKey
$ManufacturerDefaultExisted = $false
if ($ManufacturerKeyExisted) {
    $SavedManufacturerKey = Get-Item -Path $ManufacturerKey
    $ManufacturerDefaultExisted = @($SavedManufacturerKey.GetValueNames()) -contains ""
    if ($ManufacturerDefaultExisted) {
        $ManufacturerDefaultValue = $SavedManufacturerKey.GetValue(
            "", $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        $ManufacturerDefaultKind = $SavedManufacturerKey.GetValueKind("")
    }
}
$Process = $null
$Uninstalled = $false
try {
try {
    $ipsi = New-Object System.Diagnostics.ProcessStartInfo
    $ipsi.FileName = $Installer.FullName
    $ipsi.Arguments = "/S /NS /D=$InstallDir"
    $ipsi.UseShellExecute = $false
    $ip = [System.Diagnostics.Process]::Start($ipsi)
} catch [System.ComponentModel.Win32Exception] {
    if ($_.Exception.NativeErrorCode -eq 4551) {
        throw "release-win: Smart App Control blocked $($Installer.FullName); wait ~45 s and rerun"
    }
    throw
}
$ip.WaitForExit()
if ($ip.ExitCode -ne 0) {
    throw "release-win: silent install failed (exit $($ip.ExitCode))"
}
Start-Sleep -Seconds 2   # NSIS's /S install returns before file-copy is guaranteed flushed
$MainExe = Join-Path $InstallDir "kikigaki.exe"
$Dlls = @("sherpa-onnx-c-api.dll", "onnxruntime.dll", "onnxruntime_providers_shared.dll") |
    ForEach-Object { Join-Path $InstallDir $_ }
$BinaryTargets = @($MainExe) + $Dlls
foreach ($target in $BinaryTargets) {
    if (-not (Test-Path $target)) {
        throw "release-win: expected binary not found in installed payload: $target"
    }
}

Write-Host "== exact expected file set =="
$ExpectedFiles = @(
    "kikigaki.exe", "sherpa-onnx-c-api.dll", "onnxruntime.dll",
    "onnxruntime_providers_shared.dll", "NOTICE",
    "licenses\sherpa-onnx.LICENSE", "licenses\onnxruntime.LICENSE",
    "licenses\silero-vad.LICENSE", "licenses\hayamimi.LICENSE", "licenses\README",
    "licenses\kissfft.LICENSE", "licenses\onnxruntime-THIRD-PARTY-NOTICES.txt"
)
$ActualFiles = Get-ChildItem -Path $InstallDir -Recurse -File |
    ForEach-Object { $_.FullName.Substring($InstallDir.Length + 1) } |
    Where-Object { $_ -notmatch '^(Uninstall\.exe|uninstall\.dat)$' } |
    Sort-Object
$MissingFiles = Compare-Object -ReferenceObject ($ExpectedFiles | Sort-Object) `
    -DifferenceObject $ActualFiles
if ($MissingFiles) {
    throw "release-win: installed payload differs from the expected file set:`n$($MissingFiles | Out-String)"
}

Write-Host "== GPL string gate (ASCII + UTF-16LE, M13) =="
# A positive control first: confirm the scan actually finds a known-present string, so a
# silently-broken scan (wrong encoding, wrong path) can't pass vacuously.
$KnownPresent = Select-String -Path $MainExe -Pattern "kikigaki" -Encoding Ascii -AllMatches
if (-not $KnownPresent) {
    throw "release-win: GPL scan positive control failed - 'kikigaki' not found in $MainExe (scan itself is broken)"
}
$GplPatterns = "espeak_|espeak-ng-data|piper_phonemize"
foreach ($target in $BinaryTargets) {
    $AsciiHit = Select-String -Path $target -Pattern $GplPatterns -Encoding Ascii -AllMatches
    # -Encoding Unicode decodes from byte 0, so it can miss an odd-offset UTF-16LE string; the
    # ASCII leg plus the positive control is the primary gate.
    $Utf16Hit = Select-String -Path $target -Pattern $GplPatterns -Encoding Unicode -AllMatches
    if ($AsciiHit -or $Utf16Hit) {
        throw "release-win: GPL-tainted string found in $target"
    }
}

Write-Host "== DLL dependency allowlist gate (vswhere-located dumpbin, H7) =="
$VsWhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $VsWhere)) {
    throw "release-win: vswhere.exe not found at $VsWhere (install Visual Studio / Build Tools)"
}
$VsInstallPath = Invoke-Native {
    & $VsWhere -latest -products * -property installationPath
} "vswhere Visual Studio installation path"
if (-not $VsInstallPath) {
    throw "release-win: vswhere found no Visual Studio installation"
}
$DumpbinCandidates = @(
    Get-ChildItem -Path (Join-Path $VsInstallPath "VC\Tools\MSVC") -Directory |
        ForEach-Object { Join-Path $_.FullName "bin\Hostx64\x64\dumpbin.exe" } |
        Where-Object { Test-Path $_ }
)
if (-not $DumpbinCandidates) {
    throw "release-win: no dumpbin.exe found under $VsInstallPath\VC\Tools\MSVC\*\bin\Hostx64\x64"
}
$Dumpbin = $DumpbinCandidates[0]
# Allowlist derived from the pinned binaries themselves (verified via a string scan of the binaries)
# plus the observed kikigaki.exe import set - record the *actual* set on first build here and
# keep this list in sync; do not add an entry "just in case" without observing it first.
# First release kikigaki.exe (2026-09-08): bcryptprimitives.dll (Rust std),
# comctl32.dll (tao/muda), dwmapi.dll (tao), shlwapi.dll (tao/Tauri), mmdevapi.dll (cpal WASAPI).
$Allowlist = @(
    "sherpa-onnx-c-api.dll", "onnxruntime.dll", "onnxruntime_providers_shared.dll",
    "KERNEL32.dll", "ADVAPI32.dll", "SETUPAPI.dll", "dbghelp.dll", "dxgi.dll",
    "api-ms-win-core-*.dll", "api-ms-win-crt-*.dll",
    "ntdll.dll", "USER32.dll", "SHELL32.dll", "ole32.dll", "OLEAUT32.dll", "GDI32.dll",
    "WS2_32.dll", "BCRYPT.dll", "CRYPT32.dll", "SECUR32.dll", "USERENV.dll",
    "WebView2Loader.dll", "MSVCP140.dll", "VCRUNTIME140.dll", "VCRUNTIME140_1.dll",
    "bcryptprimitives.dll",
    "comctl32.dll",
    "dwmapi.dll",
    "shlwapi.dll",
    "mmdevapi.dll"
)
foreach ($target in $BinaryTargets) {
    $Output = Invoke-Native { & $Dumpbin /dependents $target } "dumpbin /dependents $target"
    if (-not $Output) {
        throw "release-win: dumpbin /dependents failed or returned nothing for $target"
    }
    $Deps = @($Output | ForEach-Object { if ($_ -match '^\s+([^\s\\]+\.dll)\s*$') { $Matches[1] } })
    foreach ($dep in $Deps) {
        $Allowed = $Allowlist | Where-Object { $dep -like $_ }
        if (-not $Allowed) {
            throw "release-win: unexpected DLL dependency in $target`: $dep (update `$Allowlist above only after confirming this dependency is expected)"
        }
    }
}

Write-Host "== ONNX Runtime provenance gate =="
# fetch-sherpa-win.ps1 already hash-verified the staged copy against the pinned value (Task 7,
# M6) - this gate only needs to confirm the *shipped* copy is byte-identical to that already-
# verified staged copy, not re-pin the same hash a second time in a second file.
$VendorOrt = Join-Path $Root "crates\kikigaki\vendor\win\onnxruntime.dll"
$VendorSha256 = (Get-FileHash -Algorithm SHA256 -Path $VendorOrt).Hash.ToLower()
$ShippedSha256 = (Get-FileHash -Algorithm SHA256 -Path (Join-Path $InstallDir "onnxruntime.dll")).Hash.ToLower()
if ($ShippedSha256 -ne $VendorSha256) {
    throw "release-win: shipped onnxruntime.dll differs from the fetch-verified vendor copy"
}

Write-Host "== PE machine type gate (x64, M14) =="
$HeaderOutput = Invoke-Native { & $Dumpbin /headers $MainExe } "dumpbin /headers $MainExe"
if (-not ($HeaderOutput | Select-String "8664 machine")) {
    throw "release-win: $MainExe is not built for the x64 machine type"
}

Write-Host "== no executable files under models\ =="
$ModelsDir = Join-Path $InstallDir "models"
if (Test-Path $ModelsDir) {
    $ExeFiles = @(Get-ChildItem -Path $ModelsDir -Recurse -Include "*.exe", "*.dll" -File)
    if ($ExeFiles) {
        throw "release-win: executable file(s) found under models\: $($ExeFiles -join ', ')"
    }
}

Write-Host "== launch smoke check (unrelated cwd, clean PATH) =="
$SmokeCwd = $env:TEMP
$CleanPath = "$env:SystemRoot\System32;$env:SystemRoot"
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $MainExe
$psi.UseShellExecute = $false
$psi.WorkingDirectory = $SmokeCwd
$psi.EnvironmentVariables["PATH"] = $CleanPath
try {
    $Process = [System.Diagnostics.Process]::Start($psi)
} catch [System.ComponentModel.Win32Exception] {
    if ($_.Exception.NativeErrorCode -eq 4551) {
        throw "release-win: Smart App Control blocked $MainExe; wait ~45 s and rerun"
    }
    throw
}
Start-Sleep -Seconds 5
if ($Process.HasExited) {
    throw "release-win: $MainExe exited immediately during the launch smoke check (exit code $($Process.ExitCode)); over ssh a tray + WebView2 process may exit in the non-interactive session - inspect %LOCALAPPDATA%\kikigaki\logs\ and rerun this gate from a console session"
}

Write-Host "== seed registry values for uninstall-hook verification =="
# Seed both values to exercise the uninstall hook's cleanup of both. The app is not asked to
# enable autostart here; acceptance item A9 covers the app's own autostart write path.
Set-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" `
    -Name "kikigaki" -Value "`"$MainExe`"" -Force
$StartupApprovedKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run"
if (-not (Test-Path $StartupApprovedKey)) {
    New-Item -Path $StartupApprovedKey -Force | Out-Null
}
Set-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" -Name "kikigaki" -Type Binary -Value ([byte[]](2,0,0,0,0,0,0,0,0,0,0,0))
$RunValue = Get-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" `
    -Name "kikigaki" -ErrorAction SilentlyContinue
if (-not $RunValue) {
    throw "release-win: could not seed the Run value for uninstall-hook verification"
}
$ApprovedValue = Get-ItemProperty -Path $StartupApprovedKey `
    -Name "kikigaki" -ErrorAction SilentlyContinue
if (-not $ApprovedValue) {
    throw "release-win: could not seed the StartupApproved\Run value for uninstall-hook verification"
}

Stop-Process -Id $Process.Id -Force -ErrorAction SilentlyContinue

Write-Host "== uninstall + registry verification (M5's NSIS hook) =="
$Uninstaller = Join-Path $InstallDir "Uninstall.exe"
if (-not (Test-Path $Uninstaller)) {
    throw "release-win: no Uninstall.exe found in $InstallDir"
}
$p = Start-Process -FilePath $Uninstaller -ArgumentList "/S", "_?=$InstallDir" -Wait -PassThru
if ($p.ExitCode -ne 0) {
    throw "release-win: silent uninstall failed (exit $($p.ExitCode))"
}
$Uninstalled = $true
if (Test-Path $MainExe) {
    throw "release-win: $MainExe still present after uninstall"
}
$PostUninstallRun = Get-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" `
    -Name "kikigaki" -ErrorAction SilentlyContinue
if ($PostUninstallRun) {
    throw "release-win: uninstall hook left the HKCU Run value for kikigaki"
}
$PostUninstallApproved = Get-ItemProperty `
    -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" `
    -Name "kikigaki" -ErrorAction SilentlyContinue
if ($PostUninstallApproved) {
    throw "release-win: uninstall hook left the StartupApproved\Run value for kikigaki (NSIS_HOOK_POSTUNINSTALL not wired?)"
}
} finally {
    if ($Process -and -not $Process.HasExited) {
        Stop-Process -Id $Process.Id -Force -ErrorAction SilentlyContinue
    }
    Remove-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" `
        -Name "kikigaki" -ErrorAction SilentlyContinue
    Remove-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" `
        -Name "kikigaki" -ErrorAction SilentlyContinue
    $CleanupUninstaller = Join-Path $InstallDir "Uninstall.exe"
    if (-not $Uninstalled -and (Test-Path $CleanupUninstaller)) {
        try {
            Start-Process -FilePath $CleanupUninstaller `
                -ArgumentList "/S", "_?=$InstallDir" -Wait -PassThru | Out-Null
        } catch {
            Write-Warning "release-win: cleanup uninstall failed: $_"
        }
    }
    if ($ManufacturerKeyExisted) {
        $k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("Software\aoki\kikigaki")
        try {
            if ($ManufacturerDefaultExisted) {
                $k.SetValue("", $ManufacturerDefaultValue, $ManufacturerDefaultKind)
            } else {
                $k.DeleteValue("", $false)
            }
        } finally {
            $k.Close()
        }
    } else {
        Remove-Item -Path $ManufacturerKey -Recurse -Force -ErrorAction SilentlyContinue
        $ManufacturerParentKey = "HKCU:\Software\aoki"
        if (Test-Path $ManufacturerParentKey) {
            $ManufacturerParent = Get-Item -Path $ManufacturerParentKey
            if (@(Get-ChildItem -Path $ManufacturerParentKey).Count -eq 0 -and
                $ManufacturerParent.ValueCount -eq 0) {
                Remove-Item -Path $ManufacturerParentKey -Force
            }
        }
    }
    Remove-Item -Recurse -Force $InstallDir -ErrorAction SilentlyContinue
}

Write-Host "== packaging: versioned output directory, refuse to overwrite (M14) =="
$OutDir = Join-Path (Split-Path $Root -Parent) "release-out\windows\$Version"
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$FinalName = "kikigaki-$Version-windows-x64-setup.exe"
$FinalPath = Join-Path $OutDir $FinalName
if (Test-Path $FinalPath) {
    throw "release-win: $FinalPath already exists - bump the version or remove it deliberately before re-releasing"
}
Copy-Item -Path $Installer.FullName -Destination $FinalPath
$Hash = Get-FileHash -Algorithm SHA256 -Path $FinalPath
[IO.File]::WriteAllText("$FinalPath.sha256", "$($Hash.Hash.ToLower())  $FinalName`n")
Write-Host "release-win: $FinalPath ready (versioned output beside target\)"
