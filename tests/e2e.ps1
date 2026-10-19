# End-to-end test: drives the real mouse and keyboard against a release build. A test form is the drag source;
# Shift opens the menu, the pointer moves onto a sector and the button is released over it. Scenarios:
#   1. PNG -> WEBP sector of the image ring
#   2. PDF -> TXT sector of the 4-sector document ring
#   3. PNG -> COMPRESS, switching to the actions ring with Tab
#   4. PNG -> COMPRESS, switching to the actions ring by resting on the hub
#   5. PNG -> CROP: the dialog opens in front, Enter applies it, the window closes and the cropped copy exists
#   6. PNG -> COMPRESS, reaching the actions ring with the mouse wheel
#   7. PDF -> PNG, reaching the image ring (menu 2) of a document with the mouse wheel
#   8. a file the engine does not read still opens the menu, and dropping on a sector does nothing
# Run after `cargo build --release` and do not touch the mouse meanwhile.
# Usage: powershell -File tests\e2e.ps1 [-Exe path] [-Out dir]   (exit code 1 when a check fails)
param(
    [string]$Exe = $(if ($env:RADIAL_EXE) { $env:RADIAL_EXE } else { Join-Path $PSScriptRoot "..\target\release\radial.exe" }),
    [string]$Out = (Join-Path $env:TEMP "radial-e2e"),
    [int[]]$Scenarios = @(1, 2, 3, 4, 5, 6, 7, 8)
)
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System; using System.Runtime.InteropServices;
public class N {
 [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
 [DllImport("user32.dll")] public static extern uint GetDpiForSystem();
 [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, uint d, UIntPtr e);
 [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte sc, uint f, UIntPtr e);
 [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindow(string c, string t);
 [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
 [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
 [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
 [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
 [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L,T,R,B; }
}
"@
[N]::SetProcessDPIAware() | Out-Null
$sb = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$scale = [N]::GetDpiForSystem() / 96.0
function MoveTo($x, $y) { [N]::mouse_event(0x8001, [int]($x * 65535 / ($sb.Width - 1)), [int]($y * 65535 / ($sb.Height - 1)), 0, [UIntPtr]::Zero) }
# One notch is 120; positive scrolls up (previous ring), negative down (next ring).
function Wheel($notches) { [N]::mouse_event(0x0800, 0, 0, [BitConverter]::ToUInt32([BitConverter]::GetBytes([int]($notches * 120)), 0), [UIntPtr]::Zero) }
function Key($vk, $up = $false) { [N]::keybd_event($vk, 0, $(if ($up) { 2 } else { 0 }), [UIntPtr]::Zero) }
function Snap($path, $l, $t, $w, $h) {
    $b = New-Object System.Drawing.Bitmap $w, $h; $g = [System.Drawing.Graphics]::FromImage($b)
    $g.CopyFromScreen([int]$l, [int]$t, 0, 0, $b.Size); $g.Dispose(); $b.Save($path); $b.Dispose()
}

# Only touch the copy under test; a private mutex and settings folder keep it away from a running instance.
$exePath = (Resolve-Path $Exe).Path
Get-Process radial -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exePath } | Stop-Process -Force
$env:RADIAL_INSTANCE = "e2e"; $env:RADIAL_NO_ONBOARDING = "1"; $env:RADIAL_ANY_SURFACE = "1"
$env:APPDATA = Join-Path $env:TEMP "radial-e2e-appdata"
Remove-Item $env:APPDATA -Recurse -Force -ErrorAction SilentlyContinue; New-Item $env:APPDATA -ItemType Directory | Out-Null
Remove-Item $Out -Recurse -Force -ErrorAction SilentlyContinue; New-Item $Out -ItemType Directory | Out-Null

# Test pictures: a flat 800x800 picture with 16 colours (compresses well) and a small solid one.
function NewPng($name, $size) {
    $bmp = New-Object System.Drawing.Bitmap $size, $size; $g = [System.Drawing.Graphics]::FromImage($bmp)
    for ($i = 0; $i -lt 16; $i++) {
        $brush = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, $i * 16, 255 - $i * 12, 90 + $i * 8))
        $g.FillRectangle($brush, ($i % 4) * $size / 4, [math]::Floor($i / 4) * $size / 4, $size / 4, $size / 4)
    }
    $bmp.Save("$Out\$name"); $g.Dispose(); $bmp.Dispose()
}
NewPng "x.png" 64; NewPng "wheel.png" 800; NewPng "tab.png" 800; NewPng "hub.png" 800; NewPng "crop.png" 400
Copy-Item (Join-Path $PSScriptRoot "fixtures\hello.pdf") "$Out\hello.pdf"
Copy-Item (Join-Path $PSScriptRoot "fixtures\hello.pdf") "$Out\wheelpdf.pdf"
[System.IO.File]::WriteAllBytes("$Out\blob.xyz", [byte[]](0, 1, 2, 3, 250, 251, 252, 253))

$proc = Start-Process $exePath -PassThru
Start-Sleep 1.5
$results = New-Object System.Collections.ArrayList
function Check($name, $ok, $detail = "") {
    [void]$results.Add([pscustomobject]@{ Check = $name; Pass = [bool]$ok; Detail = $detail })
    "{0}: {1} {2}" -f $(if ($ok) { "PASS" } else { "FAIL" }), $name, $detail
}

# Drags $File out of a form, opens the menu with Shift, optionally flips the ring ($Tab or $Hub), moves to the
# sector at $Angle degrees (clockwise from up) and releases. $Shot is a screenshot of the menu before the drop.
# $Then runs on the timer once the drop is done (dialog checks).
function Invoke-Drag($File, $Angle, $Shot, [switch]$Tab, [switch]$Hub, [int]$Wheel = 0, [scriptblock]$Then = $null) {
    # Handlers run later on the form's timer; they read script-scope variables, not this function's locals.
    $script:log = @(); $script:n = 0; $script:cx = 0; $script:cy = 0
    $script:File = $File; $script:Shot = $Shot; $script:Tab = [bool]$Tab; $script:Hub = [bool]$Hub; $script:Wheel = $Wheel; $script:Then = $Then
    $script:rad = 115 * $scale
    $script:a = $Angle * [math]::PI / 180
    $script:snap = [int](540 * $scale)
    $script:form = New-Object System.Windows.Forms.Form
    $script:form.StartPosition = "Manual"; $script:form.Location = New-Object System.Drawing.Point(100, 100)
    $script:form.Size = New-Object System.Drawing.Size(400, 400); $script:form.TopMost = $true
    $script:form.Add_MouseDown({
        $d = New-Object System.Windows.Forms.DataObject
        $d.SetData([System.Windows.Forms.DataFormats]::FileDrop, [string[]]@($script:File))
        $r = $script:form.DoDragDrop($d, [System.Windows.Forms.DragDropEffects]::Copy)
        $script:log += "DoDragDrop=$r"
    })
    $script:timer = New-Object System.Windows.Forms.Timer; $script:timer.Interval = 100
    $script:timer.Add_Tick({
        $script:n++
        $tx = $script:cx + $script:rad * [math]::Sin($script:a); $ty = $script:cy - $script:rad * [math]::Cos($script:a)
        switch ($script:n) {
            1 { MoveTo 300 300; [N]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero) }
            5 { MoveTo 320 300 }
            6 { MoveTo 340 300 }
            8 { Key 0x10 }
            10 { MoveTo 360 302 }
            12 { MoveTo 362 304 }
            16 {
                $h = [N]::FindWindow("RadialOverlay", "RadialOverlay"); $r = New-Object N+RECT; [N]::GetWindowRect($h, [ref]$r) | Out-Null
                $script:log += "overlay visible=$([N]::IsWindowVisible($h))"
                $script:cx = ($r.L + $r.R) / 2; $script:cy = ($r.T + $r.B) / 2
                if ($script:Tab -or $script:Wheel -ne 0) { MoveTo ($script:cx + 60 * $scale) $script:cy }
                elseif ($script:Hub) { MoveTo ($script:cx + 60 * $scale) $script:cy }
                else { MoveTo ($script:cx + $script:rad * [math]::Sin($script:a)) ($script:cy - $script:rad * [math]::Cos($script:a)) }
            }
            18 { if ($script:Tab) { Key 0x09; Key 0x09 $true } elseif ($script:Wheel -ne 0) { Wheel $script:Wheel } elseif ($script:Hub) { MoveTo $script:cx $script:cy } }
            # The hub dwell needs about half a second of rest; the screenshot is taken after the ring flipped.
            27 {
                $h = [N]::FindWindow("RadialOverlay", "RadialOverlay"); $r = New-Object N+RECT; [N]::GetWindowRect($h, [ref]$r) | Out-Null
                $script:log += "ring check: overlay visible=$([N]::IsWindowVisible($h)) rect=$($r.L),$($r.T),$($r.R),$($r.B)"
                Snap "$Out\$($script:Shot)-ring.png" ($script:cx - $script:snap / 2) ($script:cy - $script:snap / 2) $script:snap $script:snap }
            28 { MoveTo $tx $ty }
            30 { MoveTo ($tx + 1) ($ty + 1) }
            32 { Snap "$Out\$($script:Shot)-hover.png" ($script:cx - $script:snap / 2) ($script:cy - $script:snap / 2) $script:snap $script:snap }
            34 { [N]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero) }
            38 { Key 0x10 $true }
            42 { if ($script:Then) { & $script:Then } }
            80 { $script:timer.Stop(); $script:form.Close() }
        }
    })
    $script:form.Add_Shown({ $script:timer.Start() })
    $script:kill = New-Object System.Windows.Forms.Timer; $script:kill.Interval = 20000
    $script:kill.Add_Tick({ $script:log += "TIMEOUT"; $script:form.Close() }); $script:kill.Start()
    [System.Windows.Forms.Application]::Run($script:form)
    $script:kill.Stop()
    Key 0x10 $true
    Start-Sleep -Milliseconds 800
    $script:log | ForEach-Object { Write-Host "  log: $_" }
    return $script:log
}

if ($Scenarios -contains 1) {
    # 1. image ring: PNG -> WEBP (sector 1 of 8 is at 45 degrees; JPG takes the PNG slot at the top)
    $log = Invoke-Drag "$Out\x.png" 45 "s1"
    Check "image ring: overlay opened" ($log -match "overlay visible=True")
    Check "image ring: PNG -> WEBP written" (Test-Path "$Out\x.webp")
}

if ($Scenarios -contains 2) {
    # 2. document ring: PDF -> TXT (4 sectors DOCX, JPG, PNG, TXT; TXT is the left one)
    $log = Invoke-Drag "$Out\hello.pdf" 270 "s2"
    Check "document ring: PDF -> TXT written" (Test-Path "$Out\hello.txt")
    if (Test-Path "$Out\hello.txt") { Check "document ring: the text came through" ((Get-Content "$Out\hello.txt" -Raw) -match "Hello Radial") }
}

if ($Scenarios -contains 3) {
    # 3. actions ring through Tab: COMPRESS is the top sector
    $log = Invoke-Drag "$Out\tab.png" 0 "s3" -Tab
    Check "actions ring (Tab): COMPRESS wrote a smaller copy" (Test-Path "$Out\tab (compressed).png")
    if (Test-Path "$Out\tab (compressed).png") {
        Check "actions ring (Tab): it is smaller" ((Get-Item "$Out\tab (compressed).png").Length -lt (Get-Item "$Out\tab.png").Length) ("{0} -> {1} bytes" -f (Get-Item "$Out\tab.png").Length, (Get-Item "$Out\tab (compressed).png").Length)
    }
}

if ($Scenarios -contains 4) {
    # 4. actions ring by resting on the hub
    $log = Invoke-Drag "$Out\hub.png" 0 "s4" -Hub
    Check "actions ring (hub dwell): COMPRESS wrote a smaller copy" (Test-Path "$Out\hub (compressed).png")
}

if ($Scenarios -contains 5) {
    # 5. CROP (sector 5 of 7, 257 degrees): the dialog opens in front, Enter applies, the window closes
    $dialog = {
        $h = [N]::FindWindow("RadialDialog", [NullString]::Value)
        $script:log += "dialog open=$($h -ne [IntPtr]::Zero) visible=$([N]::IsWindowVisible($h)) foreground=$([N]::GetForegroundWindow() -eq $h)"
        if ($h -ne [IntPtr]::Zero) {
            $r = New-Object N+RECT; [N]::GetWindowRect($h, [ref]$r) | Out-Null
            Snap "$Out\crop-dialog.png" $r.L $r.T ($r.R - $r.L) ($r.B - $r.T)
            Key 0x0D; Key 0x0D $true
        }
    }
    $log = Invoke-Drag "$Out\crop.png" 257.14 "s5" -Tab -Then $dialog
    Check "CROP: the dialog opened" ($log -match "dialog open=True visible=True")
    Check "CROP: it came to the front" ($log -match "foreground=True")
    Check "CROP: Enter applied and the cropped copy exists" (Test-Path "$Out\crop (cropped).png")
    Check "CROP: the dialog window is gone" ([N]::FindWindow("RadialDialog", [NullString]::Value) -eq [IntPtr]::Zero)
}

if ($Scenarios -contains 6) {
    # 6. one notch down from the image ring is the actions ring: COMPRESS is the top sector
    $log = Invoke-Drag "$Out\wheel.png" 0 "s6" -Wheel -1
    Check "actions ring (wheel): COMPRESS wrote a smaller copy" (Test-Path "$Out\wheel (compressed).png")
}

if ($Scenarios -contains 7) {
    # 7. a PDF opens on the document ring; one notch down is the image ring, whose top sector is PNG
    $log = Invoke-Drag "$Out\wheelpdf.pdf" 0 "s7" -Wheel -1
    Check "image ring of a PDF (wheel): a PNG was written" ($null -ne (Get-ChildItem $Out -Filter "wheelpdf*.png" -ErrorAction SilentlyContinue))
}

if ($Scenarios -contains 8) {
    # 8. an unknown file still gets the menu; every sector is greyed out so nothing is written
    $files = { @(Get-ChildItem $Out | Where-Object { $_.Name -notmatch '^s\d-' }).Count }; $before = & $files
    $log = Invoke-Drag "$Out\blob.xyz" 0 "s8"
    Check "unknown file: the overlay opened" ($log -match "overlay visible=True")
    Check "unknown file: nothing was written" ((& $files) -eq $before) "files before $before"
}

Get-ChildItem $Out | ForEach-Object { "file: $($_.Name) $($_.Length)" }
Stop-Process $proc -Force -ErrorAction SilentlyContinue
$failed = @($results | Where-Object { -not $_.Pass }).Count
"{0} checks, {1} failed" -f $results.Count, $failed
exit $(if ($failed -gt 0) { 1 } else { 0 })
