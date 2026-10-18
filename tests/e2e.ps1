# End-to-end smoke test: drives the real mouse and keyboard to drag a PNG out of a test form, hold Shift and
# drop it on the WEBP sector. Run after `cargo build --release` and do not touch the mouse meanwhile.
# Expected: overlay visible=True, DoDragDrop result=Copy, and an x.webp next to x.png.
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System; using System.Runtime.InteropServices;
public class N {
 [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
 [DllImport("user32.dll")] public static extern uint GetDpiForSystem();
 [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
 [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, uint d, UIntPtr e);
 [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte sc, uint f, UIntPtr e);
 [DllImport("user32.dll")] public static extern IntPtr FindWindow(string c, string t);
 [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
 [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
 [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L,T,R,B; }
}
"@
[N]::SetProcessDPIAware() | Out-Null
$sb=[System.Windows.Forms.Screen]::PrimaryScreen.Bounds
function MoveTo($x,$y){ [N]::mouse_event(0x8001,[int]($x*65535/($sb.Width-1)),[int]($y*65535/($sb.Height-1)),0,[UIntPtr]::Zero) }
$scale = [N]::GetDpiForSystem() / 96.0
$exe = if ($env:RADIAL_EXE) { $env:RADIAL_EXE } else { Join-Path $PSScriptRoot "..\target\release\radial.exe" }
# Only touch the copy under test; a private mutex and settings folder keep it away from a running instance.
Get-Process radial -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq (Resolve-Path $exe).Path } | Stop-Process -Force
$env:RADIAL_INSTANCE = "e2e"; $env:RADIAL_NO_ONBOARDING = "1"; $env:APPDATA = Join-Path $env:TEMP "radial-e2e-appdata"
$env:RADIAL_ANY_SURFACE = "1"
$dir = Join-Path $env:TEMP "radial-e2e"; Remove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue; New-Item $dir -ItemType Directory | Out-Null
$bmp = New-Object System.Drawing.Bitmap 64,64; $g=[System.Drawing.Graphics]::FromImage($bmp); $g.Clear([System.Drawing.Color]::OrangeRed); $bmp.Save("$dir\x.png"); $g.Dispose(); $bmp.Dispose()
$proc = Start-Process $exe -PassThru
Start-Sleep 1.5
$form = New-Object System.Windows.Forms.Form
$form.StartPosition = "Manual"; $form.Location = New-Object System.Drawing.Point(100,100); $form.Size = New-Object System.Drawing.Size(400,400); $form.TopMost = $true
$script:log = @(); $script:n = 0; $script:cx=0; $script:cy=0
$form.Add_MouseDown({ $d = New-Object System.Windows.Forms.DataObject; $d.SetData([System.Windows.Forms.DataFormats]::FileDrop, [string[]]@("$dir\x.png")); $r = $form.DoDragDrop($d, [System.Windows.Forms.DragDropEffects]::Copy); $script:log += "DoDragDrop result=$r" })
$t = New-Object System.Windows.Forms.Timer; $t.Interval = 100
$t.Add_Tick({
  $script:n++
  switch ($script:n) {
    1 { MoveTo (300) (300); [N]::mouse_event(0x0002,0,0,0,[UIntPtr]::Zero) }
    5 { MoveTo (320) (300) }
    6 { MoveTo (340) (300) }
    8 { [N]::keybd_event(0x10,0,0,[UIntPtr]::Zero) }
    10 { MoveTo (360) (302) }
    12 { MoveTo (362) (304) }
    16 { $h=[N]::FindWindow("RadialOverlay","RadialOverlay"); $r = New-Object N+RECT; [N]::GetWindowRect($h,[ref]$r)|Out-Null
         $script:log += "overlay visible=$([N]::IsWindowVisible($h)) rect=$($r.L),$($r.T),$($r.R),$($r.B)"
         $script:cx = ($r.L+$r.R)/2; $script:cy = ($r.T+$r.B)/2
         MoveTo ([int]($script:cx + 115*$scale)) ( [int]$script:cy) }
    18 { MoveTo ([int]($script:cx + 118*$scale)) ( [int]$script:cy+1) }
    20 { $b = New-Object System.Drawing.Bitmap 540,540; $gg = [System.Drawing.Graphics]::FromImage($b)
         $gg.CopyFromScreen([int]($script:cx-270),[int]($script:cy-270),0,0,$b.Size); $gg.Dispose(); $b.Save("$dir\overlay.png"); $b.Dispose() }
    22 { [N]::mouse_event(0x0004,0,0,0,[UIntPtr]::Zero) }
    26 { [N]::keybd_event(0x10,0,2,[UIntPtr]::Zero) }
    30 { $t.Stop(); $form.Close() }
  }
})
$form.Add_Shown({ $t.Start() })
$kill = New-Object System.Windows.Forms.Timer; $kill.Interval = 12000; $kill.Add_Tick({ $script:log += "TIMEOUT"; $form.Close() }); $kill.Start()
[System.Windows.Forms.Application]::Run($form)
[N]::keybd_event(0x10,0,2,[UIntPtr]::Zero)
Start-Sleep 1
$script:log
"scale=$scale"
Get-ChildItem $dir | ForEach-Object { "file: $($_.Name) $($_.Length)" }
Stop-Process $proc -Force
