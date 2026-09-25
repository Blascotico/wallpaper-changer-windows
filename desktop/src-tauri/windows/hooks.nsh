; Installer hooks, spliced into Tauri's NSIS template (bundle.windows.nsis.installerHooks).
;
; KokoroPaper was called "Wallpaper Changer" until 6.0.0. The template keys the install
; folder, the Add/Remove Programs entry, the Start menu and desktop shortcuts and the
; autostart value on the product name, so the renamed build would otherwise install
; *beside* the old one: two copies, both starting with Windows, both claiming the same
; hotkeys and fighting over the wallpaper.
;
; So the old install is retired on the way in, by its own uninstaller. That uninstaller
; removes its folder, shortcuts, uninstall entry and autostart value, and - run
; silently - never touches app data; the settings in %APPDATA%\WallpaperChanger and the
; WebView2 profile under the bundle identifier are both unchanged by the rename anyway.
;
; What it takes away has to be put back under the new name: the autostart value always,
; and the shortcuts when this runs as an in-app update, because the template skips
; creating shortcuts in update mode on the assumption that an earlier install of the
; *same* name already made them.

!define OLD_PRODUCTNAME "Wallpaper Changer"
!define OLD_UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${OLD_PRODUCTNAME}"
!define AUTOSTART_KEY "Software\Microsoft\Windows\CurrentVersion\Run"

Var OldInstallDir
Var OldHadAutostart
Var OldHadStartMenuShortcut
Var OldHadDesktopShortcut

!macro NSIS_HOOK_PREINSTALL
  StrCpy $OldInstallDir ""
  StrCpy $OldHadAutostart 0
  StrCpy $OldHadStartMenuShortcut 0
  StrCpy $OldHadDesktopShortcut 0

  ; Written quoted by the template: "C:\Users\...\Wallpaper Changer"
  ReadRegStr $R9 HKCU "${OLD_UNINSTKEY}" "InstallLocation"
  ${If} $R9 != ""
    nsis_tauri_utils::StrReplace "$R9" '"' ""
    Pop $OldInstallDir
  ${EndIf}

  ${If} $OldInstallDir != ""
  ${AndIf} ${FileExists} "$OldInstallDir\uninstall.exe"
    ; Taken before the uninstaller deletes them.
    ReadRegStr $R9 HKCU "${AUTOSTART_KEY}" "${OLD_PRODUCTNAME}"
    ${If} $R9 != ""
      StrCpy $OldHadAutostart 1
    ${EndIf}
    ${If} ${FileExists} "$SMPROGRAMS\${OLD_PRODUCTNAME}.lnk"
      StrCpy $OldHadStartMenuShortcut 1
    ${EndIf}
    ${If} ${FileExists} "$DESKTOP\${OLD_PRODUCTNAME}.lnk"
      StrCpy $OldHadDesktopShortcut 1
    ${EndIf}

    DetailPrint "Removing ${OLD_PRODUCTNAME}, which ${PRODUCTNAME} replaces"
    ; `_?=` runs the uninstaller in place so ExecWait really waits for it; without it
    ; the uninstaller copies itself to %TEMP% and returns at once. The price is that
    ; it cannot delete itself or its folder, so that is done here.
    ExecWait '"$OldInstallDir\uninstall.exe" /S _?=$OldInstallDir' $R9
    DetailPrint "${OLD_PRODUCTNAME} uninstaller exited with $R9"
    Delete "$OldInstallDir\uninstall.exe"
    RMDir "$OldInstallDir"

    ; Someone who pointed this install at the old folder has just had it removed from
    ; under the template, which is about to copy files into it.
    SetOutPath $INSTDIR
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ${If} $OldInstallDir != ""
    ; The app normalises this value itself on its next launch (sync_autostart in
    ; lib.rs); it only has to exist, under the new name, for that to happen.
    ${If} $OldHadAutostart = 1
      WriteRegStr HKCU "${AUTOSTART_KEY}" "${PRODUCTNAME}" '"$INSTDIR\${MAINBINARYNAME}.exe" --minimized'
    ${EndIf}

    ${If} $UpdateMode = 1
      ${If} $OldHadStartMenuShortcut = 1
      ${AndIfNot} ${FileExists} "$SMPROGRAMS\${PRODUCTNAME}.lnk"
        CreateShortcut "$SMPROGRAMS\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
        !insertmacro SetLnkAppUserModelId "$SMPROGRAMS\${PRODUCTNAME}.lnk"
      ${EndIf}
      ${If} $OldHadDesktopShortcut = 1
      ${AndIfNot} ${FileExists} "$DESKTOP\${PRODUCTNAME}.lnk"
        CreateShortcut "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
        !insertmacro SetLnkAppUserModelId "$DESKTOP\${PRODUCTNAME}.lnk"
      ${EndIf}
    ${EndIf}
  ${EndIf}
!macroend
