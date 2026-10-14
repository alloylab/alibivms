; Alibi VMS installer. Built by installer.sh, which passes:
;   VERSION (0.1.0), ARCH (x64 | arm64), STAGE (staged install tree), ICON (.ico), LICENSE, OUTFILE
Unicode true
ManifestDPIAware true
RequestExecutionLevel admin
SetCompressor /SOLID lzma

!include MUI2.nsh
!include x64.nsh
!include LogicLib.nsh

!define APP_NAME "Alibi VMS"
!define APP_ID "io.github.alloylab.alibivms"
!define PUBLISHER "Alloy Lab, LLC"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_ID}"

Name "${APP_NAME}"
OutFile "${OUTFILE}"
InstallDir "$PROGRAMFILES64\${APP_NAME}"
InstallDirRegKey HKLM "${UNINSTALL_KEY}" "InstallLocation"
VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APP_NAME}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "CompanyName" "${PUBLISHER}"
VIAddVersionKey "FileDescription" "${APP_NAME} installer"
VIAddVersionKey "LegalCopyright" "${PUBLISHER}"

!define MUI_ICON "${ICON}"
!define MUI_UNICON "${ICON}"
!define MUI_FINISHPAGE_RUN "$INSTDIR\bin\alibivms.exe"
!insertmacro MUI_PAGE_LICENSE "${LICENSE}"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "${APP_NAME} needs 64-bit Windows."
    Abort
  ${EndIf}
  !if "${ARCH}" == "arm64"
    ${IfNot} ${IsNativeARM64}
      MessageBox MB_ICONSTOP "This is the ARM64 build of ${APP_NAME}; download the x64 build for this PC."
      Abort
    ${EndIf}
  !endif
  SetRegView 64
FunctionEnd

Function un.onInit
  SetRegView 64
FunctionEnd

; An update can't replace the files of a running copy. Windows refuses to delete a running exe, so
; deleting it first doubles as the check, before anything else is touched.
!macro WaitForAppToClose UN
  Function ${UN}WaitForAppToClose
    retry:
      ClearErrors
      Delete "$INSTDIR\bin\alibivms.exe"
      IfErrors 0 done
      MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "Alibi VMS is running. Close it, then choose Retry." IDRETRY retry
      Abort
    done:
  FunctionEnd
!macroend
!insertmacro WaitForAppToClose ""
!insertmacro WaitForAppToClose "un."

Section "Install"
  Call WaitForAppToClose
  ; Replace, never merge with, a previous version's DLLs and plugins.
  RMDir /r "$INSTDIR\bin"
  RMDir /r "$INSTDIR\lib"
  RMDir /r "$INSTDIR\share"
  SetOutPath "$INSTDIR"
  File /r "${STAGE}/*"
  File "/oname=alibivms.ico" "${ICON}"
  File "/oname=LICENSE.txt" "${LICENSE}"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateShortCut "$SMPROGRAMS\${APP_NAME}.lnk" "$INSTDIR\bin\alibivms.exe" "" "$INSTDIR\alibivms.ico"

  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\alibivms.ico"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "${UNINSTALL_KEY}" "URLInfoAbout" "https://github.com/alloylab/alibivms"
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoRepair" 1
SectionEnd

Section "Uninstall"
  Call un.WaitForAppToClose
  Delete "$SMPROGRAMS\${APP_NAME}.lnk"
  RMDir /r "$INSTDIR\bin"
  RMDir /r "$INSTDIR\lib"
  RMDir /r "$INSTDIR\share"
  Delete "$INSTDIR\alibivms.ico"
  Delete "$INSTDIR\LICENSE.txt"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKLM "${UNINSTALL_KEY}"
SectionEnd
