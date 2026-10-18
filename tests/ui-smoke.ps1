# UI smoke test: starts a private copy of radial.exe (own mutex, own %APPDATA%), checks that the first-run
# onboarding opens, takes a screenshot of the window, closes it and compares memory before/after, then measures
# idle CPU of the tray process. Usage: powershell -File tests\ui-smoke.ps1 [-Exe path] [-Out dir]
param(
    [int]$Cycles = 5,
    [string]$Exe = (Join-Path $PSScriptRoot "..\target\release\radial.exe"),
    [string]$Out = (Join-Path $env:TEMP "radial-ui-smoke")
)
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System; using System.Runtime.InteropServices;
public class W {
 [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
 [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindow(string c, string t);
 [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
 [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
 [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
 [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
 [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
 [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L,T,R,B; }
}
"@
[W]::SetProcessDPIAware() | Out-Null
New-Item $Out -ItemType Directory -Force | Out-Null
$env:APPDATA = Join-Path $Out "appdata"
Remove-Item $env:APPDATA -Recurse -Force -ErrorAction SilentlyContinue
New-Item $env:APPDATA -ItemType Directory | Out-Null
$env:RADIAL_INSTANCE = "smoke"
$p = Start-Process $Exe -PassThru
try {
    $h = [IntPtr]::Zero
    for ($i = 0; $i -lt 40 -and $h -eq [IntPtr]::Zero; $i++) { Start-Sleep -Milliseconds 150; $h = [W]::FindWindow("RadialSettings", [NullString]::Value) }
    "settings window opened on first run: $($h -ne [IntPtr]::Zero)"
    if ($h -eq [IntPtr]::Zero) { return }
    Start-Sleep -Seconds 2
    [W]::SetForegroundWindow($h) | Out-Null
    Start-Sleep -Milliseconds 500
    $r = New-Object W+RECT; [W]::GetWindowRect($h, [ref]$r) | Out-Null
    $bmp = New-Object System.Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
    $g = [System.Drawing.Graphics]::FromImage($bmp); $g.CopyFromScreen($r.L, $r.T, 0, 0, $bmp.Size); $g.Dispose()
    $shot = Join-Path $Out "settings-window.png"; $bmp.Save($shot); $bmp.Dispose()
    "screenshot: $shot"
    $p.Refresh(); "with settings open: WorkingSet $([int]($p.WorkingSet64/1KB)) KB, Private $([int]($p.PrivateMemorySize64/1KB)) KB"
    # Close through WM_CLOSE, like the X button.
    [W]::SendMessage($h, 0x10, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null
    Start-Sleep -Seconds 1
    "settings window gone: $(-not [W]::IsWindow($h))"
    $p.Refresh(); "after close: WorkingSet $([int]($p.WorkingSet64/1KB)) KB, Private $([int]($p.PrivateMemorySize64/1KB)) KB"
    "config written: $(Test-Path (Join-Path $env:APPDATA 'radial\config'))"
    # Open and close -Cycles more times through the tray command (ID_SETTINGS) and check memory does not grow.
    $main = [W]::FindWindow("RadialOverlay", [NullString]::Value)
    for ($i = 0; $i -lt $Cycles; $i++) {
        [W]::PostMessage($main, 0x111, [IntPtr]2, [IntPtr]::Zero) | Out-Null
        Start-Sleep -Milliseconds 700
        $h2 = [W]::FindWindow("RadialSettings", [NullString]::Value)
        if ($h2 -eq [IntPtr]::Zero) { "cycle $i : settings did not open"; break }
        [W]::SendMessage($h2, 0x10, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null
        Start-Sleep -Milliseconds 400
    }
    $p.Refresh(); "after $Cycles open/close cycles: Private $([int]($p.PrivateMemorySize64/1KB)) KB, handles $($p.HandleCount)"
    # Pause and resume through the tray command (ID_PAUSE); the process must stay alive.
    [W]::PostMessage($main, 0x111, [IntPtr]3, [IntPtr]::Zero) | Out-Null; Start-Sleep -Milliseconds 300
    [W]::PostMessage($main, 0x111, [IntPtr]3, [IntPtr]::Zero) | Out-Null; Start-Sleep -Milliseconds 300
    $p.Refresh(); "alive after pause/resume: $(-not $p.HasExited)"
    Start-Sleep -Seconds 1
    $p.Refresh(); $cpu0 = $p.TotalProcessorTime.TotalMilliseconds; Start-Sleep -Seconds 10; $p.Refresh()
    $cpu1 = $p.TotalProcessorTime.TotalMilliseconds
    "idle CPU over 10 s: $([math]::Round($cpu1 - $cpu0, 1)) ms"
    "idle RAM: WorkingSet $([int]($p.WorkingSet64/1KB)) KB, Private $([int]($p.PrivateMemorySize64/1KB)) KB, threads $($p.Threads.Count), handles $($p.HandleCount)"
} finally {
    Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
}
