//! Update the app itself from its GitHub releases.
//!
//! The newest published release is read from the GitHub API. Its package is only
//! used when GitHub lists a SHA-256 digest for it, and the download must match that
//! digest. The new files are unpacked next to the app, then swapped in: every file
//! that gets replaced is first renamed to `<name>.maxtrix-old` (Windows allows that
//! even for the running EXE), so a failure halfway puts everything back. The old
//! files are deleted at the next start. `Tools\` and the settings are never touched.

use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use crate::{extract, net};

pub const RELEASES_API: &str = "https://api.github.com/repos/maxmlr76/Portable-Manager/releases/latest";
pub const PACKAGE_ASSET: &str = "PortableManager.zip";
pub const EXE_NAME: &str = "PortableManager.exe";
const WORK_DIR: &str = ".maxtrix-update";
const OLD_SUFFIX: &str = ".maxtrix-old";

#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    pub zip_url: String,
    pub sha256: String,
    pub page: String,
}

/// Where to look for releases. `MAXTRIX_UPDATE_API` replaces it (for testing).
pub fn api_url() -> String {
    std::env::var("MAXTRIX_UPDATE_API").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| RELEASES_API.to_string())
}

/// "v1.2.3", "1.2.3-beta" → (1, 2, 3).
pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches(['v', 'V']).split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let major = parts.next()??;
    let minor = parts.next().unwrap_or(Some(0))?;
    let patch = parts.next().unwrap_or(Some(0))?;
    Some((major, minor, patch))
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// Read a release description from the GitHub API. `None` when it is not newer
/// than `current`, is a draft or pre-release, or has no package.
pub fn release_from_json(text: &str, current: &str) -> Result<Option<Release>> {
    let doc: serde_json::Value = serde_json::from_str(text).context("invalid release information")?;
    if doc["draft"].as_bool() == Some(true) || doc["prerelease"].as_bool() == Some(true) {
        return Ok(None);
    }
    let version = doc["tag_name"].as_str().unwrap_or("").trim_start_matches(['v', 'V']).to_string();
    if !is_newer(&version, current) {
        return Ok(None);
    }
    let Some(asset) = doc["assets"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["name"].as_str() == Some(PACKAGE_ASSET)))
    else {
        return Ok(None);
    };
    let zip_url = asset["browser_download_url"].as_str().unwrap_or("").to_string();
    if !zip_url.starts_with("https://") && std::env::var("MAXTRIX_UPDATE_API").is_err() {
        bail!(crate::tr!("the update package has no secure download address", "Das Update-Paket hat keine sichere Download-Adresse"));
    }
    // GitHub lists "sha256:<hex>" for every asset. Without it there is nothing to
    // check the download against, so the update is refused.
    let sha256 = asset["digest"]
        .as_str()
        .and_then(|d| d.strip_prefix("sha256:"))
        .filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| anyhow!(crate::tr!("the update package has no checksum", "Das Update-Paket hat keine Prüfsumme")))?
        .to_ascii_lowercase();
    Ok(Some(Release { version, zip_url, sha256, page: doc["html_url"].as_str().unwrap_or("").to_string() }))
}

/// Ask GitHub whether a newer version than `current` is published.
pub fn check(current: &str) -> Result<Option<Release>> {
    match net::fetch_text_opt(&api_url())? {
        Some(text) => release_from_json(&text, current),
        None => Ok(None), // no published release yet
    }
}

/// Download the package into the app folder and check its digest.
pub fn download(rel: &Release, app_dir: &Path, on_progress: &dyn Fn(u64, Option<u64>), cancel: &AtomicBool) -> Result<PathBuf> {
    let work = app_dir.join(WORK_DIR);
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)?;
    let zip = work.join(PACKAGE_ASSET);
    let (_, got) = net::download_to_file(&rel.zip_url, &zip, "sha256", on_progress, cancel)?;
    if !got.eq_ignore_ascii_case(&rel.sha256) {
        let _ = fs::remove_dir_all(&work);
        bail!(crate::tr!(
            "the downloaded package does not match its checksum, nothing was changed",
            "Das heruntergeladene Paket passt nicht zur Prüfsumme, nichts wurde verändert"
        ));
    }
    Ok(zip)
}

/// Unpack the package next to the app. Returns the folder that holds the new EXE.
pub fn stage(zip: &Path, app_dir: &Path) -> Result<PathBuf> {
    let dir = app_dir.join(WORK_DIR).join("new");
    let _ = fs::remove_dir_all(&dir);
    extract::extract_zip_file(zip, &dir, None)?;
    // The package holds one folder `PortableManager\`; accept a flat one too.
    for cand in [dir.join("PortableManager"), dir.clone()] {
        if cand.join(EXE_NAME).is_file() {
            return Ok(cand);
        }
    }
    bail!(crate::tr!("the update package contains no {}", "Das Update-Paket enthält keine {}", EXE_NAME))
}

/// Every file of the new version, relative to its folder. `Tools\` is left out,
/// the EXE comes last.
fn package_files(new_root: &Path) -> Result<Vec<PathBuf>> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for e in fs::read_dir(dir)? {
            let e = e?;
            let path = e.path();
            let rel = path.strip_prefix(base)?.to_path_buf();
            if e.file_type()?.is_dir() {
                if rel == Path::new("Tools") {
                    continue;
                }
                walk(base, &path, out)?;
            } else {
                out.push(rel);
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(new_root, new_root, &mut files)?;
    files.sort_by_key(|p| p == Path::new(EXE_NAME));
    Ok(files)
}

fn old_path(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(OLD_SUFFIX);
    PathBuf::from(s)
}

/// Swap the new files in. The package's EXE replaces `running_exe`. Returns the
/// number of files written; on an error everything is put back as it was.
pub fn apply(new_root: &Path, app_dir: &Path, running_exe: &Path) -> Result<usize> {
    let files = package_files(new_root)?;
    let mut done: Vec<(PathBuf, Option<PathBuf>)> = Vec::new(); // (target, backup)
    let result = (|| -> Result<()> {
        for rel in &files {
            let src = new_root.join(rel);
            let target = if rel == Path::new(EXE_NAME) { running_exe.to_path_buf() } else { app_dir.join(rel) };
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let backup = if target.exists() {
                let old = old_path(&target);
                let _ = fs::remove_file(&old);
                fs::rename(&target, &old)
                    .with_context(|| crate::tr!("{} cannot be replaced", "{} lässt sich nicht ersetzen", target.display()))?;
                Some(old)
            } else {
                None
            };
            done.push((target.clone(), backup));
            if fs::rename(&src, &target).is_err() {
                fs::copy(&src, &target)
                    .with_context(|| crate::tr!("{} cannot be written", "{} lässt sich nicht schreiben", target.display()))?;
            }
        }
        Ok(())
    })();
    if let Err(e) = result {
        for (target, backup) in done.into_iter().rev() {
            let _ = fs::remove_file(&target);
            if let Some(old) = backup {
                let _ = fs::rename(&old, &target);
            }
        }
        return Err(e);
    }
    let _ = fs::remove_dir_all(app_dir.join(WORK_DIR));
    Ok(files.len())
}

/// Remove what an earlier update left behind: the work folder and the old files
/// (the old EXE could only be deleted once it stopped running).
pub fn cleanup(app_dir: &Path) {
    let _ = fs::remove_dir_all(app_dir.join(WORK_DIR));
    remove_old_files(app_dir);
}

/// Delete the `*.maxtrix-old` files an earlier update left behind (`Tools\` excluded).
pub fn remove_old_files(app_dir: &Path) {
    fn walk(dir: &Path, top: bool) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let path = e.path();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                // Tools hold the user's programs; they are never part of the package.
                if !(top && e.file_name() == "Tools") {
                    walk(&path, false);
                }
            } else if e.file_name().to_string_lossy().ends_with(OLD_SUFFIX) {
                let _ = fs::remove_file(&path);
            }
        }
    }
    walk(app_dir, true);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mx-selfupdate-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("v0.1.10", "0.1.9"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("0.1.1", "0.1.1"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        assert!(!is_newer("garbage", "0.1.1"));
        assert_eq!(parse_version("v2.3.4-beta"), Some((2, 3, 4)));
    }

    #[test]
    fn a_release_needs_a_package_with_a_digest() {
        let hex = "ab".repeat(32);
        let rel = |tag: &str, digest: &str, extra: &str| {
            format!(
                r#"{{"tag_name":"{tag}","html_url":"https://x/r",{extra}"assets":[{{"name":"PortableManager.zip","browser_download_url":"https://x/p.zip","digest":"{digest}"}}]}}"#
            )
        };
        let r = release_from_json(&rel("v0.2.0", &format!("sha256:{hex}"), ""), "0.1.1").unwrap().unwrap();
        assert_eq!((r.version.as_str(), r.sha256.as_str()), ("0.2.0", hex.as_str()));
        assert!(release_from_json(&rel("v0.1.1", &format!("sha256:{hex}"), ""), "0.1.1").unwrap().is_none());
        assert!(release_from_json(&rel("v0.2.0", &format!("sha256:{hex}"), r#""prerelease":true,"#), "0.1.1").unwrap().is_none());
        assert!(release_from_json(&rel("v0.2.0", "", ""), "0.1.1").is_err());
        assert!(release_from_json(&rel("v0.2.0", "sha1:abc", ""), "0.1.1").is_err());
    }

    #[test]
    fn apply_swaps_files_and_cleanup_removes_the_old_ones() {
        let app = tmp("app");
        let new = tmp("new");
        fs::write(app.join(EXE_NAME), "old exe").unwrap();
        fs::write(app.join("README.txt"), "old readme").unwrap();
        fs::create_dir_all(app.join("Tools/Files/7zip")).unwrap();
        fs::write(app.join("Tools/Files/7zip/7z.exe"), "user tool").unwrap();
        fs::write(new.join(EXE_NAME), "new exe").unwrap();
        fs::write(new.join("README.txt"), "new readme").unwrap();
        fs::create_dir_all(new.join("licenses")).unwrap();
        fs::write(new.join("licenses/LICENSE.txt"), "mit").unwrap();
        fs::create_dir_all(new.join("Tools")).unwrap();
        fs::write(new.join("Tools/_your-tools-go-here.txt"), "x").unwrap();

        assert_eq!(apply(&new, &app, &app.join(EXE_NAME)).unwrap(), 3);
        assert_eq!(fs::read_to_string(app.join(EXE_NAME)).unwrap(), "new exe");
        assert_eq!(fs::read_to_string(app.join("licenses/LICENSE.txt")).unwrap(), "mit");
        assert!(app.join(format!("{EXE_NAME}{OLD_SUFFIX}")).exists());
        assert!(!app.join("Tools/_your-tools-go-here.txt").exists());
        assert_eq!(fs::read_to_string(app.join("Tools/Files/7zip/7z.exe")).unwrap(), "user tool");

        cleanup(&app);
        assert!(!app.join(format!("{EXE_NAME}{OLD_SUFFIX}")).exists());
        assert!(!app.join(format!("README.txt{OLD_SUFFIX}")).exists());
        let _ = fs::remove_dir_all(&app);
        let _ = fs::remove_dir_all(&new);
    }

    #[test]
    fn a_failed_apply_restores_the_old_files() {
        let app = tmp("app2");
        let new = tmp("new2");
        fs::write(app.join(EXE_NAME), "old exe").unwrap();
        fs::write(app.join("README.txt"), "old readme").unwrap();
        fs::write(new.join(EXE_NAME), "new exe").unwrap();
        fs::write(new.join("README.txt"), "new readme").unwrap();
        // The EXE (always the last step) is to go below a plain file: that fails.
        fs::write(app.join("not-a-folder"), "x").unwrap();
        let bad_exe = app.join("not-a-folder").join(EXE_NAME);

        assert!(apply(&new, &app, &bad_exe).is_err());
        assert_eq!(fs::read_to_string(app.join("README.txt")).unwrap(), "old readme");
        assert!(!app.join(format!("README.txt{OLD_SUFFIX}")).exists());
        assert_eq!(fs::read_to_string(app.join(EXE_NAME)).unwrap(), "old exe");
        let _ = fs::remove_dir_all(&app);
        let _ = fs::remove_dir_all(&new);
    }
}
