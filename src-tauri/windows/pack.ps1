# Pack Pathfinder for Windows (NSIS + MSI). Replaces `cargo tauri build`.
# Run from src-tauri on windows-latest after Rust toolchain is available.
param(
    [string]$Version = ""
)

$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot\..
$root = (Get-Location).Path

if (-not $Version) {
    $toml = Get-Content .\Cargo.toml -Raw
    # Prefer [package] version, not a dependency version later in the file.
    if ($toml -match '(?ms)^\[package\].*?^version\s*=\s*"([^"]+)"') {
        $Version = $Matches[1]
    } elseif ($toml -match 'version\s*=\s*"([^"]+)"') {
        $Version = $Matches[1]
    } else {
        throw "Could not read version from Cargo.toml"
    }
}

foreach ($tool in @("makensis", "candle", "light")) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        throw "$tool not found on PATH. Install NSIS and WiX Toolset v3."
    }
}

Write-Host "Building Pathfinder $Version (release)..."
cargo build --release --locked
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$stage = Join-Path $root "target\release\stage"
New-Item -ItemType Directory -Force -Path $stage | Out-Null
Copy-Item -Force (Join-Path $root "target\release\pathfinder.exe") (Join-Path $stage "pathfinder.exe")
Copy-Item -Force (Join-Path $root "pdfium\pdfium.dll") (Join-Path $stage "pdfium.dll")

$nsisOut = Join-Path $root "target\release\bundle\nsis"
$msiOut = Join-Path $root "target\release\bundle\msi"
New-Item -ItemType Directory -Force -Path $nsisOut | Out-Null
New-Item -ItemType Directory -Force -Path $msiOut | Out-Null

$setup = Join-Path $nsisOut "Pathfinder_${Version}_x64-setup.exe"
$exe = Join-Path $stage "pathfinder.exe"
$pdfium = Join-Path $stage "pdfium.dll"
$icon = Join-Path $root "icons\icon.ico"
$nsi = Join-Path $root "windows\installer.nsi"

# makensis changes into the script directory unless /NOCD is passed, so always
# feed absolute paths for inputs/outputs and the installer icon.
Write-Host "Building NSIS installer -> $setup"
& makensis /NOCD /DVERSION=$Version `
    "/DOUTFILE=$setup" `
    "/DMAINBINARYPATH=$exe" `
    "/DPDFIUM_PATH=$pdfium" `
    "/DICON_PATH=$icon" `
    "$nsi"
if ($LASTEXITCODE -ne 0) { throw "makensis failed" }

Write-Host "Building MSI..."
$wixObj = Join-Path $root "pathfinder.wixobj"
$wxs = Join-Path $root "windows\pathfinder.wxs"
& candle -nologo -arch x64 "-dVersion=$Version" "-dSourceDir=$stage" "-dIconPath=$icon" -out $wixObj $wxs
if ($LASTEXITCODE -ne 0) { throw "candle failed" }
$msi = Join-Path $msiOut "Pathfinder_${Version}_x64_en-US.msi"
& light -nologo -sice:ICE91 -out $msi $wixObj
if ($LASTEXITCODE -ne 0) { throw "light failed" }
Remove-Item -Force $wixObj -ErrorAction SilentlyContinue

Write-Host "Done."
Write-Host "  NSIS: $setup"
Write-Host "  MSI:  $msi"
