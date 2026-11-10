# Code signing & SmartScreen

## What signing does (and what it doesn't)

The EXE already carries an **icon and version metadata** (company "Maxtrix", product
name, version), which makes it recognisable. For the **SmartScreen warning** at first
start on other PCs, though, what counts is the *reputation* of the signing certificate:

| Option | Rough cost | SmartScreen |
|---|---|---|
| **EV certificate** (hardware token/HSM) | a few hundred €/year | clean right away |
| **OV certificate** (standard) | lower than EV | only after some download reputation |
| **Microsoft Artifact Signing** (formerly Azure Trusted Signing) | small monthly fee | like OV, no hardware token |
| **Self-signed** (`-SelfSigned`) | free | internal only; other PCs keep warning |

> A certificate must be **issued to a person or organisation** and paid for. Only
> the owner can do that. Everything else (signing, timestamp, verification) is a
> one-liner with the scripts here.

## Signing

`packaging/sign.ps1` finds `signtool.exe` automatically (Windows SDK), signs with
SHA-256, adds an RFC 3161 timestamp (the signature stays valid after the certificate
expires) and verifies the result.

```powershell
# With a PFX file (OV/EV export)
pwsh packaging\sign.ps1 -Pfx path\to\cert.pfx -Password secret

# With a certificate from the Windows certificate store
pwsh packaging\sign.ps1 -Thumbprint A1B2C3D4...

# Test signature (self-signed, internal only, NO SmartScreen approval)
pwsh packaging\sign.ps1 -SelfSigned
```

Without `-File` the script signs `dist\PortableManager\PortableManager.exe`
(else `target\release\maxtrix-app.exe`). Use `-File <path>` to sign a specific EXE.

## Build, sign and package in one go

```powershell
cargo build --release -p maxtrix-app
pwsh packaging\package.ps1 -Pfx cert.pfx -Password secret     # signs the EXE in dist, then zips
# or: -Thumbprint <hash>  /  -SelfSigned  /  none at all = unsigned
```

## Microsoft Artifact Signing (no hardware token)

Microsoft's signing service: create an account with a verified identity in Azure, a
signing account and a certificate profile, then sign with its signtool plug-in. Worth
it when there is no EV token. Details: https://learn.microsoft.com/azure/trusted-signing/

## Icon / metadata

Embedded at build time (`crates/app/build.rs` via `winresource`, source
`crates/app/assets/maxtrix.ico`). New icon: replace the `.ico` and rebuild. Without
`rc.exe` (Windows SDK) the app still builds, just without icon and metadata (a build
warning says so).
