# Pack Pathfinder for Windows (NSIS + MSI). Replaces `cargo tauri build`.
# Run from src-tauri on windows-latest after Rust toolchain is available.
param(
    [string]$Version = ""
)

$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot\..

if (-not $Version) {
    $toml = Get-Content .\Cargo.toml -Raw
    if ($toml -match 'version\s*=\s*"([^"]+)"') {
        $Version = $Matches[1]
    } else {
        throw "Could not read version from Cargo.toml"
    }
}

Write-Host "Building Pathfinder $Version (release)..."
cargo build --release --locked
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$stage = "target\release\stage"
New-Item -ItemType Directory -Force -Path $stage | Out-Null
Copy-Item -Force "target\release\pathfinder.exe" "$stage\pathfinder.exe"
Copy-Item -Force "pdfium\pdfium.dll" "$stage\pdfium.dll"

$nsisOut = "target\release\bundle\nsis"
$msiOut = "target\release\bundle\msi"
New-Item -ItemType Directory -Force -Path $nsisOut | Out-Null
New-Item -ItemType Directory -Force -Path $msiOut | Out-Null

$setup = Join-Path (Resolve-Path $nsisOut) "Pathfinder_${Version}_x64-setup.exe"
Write-Host "Building NSIS installer -> $setup"
& makensis /DVERSION=$Version `
    "/DOUTFILE=$setup" `
    "/DMAINBINARYPATH=$stage\pathfinder.exe" `
    "/DPDFIUM_PATH=$stage\pdfium.dll" `
    "windows\installer.nsi"
if ($LASTEXITCODE -ne 0) { throw "makensis failed" }

Write-Host "Building MSI..."
$wixObj = "pathfinder.wixobj"
& candle -nologo -arch x64 "-dVersion=$Version" "-dSourceDir=$stage" -out $wixObj "windows\pathfinder.wxs"
if ($LASTEXITCODE -ne 0) { throw "candle failed" }
$msi = Join-Path (Resolve-Path $msiOut) "Pathfinder_${Version}_x64_en-US.msi"
& light -nologo -out $msi $wixObj
if ($LASTEXITCODE -ne 0) { throw "light failed" }
Remove-Item -Force $wixObj -ErrorAction SilentlyContinue

Write-Host "Done."
Write-Host "  NSIS: $setup"
Write-Host "  MSI:  $msi"
