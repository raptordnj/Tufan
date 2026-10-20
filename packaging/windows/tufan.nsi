; Tufan NSIS installer.
; Build: makensis -DVERSION=0.1.0 -DARCH=x86_64 -DBINDIR=<dir with tufan.exe, tufan-gtk.exe, GTK DLLs> tufan.nsi
Unicode true
!include "MUI2.nsh"
!include "x64.nsh"

!ifndef VERSION
  !define VERSION "0.1.0"
!endif
!ifndef ARCH
  !define ARCH "x86_64"
!endif
!ifndef BINDIR
  !define BINDIR "..\..\target\release"
!endif

Name "Tufan"
OutFile "Tufan-${VERSION}-windows-${ARCH}-setup.exe"
; Per-user by default; no administrator rights needed.
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\Programs\Tufan"
InstallDirRegKey HKCU "Software\Tufan" "InstallDir"
VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "Tufan"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "Tufan installer"
VIAddVersionKey "LegalCopyright" "Tufan developers"

!define MUI_ABORTWARNING
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Function .onInit
  ; Refuse to upgrade over a running instance (jobs could be active).
  FindWindow $0 "" "Tufan"
  StrCmp $0 0 +3
    MessageBox MB_OK|MB_ICONEXCLAMATION "Tufan is running. Close it (and let active jobs finish) before installing."
    Abort
FunctionEnd

Section "Tufan (required)" SecMain
  SectionIn RO
  SetOutPath "$INSTDIR"
  File /r "${BINDIR}\*.*"
  WriteRegStr HKCU "Software\Tufan" "InstallDir" "$INSTDIR"
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Tufan" "DisplayName" "Tufan"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Tufan" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Tufan" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Tufan" "NoModify" 1
  CreateDirectory "$SMPROGRAMS\Tufan"
  CreateShortcut "$SMPROGRAMS\Tufan\Tufan.lnk" "$INSTDIR\tufan-gtk.exe"
SectionEnd

Section /o "Desktop shortcut" SecDesktop
  CreateShortcut "$DESKTOP\Tufan.lnk" "$INSTDIR\tufan-gtk.exe"
SectionEnd

; Off by default: never enable startup behaviour silently.
Section /o "Start Tufan at login" SecStartup
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Tufan" '"$INSTDIR\tufan-gtk.exe"'
SectionEnd

Section "Uninstall"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Tufan"
  Delete "$DESKTOP\Tufan.lnk"
  RMDir /r "$SMPROGRAMS\Tufan"
  ; Only the install directory is removed. The journal and settings in
  ; %LOCALAPPDATA%\Tufan are user data and are kept.
  RMDir /r "$INSTDIR"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Tufan"
  DeleteRegKey HKCU "Software\Tufan"
SectionEnd
