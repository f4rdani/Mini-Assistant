param([switch]$CleanData)

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
$DataDir = Join-Path $PortableDir "data"

Write-Host "=== [3/4] Menyusun Paket Portable (dist\Mini-Assistant-Portable) ===" -ForegroundColor Cyan
$AssetsDir = Join-Path $PortableDir "assets"
if ($CleanData -and (Test-Path $PortableDir)) {
    Remove-Item -Recurse -Force $PortableDir
} elseif (Test-Path $AssetsDir) {
    Remove-Item -Recurse -Force $AssetsDir
}
if (Test-Path $ZipPath) {
    Remove-Item -Force $ZipPath
}

$DataPreserved = Test-Path $DataDir
New-Item -ItemType Directory -Force -Path $PortableDir | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $PortableDir "assets\avatar") | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $PortableDir "assets\logos") | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $PortableDir "assets\musik") | Out-Null
# Preserve local data/ (SQLite DB + accounts.json) unless -CleanData is passed
New-Item -ItemType Directory -Force -Path $DataDir | Out-Null

$DstExe = Join-Path $PortableDir "MiniAssistant.exe"
Copy-Item -Force $SrcExe $DstExe

# Copy only runtime-loaded assets (fonts, HUD chrome, panel glow, and window icons are already embedded inside MiniAssistant.exe)
$SrcAssets = Join-Path $RepoRoot "ui-native\assets"
Get-ChildItem -File (Join-Path $SrcAssets "avatar") | Where-Object { $_.Name -ne "brand_glow.png" } | ForEach-Object {
    Copy-Item -Force $_.FullName (Join-Path $PortableDir "assets\avatar\$($_.Name)")
}
Copy-Item -Force (Join-Path $SrcAssets "logos\*") (Join-Path $PortableDir "assets\logos\")
Copy-Item -Force (Join-Path $SrcAssets "musik\*") (Join-Path $PortableDir "assets\musik\")

Write-Host "=== [4/4] Membuat Arsip ZIP Portable (Isolated Clean Data) ===" -ForegroundColor Cyan
$ZipStage = Join-Path $env:TEMP ("MiniAssistZipStage_" + $PID)
try {
    if (Test-Path $ZipStage) {
        Remove-Item -Recurse -Force $ZipStage
    }
    New-Item -ItemType Directory -Force -Path $ZipStage | Out-Null
    Copy-Item -Force $DstExe (Join-Path $ZipStage "MiniAssistant.exe")
    Copy-Item -Recurse -Force $AssetsDir (Join-Path $ZipStage "assets")
    # Public release ZIP always ships with a fresh empty data/ folder (zero user cookies/history)
    New-Item -ItemType Directory -Force -Path (Join-Path $ZipStage "data") | Out-Null
    Compress-Archive -Path "$ZipStage\*" -DestinationPath $ZipPath -CompressionLevel Optimal -Force
} finally {
    if (Test-Path $ZipStage) {
        Remove-Item -Recurse -Force $ZipStage -ErrorAction SilentlyContinue
    }
}

$ExeMB = [math]::Round((Get-Item $DstExe).Length / 1MB, 2)
$AssetsFiles = Get-ChildItem -Recurse -File $AssetsDir
$AssetsMB = [math]::Round((($AssetsFiles | Measure-Object Length -Sum).Sum) / 1MB, 2)
$ZipMB = [math]::Round((Get-Item $ZipPath).Length / 1MB, 2)
$DataLabel = if ($DataPreserved) { "Preserved (Sesi & DB lokal dipertahankan)" } else { "Fresh/Kosong - 100% Portable" }

# Verify PE Subsystem (2 = IMAGE_SUBSYSTEM_WINDOWS_GUI, 3 = IMAGE_SUBSYSTEM_WINDOWS_CUI)
$bytes = [System.IO.File]::ReadAllBytes($DstExe)
$peOffset = [System.BitConverter]::ToInt32($bytes, 0x3C)
$subsystem = [System.BitConverter]::ToUInt16($bytes, $peOffset + 0x5C)

Write-Host ""
Write-Host "PAKET PORTABLE SIAP:" -ForegroundColor Green
Write-Host ("  Folder      : {0}" -f $PortableDir)
Write-Host ("  Executable  : MiniAssistant.exe ({0} MB, PE Subsystem={1} [2=WINDOWS_GUI])" -f $ExeMB, $subsystem)
Write-Host ("  Assets      : {0} file runtime ({1} MB)" -f $AssetsFiles.Count, $AssetsMB)
Write-Host ("  Data Dir    : {0} ({1})" -f $DataDir, $DataLabel)
Write-Host ("  ZIP Archive : {0} ({1} MB, clean empty data/)" -f $ZipPath, $ZipMB)
