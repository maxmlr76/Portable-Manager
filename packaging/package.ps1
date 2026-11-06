# Assembles the stick-ready layout into dist\PortableManager\.
# Run:  cargo build --release -p maxtrix-app   (once), then this script.
# Optional Signierung: -Pfx <datei> [-Password <pw>] | -Thumbprint <hash> | -SelfSigned
# -OutDir <ordner>: anderes Ziel statt dist (z. B. wenn die App gerade aus dist laeuft).
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
    throw "Release-Build fehlt: $rel  ->  cargo build --release -p maxtrix-app"
}

# Der Zielordner wird komplett neu angelegt. Wird er schon benutzt (Einstellungen, Tools),
# abbrechen statt diese Daten zu loeschen.
if (Test-Path $dist) {
    $inUse = (Test-Path (Join-Path $dist ".maxtrix-config.json")) -or
        @(Get-ChildItem (Join-Path $dist "Tools") -Directory -ErrorAction SilentlyContinue).Count -gt 0
    if ($inUse) {
        throw "$dist wird benutzt (Einstellungen oder Tools darin). Anderes Ziel mit -OutDir waehlen."
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
Copy-Item (Join-Path $root "packaging\README.txt") (Join-Path $dist "LIESMICH.txt")
Copy-Item (Join-Path $root "THIRD-PARTY-NOTICES.txt") $dist
Copy-Item (Join-Path $root "LICENSE") (Join-Path $licenses "LICENSE.txt")
Copy-Item (Join-Path $root "TRADEMARKS.md") $licenses
Copy-Item (Join-Path $root "crates\app\ui\OFL-Saira.txt") $licenses
Copy-Item (Join-Path $root "crates\app\ui\OFL-Audiowide.txt") $licenses
& (Join-Path $PSScriptRoot "rust-notices.ps1") -OutFile (Join-Path $licenses "RUST-CRATES.txt")
Set-Content (Join-Path $dist "Tools\_hier-landen-deine-tools.txt") `
    "Ueber die App installierte Tools landen hier unter <Kategorie>\<Tool>\."

# Optional: EXE signieren, bevor sie ins Zip wandert.
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
Write-Output "Paket (zip)           : $zipMb MB  ->  $zip"
Write-Output "Ordner                : $dist"
Write-Output "--- Inhalt ---"
Get-ChildItem -Recurse $dist | ForEach-Object { $_.FullName.Substring($dist.Length + 1) }
