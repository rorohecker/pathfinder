; Pathfinder NSIS hooks — register per-user shell integration after install.
; Uses the same Rust registry code as Settings → "Set as default folder handler".

!macro NSIS_HOOK_POSTINSTALL
  DetailPrint "Registering Pathfinder as default folder handler (HKCU)..."
  ; /CURRENTUSER matches Tauri's default per-user install mode.
  nsExec::ExecToLog '"$INSTDIR\${MAINBINARYNAME}.exe" --install-shell-handler'
  Pop $0
  ${If} $0 != 0
    MessageBox MB_ICONEXCLAMATION|MB_OK \
      "Pathfinder installed, but default folder handler registration failed (code $0).$\n$\nYou can register later from Settings → Windows → Set as default."
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  DetailPrint "Removing Pathfinder folder handler overrides (HKCU)..."
  nsExec::ExecToLog '"$INSTDIR\${MAINBINARYNAME}.exe" --uninstall-shell-handler'
  Pop $0
!macroend
