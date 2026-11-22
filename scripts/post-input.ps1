# 开发时向某个进程的主窗口投递输入（锁屏时也能用：消息直接发给窗口，不经过系统输入）。
# 用法（坐标都是窗口客户区坐标，物理像素）：
#   powershell -File scripts/post-input.ps1 -Process termlet-desktop -Move 640,420    # 只移动指针（看悬停提示）
#   powershell -File scripts/post-input.ps1 -Process termlet-desktop -Click 640,420
#   powershell -File scripts/post-input.ps1 -Process termlet-desktop -DoubleClick 640,420
#   powershell -File scripts/post-input.ps1 -Process termlet-desktop -RightClick 640,420
#   powershell -File scripts/post-input.ps1 -Process termlet-desktop -Drag 100,200,100,300   # 按住左键从前一点拖到后一点
#   powershell -File scripts/post-input.ps1 -Process termlet-desktop -Text "dir"      # 逐字符 WM_CHAR
#   powershell -File scripts/post-input.ps1 -Process termlet-desktop -Wheel 640,420,-3   # 滚轮（负数向下，单位为格）
#   powershell -File scripts/post-input.ps1 -Process termlet-desktop -Resize 736,520     # 改窗口大小（窗口管理接口）
#   powershell -File scripts/post-input.ps1 -Process termlet-desktop -Key Enter   # 另有 Tab、Escape、方向键、Home、End、PageUp、PageDown、F2、F5、F7、Apps（菜单键）、单个字母或数字等
# 带修饰键的快捷键投递不了（窗口查询的是真实的键盘状态），用 UI 测试。
# 按进程名找时取第一个有窗口的；用户自己开着 Termlet、或同时开着几个测试实例时用 -Pid 指定进程号（代替 -Process），
# 免得投错窗口：
#   powershell -File scripts/post-input.ps1 -Pid 12345 -Click 640,420
[CmdletBinding(DefaultParameterSetName = 'Name')]
param(
    [Parameter(Mandatory = $true, ParameterSetName = 'Name')][string]$Process,
    # $PID 是 PowerShell 的自动变量，参数名用 ProcessId，-Pid 是它的别名
    [Parameter(Mandatory = $true, ParameterSetName = 'Id')][Alias('Pid')][int]$ProcessId,
    [string]$Move,
    [string]$Click,
    [string]$DoubleClick,
    [string]$RightClick,
    [string]$Drag,
    [string]$Text,
    [string]$Key,
    [string]$Wheel,
    [string]$Resize
)
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class PostNative {
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X; public int Y; }
    [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr hwnd, uint msg, IntPtr wparam, IntPtr lparam);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr hwnd, ref POINT point);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hwnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
}
"@
if ($PSCmdlet.ParameterSetName -eq 'Id') {
    $found = Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
    $missing = "没有找到进程 $ProcessId 的窗口"
} else {
    $found = Get-Process -Name $Process -ErrorAction SilentlyContinue
    $missing = "没有找到 $Process 的窗口"
}
$target = $found | Where-Object { $_.MainWindowHandle -ne [IntPtr]::Zero } | Select-Object -First 1
if (-not $target) { Write-Error $missing; exit 1 }
$hwnd = $target.MainWindowHandle

function Point([int]$x, [int]$y) { [IntPtr](($y -shl 16) -bor ($x -band 0xFFFF)) }
function Post([uint32]$msg, [int]$wparam, [IntPtr]$lparam) {
    [PostNative]::PostMessageW($hwnd, $msg, [IntPtr]$wparam, $lparam) | Out-Null
}
# WM_MOUSEMOVE 0x200、WM_LBUTTONDOWN 0x201、WM_LBUTTONUP 0x202、WM_RBUTTONDOWN 0x204、WM_RBUTTONUP 0x205；MK_LBUTTON 1、MK_RBUTTON 2
function LeftClick([IntPtr]$at) {
    Post 0x0201 1 $at
    Start-Sleep -Milliseconds 30
    Post 0x0202 0 $at
}

if ($Resize) {
    # 改窗口大小（窗口管理接口，不是输入）：SWP_NOMOVE 2 | SWP_NOZORDER 4 | SWP_NOACTIVATE 0x10
    $w, $h = $Resize.Split(',') | ForEach-Object { [int]$_ }
    [PostNative]::SetWindowPos($hwnd, [IntPtr]::Zero, 0, 0, $w, $h, 0x16) | Out-Null
}
if ($Wheel) {
    # 滚轮：客户区坐标与格数（负数向下滚）；WM_MOUSEWHEEL 0x20A 的坐标是屏幕坐标，wParam 高 16 位是 120 的倍数
    $x, $y, $notches = $Wheel.Split(',') | ForEach-Object { [int]$_ }
    $p = New-Object PostNative+POINT
    $p.X = $x; $p.Y = $y
    [PostNative]::ClientToScreen($hwnd, [ref]$p) | Out-Null
    Post 0x0200 0 (Point $x $y)
    Start-Sleep -Milliseconds 50
    # 高 16 位是有符号数（最多 ±273 格）：格数多时分几次投递，否则溢出后方向反过来
    $left = $notches
    while ($left -ne 0) {
        $step = [Math]::Max(-250, [Math]::Min(250, $left))
        $left -= $step
        $delta = ($step * 120) -band 0xFFFF
        Post 0x020A ($delta -shl 16) (Point $p.X $p.Y)
        if ($left -ne 0) { Start-Sleep -Milliseconds 20 }
    }
}
if ($Move) {
    $x, $y = $Move.Split(',') | ForEach-Object { [int]$_ }
    Post 0x0200 0 (Point $x $y)
}
if ($Click) {
    $x, $y = $Click.Split(',') | ForEach-Object { [int]$_ }
    $at = Point $x $y
    Post 0x0200 0 $at
    Start-Sleep -Milliseconds 50
    LeftClick $at
}
if ($DoubleClick) {
    $x, $y = $DoubleClick.Split(',') | ForEach-Object { [int]$_ }
    $at = Point $x $y
    Post 0x0200 0 $at
    Start-Sleep -Milliseconds 50
    LeftClick $at
    Start-Sleep -Milliseconds 60
    LeftClick $at
}
if ($RightClick) {
    $x, $y = $RightClick.Split(',') | ForEach-Object { [int]$_ }
    $at = Point $x $y
    Post 0x0200 0 $at
    Start-Sleep -Milliseconds 50
    Post 0x0204 2 $at
    Start-Sleep -Milliseconds 30
    Post 0x0205 0 $at
}
if ($Drag) {
    $x1, $y1, $x2, $y2 = $Drag.Split(',') | ForEach-Object { [int]$_ }
    Post 0x0200 0 (Point $x1 $y1)
    Start-Sleep -Milliseconds 50
    Post 0x0201 1 (Point $x1 $y1)
    # 分几步移过去（超过拖动阈值才开始拖动），到终点后停一会儿再放开
    for ($i = 1; $i -le 10; $i++) {
        $x = [int]($x1 + ($x2 - $x1) * $i / 10)
        $y = [int]($y1 + ($y2 - $y1) * $i / 10)
        Post 0x0200 1 (Point $x $y)
        Start-Sleep -Milliseconds 40
    }
    Start-Sleep -Milliseconds 200
    Post 0x0200 1 (Point $x2 $y2)
    Start-Sleep -Milliseconds 50
    Post 0x0202 0 (Point $x2 $y2)
}
if ($Text) {
    foreach ($ch in $Text.ToCharArray()) {
        Post 0x0102 ([int]$ch) ([IntPtr]1)   # WM_CHAR
        Start-Sleep -Milliseconds 10
    }
}
if ($Key) {
    $vk = @{ Enter = 0x0D; Tab = 0x09; Escape = 0x1B; Backspace = 0x08; Up = 0x26; Down = 0x28; Left = 0x25; Right = 0x27; Delete = 0x2E; F2 = 0x71; Space = 0x20;
        Home = 0x24; End = 0x23; PageUp = 0x21; PageDown = 0x22; F5 = 0x74; F7 = 0x76; Apps = 0x5D }[$Key]
    # 单个字母或数字：虚拟键码就是它的大写 ASCII 码
    if ($null -eq $vk -and $Key -match '^[A-Za-z0-9]$') { $vk = [int][char]$Key.ToUpper() }
    if ($null -eq $vk) { throw "不认识的键：$Key" }
    Post 0x0100 $vk ([IntPtr]1)   # WM_KEYDOWN
    Start-Sleep -Milliseconds 30
    Post 0x0101 $vk ([IntPtr]0xC0000001)   # WM_KEYUP
}
