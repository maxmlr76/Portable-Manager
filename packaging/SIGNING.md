# Code-Signierung & SmartScreen

## Was Signieren bringt (und was nicht)

Die EXE trägt bereits **Icon + Versions-Metadaten** (Firma „Maxtrix", Produktname,
Version) — das macht sie schon erkennbar. Für die **SmartScreen-Warnung** beim ersten
Start auf fremden PCs zählt aber die *Reputation* des Signatur-Zertifikats:

| Weg | Kosten | SmartScreen |
|---|---|---|
| **EV-Zertifikat** (Hardware-Token/HSM) | ~300–600 €/Jahr | sofort sauber |
| **OV-Zertifikat** (normal) | ~150–300 €/Jahr | erst nach etwas Download-Reputation |
| **Azure Trusted Signing** | ~10 $/Monat | wie OV, ohne Hardware-Token |
| **Selbstsigniert** (`-SelfSigned`) | kostenlos | nur intern; auf fremden PCs weiter Warnung |

> Ein Zertifikat muss **auf einen Namen/eine Organisation ausgestellt** und bezahlt
> werden — das kann nur der Inhaber selbst tun. Der Rest (Signieren, Zeitstempel,
> Verifizieren) ist mit den Skripten hier ein Einzeiler.

## Signieren

`packaging/sign.ps1` findet `signtool.exe` automatisch (Windows SDK), signiert mit
SHA-256, setzt einen RFC-3161-Zeitstempel (Signatur bleibt nach Ablauf des Zertifikats
gültig) und verifiziert danach.

```powershell
# Mit PFX-Datei (OV/EV-Export)
pwsh packaging\sign.ps1 -Pfx pfad\zu\cert.pfx -Password geheim

# Mit Zertifikat aus dem Windows-Zertifikatspeicher
pwsh packaging\sign.ps1 -Thumbprint A1B2C3D4...

# Test-Signatur (selbstsigniert, nur intern – KEINE SmartScreen-Freigabe)
pwsh packaging\sign.ps1 -SelfSigned
```

Ohne `-File` signiert das Skript `dist\PortableManager\PortableManager.exe`
(sonst `target\release\maxtrix-app.exe`). Mit `-File <pfad>` gezielt eine EXE.

## In einem Rutsch bauen + signieren + packen

```powershell
cargo build --release -p maxtrix-app
pwsh packaging\package.ps1 -Pfx cert.pfx -Password geheim     # signiert die EXE im dist-Ordner, dann Zip
# oder: -Thumbprint <hash>  /  -SelfSigned  /  ganz ohne = unsigniert
```

## Azure Trusted Signing (Alternative ohne Hardware-Token)

Microsofts Dienst (früher „Azure Code Signing"): Konto + verifizierte Identität in Azure
anlegen, „Trusted Signing Account" + „Certificate Profile" erstellen, dann mit der
`Azure.CodeSigning`-Dlib und signtool signieren. Lohnt sich, wenn kein EV-Token da ist.
Details: https://learn.microsoft.com/azure/trusted-signing/

## Icon / Metadaten

Werden beim Build eingebettet (`crates/app/build.rs` via `winresource`, Quelle
`crates/app/assets/maxtrix.ico`). Neues Icon: `.ico` ersetzen und neu bauen. Fehlt die
`rc.exe` (Windows SDK), baut die App trotzdem — nur ohne Icon/Metadaten (Warnung im Build).
