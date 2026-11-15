# 把某个进程的主窗口内容保存为 PNG，供开发时检查界面（锁屏时也能用：PrintWindow 直接取窗口的渲染内容）。
# 用法：powershell -NoProfile -File scripts/capture-window.ps1 -Process termlet-desktop -Out shot.png
param(
    [Parameter(Mandatory = $true)][string]$Process,
    [Parameter(Mandatory = $true)][string]$Out
)

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class CaptureNative {
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hwnd, IntPtr hdc, uint flags);
}
"@

$target = Get-Process -Name $Process -ErrorAction SilentlyContinue |
    Where-Object { $_.MainWindowHandle -ne [IntPtr]::Zero } |
    Select-Object -First 1
if (-not $target) {
    Write-Error "没有找到 $Process 的窗口"
    exit 1
}

$rect = New-Object CaptureNative+RECT
[CaptureNative]::GetWindowRect($target.MainWindowHandle, [ref]$rect) | Out-Null
$width = $rect.Right - $rect.Left
$height = $rect.Bottom - $rect.Top
$bitmap = New-Object System.Drawing.Bitmap $width, $height
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$hdc = $graphics.GetHdc()
# PW_RENDERFULLCONTENT = 2：DirectX 渲染的窗口也能取到内容
[CaptureNative]::PrintWindow($target.MainWindowHandle, $hdc, 2) | Out-Null
$graphics.ReleaseHdc($hdc)
$bitmap.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$graphics.Dispose()
$bitmap.Dispose()
Write-Output "$Out ${width}x${height}"
