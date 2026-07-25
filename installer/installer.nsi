Unicode true
Name "Screen Translator"
OutFile "ScreenTranslator-Setup.exe"
; 默认装到用户目录：免提权，且每日自动更新（原地替换文件）不需要管理员权限。
; InstallDirRegKey 记住用户上次选择的目录，覆盖安装时沿用。
InstallDir "$LOCALAPPDATA\Programs\ScreenTranslator"
InstallDirRegKey HKCU "Software\ScreenTranslator" "InstallDir"
RequestExecutionLevel user

!include "MUI2.nsh"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "SimpChinese"

Section "主程序（必选）" SEC_MAIN
  SectionIn RO
  SetOutPath "$INSTDIR"
  File /oname=ScreenTranslator.exe "..\target\release\screen-translator.exe"
  File "..\onnxruntime.dll"
  File "..\README.md"
  File "..\LICENSE"
  File /oname=licenses.md "..\docs\licenses.md"
  File /oname=privacy.md "..\docs\privacy.md"
  SetOutPath "$INSTDIR\assets"
  File /r "..\assets\*"
  SetOutPath "$INSTDIR"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  CreateShortCut "$SMPROGRAMS\Screen Translator.lnk" "$INSTDIR\ScreenTranslator.exe"
  WriteRegStr HKCU "Software\ScreenTranslator" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ScreenTranslator" \
    "DisplayName" "Screen Translator"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ScreenTranslator" \
    "UninstallString" '"$INSTDIR\uninstall.exe"'
SectionEnd

Section /o "桌面快捷方式" SEC_DESKTOP
  CreateShortCut "$DESKTOP\Screen Translator.lnk" "$INSTDIR\ScreenTranslator.exe"
SectionEnd

Section /o "开机自启" SEC_AUTOSTART
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Run" \
    "ScreenTranslator" '"$INSTDIR\ScreenTranslator.exe"'
SectionEnd

Section "Uninstall"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "ScreenTranslator"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ScreenTranslator"
  DeleteRegKey HKCU "Software\ScreenTranslator"
  Delete "$SMPROGRAMS\Screen Translator.lnk"
  Delete "$DESKTOP\Screen Translator.lnk"
  RMDir /r "$INSTDIR"
  ; 配置与历史（%APPDATA%\ScreenTranslator，含加密的 API Key）默认保留，
  ; 由用户选择是否一并清除。
  MessageBox MB_YESNO "是否同时删除配置与翻译历史（%APPDATA%\ScreenTranslator）？" IDNO done
  RMDir /r "$APPDATA\ScreenTranslator"
done:
SectionEnd
