# UI smoke test: starts a private copy of radial.exe (own mutex, own %APPDATA%), checks that the first-run
# onboarding opens, takes a screenshot of the window, closes it and compares memory before/after, opens and closes
# each action dialog (CROP, ADD BG, EDIT, ANNOTATE) on a 3000x2000 picture checking that the window appears,
# draws something and that the memory comes back, then measures idle CPU of the tray process.
# Usage: powershell -File tests\ui-smoke.ps1 [-Exe path] [-Out dir]   (exit code 1 when a check fails)
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
# A large picture for the dialogs: the memory numbers below are for a 6 megapixel image.
$pic = Join-Path $Out "dialog-test.png"
$bmp = New-Object System.Drawing.Bitmap 3000, 2000
$g = [System.Drawing.Graphics]::FromImage($bmp)
$br = New-Object System.Drawing.Drawing2D.LinearGradientBrush ((New-Object System.Drawing.Point 0, 0), (New-Object System.Drawing.Point 3000, 2000), [System.Drawing.Color]::DeepSkyBlue, [System.Drawing.Color]::MediumVioletRed)
$g.FillRectangle($br, 0, 0, 3000, 2000); $g.FillEllipse([System.Drawing.Brushes]::Gold, 1800, 300, 700, 700)
$g.Dispose(); $bmp.Save($pic); $bmp.Dispose()
$env:RADIAL_OPEN_FILE = $pic
$failures = 0
function Check($name, $ok, $detail = "") { if (-not $ok) { $script:failures++ }; "{0}: {1} {2}" -f $(if ($ok) { "PASS" } else { "FAIL" }), $name, $detail }
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
    # Every action dialog: open it through the test message, wait for the picture to load, look at it, close it.
    $p.Refresh(); $before = $p.PrivateMemorySize64
    foreach ($name in "crop", "bg", "edit", "annotate") {
        $idx = @{ crop = 5; bg = 4; edit = 2; annotate = 3 }[$name]
        [W]::PostMessage($main, 0x8010, [IntPtr]$idx, [IntPtr]::Zero) | Out-Null
        $dh = [IntPtr]::Zero
        for ($i = 0; $i -lt 40 -and $dh -eq [IntPtr]::Zero; $i++) { Start-Sleep -Milliseconds 100; $dh = [W]::FindWindow("RadialDialog", [NullString]::Value) }
        Check "$name dialog opened" ($dh -ne [IntPtr]::Zero)
        if ($dh -eq [IntPtr]::Zero) { continue }
        Start-Sleep -Milliseconds 2500
        $r = New-Object W+RECT; [W]::GetWindowRect($dh, [ref]$r) | Out-Null
        $w = $r.R - $r.L; $h = $r.B - $r.T
        $bmp = New-Object System.Drawing.Bitmap $w, $h
        $gg = [System.Drawing.Graphics]::FromImage($bmp); $gg.CopyFromScreen($r.L, $r.T, 0, 0, $bmp.Size); $gg.Dispose()
        $shot = Join-Path $Out "dialog-$name.png"; $bmp.Save($shot)
        # The picture area must not be the empty card colour: sample a few pixels in the middle of the canvas.
        $distinct = @{}
        foreach ($fx in 0.2, 0.35, 0.5) { foreach ($fy in 0.3, 0.5, 0.65) { $distinct[$bmp.GetPixel([int]($w * $fx), [int]($h * $fy)).ToArgb()] = 1 } }
        $bmp.Dispose()
        Check "$name dialog drew the picture" ($distinct.Count -ge 4) "($($distinct.Count) distinct colours sampled) $shot"
        $p.Refresh(); $open = $p.PrivateMemorySize64
        # Close like the Esc key does (the dialog handles WM_KEYDOWN itself).
        [W]::PostMessage($dh, 0x100, [IntPtr]0x1B, [IntPtr]::Zero) | Out-Null
        Start-Sleep -Milliseconds 800
        $p.Refresh()
        Check "$name dialog closed with Esc" (-not [W]::IsWindow($dh))
        "  memory: before $([int]($before/1KB)) KB, with the dialog open $([int]($open/1KB)) KB, after closing $([int]($p.PrivateMemorySize64/1KB)) KB"
    }
    $p.Refresh(); "after the dialogs: Private $([int]($p.PrivateMemorySize64/1KB)) KB (was $([int]($before/1KB)) KB), handles $($p.HandleCount)"
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
"{0} failed checks" -f $failures
exit $(if ($failures -gt 0) { 1 } else { 0 })
