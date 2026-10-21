; Tufan NSIS installer.
; Build: makensis -DVERSION=0.2.1 -DARCH=x86_64 -DBINDIR=<dir with tufan.exe, tufan-gtk.exe, GTK DLLs> tufan.nsi
Unicode true
!include "MUI2.nsh"
!include "x64.nsh"

!ifndef VERSION
  !define VERSION "0.2.1"
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
!define MUI_ICON "tufan.ico"
!define MUI_UNICON "tufan.ico"
!define MUI_FINISHPAGE_RUN "$INSTDIR\tufan-gtk.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Launch Tufan"
!insertmacro MUI_PAGE_LICENSE "..\..\LICENSE"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Function .onInit
  ; Refuse to upgrade over a running instance (jobs could be active).
  ; The tray window class exists while Tufan runs, even when hidden.
  FindWindow $0 "TufanTray"
  StrCmp $0 0 +3
    MessageBox MB_OK|MB_ICONEXCLAMATION "Tufan is running. Let active jobs finish, then choose Exit from its tray icon before installing."
    Abort
FunctionEnd

Function un.onInit
  FindWindow $0 "TufanTray"
  StrCmp $0 0 +3
    MessageBox MB_OK|MB_ICONEXCLAMATION "Tufan is running. Choose Exit from its tray icon, then uninstall."
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

; Takes over Ctrl+V / Ctrl+X paste in Explorer; also starts Tufan hidden
; in the tray at login, since the takeover only works while it runs.
; Both can be switched off later from the tray menu.
Section "Use Tufan for Explorer copy/paste (starts with Windows)" SecExplorer
  WriteRegDWORD HKCU "Software\Tufan" "ExplorerPaste" 1
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Tufan" '"$INSTDIR\tufan-gtk.exe" --background'
SectionEnd

; Off by default: never enable startup behaviour silently.
Section /o "Start Tufan at login" SecStartup
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Tufan" '"$INSTDIR\tufan-gtk.exe" --background'
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
