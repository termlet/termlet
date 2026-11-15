; Termlet 安装程序（NSIS 3，Unicode）。作者：ruan
;
; 由 scripts/release.mjs 构建（它先准备好暂存目录，再调用 makensis）：
;   makensis /INPUTCHARSET UTF8 /DVERSION=1.0.0 /DVERSION_NUMERIC=1.0.0.0 /DARCH=x64 /DSTAGE=<暂存目录> /DOUTFILE=<安装包> installer.nsi
; ARCH 是暂存目录中程序的架构：x64 或 arm64（不给时为 x64）。两种安装包只有安装前的检查不同（安装程序本身都是
; 32 位的 NSIS，arm64 的 Windows 经仿真运行它），装到同一个位置、写同样的注册表项，可以互相覆盖升级。
; 暂存目录中：Termlet.exe、bin\termlet.exe、licenses\、license_zh_CN.txt / license_zh_TW.txt / license_en.txt（带 BOM、CRLF）、
; helper.ps1（带 BOM）。
;
; 安装向导的行为：
; - 许可 → 选择安装位置（默认 %LOCALAPPDATA%\Programs\Termlet，升级时沿用原来的位置）→ 安装 → 完成（可勾选运行 Termlet）；
; - 向导按系统语言显示简体中文、繁體中文、English、日本語或한국어，许可说明有简体中文、繁體中文与英文三版（后面都接
;   AGPL-3.0 的英文全文，见 release.mjs 的 writeNotices）：日文、韩文系统上显示英文版（不做机器翻译），许可页的上方写明这一点；
; - 只为当前用户安装，不需要管理员权限；需要 64 位的 Windows 10 1809（内部版本 17763）或更高版本，arm64 的安装包
;   只能装在 arm64 的 Windows 上（x64 的安装包在 arm64 的 Windows 11 上经仿真也能用）；
; - 安装目录根部放 Termlet.exe，命令行工具放 bin\termlet.exe（文件名不分大小写，二者不能放在同一目录）；
; - 开始菜单与桌面快捷方式；自动更新时（--updated）只刷新已有的，用户删掉的不再创建；
; - 自动更新：Termlet 下载并核对安装包后以 /S --updated [--force-run] /D=<安装目录> 静默运行，--force-run 时装完启动 Termlet；
; - 安装、卸载前处理运行中的 Termlet（helper.ps1）：只剩后台实例（没有窗口，例如为命令行工具服务的）或命令行工具时
;   直接结束；有窗口时询问（静默运行时等它退出，最多 30 秒，之后结束）；
; - 卸载：从当前用户的 PATH 中移除 bin、删除快捷方式与更新缓存；用户数据目录（%APPDATA%\termlet）保留。
;
; 在 Git Bash 中运行安装程序时要设 MSYS_NO_PATHCONV=1，否则 /S 会被转换成路径。

Unicode true
ManifestDPIAware true
RequestExecutionLevel user
SetCompressor /SOLID lzma
SetCompressorDictSize 32

!ifndef VERSION | VERSION_NUMERIC | STAGE | OUTFILE
  !error "用 /DVERSION=… /DVERSION_NUMERIC=… /DSTAGE=… /DOUTFILE=… 指定版本、暂存目录与输出文件（见 scripts/release.mjs）"
!endif
!ifndef ARCH
  !define ARCH x64
!endif
!if "${ARCH}" != "x64"
!if "${ARCH}" != "arm64"
  !error "ARCH 应为 x64 或 arm64：${ARCH}"
!endif
!endif

!define PRODUCT "Termlet"
!define PUBLISHER "ruan"
!define APP_KEY "Software\Termlet"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Termlet"
!define UNINSTALLER "Uninstall Termlet.exe"
!define ICON "..\..\crates\termlet-desktop\resources\windows\termlet.ico"
!define POWERSHELL "$SYSDIR\WindowsPowerShell\v1.0\powershell.exe"

Name "${PRODUCT}"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\Termlet"
; 升级、重装时沿用原来的位置（自动更新另用 /D= 指定为正在运行的程序所在的目录）
InstallDirRegKey HKCU "${APP_KEY}" "InstallLocation"
BrandingText "${PRODUCT} ${VERSION}"
ShowInstDetails hide
ShowUninstDetails hide

!include MUI2.nsh
!include LogicLib.nsh
!include FileFunc.nsh
!include WinMessages.nsh
!include x64.nsh

Var Updated
Var ForceRun

; ---------------------------------------------------------------- 页面

!define MUI_ICON "${ICON}"
!define MUI_UNICON "${ICON}"
; 完成页左侧的图（164×314 的 24 位 BMP，由 packaging/render-icons.mjs 渲染）；卸载程序沿用同一张
!define MUI_WELCOMEFINISHPAGE_BITMAP "installer-sidebar.bmp"
!define MUI_UNWELCOMEFINISHPAGE_BITMAP "installer-sidebar.bmp"
!define MUI_ABORTWARNING

; 许可页上方的说明：日文、韩文写明许可说明是英文版，其余语言用 NSIS 自带的（按 Page Down 阅读）
!define MUI_LICENSEPAGE_TEXT_TOP "$(TermletLicenseTop)"
!insertmacro MUI_PAGE_LICENSE "$(TermletLicense)"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_FUNCTION RunTermlet
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_UNPAGE_FINISH

; 按系统语言选择；都不是时用第一个（英文）。香港、澳门、新加坡的中文系统见 PICK_LANGUAGE
!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "SimpChinese"
!insertmacro MUI_LANGUAGE "TradChinese"
!insertmacro MUI_LANGUAGE "Japanese"
!insertmacro MUI_LANGUAGE "Korean"

; 日文、韩文显示英文版的许可说明（没有日文、韩文版，不做机器翻译）
LicenseLangString TermletLicense ${LANG_ENGLISH} "${STAGE}\license_en.txt"
LicenseLangString TermletLicense ${LANG_SIMPCHINESE} "${STAGE}\license_zh_CN.txt"
LicenseLangString TermletLicense ${LANG_TRADCHINESE} "${STAGE}\license_zh_TW.txt"
LicenseLangString TermletLicense ${LANG_JAPANESE} "${STAGE}\license_en.txt"
LicenseLangString TermletLicense ${LANG_KOREAN} "${STAGE}\license_en.txt"

LangString TermletLicenseTop ${LANG_ENGLISH} "$(MUI_INNERTEXT_LICENSE_TOP)"
LangString TermletLicenseTop ${LANG_SIMPCHINESE} "$(MUI_INNERTEXT_LICENSE_TOP)"
LangString TermletLicenseTop ${LANG_TRADCHINESE} "$(MUI_INNERTEXT_LICENSE_TOP)"
LangString TermletLicenseTop ${LANG_JAPANESE} "使用許諾契約は英語版のみです。[Page Down] を押して契約をすべてお読みください。"
LangString TermletLicenseTop ${LANG_KOREAN} "사용권 계약은 영어로만 제공됩니다. [Page Down] 키를 눌러 계약 전문을 읽어 주세요."

LangString MsgNotX64 ${LANG_ENGLISH} "This installer requires 64-bit (x64) Windows."
LangString MsgNotX64 ${LANG_SIMPCHINESE} "这个安装包只能安装在 64 位（x64）的 Windows 上。"
LangString MsgNotX64 ${LANG_TRADCHINESE} "這個安裝程式只能安裝在 64 位元（x64）的 Windows 上。"
LangString MsgNotX64 ${LANG_JAPANESE} "このインストーラーは 64 ビット（x64）の Windows にのみインストールできます。"
LangString MsgNotX64 ${LANG_KOREAN} "이 설치 프로그램은 64비트(x64) Windows에만 설치할 수 있습니다."
LangString MsgNotArm64 ${LANG_ENGLISH} "This installer is for Windows on Arm (arm64). On an x64 PC, download the x64 installer (-win-x64.exe)."
LangString MsgNotArm64 ${LANG_SIMPCHINESE} "这个安装包只能安装在 arm64（ARM 处理器）的 Windows 上。x64 的电脑请下载 x64 的安装包（-win-x64.exe）。"
LangString MsgNotArm64 ${LANG_TRADCHINESE} "這個安裝程式只能安裝在 arm64（ARM 處理器）的 Windows 上。x64 的電腦請下載 x64 的安裝程式（-win-x64.exe）。"
LangString MsgNotArm64 ${LANG_JAPANESE} "このインストーラーは arm64（ARM プロセッサ）の Windows にのみインストールできます。x64 の PC では x64 のインストーラー（-win-x64.exe）をダウンロードしてください。"
LangString MsgNotArm64 ${LANG_KOREAN} "이 설치 프로그램은 arm64(ARM 프로세서) Windows에만 설치할 수 있습니다. x64 PC에서는 x64 설치 프로그램(-win-x64.exe)을 다운로드하세요."
LangString MsgOsTooOld ${LANG_ENGLISH} "Termlet requires Windows 10 1809 (build 17763) or later."
LangString MsgOsTooOld ${LANG_SIMPCHINESE} "Termlet 需要 Windows 10 1809（内部版本 17763）或更高版本。"
LangString MsgOsTooOld ${LANG_TRADCHINESE} "Termlet 需要 Windows 10 1809（組建 17763）或更新版本。"
LangString MsgOsTooOld ${LANG_JAPANESE} "Termlet には Windows 10 1809（ビルド 17763）以降が必要です。"
LangString MsgOsTooOld ${LANG_KOREAN} "Termlet을 사용하려면 Windows 10 1809(빌드 17763) 이상이 필요합니다."
LangString MsgRunning ${LANG_ENGLISH} "Termlet is running. Save your work and close all Termlet windows, then click Retry. Click Cancel to quit."
LangString MsgRunning ${LANG_SIMPCHINESE} "Termlet 正在运行。请保存工作并关闭 Termlet 的所有窗口，然后点“重试”；点“取消”退出。"
LangString MsgRunning ${LANG_TRADCHINESE} "Termlet 正在執行。請儲存工作並關閉 Termlet 的所有視窗，然後按「重試」；按「取消」結束。"
LangString MsgRunning ${LANG_JAPANESE} "Termlet が実行中です。作業内容を保存して Termlet のウィンドウをすべて閉じてから、「再試行」をクリックしてください。終了するには「キャンセル」をクリックしてください。"
LangString MsgRunning ${LANG_KOREAN} "Termlet이 실행 중입니다. 작업을 저장하고 Termlet 창을 모두 닫은 다음 '다시 시도'를 클릭하세요. 종료하려면 '취소'를 클릭하세요."

VIProductVersion "${VERSION_NUMERIC}"
VIAddVersionKey /LANG=${LANG_ENGLISH} "ProductName" "${PRODUCT}"
VIAddVersionKey /LANG=${LANG_ENGLISH} "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=${LANG_ENGLISH} "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=${LANG_ENGLISH} "FileDescription" "${PRODUCT} Setup"
VIAddVersionKey /LANG=${LANG_ENGLISH} "CompanyName" "${PUBLISHER}"
VIAddVersionKey /LANG=${LANG_ENGLISH} "LegalCopyright" "Copyright (C) 2026 ${PUBLISHER}"

; 与 Termlet 的界面语言一致：香港、澳门的中文系统用繁体，新加坡的用简体（NSIS 只精确匹配 zh-TW、zh-CN，
; 其余中文地区按主语言匹配，落到哪一种中文不确定）。LANGID：3076 zh-HK、5124 zh-MO、4100 zh-SG。
; 在 .onInit 中最先调用，之后的提示框才用选定的语言
!macro PICK_LANGUAGE
  System::Call 'kernel32::GetUserDefaultUILanguage() i .r0'
  ${If} $0 = 3076
  ${OrIf} $0 = 5124
    StrCpy $LANGUAGE ${LANG_TRADCHINESE}
  ${ElseIf} $0 = 4100
    StrCpy $LANGUAGE ${LANG_SIMPCHINESE}
  ${EndIf}
!macroend

; ---------------------------------------------------------------- 运行中的 Termlet

; 安装与卸载程序各一份（卸载程序的函数要以 un. 开头）
!macro DEFINE_STOP_RUNNING PREFIX
Function ${PREFIX}StopRunning
  InitPluginsDir
  SetOutPath "$PLUGINSDIR"
  File "${STAGE}\helper.ps1"
  ; 不把工作目录留在安装目录中（卸载时要删除它）
  SetOutPath "$TEMP"
  StrCpy $R9 0
  check:
    nsExec::Exec '"${POWERSHELL}" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$PLUGINSDIR\helper.ps1" -Action Check -Dir "$INSTDIR"'
    Pop $0
    ${If} $0 == 1
      ; 只剩后台实例或命令行工具：直接结束（与仓库的安装约定相同，不用询问）
      nsExec::Exec '"${POWERSHELL}" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$PLUGINSDIR\helper.ps1" -Action StopBackground -Dir "$INSTDIR"'
      Pop $0
      Goto check
    ${ElseIf} $0 == 2
      ${If} ${Silent}
        ; 自动更新：Termlet 启动安装程序后正在退出，等它退出；30 秒后仍在运行就结束它
        ${If} $R9 >= 30
          nsExec::Exec '"${POWERSHELL}" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$PLUGINSDIR\helper.ps1" -Action StopAll -Dir "$INSTDIR"'
          Pop $0
          Return
        ${EndIf}
        Sleep 1000
        IntOp $R9 $R9 + 1
        Goto check
      ${EndIf}
      MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "$(MsgRunning)" IDRETRY check
      Quit
    ${EndIf}
    ; 0：没有在运行的；查不了（没有 PowerShell 等）时也继续，文件被占用时 NSIS 会提示重试
FunctionEnd
!macroend
!insertmacro DEFINE_STOP_RUNNING ""
!insertmacro DEFINE_STOP_RUNNING "un."

; ---------------------------------------------------------------- 安装

Function .onInit
  SetShellVarContext current
  !insertmacro PICK_LANGUAGE
!if "${ARCH}" == "arm64"
  ; 按系统本身的架构判断（安装程序是 32 位的，在 x64 与 arm64 的 Windows 上 RunningX64 都成立）
  ${IfNot} ${IsNativeARM64}
    MessageBox MB_ICONSTOP "$(MsgNotArm64)" /SD IDOK
    Quit
  ${EndIf}
!else
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "$(MsgNotX64)" /SD IDOK
    Quit
  ${EndIf}
!endif
  ; 读取后恢复原来的注册表视图
  SetRegView 64
  ReadRegStr $0 HKLM "SOFTWARE\Microsoft\Windows NT\CurrentVersion" "CurrentBuildNumber"
  SetRegView lastused
  ${If} $0 < 17763
    MessageBox MB_ICONSTOP "$(MsgOsTooOld)" /SD IDOK
    Quit
  ${EndIf}
  ${GetParameters} $R0
  StrCpy $Updated 0
  StrCpy $ForceRun 0
  ClearErrors
  ${GetOptions} $R0 "--updated" $R1
  ${IfNot} ${Errors}
    StrCpy $Updated 1
  ${EndIf}
  ClearErrors
  ${GetOptions} $R0 "--force-run" $R1
  ${IfNot} ${Errors}
    StrCpy $ForceRun 1
  ${EndIf}
FunctionEnd

Function RunTermlet
  Exec '"$INSTDIR\Termlet.exe"'
FunctionEnd

; 快捷方式：首次安装时创建；自动更新时只刷新已有的（用户删掉的不再创建）
!macro SHORTCUT PATH
  ${If} $Updated == 0
  ${OrIf} ${FileExists} "${PATH}"
    CreateShortCut "${PATH}" "$INSTDIR\Termlet.exe" "" "$INSTDIR\Termlet.exe" 0
  ${EndIf}
!macroend

Section "Termlet"
  Call StopRunning

  SetOutPath "$INSTDIR"
  File "${STAGE}\Termlet.exe"
  SetOutPath "$INSTDIR\bin"
  File "${STAGE}\bin\termlet.exe"
  ; 第三方许可：整个目录换成这个版本的
  RMDir /r "$INSTDIR\licenses"
  SetOutPath "$INSTDIR\licenses"
  File /r "${STAGE}\licenses\*.*"
  SetOutPath "$INSTDIR"
  WriteUninstaller "$INSTDIR\${UNINSTALLER}"

  WriteRegStr HKCU "${APP_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "${PRODUCT}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\Termlet.exe,0"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\${UNINSTALLER}"'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\${UNINSTALLER}" /S'
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "EstimatedSize" "$0"

  !insertmacro SHORTCUT "$SMPROGRAMS\${PRODUCT}.lnk"
  !insertmacro SHORTCUT "$DESKTOP\${PRODUCT}.lnk"

  ${If} $ForceRun == 1
    Exec '"$INSTDIR\Termlet.exe"'
  ${EndIf}
SectionEnd

; ---------------------------------------------------------------- 卸载

Function un.onInit
  SetShellVarContext current
  !insertmacro PICK_LANGUAGE
FunctionEnd

Section "Uninstall"
  Call un.StopRunning

  ; 从当前用户的 PATH 中移除命令行工具的目录（在“命令行工具”设置中加入过时），并通知资源管理器等进程
  nsExec::Exec '"${POWERSHELL}" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$PLUGINSDIR\helper.ps1" -Action RemovePath -Dir "$INSTDIR"'
  Pop $0
  SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000

  Delete "$SMPROGRAMS\${PRODUCT}.lnk"
  Delete "$DESKTOP\${PRODUCT}.lnk"

  ; 只删除安装的文件：安装目录是用户选的，其中可能还有别的文件
  Delete "$INSTDIR\Termlet.exe"
  Delete "$INSTDIR\bin\termlet.exe"
  RMDir "$INSTDIR\bin"
  RMDir /r "$INSTDIR\licenses"
  Delete "$INSTDIR\${UNINSTALLER}"
  RMDir "$INSTDIR"

  ; 自动更新下载的安装包（默认数据目录中的）；用户数据保留
  RMDir /r "$APPDATA\termlet\updates"

  DeleteRegKey HKCU "${UNINSTALL_KEY}"
  DeleteRegKey HKCU "${APP_KEY}"
SectionEnd
