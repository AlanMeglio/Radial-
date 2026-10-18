# Builds assets/radial.ico from the app's own tray icon renderer (PNG-compressed ICO entries, 16 to 256 px).
# Usage: powershell -File tools\make-icon.ps1 [-Exe target\release\radial.exe]
param([string]$Exe = (Join-Path $PSScriptRoot "..\target\release\radial.exe"))
$sizes = 16, 24, 32, 48, 64, 128, 256
$tmp = Join-Path $env:TEMP "radial-icon"; New-Item $tmp -ItemType Directory -Force | Out-Null
$pngs = foreach ($s in $sizes) {
    $f = Join-Path $tmp "i$s.png"
    Start-Process $Exe -ArgumentList "--tray-icon", "`"$f`"", "--size", $s -Wait -NoNewWindow
    , [System.IO.File]::ReadAllBytes($f)
}
$ms = New-Object System.IO.MemoryStream
$w = New-Object System.IO.BinaryWriter $ms
$w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $s = $sizes[$i]; $dim = if ($s -ge 256) { 0 } else { $s }
    $w.Write([byte]$dim); $w.Write([byte]$dim); $w.Write([byte]0); $w.Write([byte]0)
    $w.Write([uint16]1); $w.Write([uint16]32)
    $w.Write([uint32]$pngs[$i].Length); $w.Write([uint32]$offset)
    $offset += $pngs[$i].Length
}
foreach ($p in $pngs) { $w.Write($p) }
$out = Join-Path $PSScriptRoot "..\assets\radial.ico"
[System.IO.File]::WriteAllBytes($out, $ms.ToArray())
"wrote $out ($($ms.Length) bytes)"
