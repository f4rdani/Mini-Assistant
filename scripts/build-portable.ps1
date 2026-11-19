$ErrorActionPreference = "Stop"
$RepoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $RepoRoot

Write-Host "=== [1/4] Menghentikan instance lama (jika ada) ===" -ForegroundColor Cyan
foreach ($procName in @("MiniAssistant", "mini-assistant-ui", "api-ai")) {
    Get-Process -Name $procName -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
}

Write-Host "=== [2/4] Kompilasi Consumer Release (Single-Binary Embedded API, 0 Terminal) ===" -ForegroundColor Cyan
cargo build --release --manifest-path "$RepoRoot\ui-native\Cargo.toml"
if ($LASTEXITCODE -ne 0) {
    throw "cargo build --release gagal dengan kode $LASTEXITCODE"
}

$SrcExe = Join-Path $RepoRoot "ui-native\target\release\mini-assistant-ui.exe"
if (-not (Test-Path $SrcExe)) {
    throw "Binary release tidak ditemukan: $SrcExe"
}

$DistRoot = Join-Path $RepoRoot "dist"
$PortableDir = Join-Path $DistRoot "Mini-Assistant-Portable"
$ZipPath = Join-Path $DistRoot "Mini-Assistant-Portable.zip"

Write-Host "=== [3/4] Menyusun Paket Portable (dist\Mini-Assistant-Portable) ===" -ForegroundColor Cyan
if (Test-Path $PortableDir) {
    Remove-Item -Recurse -Force $PortableDir
}
if (Test-Path $ZipPath) {
    Remove-Item -Force $ZipPath
}

New-Item -ItemType Directory -Force -Path $PortableDir | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $PortableDir "assets\avatar") | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $PortableDir "assets\logos") | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $PortableDir "assets\musik") | Out-Null
# Fresh empty data/ folder triggers 100% Portable Mode (SQLite DB + accounts.json + cache)
New-Item -ItemType Directory -Force -Path (Join-Path $PortableDir "data") | Out-Null

$DstExe = Join-Path $PortableDir "MiniAssistant.exe"
Copy-Item -Force $SrcExe $DstExe

# Copy only runtime-loaded assets (fonts, HUD chrome, panel glow, and window icons are already embedded inside MiniAssistant.exe)
$SrcAssets = Join-Path $RepoRoot "ui-native\assets"
Get-ChildItem -File (Join-Path $SrcAssets "avatar") | Where-Object { $_.Name -ne "brand_glow.png" } | ForEach-Object {
    Copy-Item -Force $_.FullName (Join-Path $PortableDir "assets\avatar\$($_.Name)")
}
Copy-Item -Force (Join-Path $SrcAssets "logos\*") (Join-Path $PortableDir "assets\logos\")
Copy-Item -Force (Join-Path $SrcAssets "musik\*") (Join-Path $PortableDir "assets\musik\")

Write-Host "=== [4/4] Membuat Arsip ZIP Portable ===" -ForegroundColor Cyan
Compress-Archive -Path "$PortableDir\*" -DestinationPath $ZipPath -CompressionLevel Optimal -Force

$ExeMB = [math]::Round((Get-Item $DstExe).Length / 1MB, 2)
$AssetsFiles = Get-ChildItem -Recurse -File (Join-Path $PortableDir "assets")
$AssetsMB = [math]::Round((($AssetsFiles | Measure-Object Length -Sum).Sum) / 1MB, 2)
$ZipMB = [math]::Round((Get-Item $ZipPath).Length / 1MB, 2)

# Verify PE Subsystem (2 = IMAGE_SUBSYSTEM_WINDOWS_GUI, 3 = IMAGE_SUBSYSTEM_WINDOWS_CUI)
$bytes = [System.IO.File]::ReadAllBytes($DstExe)
$peOffset = [System.BitConverter]::ToInt32($bytes, 0x3C)
$subsystem = [System.BitConverter]::ToUInt16($bytes, $peOffset + 0x5C)

Write-Host ""
Write-Host "PAKET PORTABLE SIAP:" -ForegroundColor Green
Write-Host ("  Folder      : {0}" -f $PortableDir)
Write-Host ("  Executable  : MiniAssistant.exe ({0} MB, PE Subsystem={1} [2=WINDOWS_GUI])" -f $ExeMB, $subsystem)
Write-Host ("  Assets      : {0} file runtime ({1} MB)" -f $AssetsFiles.Count, $AssetsMB)
Write-Host ("  Data Dir    : {0}\data (Fresh/Kosong - 100% Portable)" -f $PortableDir)
Write-Host ("  ZIP Archive : {0} ({1} MB)" -f $ZipPath, $ZipMB)
