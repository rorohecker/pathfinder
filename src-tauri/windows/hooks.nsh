; Pathfinder NSIS hooks.
;
; Auto-registration of the default folder handler was removed in v0.8.7.
; The previous postinstall step invoked `--install-shell-handler`, which
; writes 7 HKCU keys under Software\Classes\Folder\shell\open\command and
; the App Paths\explorer.exe redirect. From an unsigned binary, that exact
; pattern triggers Windows Defender's "Trojan:Win32/Bearfoos.A!ml" heuristic
; even though the writes are entirely legitimate and per-user only.
;
; Users who want Pathfinder as their default folder handler can opt in any
; time via Settings -> Windows -> "Set as default folder handler." The CLI
; flags --install-shell-handler / --uninstall-shell-handler still exist for
; scripted deployments and remain unchanged.
;
; Uninstall still attempts to clean up any HKCU keys the user may have set
; via Settings, so an uninstall fully reverts the system state.

!macro NSIS_HOOK_POSTINSTALL
  ; Intentionally empty. See header comment above.
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  DetailPrint "Removing Pathfinder folder handler overrides (HKCU)..."
  nsExec::ExecToLog '"$INSTDIR\${MAINBINARYNAME}.exe" --uninstall-shell-handler'
  Pop $0
!macroend
