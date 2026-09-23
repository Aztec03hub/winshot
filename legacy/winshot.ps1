param([string]$Out = "$env:TEMP\winshot.png", [string]$Window = "")
Add-Type @"
using System;using System.Runtime.InteropServices;
public class W {
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h,int n);
  [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr c);
}
"@
# PER_MONITOR_AWARE_V2 (-4) BEFORE Windows.Forms loads; without it every scaled
# display is captured at logical resolution, silently (the original v0 bug).
if (-not [W]::SetProcessDpiAwarenessContext([IntPtr](-4))) { throw "winshot: could not become DPI-aware" }
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
if ($Window -ne "") {
  $p = Get-Process | Where-Object { $_.MainWindowTitle -like "*$Window*" } | Select-Object -First 1
  if ($p) {
    [W]::ShowWindow($p.MainWindowHandle, 9) | Out-Null
    [W]::SetForegroundWindow($p.MainWindowHandle) | Out-Null
    Start-Sleep -Milliseconds 800
  } else {
    Write-Output "NOWINDOW"
  }
}
$b = [System.Windows.Forms.SystemInformation]::VirtualScreen
$bmp = New-Object System.Drawing.Bitmap $b.Width, $b.Height
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($b.Location, [System.Drawing.Point]::Empty, $b.Size)
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()
Write-Output "$($b.Width)x$($b.Height)"
