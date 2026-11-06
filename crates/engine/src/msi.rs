//! Unpacking of `.msi` packages without installing them.
//!
//! Uses Windows Installer's *administrative install* (`msiexec /a`), which only
//! lays out the package's files under `TARGETDIR` — no product registration, no
//! admin rights needed for a user-writable target. This is also Scoop's default
//! MSI strategy. The result is then treated like an extracted archive
//! (`extract_dir` selects the subfolder, e.g. `Files\7-Zip`).

use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::fsutil;

/// `ERROR_INSTALL_ALREADY_RUNNING`: another Windows Installer operation is busy.
const MSI_BUSY: i32 = 1618;
/// `ERROR_SUCCESS_REBOOT_REQUIRED` — irrelevant for an administrative image.
const MSI_OK_REBOOT: i32 = 3010;

/// Lay out the package `msi` and move its files (optionally only `extract_dir`)
/// into `dest`. The layout happens in the host's temp dir, not next to the tool:
/// Windows Installer is limited to 260-character paths, and a stick root chosen
/// deep inside a folder tree plus a package's own folders exceeds that
/// (error 1603 — seen with Eraser).
pub fn extract_msi_file(msi: &Path, dest: &Path, extract_dir: Option<&Path>) -> Result<()> {
    let out = fsutil::unique_temp_dir("maxtrix-msi")?;
    let result = (|| -> Result<()> {
        let mut code = run_admin_install(msi, &out)?;
        // The installer service allows one operation at a time — wait and retry.
        for _ in 0..4 {
            if code != MSI_BUSY {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(3));
            code = run_admin_install(msi, &out)?;
        }
        if code != 0 && code != MSI_OK_REBOOT {
            bail!("msiexec /a meldet Fehler {code}");
        }

        // The administrative image also contains a copy of the .msi — not wanted.
        if let Some(name) = msi.file_name() {
            let _ = fs::remove_file(out.join(name));
        }

        let src = match extract_dir {
            Some(ed) => out.join(ed),
            None => out.clone(),
        };
        if !src.is_dir() {
            bail!(
                "Ordner '{}' nicht im MSI-Paket gefunden",
                extract_dir.map(|p| p.display().to_string()).unwrap_or_default()
            );
        }
        fsutil::move_merge(&src, dest).context("Entpackte Dateien nicht verschiebbar")?;
        Ok(())
    })();
    let _ = fs::remove_dir_all(&out);
    result
}
#[cfg(windows)]
fn run_admin_install(msi: &Path, out: &Path) -> Result<i32> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // msiexec parses its own command line: the property must read TARGETDIR="…",
    // which Rust's per-argument quoting would mangle — hence raw_arg.
    let status = Command::new("msiexec.exe")
        .raw_arg(format!("/a \"{}\" /qn TARGETDIR=\"{}\"", msi.display(), out.display()))
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .context("msiexec konnte nicht gestartet werden")?;
    status.code().ok_or_else(|| anyhow!("msiexec wurde abgebrochen"))
}

#[cfg(not(windows))]
fn run_admin_install(_msi: &Path, _out: &Path) -> Result<i32> {
    bail!("MSI-Pakete lassen sich nur unter Windows entpacken")
}
