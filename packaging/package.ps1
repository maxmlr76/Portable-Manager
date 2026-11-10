# Assembles the stick-ready layout into dist\PortableManager\.
# Run:  cargo build --release -p maxtrix-app   (once), then this script.
# Optional signing: -Pfx <file> [-Password <pw>] | -Thumbprint <hash> | -SelfSigned
# -OutDir <folder>: another target instead of dist (e.g. when the app is running from dist).
param(
  [string]$Pfx,
  [string]$Password,
  [string]$Thumbprint,
  [switch]$SelfSigned,
  [string]$OutDir
)
$ErrorActionPreference = "Stop"

$root = Split-Path $PSScriptRoot -Parent
$rel  = Join-Path $root "target\release\maxtrix-app.exe"
if (-not $OutDir) { $OutDir = Join-Path $root "dist" }
$OutDir = [IO.Path]::GetFullPath($OutDir, (Get-Location).Path)
$dist = Join-Path $OutDir "PortableManager"

if (-not (Test-Path $rel)) {
    throw "Release build missing: $rel  ->  cargo build --release -p maxtrix-app"
}

# The target folder is created from scratch. If it is in use (settings, tools),
# stop instead of deleting that data.
if (Test-Path $dist) {
    $inUse = (Test-Path (Join-Path $dist ".maxtrix-config.json")) -or
        @(Get-ChildItem (Join-Path $dist "Tools") -Directory -ErrorAction SilentlyContinue).Count -gt 0
    if ($inUse) {
        throw "$dist is in use (settings or tools inside). Choose another target with -OutDir."
    }
    Remove-Item -Recurse -Force $dist
}

& (Join-Path $PSScriptRoot "fetch-7zip.ps1")

New-Item -ItemType Directory -Force (Join-Path $dist "vendor\7-zip") | Out-Null
New-Item -ItemType Directory -Force (Join-Path $dist "Tools") | Out-Null
$licenses = Join-Path $dist "licenses"
New-Item -ItemType Directory -Force $licenses | Out-Null

Copy-Item $rel (Join-Path $dist "PortableManager.exe")
foreach ($name in "7z.exe", "7z.dll", "License.txt") {
    Copy-Item (Join-Path $root "vendor\7-zip\$name") (Join-Path $dist "vendor\7-zip\")
}
Copy-Item (Join-Path $root "packaging\README.txt") (Join-Path $dist "README.txt")
Copy-Item (Join-Path $root "packaging\LIESMICH.txt") (Join-Path $dist "LIESMICH.txt")
Copy-Item (Join-Path $root "THIRD-PARTY-NOTICES.txt") $dist
Copy-Item (Join-Path $root "LICENSE") (Join-Path $licenses "LICENSE.txt")
Copy-Item (Join-Path $root "TRADEMARKS.md") $licenses
Copy-Item (Join-Path $root "crates\app\ui\OFL-Saira.txt") $licenses
Copy-Item (Join-Path $root "crates\app\ui\OFL-Audiowide.txt") $licenses
& (Join-Path $PSScriptRoot "rust-notices.ps1") -OutFile (Join-Path $licenses "RUST-CRATES.txt")
Set-Content (Join-Path $dist "Tools\_your-tools-go-here.txt") `
    "Tools installed through the app are stored here under <category>\<tool>\."

# Optional: sign the EXE before it goes into the zip.
$exe = Join-Path $dist "PortableManager.exe"
$sign = Join-Path $PSScriptRoot "sign.ps1"
if ($Pfx) {
    & $sign -File $exe -Pfx $Pfx -Password $Password
} elseif ($Thumbprint) {
    & $sign -File $exe -Thumbprint $Thumbprint
} elseif ($SelfSigned) {
    & $sign -File $exe -SelfSigned
}

# Zip for easy transfer
$zip = Join-Path $OutDir "PortableManager.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path $dist -DestinationPath $zip -Force

# Report
$exeMb = [math]::Round((Get-Item (Join-Path $dist "PortableManager.exe")).Length / 1MB, 1)
$zipMb = [math]::Round((Get-Item $zip).Length / 1MB, 1)
Write-Output "PortableManager.exe : $exeMb MB"
Write-Output "Package (zip)         : $zipMb MB  ->  $zip"
Write-Output "Folder                : $dist"
Write-Output "--- Contents ---"
Get-ChildItem -Recurse $dist | ForEach-Object { $_.FullName.Substring($dist.Length + 1) }
