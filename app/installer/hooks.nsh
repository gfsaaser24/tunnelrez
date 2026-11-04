; TunnelRez installer hooks for the Tauri NSIS bundle (per user, no admin).
; PINNED: see docs/PINNED.md (written against the Tauri bundler 2.10 NSIS template: the hook macros, MUI's
; GUI-init hook free for us, $UpdateMode, the uninstaller relaunch).
;
; This file is included near the top of the template, before PRODUCTNAME and $UpdateMode exist: the functions here
; spell the name out and read /UPDATE themselves. The hook macros are expanded later, where both exist.
;
; Install:
;   - goes to %LOCALAPPDATA%\Programs\TunnelRez (Tauri's default %LOCALAPPDATA%\TunnelRez is the sender's data folder);
;   - replaces the old SlipStream kit (%LOCALAPPDATA%\Programs\SlipStream and its Start menu shortcut), after asking a
;     running old app to quit (it unplugs its TV monitor and puts the sound back itself);
;   - moves "Start with Windows" from the old SlipStream value to TunnelRez.
; Uninstall (not during an upgrade):
;   - quits the app, unplugs every TV monitor, puts the default sound devices back;
;   - removes the TV speaker only if TunnelRez (or SlipStream) installed it (one UAC prompt then);
;   - removes the Parsec Virtual Display Driver only when the box on the first page is ticked (or /REMOVEPARSEC).

!include nsDialogs.nsh
!include LogicLib.nsh
!include FileFunc.nsh

!define TR_OLD_DIR "$LOCALAPPDATA\Programs\SlipStream"
!define TR_RUN_KEY "Software\Microsoft\Windows\CurrentVersion\Run"
!define TR_PARSEC_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\ParsecVDD"
; Set by this installer for the old uninstaller it starts: an upgrade, so keep the drivers.
!define TR_UPGRADE_ENV "TUNNELREZ_INSTALLER_UPGRADE"

Var TrRemoveParsec
Var TrRemoveParsecBox
; The TunnelRez Run value, kept across an upgrade (Tauri's uninstall section deletes it unless /UPDATE is given).
Var TrRunValue

; Whether a process with this image name runs: $R9 = 1 or 0.
!macro TR_RUNNING exe
  nsExec::ExecToStack 'cmd.exe /c tasklist /NH /FI "IMAGENAME eq ${exe}" | find /I "${exe}"'
  Pop $R9
  Pop $R8
  ${If} $R9 == 0
    StrCpy $R9 1
  ${Else}
    StrCpy $R9 0
  ${EndIf}
!macroend

; Ask a running copy of the app to quit (it tears down cleanly), and wait up to 15 s for it to go. Tauri's own check
; that follows kills it only when it is still there.
!macro TR_QUIT path exe
  !insertmacro TR_RUNNING "${exe}"
  ${If} $R9 = 1
  ${AndIf} ${FileExists} "${path}"
    DetailPrint "Asking ${exe} to quit"
    ExecWait '"${path}" --quit'
    StrCpy $R7 0
    ${Do}
      Sleep 500
      IntOp $R7 $R7 + 1
      !insertmacro TR_RUNNING "${exe}"
      ${If} $R9 = 0
        ${ExitDo}
      ${EndIf}
    ${LoopUntil} $R7 >= 30
  ${EndIf}
!macroend

; ------------------------------------------------------------------------------------------------ install

; MUI owns .onGUIInit and calls this from it.
!define MUI_CUSTOMFUNCTION_GUIINIT TrGuiInit
Function TrGuiInit
  ; The directory page shows the real default.
  ${If} $INSTDIR == "$LOCALAPPDATA\TunnelRez"
    StrCpy $INSTDIR "$LOCALAPPDATA\Programs\TunnelRez"
  ${EndIf}
  ; An older TunnelRez that the reinstall page uninstalls first is being upgraded, not removed.
  System::Call 'kernel32::SetEnvironmentVariable(t "${TR_UPGRADE_ENV}", t "1")'
FunctionEnd

!macro NSIS_HOOK_PREINSTALL
  ; Silent installs skip .onGUIInit.
  ${If} $INSTDIR == "$LOCALAPPDATA\${PRODUCTNAME}"
    StrCpy $INSTDIR "$LOCALAPPDATA\Programs\${PRODUCTNAME}"
    SetOutPath $INSTDIR
  ${EndIf}
  System::Call 'kernel32::SetEnvironmentVariable(t "${TR_UPGRADE_ENV}", t "1")'

  ; A running TunnelRez: let it stop the stream and unplug its monitor before its files are replaced.
  !insertmacro TR_QUIT "$INSTDIR\${MAINBINARYNAME}.exe" "${MAINBINARYNAME}.exe"

  ; The SlipStream kit (tools\kit\install.ps1) lived in %LOCALAPPDATA%\Programs\SlipStream.
  ${If} ${FileExists} "${TR_OLD_DIR}\slipstream.exe"
    DetailPrint "Replacing the SlipStream kit in ${TR_OLD_DIR}"
    !insertmacro TR_QUIT "${TR_OLD_DIR}\slipstream.exe" "slipstream.exe"
    !insertmacro TR_RUNNING "slipstream.exe"
    ${If} $R9 = 1
      MessageBox MB_OK|MB_ICONEXCLAMATION "SlipStream is still running. Quit it from its tray icon, then press OK." /SD IDOK
    ${EndIf}
    RMDir /r /REBOOTOK "${TR_OLD_DIR}"
  ${EndIf}
  Delete "$SMPROGRAMS\SlipStream\SlipStream.lnk"
  RMDir "$SMPROGRAMS\SlipStream"

  ; "Start with Windows" from the old app now starts TunnelRez.
  ReadRegStr $R9 HKCU "${TR_RUN_KEY}" "SlipStream"
  ${If} $R9 != ""
    DeleteRegValue HKCU "${TR_RUN_KEY}" "SlipStream"
    WriteRegStr HKCU "${TR_RUN_KEY}" "${PRODUCTNAME}" '"$INSTDIR\${MAINBINARYNAME}.exe" --autostart'
  ${EndIf}
  ; An enabled "Start with Windows" (kept by an upgrade, or set before) points at this install folder, which the
  ; directory page may have changed. Disabled (no value) stays disabled.
  ReadRegStr $R9 HKCU "${TR_RUN_KEY}" "${PRODUCTNAME}"
  ${If} $R9 != ""
    WriteRegStr HKCU "${TR_RUN_KEY}" "${PRODUCTNAME}" '"$INSTDIR\${MAINBINARYNAME}.exe" --autostart'
  ${EndIf}
!macroend

; ------------------------------------------------------------------------------------------------ uninstall

; The uninstall is real (not the first half of an upgrade): $R9 = 1.
!macro TR_REAL_UNINSTALL
  ReadEnvStr $R9 "${TR_UPGRADE_ENV}"
  ${If} $UpdateMode = 1
  ${OrIf} $R9 == "1"
    StrCpy $R9 0
  ${Else}
    StrCpy $R9 1
  ${EndIf}
!macroend

; First uninstaller page: what will happen, and the Parsec box (unticked).
UninstPage custom un.TrOptionsShow un.TrOptionsLeave

Function un.TrOptionsShow
  StrCpy $TrRemoveParsec 0
  ; An upgrade (this installer's flag, or the updater's /UPDATE) shows no page and keeps the drivers.
  ReadEnvStr $R9 "${TR_UPGRADE_ENV}"
  ${If} $R9 == "1"
    Abort
  ${EndIf}
  ClearErrors
  ${GetOptions} $CMDLINE "/UPDATE" $R9
  ${IfNot} ${Errors}
    Abort
  ${EndIf}
  !insertmacro MUI_HEADER_TEXT "Uninstall TunnelRez" "What is removed"
  nsDialogs::Create 1018
  Pop $R8
  ${If} $R8 == error
    Abort
  ${EndIf}
  ${NSD_CreateLabel} 0 0 100% 48u "The TV monitor is unplugged and your sound devices are put back. The TV speaker is removed only if TunnelRez installed it (Windows asks once)."
  Pop $R8
  ${NSD_CreateCheckbox} 0 56u 100% 12u "Also remove the Parsec Virtual Display Driver (the TV monitor driver)"
  Pop $TrRemoveParsecBox
  ${NSD_CreateLabel} 12u 70u 100% 24u "Leave it unticked when Parsec or another app uses it. Windows asks once."
  Pop $R8
  nsDialogs::Show
FunctionEnd

Function un.TrOptionsLeave
  ${NSD_GetState} $TrRemoveParsecBox $TrRemoveParsec
  ${If} $TrRemoveParsec == ${BST_CHECKED}
    StrCpy $TrRemoveParsec 1
  ${Else}
    StrCpy $TrRemoveParsec 0
  ${EndIf}
FunctionEnd

!macro TR_FORGET_GPU_PREF exe
  DeleteRegValue HKCU "Software\Microsoft\DirectX\UserGpuPreferences" "$INSTDIR\${exe}"
!macroend

; The sender's GPU-preference hardlinks beside it (sender/src/tools.rs), made at run time, so Tauri does not know
; them: without this the install folder stays behind. They come back at the next start after an upgrade.
!macro NSIS_HOOK_POSTUNINSTALL
  ; "Delete the application data" (Tauri's box) means our folders too: Tauri's own cleanup only knows
  ; %APPDATA%\dev.tunnelrez.app. The SlipStream folders from before the rename go as well: left in place, a later
  ; install would copy them again and bring the deleted settings back. Not during an upgrade.
  !insertmacro TR_REAL_UNINSTALL
  ${If} $R9 = 1
  ${AndIf} $DeleteAppDataCheckboxState = 1
    SetShellVarContext current
    RMDir /r "$APPDATA\TunnelRez"
    RMDir /r "$LOCALAPPDATA\TunnelRez"
    RMDir /r "$APPDATA\SlipStream"
    RMDir /r "$LOCALAPPDATA\SlipStream"
  ${EndIf}
  ; An upgrade through the reinstall page: Tauri just removed "Start with Windows"; put it back. A real uninstall
  ; leaves it removed (Tauri deletes the ${PRODUCTNAME} value then).
  ${If} $TrRunValue != ""
    WriteRegStr HKCU "${TR_RUN_KEY}" "${PRODUCTNAME}" $TrRunValue
  ${EndIf}
  Delete "$INSTDIR\tunnelrez-probe-igpu.exe"
  Delete "$INSTDIR\tunnelrez-probe-dgpu.exe"
  Delete "$INSTDIR\ffmpeg-igpu.exe"
  Delete "$INSTDIR\ffmpeg-dgpu.exe"
  RMDir "$INSTDIR\drivers\steam-streaming-speakers\x64"
  RMDir "$INSTDIR\drivers\steam-streaming-speakers"
  RMDir "$INSTDIR\drivers\parsec-vdd"
  RMDir "$INSTDIR\drivers"
  RMDir "$INSTDIR"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro TR_REAL_UNINSTALL
  StrCpy $TrRunValue ""
  ${If} $R9 = 0
    ReadRegStr $TrRunValue HKCU "${TR_RUN_KEY}" "${PRODUCTNAME}"
  ${EndIf}
  ${If} $R9 = 1
    ; Silent uninstall: /REMOVEPARSEC stands for the box.
    ClearErrors
    ${GetOptions} $CMDLINE "/REMOVEPARSEC" $R8
    ${IfNot} ${Errors}
      StrCpy $TrRemoveParsec 1
    ${EndIf}

    !insertmacro TR_QUIT "$INSTDIR\${MAINBINARYNAME}.exe" "${MAINBINARYNAME}.exe"
    ${If} ${FileExists} "$INSTDIR\tunnelrez-send.exe"
      DetailPrint "Unplugging the TV monitor"
      nsExec::ExecToLog '"$INSTDIR\tunnelrez-send.exe" vdd unplug'
      Pop $R8
      DetailPrint "Putting the default sound devices back"
      nsExec::ExecToLog '"$INSTDIR\tunnelrez-send.exe" audio-restore'
      Pop $R8
      ; Refuses (no prompt) when the TV speaker was not installed by us: Steam may use it.
      DetailPrint "Removing the TV speaker if TunnelRez installed it"
      nsExec::ExecToLog '"$INSTDIR\tunnelrez-send.exe" audio-sink uninstall'
      Pop $R8
    ${EndIf}

    ; The per-exe GPU preferences the sender set for its helper links.
    !insertmacro TR_FORGET_GPU_PREF "tunnelrez-probe-igpu.exe"
    !insertmacro TR_FORGET_GPU_PREF "tunnelrez-probe-dgpu.exe"
    !insertmacro TR_FORGET_GPU_PREF "ffmpeg-igpu.exe"
    !insertmacro TR_FORGET_GPU_PREF "ffmpeg-dgpu.exe"

    ${If} $TrRemoveParsec = 1
      SetRegView 64
      ReadRegStr $R8 HKLM "${TR_PARSEC_KEY}" "UninstallString"
      SetRegView default
      ${If} $R8 != ""
        DetailPrint "Removing the Parsec Virtual Display Driver"
        ; UninstallString is quoted; ExecShell wants the bare path.
        StrCpy $R7 $R8 1
        ${If} $R7 == '"'
          StrCpy $R8 $R8 "" 1
          StrCpy $R8 $R8 -1
        ${EndIf}
        ExecShell "runas" "$R8" "/S"
      ${EndIf}
    ${EndIf}
  ${EndIf}
!macroend
