Unicode true
Name "Screen Translator"
OutFile "ScreenTranslator-Setup.exe"
InstallDir "$PROGRAMFILES64\ScreenTranslator"
RequestExecutionLevel admin

Section "Main" SEC_MAIN
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
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\ScreenTranslator" \
    "DisplayName" "Screen Translator"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\ScreenTranslator" \
    "UninstallString" '"$INSTDIR\uninstall.exe"'
SectionEnd

Section /o "开机自启" SEC_AUTOSTART
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Run" \
    "ScreenTranslator" '"$INSTDIR\ScreenTranslator.exe"'
SectionEnd

Section "Uninstall"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "ScreenTranslator"
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\ScreenTranslator"
  Delete "$SMPROGRAMS\Screen Translator.lnk"
  RMDir /r "$INSTDIR"
SectionEnd
