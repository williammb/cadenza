; Cadenza — NSIS installer hooks
;
; The Tauri installer bundles `cadenza-cli.exe` as a resource (see
; `bundle.resources` in tauri.conf.json). At install time we copy it to
; the install root so it sits next to `cadenza.exe`, and we append the
; install dir to the *user* PATH. This way any shell on the user's
; machine — and any AI agent the user launches from one — can resolve
; `cadenza-cli` without further setup.
;
; PREINSTALL force-closes any running cadenza processes so reinstall
; over the previous version succeeds without "file in use" errors —
; users shouldn't have to quit the tray app or uninstall first.
;
; POSTINSTALL also wipes any leftover `_up_\` tree from older bundles
; where the CLI mistakenly landed under `_up_\target\release\` instead
; of `resources\` (fixed by switching `bundle.resources` from array to
; map). One-shot cleanup; harmless once everyone's past that version.
;
; Uninstall reverses the PATH step. PATH manipulation matches the
; three possible position patterns (";<dir>", "<dir>;", and "<dir>"
; alone) so the registry value comes out clean regardless of where the
; entry landed when other installers also wrote to PATH.

!include "LogicLib.nsh"
!include "WinMessages.nsh"
!include "StrFunc.nsh"

${Using:StrFunc} StrStr
${Using:StrFunc} StrRep
${Using:StrFunc} UnStrRep

!define CADENZA_ENV_KEY 'HKCU "Environment"'

!macro NSIS_HOOK_PREINSTALL
  ; Kill any running Cadenza so the installer can replace cadenza.exe /
  ; cadenza-cli.exe without an in-use error. taskkill exits non-zero
  ; when the process isn't running — discard and continue.
  nsExec::Exec 'taskkill.exe /F /T /IM cadenza.exe'
  Pop $0
  nsExec::Exec 'taskkill.exe /F /T /IM cadenza-cli.exe'
  Pop $0
  Sleep 500 ; let Windows release the file handles
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; One-shot cleanup: older bundles wrote cadenza-cli.exe to
  ; $INSTDIR\_up_\target\release\ due to the array-form resource path.
  ; Wipe that subtree so the flat-layout copy below is the only CLI.
  IfFileExists "$INSTDIR\_up_\*.*" 0 +2
    RMDir /r "$INSTDIR\_up_"

  ; Place cadenza-cli.exe next to cadenza.exe. Resources land under
  ; $INSTDIR\resources\ by Tauri's convention; we want a flat layout
  ; so PATH lookups on $INSTDIR alone catch both binaries.
  IfFileExists "$INSTDIR\resources\cadenza-cli.exe" 0 +2
    CopyFiles /SILENT "$INSTDIR\resources\cadenza-cli.exe" "$INSTDIR\cadenza-cli.exe"

  ReadRegStr $0 ${CADENZA_ENV_KEY} "Path"
  ${StrStr} $1 "$0" "$INSTDIR"
  ${If} $1 == ""
    ${If} $0 == ""
      WriteRegExpandStr ${CADENZA_ENV_KEY} "Path" "$INSTDIR"
    ${Else}
      WriteRegExpandStr ${CADENZA_ENV_KEY} "Path" "$0;$INSTDIR"
    ${EndIf}
    SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ; Drop the copy we made (the bundled resource goes away with $INSTDIR
  ; itself, but the flat-layout copy needs an explicit delete).
  Delete "$INSTDIR\cadenza-cli.exe"

  ReadRegStr $0 ${CADENZA_ENV_KEY} "Path"
  ${UnStrRep} $1 "$0" ";$INSTDIR" ""
  ${UnStrRep} $1 "$1" "$INSTDIR;" ""
  ${UnStrRep} $1 "$1" "$INSTDIR"  ""
  ${If} $0 != $1
    WriteRegExpandStr ${CADENZA_ENV_KEY} "Path" "$1"
    SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000
  ${EndIf}
!macroend
