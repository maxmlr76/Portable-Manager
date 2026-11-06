//! High-level install: stream each artifact to the host's temp dir while hashing
//! it -> verify -> unpack into a *staging* folder next to the tool. The real tool
//! folder is not touched here; `catalog::install_resolved` validates the staging
//! folder, carries user data over and swaps it in (`fsutil::commit_dir`).

use anyhow::{bail, Result};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::manifest::{Artifact, Manifest};
use crate::{extract, fsutil, msi, net, sevenzip, Cancelled};

/// Progress signal emitted during an install so a UI can show status.
pub enum InstallEvent<'a> {
    /// Human-readable phase text (also used for logging).
    Phase(&'a str),
    /// Download progress: bytes received so far and the total, if known.
    Download { done: u64, total: Option<u64> },
}

/// Outcome of an install attempt.
#[derive(Debug)]
pub struct InstallReport {
    pub name: String,
    pub version: String,
    pub bytes: usize,
    pub hash_alg: String,
    pub verified: bool,
    pub archive_kind: String,
    /// Final tool folder `<root>/Tools/<category>/<name>`.
    pub target: PathBuf,
    /// Folder holding the unpacked new version until it is swapped in.
    pub staging: PathBuf,
    pub files: usize,
    /// Set when the artifact verified but its format isn't extractable yet
    /// (the staging folder is already gone then).
    pub pending_reason: Option<String>,
}

/// How an artifact gets unpacked.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Zip,
    SevenZip,
    Msi,
    /// Not an archive (e.g. a portable `rufus.exe`): saved as-is.
    Single,
}

impl Kind {
    fn of(ext: &str) -> Kind {
        match ext {
            "zip" => Kind::Zip,
            "msi" => Kind::Msi,
            e if is_archive_ext(e) => Kind::SevenZip,
            _ => Kind::Single,
        }
    }
    fn label(self, ext: &str) -> String {
        match self {
            Kind::Zip => "zip".into(),
            Kind::Msi => "msi".into(),
            Kind::SevenZip => ext.to_string(),
            Kind::Single => "einzeldatei".into(),
        }
    }
}

/// A manifest-supplied relative path (`extract_to`, `extract_dir`, `persist`)
/// must stay inside its base folder: only plain components, no `..`, no drive/root.
pub(crate) fn safe_rel(p: &str) -> Result<PathBuf> {
    let path = Path::new(p);
    if path.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir)) {
        Ok(path.to_path_buf())
    } else {
        bail!(crate::tr!("illegal path in manifest: '{p}'", "unzulässiger Pfad im Manifest: '{p}'"))
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Cancelled.into())
    } else {
        Ok(())
    }
}

/// Download, verify and unpack `m` into a fresh staging folder
/// `<root>/Tools/<category>/.maxtrix-new-<name>-…` (see module docs). Setting
/// `cancel` aborts at the next chunk / step with a [`Cancelled`] error. On any
/// error the staging folder is removed again.
pub fn install(
    m: &Manifest,
    root: &Path,
    category: &str,
    on: &dyn Fn(InstallEvent),
    cancel: &AtomicBool,
) -> Result<InstallReport> {
    let target = root.join("Tools").join(category).join(&m.name);
    let cat_dir = root.join("Tools").join(category);
    fs::create_dir_all(&cat_dir)?;
    let staging = fsutil::unique_dir(&cat_dir, &format!("new-{}", m.name))?;
    let downloads = fsutil::unique_temp_dir("maxtrix-dl")?;

    let result = stage_artifacts(m, &staging, &downloads, &cat_dir, on, cancel);
    let _ = fs::remove_dir_all(&downloads);

    let mut report = InstallReport {
        name: m.name.clone(),
        version: m.version.clone(),
        bytes: 0,
        hash_alg: m.hash_alg.clone(),
        verified: true,
        archive_kind: Kind::of(&m.ext).label(&m.ext),
        target,
        staging: staging.clone(),
        files: 0,
        pending_reason: None,
    };
    match result {
        Ok(Staged { bytes, pending: None }) => {
            report.bytes = bytes as usize;
            report.files = fsutil::count_files(&staging).unwrap_or(0);
            on(InstallEvent::Phase(&crate::tr!("  {} files unpacked", "  {} Dateien entpackt", report.files)));
            Ok(report)
        }
        Ok(Staged { bytes, pending: Some((kind, reason)) }) => {
            let _ = fs::remove_dir_all(&staging);
            report.bytes = bytes as usize;
            report.archive_kind = kind;
            report.pending_reason = Some(reason);
            Ok(report)
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            Err(e)
        }
    }
}

struct Staged {
    bytes: u64,
    /// (archive kind, reason) when an artifact verified but couldn't be unpacked.
    pending: Option<(String, String)>,
}

fn stage_artifacts(
    m: &Manifest,
    staging: &Path,
    downloads: &Path,
    work_parent: &Path,
    on: &dyn Fn(InstallEvent),
    cancel: &AtomicBool,
) -> Result<Staged> {
    let parts = m.artifacts.len();
    let mut total: u64 = 0;
    for (i, a) in m.artifacts.iter().enumerate() {
        check_cancel(cancel)?;
        let part = if parts > 1 { format!(" (Teil {}/{parts})", i + 1) } else { String::new() };
        on(InstallEvent::Phase(&crate::tr!("Downloading {} v{}{part} …", "Lade {} v{}{part} …", m.name, m.version)));
        on(InstallEvent::Phase(&crate::tr!("  Source: {}", "  Quelle: {}", a.url)));

        // Each part in its own folder: parts may share a file name.
        let dir = downloads.join(i.to_string());
        fs::create_dir_all(&dir)?;
        let file = dir.join(download_name(a));
        let (n, actual) = net::download_to_file(&a.url, &file, &a.hash_alg, &|done, total| {
            on(InstallEvent::Download { done, total })
        }, cancel)?;
        on(InstallEvent::Phase(&crate::tr!("  {} downloaded ({n} bytes)", "  {} heruntergeladen ({n} Bytes)", human(n))));

        on(InstallEvent::Phase(&crate::tr!("  Checking {} …", "  Prüfe {} …", a.hash_alg.to_uppercase())));
        net::check_hash(&a.hash_alg, &a.hash_hex, &actual)?;
        on(InstallEvent::Phase(&crate::tr!("  Hash verified \u{2713}", "  Hash verifiziert \u{2713}")));
        total += n;

        check_cancel(cancel)?;
        on(InstallEvent::Phase(&crate::tr!("  Unpacking …", "  Entpacke …")));
        if let Err(e) = unpack(a, &file, staging, work_parent) {
            let reason = e.to_string();
            on(InstallEvent::Phase(&crate::tr!("  Unpacking skipped: {reason}", "  Entpacken übersprungen: {reason}")));
            return Ok(Staged { bytes: total, pending: Some((Kind::of(&a.ext).label(&a.ext), reason)) });
        }
        // Free the host's temp space right away (a later part may be large too).
        let _ = fs::remove_dir_all(&dir);
    }
    Ok(Staged { bytes: total, pending: None })
}

/// File name to save a download as: the manifest's name without any folder part.
fn download_name(a: &Artifact) -> String {
    Path::new(&a.filename)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "download.bin".to_string())
}

/// Unpack one verified artifact file into the staging dir (honouring `extract_to`).
fn unpack(a: &Artifact, file: &Path, staging: &Path, work_parent: &Path) -> Result<()> {
    let dest = match &a.extract_to {
        Some(sub) => staging.join(safe_rel(sub)?),
        None => staging.to_path_buf(),
    };
    let extract_dir = match &a.extract_dir {
        Some(ed) => Some(safe_rel(ed)?),
        None => None,
    };
    let ed = extract_dir.as_deref();
    match Kind::of(&a.ext) {
        Kind::Zip => {
            extract::extract_zip_file(file, &dest, ed)?;
        }
        Kind::SevenZip => sevenzip::extract_7z_file(file, &dest, ed, work_parent)?,
        Kind::Msi => msi::extract_msi_file(file, &dest, ed)?,
        Kind::Single => {
            // Single portable file: place it directly under the tool folder.
            fs::create_dir_all(&dest)?;
            fsutil::move_path(file, &dest.join(download_name(a)))?;
        }
    }
    Ok(())
}
/// Whether a file extension is an archive/installer 7-Zip should unpack.
/// Matches Scoop's `file_requires_7zip` set (plus a few 7-Zip also handles).
/// A bare `.exe` is deliberately excluded — it is kept as-is; `.msi` has its
/// own path via `msiexec /a`.
fn is_archive_ext(ext: &str) -> bool {
    matches!(
        ext,
        "7z" | "gz" | "tar" | "tgz" | "tbz" | "tbz2" | "taz" | "txz"
            | "bz2" | "bz" | "lzma" | "xz" | "zst" | "001" | "rar" | "iso"
            | "lzh" | "nupkg" | "cab" | "wim"
    )
}

/// Human-readable byte size.
pub fn human(n: u64) -> String {
    const U: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut f = n as f64;
    let mut i = 0;
    while f >= 1024.0 && i < U.len() - 1 {
        f /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{f:.1} {}", U[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_paths_must_stay_inside() {
        assert!(safe_rel("Plugins").is_ok());
        assert!(safe_rel("Files\\7-Zip").is_ok());
        assert!(safe_rel("..\\..\\Windows").is_err());
        assert!(safe_rel("C:\\Windows").is_err());
        assert!(safe_rel("\\evil").is_err());
    }

    #[test]
    fn kinds() {
        assert!(Kind::of("msi") == Kind::Msi);
        assert!(Kind::of("7z") == Kind::SevenZip);
        assert!(Kind::of("exe") == Kind::Single);
    }
}
