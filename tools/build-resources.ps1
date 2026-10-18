# Compiles assets/radial.rc (manifest, icon, version info) into assets/radial.res, which build.rs links.
# Needs rc.exe from the Windows SDK (installed with Visual Studio Build Tools). The .res is committed, so a normal
# `cargo build` does not need the SDK; run this only after changing the manifest, the icon or the version.
# Usage: powershell -File tools\build-resources.ps1
$pf = [Environment]::GetEnvironmentVariable("ProgramFiles(x86)")
if (-not $pf) { $pf = "C:\Program Files (x86)" }
$kits = Join-Path $pf "Windows Kits\10"
$rc = Get-ChildItem (Join-Path $kits "bin") -Recurse -Filter rc.exe -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -like "*\x64\rc.exe" } | Sort-Object FullName -Descending | Select-Object -First 1
if (-not $rc) { throw "rc.exe not found; install the Windows SDK" }
$inc = Get-ChildItem (Join-Path $kits "Include") -Directory | Sort-Object Name -Descending | Select-Object -First 1
$assets = Join-Path $PSScriptRoot "..\assets"
& $rc.FullName /nologo /I (Join-Path $inc.FullName "um") /I (Join-Path $inc.FullName "shared") /fo (Join-Path $assets "radial.res") (Join-Path $assets "radial.rc")
if ($LASTEXITCODE -ne 0) { throw "rc.exe failed" }
"wrote $(Join-Path $assets 'radial.res')"
