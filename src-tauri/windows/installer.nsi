; Pathfinder Windows NSIS installer (replaces the Tauri bundler).
; Build (from src-tauri after `cargo build --release`):
;   makensis /DVERSION=1.0.14 windows/installer.nsi
;
; Defines (optional overrides):
;   VERSION          - product version (required)
;   OUTFILE          - installer path
;   MAINBINARYPATH   - path to pathfinder.exe
;   PDFIUM_PATH      - path to pdfium.dll

!ifndef VERSION
  !error "Pass /DVERSION=x.y.z"
!endif

!ifndef MAINBINARYPATH
  !define MAINBINARYPATH "target\release\pathfinder.exe"
!endif
!ifndef PDFIUM_PATH
  !define PDFIUM_PATH "pdfium\pdfium.dll"
!endif
!ifndef OUTFILE
  !define OUTFILE "target\release\bundle\nsis\Pathfinder_${VERSION}_x64-setup.exe"
!endif

!define PRODUCTNAME "Pathfinder"
!define MAINBINARYNAME "pathfinder"
!define MANUFACTURER "Pathfinder"
!define BUNDLEID "com.fusntuff.pathfinder"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCTNAME}"
!define MANUPRODUCTKEY "Software\${MANUFACTURER}\${PRODUCTNAME}"

Unicode true
ManifestDPIAware true
SetCompressor /SOLID lzma
Name "${PRODUCTNAME}"
BrandingText "${PRODUCTNAME} ${VERSION}"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\${PRODUCTNAME}"
InstallDirRegKey HKCU "${MANUPRODUCTKEY}" ""
RequestExecutionLevel user
Icon "icons\icon.ico"
UninstallIcon "icons\icon.ico"

!include MUI2.nsh
!include FileFunc.nsh

!define MUI_ICON "icons\icon.ico"
!define MUI_UNICON "icons\icon.ico"
!define MUI_ABORTWARNING

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN "$INSTDIR\${MAINBINARYNAME}.exe"
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

Section "Install"
  SetOutPath $INSTDIR
  File "/oname=${MAINBINARYNAME}.exe" "${MAINBINARYPATH}"
  File "/oname=pdfium.dll" "${PDFIUM_PATH}"

  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateDirectory "$SMPROGRAMS\${PRODUCTNAME}"
  CreateShortCut "$SMPROGRAMS\${PRODUCTNAME}\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
  CreateShortCut "$SMPROGRAMS\${PRODUCTNAME}\Uninstall.lnk" "$INSTDIR\uninstall.exe"
  CreateShortCut "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"

  WriteRegStr HKCU "${MANUPRODUCTKEY}" "" $INSTDIR
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayName" "${PRODUCTNAME}"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayIcon" "$INSTDIR\${MAINBINARYNAME}.exe"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTKEY}" "Publisher" "${MANUFACTURER}"
  WriteRegStr HKCU "${UNINSTKEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTKEY}" "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINSTKEY}" "EstimatedSize" "$0"
SectionEnd

Section "Uninstall"
  ; Former Tauri NSIS_HOOK_PREUNINSTALL — clear optional default-folder handler.
  DetailPrint "Removing Pathfinder folder handler overrides (HKCU)..."
  nsExec::ExecToLog '"$INSTDIR\${MAINBINARYNAME}.exe" --uninstall-shell-handler'
  Pop $0

  Delete "$INSTDIR\${MAINBINARYNAME}.exe"
  Delete "$INSTDIR\pdfium.dll"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\${PRODUCTNAME}\${PRODUCTNAME}.lnk"
  Delete "$SMPROGRAMS\${PRODUCTNAME}\Uninstall.lnk"
  RMDir "$SMPROGRAMS\${PRODUCTNAME}"
  Delete "$DESKTOP\${PRODUCTNAME}.lnk"

  DeleteRegKey HKCU "${UNINSTKEY}"
  DeleteRegKey HKCU "${MANUPRODUCTKEY}"
SectionEnd
