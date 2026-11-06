<#
  sign.ps1 - Authenticode-Signierung der PortableManager.exe.

  Signieren allein entfernt die SmartScreen-Warnung NICHT sofort. Was hilft:
    * EV-Zertifikat (Hardware-Token/HSM)  -> SmartScreen sofort sauber.
    * OV-Zertifikat                       -> SmartScreen erst nach etwas "Reputation".
    * Azure Trusted Signing (~10 $/Monat) -> wie OV, ohne Hardware-Token.
    * Selbstsigniert (-SelfSigned)        -> NUR fuer interne Tests; auf fremden PCs
                                             weiterhin SmartScreen (Zertifikat ist dort
                                             nicht vertrauenswuerdig).

  Beispiele:
    pwsh packaging\sign.ps1 -Pfx cert.pfx -Password geheim
    pwsh packaging\sign.ps1 -Thumbprint A1B2C3...      # Zertifikat aus dem Zertifikatspeicher
    pwsh packaging\sign.ps1 -SelfSigned                # Testsignatur erzeugen
#>
[CmdletBinding(DefaultParameterSetName = "Pfx")]
param(
  [Parameter(ParameterSetName = "Pfx")]    [string]$Pfx,
  [Parameter(ParameterSetName = "Pfx")]    [string]$Password,
  [Parameter(ParameterSetName = "Store")]  [string]$Thumbprint,
  [Parameter(ParameterSetName = "Self")]   [switch]$SelfSigned,
  [string]$File,
  [string]$TimestampUrl = "http://timestamp.digicert.com"
)
$ErrorActionPreference = "Stop"

$root = Split-Path $PSScriptRoot -Parent
if (-not $File) {
  $cand = @(
    (Join-Path $root "dist\PortableManager\PortableManager.exe"),
    (Join-Path $root "target\release\maxtrix-app.exe")
  ) | Where-Object { Test-Path $_ }
  if (-not $cand) { throw "Keine EXE gefunden. Erst bauen/packen oder -File angeben." }
  $File = $cand[0]
}
if (-not (Test-Path $File)) { throw "Datei nicht gefunden: $File" }

# --- signtool.exe finden (neueste x64-Version aus dem Windows SDK) ---
function Find-SignTool {
  $bases = @("${env:ProgramFiles(x86)}\Windows Kits\10\bin", "${env:ProgramFiles}\Windows Kits\10\bin")
  foreach ($b in $bases) {
    if (Test-Path $b) {
      $hit = Get-ChildItem $b -Recurse -Filter signtool.exe -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match "\\x64\\" } |
        Sort-Object FullName -Descending | Select-Object -First 1
      if ($hit) { return $hit.FullName }
    }
  }
  $c = Get-Command signtool.exe -ErrorAction SilentlyContinue
  if ($c) { return $c.Source }
  throw "signtool.exe nicht gefunden - Windows SDK installieren."
}
$signtool = Find-SignTool
Write-Host "signtool : $signtool" -ForegroundColor DarkGray
Write-Host "Datei    : $File" -ForegroundColor DarkGray

# --- Signieren je nach Modus ---
$common = @("sign", "/fd", "sha256", "/tr", $TimestampUrl, "/td", "sha256", "/v")

switch ($PSCmdlet.ParameterSetName) {
  "Pfx" {
    if (-not $Pfx) { throw "Bitte -Pfx <datei.pfx> [-Password <pw>] angeben (oder -Thumbprint / -SelfSigned)." }
    if (-not (Test-Path $Pfx)) { throw "PFX nicht gefunden: $Pfx" }
    $args = $common + @("/f", $Pfx)
    if ($Password) { $args += @("/p", $Password) }
    $args += $File
  }
  "Store" {
    $args = $common + @("/sha1", $Thumbprint, $File)
  }
  "Self" {
    $subject = "CN=Maxtrix (Test-Signatur), O=Maxtrix"
    $cert = Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert -ErrorAction SilentlyContinue |
      Where-Object { $_.Subject -eq $subject } | Select-Object -First 1
    if (-not $cert) {
      Write-Host "Erzeuge selbstsigniertes Test-Zertifikat ..." -ForegroundColor Yellow
      $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject $subject `
        -CertStoreLocation Cert:\CurrentUser\My -KeyUsage DigitalSignature `
        -KeyExportPolicy Exportable -NotAfter (Get-Date).AddYears(3)
    }
    Write-Host "Zertifikat: $($cert.Thumbprint)" -ForegroundColor DarkGray
    $args = $common + @("/sha1", $cert.Thumbprint, $File)
  }
}

& $signtool @args
if ($LASTEXITCODE -ne 0) { throw "signtool sign fehlgeschlagen (Exit $LASTEXITCODE)." }

Write-Host "`nVerifiziere ..." -ForegroundColor DarkGray
& $signtool verify /pa /v $File
if ($LASTEXITCODE -ne 0) {
  Write-Warning "Verifikation meldet einen Fehler. Bei selbstsignierten Zertifikaten ist das normal (nicht als vertrauenswuerdig hinterlegt)."
} else {
  Write-Host "`nSignatur ok." -ForegroundColor Green
}
