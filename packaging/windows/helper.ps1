# Termlet 安装与卸载程序的辅助脚本（installer.nsi 经 nsExec 调用，Windows PowerShell 5.1）。
# 打包时 scripts/release.mjs 给它加上 UTF-8 BOM（没有 BOM 时 PowerShell 5.1 按系统代码页读取脚本）。
# 作者：ruan
#
#   -Action Check           检查安装目录中运行的 Termlet。退出码：0 没有；1 只有后台实例（没有窗口，例如为命令行工具
#                           服务的 headless 实例）、命令行工具或随带的 OpenConsole.exe；2 有带窗口的 Termlet
#   -Action StopBackground  结束后台实例、命令行工具与 OpenConsole.exe（带窗口的不动）
#   -Action StopAll         全部结束（静默安装等了一段时间仍未退出时）
#   -Action RemovePath      从当前用户的 PATH 中移除 <安装目录>\bin（保留 REG_SZ / REG_EXPAND_SZ 类型，
#                           比较时不区分大小写、忽略末尾的分隔符，与 termlet-core 的 userpath 相同）
#   -Action RemoveFolderMenu
#                           删除资源管理器右键菜单中指向 <安装目录>\Termlet.exe 的“在此处打开 Termlet”（设置的“本地终端”
#                           中打开过时由 Termlet 写入，见 termlet-core 的 folder_menu）；指向别处的（另一份便携版）不动。
#                           -Classes 是当前用户下放菜单键的位置，默认 Software\Classes（测试中换成临时的键）
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('Check', 'StopBackground', 'StopAll', 'RemovePath', 'RemoveFolderMenu')]
    [string]$Action,
    [Parameter(Mandatory = $true)]
    [string]$Dir,
    [string]$Classes = 'Software\Classes'
)

$ErrorActionPreference = 'Stop'
$root = [System.IO.Path]::GetFullPath($Dir).TrimEnd('\', '/')
$app = Join-Path $root 'Termlet.exe'

function Get-TermletProcesses {
    # 名称比较不区分大小写：Termlet.exe（应用）与 bin\termlet.exe（命令行工具）都在其中。随带的 OpenConsole.exe
    # （本地终端的宿主）平时随 Termlet 退出，留下的也要结束，否则覆盖、删除它时文件被占用；只认安装目录中的，
    # Windows Terminal 等别的程序的不动
    $prefix = $root + '\'
    Get-CimInstance -ClassName Win32_Process -Filter "Name = 'termlet.exe' OR Name = 'OpenConsole.exe'" |
        Where-Object { $_.ExecutablePath -and $_.ExecutablePath.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase) }
}

function Test-HasWindow($process) {
    if (-not [string]::Equals($process.ExecutablePath, $app, [System.StringComparison]::OrdinalIgnoreCase)) {
        return $false
    }
    $p = Get-Process -Id $process.ProcessId -ErrorAction SilentlyContinue
    return ($null -ne $p) -and ($p.MainWindowHandle -ne [IntPtr]::Zero)
}

function Stop-Termlet([bool]$all) {
    $stopped = @()
    foreach ($process in @(Get-TermletProcesses)) {
        if ($all -or -not (Test-HasWindow $process)) {
            Stop-Process -Id $process.ProcessId -Force -ErrorAction SilentlyContinue
            $stopped += $process.ProcessId
        }
    }
    # 等它们退出、释放文件
    foreach ($id in $stopped) {
        Wait-Process -Id $id -Timeout 10 -ErrorAction SilentlyContinue
    }
}

function Remove-FromUserPath {
    $bin = (Join-Path $root 'bin').TrimEnd('\', '/')
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
    if ($null -eq $key) { return }
    try {
        $value = $key.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        if ($null -eq $value) { return }
        $kind = $key.GetValueKind('Path')
        $entries = @($value -split ';' | Where-Object { $_ -ne '' })
        $kept = @($entries | Where-Object { -not [string]::Equals($_.TrimEnd('\', '/'), $bin, [System.StringComparison]::OrdinalIgnoreCase) })
        if ($kept.Count -ne $entries.Count) {
            $key.SetValue('Path', ($kept -join ';'), $kind)
        }
    } finally {
        $key.Close()
    }
}

function Remove-FolderMenu {
    foreach ($place in @('Directory\shell', 'Directory\Background\shell')) {
        $path = "$Classes\$place\Termlet"
        $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("$path\command", $false)
        if ($null -eq $key) { continue }
        try {
            $command = [string]$key.GetValue('', '')
        } finally {
            $key.Close()
        }
        # 命令行的第一项是程序（Termlet 写的带引号）
        if ($command.StartsWith('"')) {
            $program = $command.Substring(1).Split('"')[0]
        } else {
            $program = ($command.Trim() -split '\s+')[0]
        }
        # 与 $app 一样经 GetFullPath：Windows PowerShell 的它会把短文件名展开成长文件名（C:\Users\RUNNER~1\… 换成
        # C:\Users\runneradmin\…）。菜单中写的程序与安装目录一边是短名、一边是长名时，不这样就认不出是同一个
        try {
            $program = [System.IO.Path]::GetFullPath($program)
        } catch {
            continue
        }
        if ([string]::Equals($program, $app, [System.StringComparison]::OrdinalIgnoreCase)) {
            [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree($path, $false)
        }
    }
}

try {
    switch ($Action) {
        'Check' {
            $processes = @(Get-TermletProcesses)
            if ($processes.Count -eq 0) { exit 0 }
            foreach ($process in $processes) {
                if (Test-HasWindow $process) { exit 2 }
            }
            exit 1
        }
        'StopBackground' { Stop-Termlet $false; exit 0 }
        'StopAll' { Stop-Termlet $true; exit 0 }
        'RemovePath' { Remove-FromUserPath; exit 0 }
        'RemoveFolderMenu' { Remove-FolderMenu; exit 0 }
    }
} catch {
    # 查不了进程时按没有在运行处理（安装时文件被占用，NSIS 会提示重试）；其余失败不影响安装与卸载
    exit 0
}
