//! File-system helpers for staged installs: unique work folders, fast moves
//! (rename on the same volume, copy as fallback), and the atomic swap that puts a
//! freshly built app folder in place of the old one.

use anyhow::{anyhow, Result};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Prefix of every temporary folder Maxtrix creates next to a tool. Folders with
/// this prefix are never listed as tools and get cleaned up when left behind.
pub const WORK_PREFIX: &str = ".maxtrix-";

fn seq() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// A fresh, empty folder `<parent>/.maxtrix-<tag>-<pid>-<n>`.
pub fn unique_dir(parent: &Path, tag: &str) -> Result<PathBuf> {
    let dir = parent.join(format!("{WORK_PREFIX}{tag}-{}-{}", std::process::id(), seq()));
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// A fresh folder in the host's temp dir (downloads land here, never in RAM).
pub fn unique_temp_dir(prefix: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("{prefix}-{}-{}", std::process::id(), seq()));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Recursively copy the contents of `src` into `dst`.
pub fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// Count files (not directories) under `dir`, recursively.
pub fn count_files(dir: &Path) -> io::Result<usize> {
    let mut n = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            n += count_files(&entry.path())?;
        } else {
            n += 1;
        }
    }
    Ok(n)
}

/// Move a file or folder. A rename on the same volume is instant; across volumes
/// (host temp → stick) it falls back to copy + delete.
pub fn move_path(src: &Path, dst: &Path) -> io::Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(_) if src.is_dir() => {
            copy_dir_all(src, dst)?;
            fs::remove_dir_all(src)
        }
        Err(_) => {
            fs::copy(src, dst)?;
            fs::remove_file(src)
        }
    }
}

/// Move the *children* of `src` into `dst`, merging folders that exist on both
/// sides and replacing files. Used to lay several download parts into one app dir.
pub fn move_merge(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let from_is_dir = entry.file_type()?.is_dir();
        if to.is_dir() && from_is_dir {
            move_merge(&from, &to)?;
        } else {
            if to.is_dir() {
                fs::remove_dir_all(&to)?;
            } else if to.exists() {
                fs::remove_file(&to)?;
            }
            move_path(&from, &to)?;
        }
    }
    Ok(())
}

/// `true` if an OS error means "some file in there is open" (e.g. the tool runs).
fn in_use(e: &io::Error) -> bool {
    // 5 = ERROR_ACCESS_DENIED, 32 = ERROR_SHARING_VIOLATION, 33 = ERROR_LOCK_VIOLATION
    matches!(e.raw_os_error(), Some(5) | Some(32) | Some(33)) || e.kind() == io::ErrorKind::PermissionDenied
}

fn in_use_error(name: &str) -> anyhow::Error {
    anyhow!("{name} läuft noch oder eine Datei daraus ist geöffnet – bitte schließen und erneut versuchen")
}

/// A program or library inside `dir` that is currently loaded. Renaming the
/// folder is no proof — Windows lets a folder be renamed while a program from it
/// runs — but a loaded `.exe`/`.dll` can't be opened for writing (sharing
/// violation). Opening doesn't modify the file.
fn busy_file(dir: &Path, depth: usize) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            if depth < 12 {
                if let Some(p) = busy_file(&entry.path(), depth + 1) {
                    return Some(p);
                }
            }
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if !(name.ends_with(".exe") || name.ends_with(".dll")) {
            continue;
        }
        if let Err(e) = fs::OpenOptions::new().write(true).open(entry.path()) {
            if matches!(e.raw_os_error(), Some(32) | Some(33)) {
                return Some(entry.path());
            }
        }
    }
    None
}

/// Fail early (before a long download, and again right before touching the old
/// version) if the tool runs or its folder is otherwise held open (e.g. a console
/// whose working directory is in there).
pub fn ensure_not_in_use(dir: &Path, name: &str) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    if busy_file(dir, 0).is_some() {
        return Err(in_use_error(name));
    }
    let probe = dir.with_file_name(format!("{WORK_PREFIX}probe-{}-{}", std::process::id(), seq()));
    match fs::rename(dir, &probe) {
        Ok(()) => fs::rename(&probe, dir).map_err(|e| anyhow!("Ordner {} nicht zurückbenannt: {e}", probe.display())),
        Err(e) if in_use(&e) => Err(in_use_error(name)),
        Err(e) => Err(anyhow!("Ordner {} nicht zugreifbar: {e}", dir.display())),
    }
}

/// Swap the finished `staging` folder in as `target`. The old version is renamed
/// away first and only deleted after the new one is in place; if anything fails,
/// the old version is restored — a half-updated tool can't happen.
pub fn commit_dir(staging: &Path, target: &Path, name: &str) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    if !target.exists() {
        return fs::rename(staging, target).map_err(|e| anyhow!("Installation nicht abschließbar: {e}"));
    }
    if busy_file(target, 0).is_some() {
        return Err(in_use_error(name));
    }
    let old =target.with_file_name(format!("{WORK_PREFIX}old-{}-{}", std::process::id(), seq()));
    fs::rename(target, &old).map_err(|e| if in_use(&e) { in_use_error(name) } else { anyhow!("alte Version nicht verschiebbar: {e}") })?;
    match fs::rename(staging, target) {
        Ok(()) => {
            let _ = fs::remove_dir_all(&old);
            Ok(())
        }
        Err(e) => {
            let _ = fs::rename(&old, target);
            Err(anyhow!("neue Version nicht einsetzbar (alte wiederhergestellt): {e}"))
        }
    }
}

/// Older than an hour — old enough that no install still running uses it.
fn is_stale(entry: &fs::DirEntry) -> bool {
    entry
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age > std::time::Duration::from_secs(3600))
}

/// Remove what a crashed or killed install left behind: work folders under
/// `<root>/Tools/<category>/` and half-finished downloads in the host's temp dir
/// (only those older than an hour, so a running install elsewhere isn't disturbed).
pub fn cleanup_stale(root: &Path) {
    if let Ok(cats) = fs::read_dir(root.join("Tools")) {
        for cat in cats.flatten() {
            let Ok(items) = fs::read_dir(cat.path()) else { continue };
            for it in items.flatten() {
                if it.file_name().to_string_lossy().starts_with(WORK_PREFIX) && is_stale(&it) {
                    let _ = fs::remove_dir_all(it.path());
                }
            }
        }
    }
    if let Ok(items) = fs::read_dir(std::env::temp_dir()) {
        for it in items.flatten() {
            let name = it.file_name().to_string_lossy().into_owned();
            if (name.starts_with("maxtrix-dl-") || name.starts_with("maxtrix-msi-")) && is_stale(&it) {
                let _ = fs::remove_dir_all(it.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("maxtrix-test-{tag}-{}-{}", std::process::id(), seq()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn move_merge_merges_folders_and_replaces_files() {
        let t = tmp("merge");
        let (src, dst) = (t.join("src"), t.join("dst"));
        fs::create_dir_all(src.join("Plugins")).unwrap();
        fs::write(src.join("Plugins/a.dll"), "new").unwrap();
        fs::write(src.join("app.exe"), "new").unwrap();
        fs::create_dir_all(dst.join("Plugins")).unwrap();
        fs::write(dst.join("Plugins/b.dll"), "keep").unwrap();
        fs::write(dst.join("app.exe"), "old").unwrap();
        move_merge(&src, &dst).unwrap();
        assert_eq!(fs::read_to_string(dst.join("app.exe")).unwrap(), "new");
        assert!(dst.join("Plugins/a.dll").is_file() && dst.join("Plugins/b.dll").is_file());
        fs::remove_dir_all(&t).unwrap();
    }

    #[test]
    fn commit_swaps_in_new_version_and_drops_old() {
        let t = tmp("commit");
        let (stage, target) = (t.join("stage"), t.join("Tool"));
        fs::create_dir_all(&stage).unwrap();
        fs::write(stage.join("v2.txt"), "2").unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("v1.txt"), "1").unwrap();
        commit_dir(&stage, &target, "Tool").unwrap();
        assert!(target.join("v2.txt").is_file());
        assert!(!target.join("v1.txt").exists(), "stale files of the old version must be gone");
        assert!(!stage.exists());
        let leftovers = fs::read_dir(&t).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with(WORK_PREFIX)).count();
        assert_eq!(leftovers, 0);
        fs::remove_dir_all(&t).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn a_running_tool_blocks_the_swap_and_stays_intact() {
        use std::os::windows::fs::OpenOptionsExt;
        let t = tmp("inuse");
        let (stage, target) = (t.join("stage"), t.join("Tool"));
        fs::create_dir_all(&stage).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("tool.exe"), "1").unwrap();
        // Like a running exe: opened without sharing.
        let lock = fs::OpenOptions::new().read(true).share_mode(0).open(target.join("tool.exe")).unwrap();
        let e = ensure_not_in_use(&target, "Tool").unwrap_err().to_string();
        assert!(e.contains("läuft noch"), "{e}");
        assert!(commit_dir(&stage, &target, "Tool").unwrap_err().to_string().contains("läuft noch"));
        assert!(target.join("tool.exe").is_file() && stage.is_dir());
        drop(lock);
        fs::remove_dir_all(&t).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn a_started_program_counts_as_running() {
        // Windows would happily rename the folder under a running program — the
        // check must catch it anyway.
        use std::os::windows::process::CommandExt;
        let t = tmp("running");
        let target = t.join("Tool");
        fs::create_dir_all(target.join("bin")).unwrap();
        fs::copy(r"C:\Windows\System32\PING.EXE", target.join("bin").join("tool.exe")).unwrap();
        let mut child = std::process::Command::new(target.join("bin").join("tool.exe"))
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .creation_flags(0x0800_0000)
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let busy = ensure_not_in_use(&target, "Tool");
        child.kill().unwrap();
        let _ = child.wait();
        assert!(busy.unwrap_err().to_string().contains("läuft noch"));
        assert!(ensure_not_in_use(&target, "Tool").is_ok(), "free again after the program ended");
        fs::remove_dir_all(&t).unwrap();
    }
}
