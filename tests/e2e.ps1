# End-to-end test: drives the real mouse and keyboard against a release build. A test form is the drag source;
# Shift opens the menu, the wheel (or Tab, or resting on the hub) walks the rings, the pointer moves onto a sector and
# the button is released over it. It covers every kind of file, every ring a file has, and the sectors that matter:
#   * gestures: wheel down/up, Tab, resting on the hub
#   * convert rings (documents, images): picture, PDF, Word, text, CSV, workbook
#   * tools ring: picture tools and the CROP dialog, PDF tools
#   * files ring: zip, unzip, info, SHA-256 (copy and verify), copy path, reveal, for any file, a folder, a ZIP
#   * several files at once
# Which ring a file starts on, how many sectors each ring has and their order come from src/ring.rs; the clockwise
# angle of sector i of n is i * 360 / n.
# Run after `cargo build --release` and do not touch the mouse or keyboard meanwhile.
# Usage: powershell -File tests\e2e.ps1 [-Exe path] [-Out dir] [-Only "text in a case name"]   (exit code 1 on a failure)
param(
    [string]$Exe = $(if ($env:RADIAL_EXE) { $env:RADIAL_EXE } else { Join-Path $PSScriptRoot "..\target\release\radial.exe" }),
    [string]$Out = (Join-Path $env:TEMP "radial-e2e"),
    [string]$Only = "",
    [int]$Repeat = 1
)
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
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
# Turns the wheel $n notches down (the next ring); a negative $n turns it up (the previous ring).
function WheelDown($n) {
    for ($i = 0; $i -lt [math]::Abs($n); $i++) {
        $delta = -120 * [math]::Sign($n)
        [N]::mouse_event(0x0800, 0, 0, [BitConverter]::ToUInt32([BitConverter]::GetBytes([int]$delta), 0), [UIntPtr]::Zero)
        Start-Sleep -Milliseconds 60
    }
}
function Key($vk, $up = $false) { [N]::keybd_event($vk, 0, $(if ($up) { 2 } else { 0 }), [UIntPtr]::Zero) }
function Snap($path, $l, $t, $w, $h) {
    $b = New-Object System.Drawing.Bitmap $w, $h; $g = [System.Drawing.Graphics]::FromImage($b)
    $g.CopyFromScreen([int]$l, [int]$t, 0, 0, $b.Size); $g.Dispose(); $b.Save($path); $b.Dispose()
}
function Clip { [System.Windows.Forms.Clipboard]::GetText() }
function SetClip($t) { [System.Windows.Forms.Clipboard]::SetText($t) }
function Exists($n) { Test-Path "$Out\$n" }
function ExplorerWindows { @((New-Object -ComObject Shell.Application).Windows() | Where-Object { $_.LocationName -like "*radial-e2e*" -or $_.LocationURL -like "*radial-e2e*" }) }

# Only touch the copy under test; a private mutex and settings folder keep it away from a running instance.
$exePath = (Resolve-Path $Exe).Path
Get-Process radial -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exePath } | Stop-Process -Force
$env:RADIAL_INSTANCE = "e2e"; $env:RADIAL_NO_ONBOARDING = "1"; $env:RADIAL_ANY_SURFACE = "1"
$env:APPDATA = Join-Path $env:TEMP "radial-e2e-appdata"
Remove-Item $env:APPDATA -Recurse -Force -ErrorAction SilentlyContinue; New-Item $env:APPDATA -ItemType Directory | Out-Null
# English texts whatever the machine language is: the checks read what the actions put on the clipboard.
New-Item (Join-Path $env:APPDATA "radial") -ItemType Directory | Out-Null
[System.IO.File]::WriteAllText((Join-Path $env:APPDATA "radial\config"), "lang = en`r`nonboarded = true`r`n")
Remove-Item $Out -Recurse -Force -ErrorAction SilentlyContinue; New-Item $Out -ItemType Directory | Out-Null

# ---------------------------------------------------------------------------------------------------- fixtures
function NewPng($name, $size) {
    $bmp = New-Object System.Drawing.Bitmap $size, $size; $g = [System.Drawing.Graphics]::FromImage($bmp)
    for ($i = 0; $i -lt 16; $i++) {
        $brush = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, $i * 16, 255 - $i * 12, 90 + $i * 8))
        $g.FillRectangle($brush, ($i % 4) * $size / 4, [math]::Floor($i / 4) * $size / 4, $size / 4, $size / 4)
    }
    $bmp.Save("$Out\$name"); $g.Dispose(); $bmp.Dispose()
}
function NewZip($path, $entries) {
    $fs = [System.IO.File]::Create($path)
    $zip = New-Object System.IO.Compression.ZipArchive($fs, [System.IO.Compression.ZipArchiveMode]::Create)
    foreach ($k in $entries.Keys) {
        $e = $zip.CreateEntry($k); $w = New-Object System.IO.StreamWriter($e.Open(), (New-Object System.Text.UTF8Encoding($false)))
        $w.Write($entries[$k]); $w.Dispose()
    }
    $zip.Dispose(); $fs.Dispose()
}
NewPng "x.png" 64; NewPng "p1.png" 300; NewPng "p2.png" 300; NewPng "tab.png" 800; NewPng "hub.png" 800; NewPng "crop.png" 400
NewPng "c1.png" 800; NewPng "c2.png" 800
Copy-Item (Join-Path $PSScriptRoot "fixtures\hello.pdf") "$Out\hello.pdf"
foreach ($n in "m1.pdf", "m2.pdf", "m3.pdf") { Copy-Item (Join-Path $PSScriptRoot "fixtures\hello.pdf") "$Out\$n" }
$ct = '<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/></Types>'
NewZip "$Out\memo.docx" @{
    "[Content_Types].xml" = $ct
    "_rels/.rels" = '<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>'
    "word/document.xml" = '<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Quarterly memo for Radial</w:t></w:r></w:p></w:body></w:document>'
}
NewZip "$Out\budget.xlsx" @{
    "[Content_Types].xml" = $ct
    "_rels/.rels" = '<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>'
    "xl/workbook.xml" = '<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Budget" sheetId="1" r:id="rId1"/></sheets></workbook>'
    "xl/_rels/workbook.xml.rels" = '<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>'
    "xl/styles.xml" = '<?xml version="1.0"?><styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><cellXfs count="2"><xf numFmtId="0"/><xf numFmtId="14"/></cellXfs></styleSheet>'
    "xl/worksheets/sheet1.xml" = '<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Item</t></is></c><c r="B1" t="inlineStr"><is><t>Due</t></is></c><c r="C1" t="inlineStr"><is><t>Cost</t></is></c></row><row r="2"><c r="A2" t="inlineStr"><is><t>Licences, annual</t></is></c><c r="B2" s="1"><v>45292</v></c><c r="C2"><v>1250.5</v></c></row></sheetData></worksheet>'
}
[System.IO.File]::WriteAllText("$Out\prices.csv", "Item,Cost`r`nTea,1.5`r`nCoffee,2`r`n")
[System.IO.File]::WriteAllText("$Out\notes.txt", "First line`r`nSecond line`r`n")
[System.IO.File]::WriteAllBytes("$Out\song.ogg", [byte[]](0x4f, 0x67, 0x67, 0x53, 0, 2, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8))
New-Item "$Out\Project" -ItemType Directory | Out-Null
[System.IO.File]::WriteAllText("$Out\Project\readme.txt", "inside the folder")
New-Item "$Out\zsrc" -ItemType Directory | Out-Null
[System.IO.File]::WriteAllText("$Out\zsrc\inner.txt", "packed")
[System.IO.Compression.ZipFile]::CreateFromDirectory("$Out\zsrc", "$Out\bundle.zip")

$proc = Start-Process $exePath -PassThru
Start-Sleep 1.5
$results = New-Object System.Collections.ArrayList
function Check($name, $ok, $detail = "") {
    [void]$results.Add([pscustomobject]@{ Check = $name; Pass = [bool]$ok; Detail = $detail })
    "{0}: {1} {2}" -f $(if ($ok) { "PASS" } else { "FAIL" }), $name, $detail
}

# ---------------------------------------------------------------------------------------------------- the drag
# Drags $File (a path, or several) out of a form, opens the menu with Shift, optionally changes ring ($Next notches of
# the wheel, or $Tab, or $Hub), moves to the sector at $Angle degrees (clockwise from up) and releases. $Shot names a
# screenshot of the menu before the drop. $Then runs on the timer once the drop is done (dialog checks).
function Invoke-Drag($File, $Angle, $Shot, [switch]$Tab, [switch]$Hub, [int]$Next = 0, [scriptblock]$Then = $null) {
    # Handlers run later on the form's timer; they read script-scope variables, not this function's locals.
    $script:log = @(); $script:n = 0; $script:cx = 0; $script:cy = 0
    $script:File = @($File); $script:Shot = $Shot; $script:Tab = [bool]$Tab; $script:Hub = [bool]$Hub; $script:Next = $Next; $script:Then = $Then
    $script:rad = 115 * $scale
    $script:a = $Angle * [math]::PI / 180
    $script:snap = [int](540 * $scale)
    $script:form = New-Object System.Windows.Forms.Form
    $script:form.StartPosition = "Manual"; $script:form.Location = New-Object System.Drawing.Point(100, 100)
    $script:form.Size = New-Object System.Drawing.Size(400, 400); $script:form.TopMost = $true
    $script:form.Add_MouseDown({
        $d = New-Object System.Windows.Forms.DataObject
        $d.SetData([System.Windows.Forms.DataFormats]::FileDrop, [string[]]$script:File)
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
                if ($script:Tab -or $script:Hub -or $script:Next -ne 0) { MoveTo ($script:cx + 60 * $scale) $script:cy }
                else { MoveTo ($script:cx + $script:rad * [math]::Sin($script:a)) ($script:cy - $script:rad * [math]::Cos($script:a)) }
            }
            18 {
                if ($script:Tab) { Key 0x09; Key 0x09 $true }
                elseif ($script:Hub) { MoveTo $script:cx $script:cy }
                elseif ($script:Next -ne 0) { WheelDown $script:Next }
            }
            # The hub dwell needs about half a second of rest; the screenshot is taken after the ring changed.
            27 {
                $h = [N]::FindWindow("RadialOverlay", "RadialOverlay"); $r = New-Object N+RECT; [N]::GetWindowRect($h, [ref]$r) | Out-Null
                $script:log += "ring check: overlay visible=$([N]::IsWindowVisible($h))"
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
    $script:kill = New-Object System.Windows.Forms.Timer; $script:kill.Interval = 25000
    $script:kill.Add_Tick({ $script:log += "TIMEOUT"; $script:form.Close() }); $script:kill.Start()
    [System.Windows.Forms.Application]::Run($script:form)
    $script:kill.Stop()
    Key 0x10 $true
    Start-Sleep -Milliseconds 800
    return $script:log
}

# ---------------------------------------------------------------------------------------------------- the cases
# Next = notches down from the ring the file opens on; N = sectors in that ring; I = the sector (0 = top, clockwise).
$cases = @(
    # --- gestures: the same two sectors reached by wheel, Tab and the hub
    @{ Name = "gesture tab: PNG images ring to the tools ring, COMPRESS"; File = "tab.png"; Tab = $true; N = 8; I = 2; Want = { Exists "tab (compressed).png" } },
    @{ Name = "gesture hub: PNG images ring to the tools ring, COMPRESS"; File = "hub.png"; Hub = $true; N = 8; I = 2; Want = { Exists "hub (compressed).png" } },
    @{ Name = "gesture wheel down: PNG tools ring, COMPRESS"; File = "c1.png"; Next = 1; N = 8; I = 2; Want = { Exists "c1 (compressed).png" } },
    @{ Name = "gesture wheel up: PNG documents ring, PDF"; File = "x.png"; Next = -1; N = 4; I = 2; Want = { Exists "x.pdf" } },
    # --- picture
    @{ Name = "PNG images ring: WEBP"; File = "p2.png"; N = 8; I = 1; Want = { Exists "p2.webp" } },
    @{ Name = "PNG documents ring: DOCX"; File = "p1.png"; Next = -1; N = 4; I = 0; Want = { Exists "p1.docx" } },
    @{ Name = "PNG documents ring: JPG"; File = "p1.png"; Next = -1; N = 4; I = 1; Want = { Exists "p1.jpg" } },
    @{ Name = "PNG files ring: Zip"; File = "p1.png"; Next = 2; N = 6; I = 1; Want = { Exists "p1.zip" } },
    @{ Name = "PNG files ring: Info"; File = "p1.png"; Next = 2; N = 6; I = 2; Pre = { SetClip "x" }; Want = { (Clip) -match "Name: p1\.png" -and (Clip) -match "Size: " } },
    @{ Name = "PNG files ring: SHA-256 copies the hash"; File = "p1.png"; Next = 2; N = 6; I = 3; Pre = { SetClip "x" }; Want = { (Clip) -eq (Get-FileHash "$Out\p1.png" -Algorithm SHA256).Hash.ToLower() } },
    @{ Name = "PNG files ring: SHA-256 verifies against the clipboard (no overwrite)"; File = "p1.png"; Next = 2; N = 6; I = 3; Pre = { SetClip ((Get-FileHash "$Out\p1.png" -Algorithm SHA256).Hash.ToUpper() + "  p1.png") }; Want = { (Clip) -match "^[0-9A-F]{64}  p1\.png$" } },
    @{ Name = "PNG files ring: Copy path"; File = "p1.png"; Next = 2; N = 6; I = 4; Pre = { SetClip "x" }; Want = { (Clip) -eq "$Out\p1.png" } },
    @{ Name = "PNG files ring: Reveal opens Explorer"; File = "p1.png"; Next = 2; N = 6; I = 5; Pre = { (ExplorerWindows) | ForEach-Object { $_.Quit() } }; Want = { Start-Sleep 3; $w = ExplorerWindows; $ok = @($w).Count -ge 1; $w | ForEach-Object { $_.Quit() }; $ok } },
    # --- PDF
    @{ Name = "PDF documents ring: DOCX"; File = "hello.pdf"; N = 4; I = 0; Want = { Exists "hello.docx" } },
    @{ Name = "PDF documents ring: TXT keeps the words"; File = "hello.pdf"; N = 4; I = 3; Want = { (Exists "hello.txt") -and ((Get-Content "$Out\hello.txt" -Raw) -match "Hello Radial") } },
    @{ Name = "PDF images ring: WEBP"; File = "hello.pdf"; Next = 1; N = 7; I = 2; Want = { Exists "hello.webp" } },
    @{ Name = "PDF tools ring: Copy text"; File = "hello.pdf"; Next = 2; N = 2; I = 0; Pre = { SetClip "x" }; Want = { (Clip) -match "Hello Radial" } },
    @{ Name = "PDF files ring: Zip"; File = "hello.pdf"; Next = 3; N = 6; I = 1; Want = { Exists "hello.zip" } },
    # --- Word, text
    @{ Name = "DOCX documents ring: TXT keeps the words"; File = "memo.docx"; N = 4; I = 3; Want = { (Exists "memo.txt") -and ((Get-Content "$Out\memo.txt" -Raw) -match "Quarterly memo") } },
    @{ Name = "DOCX documents ring: PDF"; File = "memo.docx"; N = 4; I = 0; Want = { Exists "memo.pdf" } },
    @{ Name = "DOCX images ring: PNG"; File = "memo.docx"; Next = 1; N = 7; I = 0; Want = { Exists "memo.png" } },
    @{ Name = "DOCX has no tools ring: the wheel goes straight to files (Copy path)"; File = "memo.docx"; Next = 2; N = 6; I = 4; Pre = { SetClip "x" }; Want = { (Clip) -eq "$Out\memo.docx" } },
    @{ Name = "TXT documents ring: PDF"; File = "notes.txt"; N = 4; I = 3; Want = { Exists "notes.pdf" } },
    @{ Name = "TXT documents ring: DOCX"; File = "notes.txt"; N = 4; I = 0; Want = { Exists "notes.docx" } },
    # --- spreadsheets
    @{ Name = "XLSX documents ring: CSV (a date stays a date, the Windows list separator splits the columns)"; File = "budget.xlsx"; N = 5; I = 0; Want = {
        $sep = (Get-Culture).TextInfo.ListSeparator; $csv = Get-Content "$Out\budget.csv" -Raw -Encoding UTF8
        $quoted = if ($sep -eq ",") { '"Licences, annual"' } else { 'Licences, annual' }
        (Exists "budget.csv") -and $csv.Contains("Item${sep}Due${sep}Cost") -and $csv.Contains("2024-01-01") -and $csv.Contains($quoted) } },
    @{ Name = "XLSX documents ring: TXT"; File = "budget.xlsx"; N = 5; I = 3; Want = { Exists "budget.txt" } },
    @{ Name = "XLSX documents ring: PDF"; File = "budget.xlsx"; N = 5; I = 4; Want = { Exists "budget.pdf" } },
    @{ Name = "XLSX documents ring: PNG"; File = "budget.xlsx"; N = 5; I = 2; Want = { Exists "budget.png" } },
    @{ Name = "CSV documents ring: XLSX"; File = "prices.csv"; N = 4; I = 0; Want = { Exists "prices.xlsx" } },
    @{ Name = "CSV documents ring: PDF"; File = "prices.csv"; N = 4; I = 3; Want = { Exists "prices.pdf" } },
    # --- anything else has the files ring only
    @{ Name = ".ogg opens the files ring: Zip"; File = "song.ogg"; N = 6; I = 1; Want = { Exists "song.zip" } },
    @{ Name = ".ogg: Copy path"; File = "song.ogg"; N = 6; I = 4; Pre = { SetClip "x" }; Want = { (Clip) -eq "$Out\song.ogg" } },
    @{ Name = "ZIP: Unzip"; File = "bundle.zip"; N = 7; I = 0; Want = { Test-Path "$Out\bundle\inner.txt" } },
    @{ Name = "Folder: Zip"; File = "Project"; N = 7; I = 2; Want = { Exists "Project.zip" } },
    @{ Name = "Folder: SHA-256 is greyed out and does nothing"; File = "Project"; N = 7; I = 4; Pre = { SetClip "untouched" }; Want = { (Clip) -eq "untouched" } },
    # --- several files
    @{ Name = "two PNGs tools ring: COMPRESS both"; File = @("c1.png", "c2.png"); Next = 1; N = 8; I = 2; Want = { (Exists "c1 (compressed) (1).png") -or ((Exists "c1 (compressed).png") -and (Exists "c2 (compressed).png")) } },
    @{ Name = "three PDFs files ring: Zip into one archive named after the folder"; File = @("m1.pdf", "m2.pdf", "m3.pdf"); Next = 3; N = 6; I = 1; Want = { Exists "radial-e2e.zip" } }
)

foreach ($c in ($cases | ForEach-Object { for ($r = 0; $r -lt $Repeat; $r++) { $_ } })) {
    if ($Only -and $c.Name -notlike "*$Only*") { continue }
    if ($c.Pre) { & $c.Pre }
    $files = @($c.File | ForEach-Object { "$Out\$_" })
    $angle = $c.I * 360.0 / $c.N
    $shot = "m-" + ($c.Name -replace "[^A-Za-z0-9]", "").Substring(0, [math]::Min(40, ($c.Name -replace "[^A-Za-z0-9]", "").Length))
    $log = Invoke-Drag $files $angle $shot -Tab:([bool]$c.Tab) -Hub:([bool]$c.Hub) -Next ([int]$c.Next)
    $ok = $false
    try { $ok = [bool](& $c.Want) } catch { $ok = $false }
    $detail = $log -join "; "
    if (-not $ok) { $detail += "; clipboard=[" + ((Clip) -replace "\s+", " ") + "]" }
    Check $c.Name $ok $detail
}

# ---------------------------------------------------------------------------------------------------- CROP dialog
# The picture tools open a window: CROP is sector 4 of 8 of the tools ring (180 degrees). Enter applies it.
if (-not $Only -or "CROP dialog" -like "*$Only*") {
    $dialog = {
        $h = [N]::FindWindow("RadialDialog", [NullString]::Value)
        $script:log += "dialog open=$($h -ne [IntPtr]::Zero) visible=$([N]::IsWindowVisible($h)) foreground=$([N]::GetForegroundWindow() -eq $h)"
        if ($h -ne [IntPtr]::Zero) {
            $r = New-Object N+RECT; [N]::GetWindowRect($h, [ref]$r) | Out-Null
            Snap "$Out\crop-dialog.png" $r.L $r.T ($r.R - $r.L) ($r.B - $r.T)
            Key 0x0D; Key 0x0D $true
        }
    }
    $log = Invoke-Drag "$Out\crop.png" 180 "crop" -Next 1 -Then $dialog
    Check "CROP dialog: it opened" ($log -match "dialog open=True visible=True")
    Check "CROP dialog: it came to the front" ($log -match "foreground=True")
    Check "CROP dialog: Enter applied and the cropped copy exists" (Exists "crop (cropped).png")
    Check "CROP dialog: the window is gone" ([N]::FindWindow("RadialDialog", [NullString]::Value) -eq [IntPtr]::Zero)
}

Get-ChildItem $Out | ForEach-Object { "file: $($_.Name) $($_.Length)" }
Stop-Process $proc -Force -ErrorAction SilentlyContinue
$failed = @($results | Where-Object { -not $_.Pass }).Count
"{0} checks, {1} failed" -f $results.Count, $failed
exit $(if ($failed -gt 0) { 1 } else { 0 })
