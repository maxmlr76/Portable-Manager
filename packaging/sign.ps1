<#
  sign.ps1 - Authenticode signing of PortableManager.exe.

  Signing alone does NOT remove the SmartScreen warning right away. What helps:
    * EV certificate (hardware token/HSM) -> SmartScreen clean right away.
    * OV certificate                      -> SmartScreen only after some "reputation".
    * Microsoft Artifact Signing          -> like OV, without a hardware token.
    * Self-signed (-SelfSigned)           -> ONLY for internal tests; other PCs keep
                                             showing SmartScreen (the certificate is not
                                             trusted there).

  Examples:
    pwsh packaging\sign.ps1 -Pfx cert.pfx -Password secret
    pwsh packaging\sign.ps1 -Thumbprint A1B2C3...      # certificate from the certificate store
    pwsh packaging\sign.ps1 -SelfSigned                # create a test signature
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
  if (-not $cand) { throw "No EXE found. Build/package first or pass -File." }
  $File = $cand[0]
}
if (-not (Test-Path $File)) { throw "File not found: $File" }

# --- find signtool.exe (newest x64 version from the Windows SDK) ---
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
  throw "signtool.exe not found - install the Windows SDK."
}
$signtool = Find-SignTool
Write-Host "signtool : $signtool" -ForegroundColor DarkGray
Write-Host "File     : $File" -ForegroundColor DarkGray

# --- sign according to the mode ---
$common = @("sign", "/fd", "sha256", "/tr", $TimestampUrl, "/td", "sha256", "/v")

switch ($PSCmdlet.ParameterSetName) {
  "Pfx" {
    if (-not $Pfx) { throw "Pass -Pfx <file.pfx> [-Password <pw>] (or -Thumbprint / -SelfSigned)." }
    if (-not (Test-Path $Pfx)) { throw "PFX not found: $Pfx" }
    $args = $common + @("/f", $Pfx)
    if ($Password) { $args += @("/p", $Password) }
    $args += $File
  }
  "Store" {
    $args = $common + @("/sha1", $Thumbprint, $File)
  }
  "Self" {
    $subject = "CN=Maxtrix (test signature), O=Maxtrix"
    $cert = Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert -ErrorAction SilentlyContinue |
      Where-Object { $_.Subject -eq $subject } | Select-Object -First 1
    if (-not $cert) {
      Write-Host "Creating a self-signed test certificate ..." -ForegroundColor Yellow
      $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject $subject `
        -CertStoreLocation Cert:\CurrentUser\My -KeyUsage DigitalSignature `
        -KeyExportPolicy Exportable -NotAfter (Get-Date).AddYears(3)
    }
    Write-Host "Certificate: $($cert.Thumbprint)" -ForegroundColor DarkGray
    $args = $common + @("/sha1", $cert.Thumbprint, $File)
  }
}

& $signtool @args
if ($LASTEXITCODE -ne 0) { throw "signtool sign failed (exit $LASTEXITCODE)." }

Write-Host "`nVerifying ..." -ForegroundColor DarkGray
& $signtool verify /pa /v $File
if ($LASTEXITCODE -ne 0) {
  Write-Warning "Verification reports an error. That is normal for self-signed certificates (not stored as trusted)."
} else {
  Write-Host "`nSignature ok." -ForegroundColor Green
}
