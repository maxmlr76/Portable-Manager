//! Extraction of non-ZIP archives (real .7z, NSIS/Inno installers, MSI, …) via a
//! 7-Zip binary. Windows-first: the app ships `vendor/7-zip/7z.exe`; we also probe
//! `$MAXTRIX_7Z` and common install locations so dev machines work out of the box.

use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::fsutil;

/// Locate a usable 7-Zip executable, or `None` if none is available.
pub fn find_7z() -> Option<PathBuf> {
    // 1. explicit override
    if let Ok(p) = std::env::var("MAXTRIX_7Z") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    // 2. bundled next to our own executable (the stick-root layout in production)
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for cand in ["vendor/7-zip/7z.exe", "vendor/7-zip/7za.exe"] {
                let p = dir.join(cand);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    // 3. Development only: in a release this would run a planted 7z.exe from any folder.
    #[cfg(debug_assertions)]
    for cand in ["vendor/7-zip/7z.exe", "vendor/7-zip/7za.exe"] {
        let p = PathBuf::from(cand);
        if p.is_file() {
            return Some(p);
        }
    }
    // 4. common install locations
    for p in [
        r"C:\Program Files\7-Zip\7z.exe",
        r"C:\Program Files (x86)\7-Zip\7z.exe",
    ] {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Extract the archive file `archive` into `dest`, optionally keeping only the
/// contents of a top-level `extract_dir`. 7-Zip unpacks into a work folder under
/// `work_parent` (same volume as `dest`), whose contents are then *moved* into
/// place — no second copy of the data.
pub fn extract_7z_file(archive: &Path, dest: &Path, extract_dir: Option<&Path>, work_parent: &Path) -> Result<()> {
    let seven = find_7z().ok_or_else(|| {
        anyhow!(crate::tr!("7-Zip not found: ship vendor/7-zip/7z.exe or set MAXTRIX_7Z", "kein 7-Zip gefunden — vendor/7-zip/7z.exe mitliefern oder MAXTRIX_7Z setzen"))
    })?;
    let out = fsutil::unique_dir(work_parent, "x")?;
    let result = (|| -> Result<()> {
        // 7-Zip detects the real format (7z, NSIS, Inno, …) by content signature.
        let mut cmd = Command::new(&seven);
        cmd.arg("x")
            .arg("-y")
            .arg(format!("-o{}", out.display()))
            .arg(archive);
        // Don't flash a console window when launched from the GUI (windows subsystem).
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let output = cmd
            .output()
            .with_context(|| crate::tr!("running 7-Zip failed ({})", "7-Zip-Aufruf fehlgeschlagen ({})", seven.display()))?;

        // Exit codes: 0 = ok, 1 = warning (non-fatal), >=2 = fatal.
        let code = output.status.code().unwrap_or(-1);
        if code != 0 && code != 1 {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let msg = if !stderr.trim().is_empty() {
                stderr.trim().to_string()
            } else {
                stdout.trim().to_string()
            };
            bail!("7-Zip Exit {code}: {msg}");
        }

        let src = match extract_dir {
            Some(ed) => out.join(ed),
            None => out.clone(),
        };
        if !src.is_dir() {
            bail!(crate::tr!(
                "extract_dir '{}' not found in the archive",
                "extract_dir '{}' nicht im Archiv gefunden",
                extract_dir.map(|p| p.display().to_string()).unwrap_or_default()
            ));
        }
        fsutil::move_merge(&src, dest).context(crate::tr!("could not move the unpacked files", "Entpackte Dateien nicht verschiebbar"))?;
        Ok(())
    })();
    let _ = fs::remove_dir_all(&out);
    result
}