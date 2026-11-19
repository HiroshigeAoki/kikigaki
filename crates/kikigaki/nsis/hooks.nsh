; Referenced by tauri.windows.conf.json's bundle.windows.nsis.installerHooks (Task 7).
; NSIS_HOOK_POSTUNINSTALL runs after the generated uninstaller removes the app's own files -
; the generated uninstaller already removes Run on a real uninstall, while this hook adds the
; StartupApproved\Run cleanup (and Run as belt-and-braces), skipped on /UPDATE.
!macro NSIS_HOOK_POSTUNINSTALL
  ; Not on /UPDATE: mirror the generated uninstaller, which preserves the Run value while upgrading.
  ${If} $UpdateMode <> 1
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "kikigaki"
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" "kikigaki"
  ${EndIf}
!macroend
