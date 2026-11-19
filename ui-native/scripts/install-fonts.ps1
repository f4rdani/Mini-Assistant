# Installs the three bundled Google Fonts for the CURRENT user only.
#
# No administrator rights required: the files go into
#   %LOCALAPPDATA%\Microsoft\Windows\Fonts
# and are registered under
#   HKCU\Software\Microsoft\Windows NT\CurrentVersion\Fonts
#
# Slint reads the system font collection, so once the fonts are registered the
# UI picks them up automatically (start the app again afterwards).
#
# Run:  powershell -ExecutionPolicy Bypass -File .\scripts\install-fonts.ps1
# Undo: powershell -ExecutionPolicy Bypass -File .\scripts\install-fonts.ps1 -Uninstall

param([switch]$Uninstall)

$ErrorActionPreference = "Stop"

$fontDir = Join-Path $env:LOCALAPPDATA "Microsoft\Windows\Fonts"
$regKey  = "HKCU:\Software\Microsoft\Windows NT\CurrentVersion\Fonts"

# name-in-registry  <-  source file
$fonts = @(
    @{ Reg = "Space Grotesk (TrueType)";      File = "SpaceGrotesk-Variable.ttf" },
    @{ Reg = "Plus Jakarta Sans (TrueType)";  File = "PlusJakartaSans-Variable.ttf" },
    @{ Reg = "JetBrains Mono (TrueType)";     File = "JetBrainsMono-Variable.ttf" }
)

$src = Join-Path (Split-Path -Parent $PSScriptRoot) "assets\fonts"

if (-not (Test-Path $regKey)) {
    New-Item -Path $regKey -Force | Out-Null
}

if ($Uninstall) {
    foreach ($f in $fonts) {
        $target = Join-Path $fontDir $f.File
        if (Test-Path $target) { Remove-Item $target -Force }
        Remove-ItemProperty -Path $regKey -Name $f.Reg -ErrorAction SilentlyContinue
        Write-Host "removed  $($f.Reg)"
    }
    Write-Host "`nFonts uninstalled for the current user."
    exit 0
}

New-Item -ItemType Directory -Force -Path $fontDir | Out-Null

foreach ($f in $fonts) {
    $source = Join-Path $src $f.File
    if (-not (Test-Path $source)) {
        Write-Warning "missing source: $source"
        continue
    }
    $target = Join-Path $fontDir $f.File
    Copy-Item $source $target -Force
    New-ItemProperty -Path $regKey -Name $f.Reg -Value $target -PropertyType String -Force | Out-Null
    Write-Host "installed $($f.Reg)  ->  $target"
}

# Ask every running app to refresh its font cache.
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class FontBroadcast {
    [DllImport("user32.dll", CharSet = CharSet.Auto)]
    public static extern IntPtr SendMessageTimeout(
        IntPtr hWnd, uint Msg, IntPtr wParam, IntPtr lParam,
        uint fuFlags, uint uTimeout, out IntPtr lpdwResult);
}
"@ -ErrorAction SilentlyContinue

$result = [IntPtr]::Zero
[FontBroadcast]::SendMessageTimeout(
    [IntPtr]0xffff, 0x001D, [IntPtr]::Zero, [IntPtr]::Zero, 2, 1000, [ref]$result) | Out-Null

Write-Host "`nDone. Start mini-assistant-ui again to use the bundled typography."
