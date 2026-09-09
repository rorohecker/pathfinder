; Pathfinder NSIS hook notes (legacy Tauri hooks file).
;
; The installer now lives in windows/installer.nsi. Uninstall still runs
; `"$INSTDIR\pathfinder.exe" --uninstall-shell-handler` before deleting files
; so HKCU default-folder-handler overrides are cleared.
;
; Auto-registration of the default folder handler was removed in v0.8.7 —
; unsigned postinstall registry writes tripped Defender Bearfoos heuristics.
; Users opt in via Settings -> Windows -> "Set as default folder handler."

!macro NSIS_HOOK_POSTINSTALL
  ; Intentionally empty. See header comment above.
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  DetailPrint "Removing Pathfinder folder handler overrides (HKCU)..."
  nsExec::ExecToLog '"$INSTDIR\${MAINBINARYNAME}.exe" --uninstall-shell-handler'
  Pop $0
!macroend
