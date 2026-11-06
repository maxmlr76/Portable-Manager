//! Scoop `persist` on a stick: keep user data across updates and switch apps into
//! their portable mode.
//!
//! Scoop keeps persisted data in a separate folder and links it into the app dir.
//! On the stick everything already lives next to the app, so the rules are simpler:
//! * **Update**: each persist entry is *moved* over from the previous install
//!   (instant on the same volume — browser profiles can be large). A folder the
//!   new package ships as well (OBS `data`, Notepad++ `plugins`) is merged: the new
//!   version's files stay, the user's additions move in.
//! * **Fresh install**: a missing entry is created the way Scoop's scripts
//!   effectively do — a shipped template is copied (`portable.ini.template`), a
//!   marker-like file is created empty (`.portable`, `portable_mode.txt`, `*.ini`),
//!   anything else becomes a folder (`data`, `portable_config`, `profile` …).
//!   Structured files (`*.xml`, `*.json`) are never created empty — the app writes
//!   them itself; an empty one would be a parse error.
//! * Loose settings files of the old install that the new package doesn't ship
//!   (`Everything.ini` …) are carried over as well.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use crate::fsutil::move_path;
use crate::install::safe_rel;

/// File types that are settings/data (safe to carry over, never program code).
const DATA_EXT: &[&str] = &["ini", "cfg", "conf", "config", "json", "xml", "db", "sqlite", "dat", "txt", "potfile", "ctl"];
/// File types that may be created empty (an empty file is still valid for them).
const EMPTY_OK_EXT: &[&str] = &["ini", "cfg", "conf", "txt", "potfile", "ctl", "log"];

fn ext_of(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let (stem, ext) = base.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty() && ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric()))
        .then(|| ext.to_ascii_lowercase())
}

/// A dotfile marker such as `.portable`.
fn is_dotfile(name: &str) -> bool {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    base.starts_with('.') && base.len() > 1 && !base[1..].contains('.')
}

/// How a missing persist entry should be created.
#[derive(Debug, PartialEq, Eq)]
enum Create {
    Dir,
    EmptyFile,
    /// Structured file (xml/json …) — leave it to the app.
    Skip,
}

fn create_kind(entry: &str) -> Create {
    if is_dotfile(entry) {
        return Create::EmptyFile;
    }
    match ext_of(entry) {
        None => Create::Dir,
        Some(e) if EMPTY_OK_EXT.contains(&e.as_str()) => Create::EmptyFile,
        Some(_) => Create::Skip,
    }
}

/// A template the package ships for `entry`: `<entry>.template` or `<stem>.model.<ext>`.
fn template_for(app: &Path, entry: &Path) -> Option<PathBuf> {
    let name = entry.file_name()?.to_string_lossy().into_owned();
    let dir = app.join(entry.parent().unwrap_or(Path::new("")));
    let mut candidates = vec![dir.join(format!("{name}.template"))];
    if let Some((stem, ext)) = name.rsplit_once('.') {
        candidates.push(dir.join(format!("{stem}.model.{ext}")));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// What `prepare` moved out of the old install (relative paths), so a failed swap
/// can put everything back with [`restore`].
#[derive(Debug, Default)]
pub struct Carried(Vec<PathBuf>);

impl Carried {
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Prepare the freshly unpacked app dir `new` (see module docs). `old` is the
/// previous install, if any; `markers` are extra files a tool needs to run
/// portably (e.g. GnuPG's `bin\gpgconf.ctl`).
pub fn prepare(new: &Path, old: Option<&Path>, persist: &[String], markers: &[&str]) -> Result<Carried> {
    let mut carried = Carried::default();
    let result = (|| -> Result<()> {
        for entry in persist {
            let rel = safe_rel(entry)?;
            let dst = new.join(&rel);
            match old.map(|o| o.join(&rel)).filter(|s| s.exists()) {
                // A folder the new version ships as well (OBS `data`, Notepad++
                // `plugins`): its files must match the new version, so they stay;
                // everything only the user's copy has is moved over. Same result
                // as Scoop's OBS script.
                Some(src) if src.is_dir() && dst.is_dir() => {
                    merge_missing(&src, &dst, &rel, &mut carried)
                        .with_context(|| format!("Daten '{entry}' nicht übernehmbar"))?;
                }
                // Otherwise the user's data wins over whatever the package ships
                // (a default `.ini`, an empty profile folder …).
                Some(src) => {
                    if dst.is_dir() {
                        fs::remove_dir_all(&dst)?;
                    } else if dst.exists() {
                        fs::remove_file(&dst)?;
                    }
                    move_path(&src, &dst).with_context(|| format!("Daten '{entry}' nicht übernehmbar"))?;
                    carried.0.push(rel);
                }
                None if !dst.exists() => create_missing(new, &rel, entry)?,
                None => {}
            }
        }
        for m in markers {
            let rel = safe_rel(m)?;
            let dst = new.join(&rel);
            if !dst.exists() {
                if let Some(p) = dst.parent() {
                    fs::create_dir_all(p)?;
                }
                fs::write(&dst, b"")?;
            }
        }
        if let Some(old) = old {
            carry_loose_settings(new, old, &mut carried)?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => Ok(carried),
        Err(e) => {
            if let Some(old) = old {
                restore(&carried, new, old);
            }
            Err(e)
        }
    }
}

/// Move whatever `src` (old) has and `dst` (new) lacks into `dst`, recursively;
/// where both have a file, the new version's stays.
fn merge_missing(src: &Path, dst: &Path, rel: &Path, carried: &mut Carried) -> Result<()> {
    for e in fs::read_dir(src)?.flatten() {
        let (from, to, r) = (e.path(), dst.join(e.file_name()), rel.join(e.file_name()));
        if !to.exists() {
            move_path(&from, &to)?;
            carried.0.push(r);
        } else if from.is_dir() && to.is_dir() {
            merge_missing(&from, &to, &r, carried)?;
        }
    }
    Ok(())
}

fn create_missing(app: &Path, rel: &Path, entry: &str) -> Result<()> {
    let dst = app.join(rel);
    if let Some(tpl) = template_for(app, rel) {
        fs::copy(&tpl, &dst)?;
        return Ok(());
    }
    match create_kind(entry) {
        Create::Dir => fs::create_dir_all(&dst)?,
        Create::EmptyFile => {
            if let Some(p) = dst.parent() {
                fs::create_dir_all(p)?;
            }
            fs::write(&dst, b"")?;
        }
        Create::Skip => {}
    }
    Ok(())
}

/// Settings files at the top of the old install that the new package doesn't
/// contain (the app created them) move along; program files never do.
fn carry_loose_settings(new: &Path, old: &Path, carried: &mut Carried) -> Result<()> {
    for entry in fs::read_dir(old)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(".maxtrix") || !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let is_data = ext_of(&name).map(|e| DATA_EXT.contains(&e.as_str())).unwrap_or(false);
        if is_data && !new.join(&name).exists() {
            move_path(&entry.path(), &new.join(&name))?;
            carried.0.push(PathBuf::from(name));
        }
    }
    Ok(())
}

/// Undo `prepare`'s moves after a failed swap: carried entries go back to `old`.
pub fn restore(carried: &Carried, new: &Path, old: &Path) {
    for rel in carried.0.iter().rev() {
        let (from, to) = (new.join(rel), old.join(rel));
        if from.exists() && !to.exists() {
            let _ = move_path(&from, &to);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("maxtrix-persist-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn creation_rules() {
        assert_eq!(create_kind("data"), Create::Dir);
        assert_eq!(create_kind("Portable Settings"), Create::Dir);
        assert_eq!(create_kind("Ghidra/Configurations"), Create::Dir);
        assert_eq!(create_kind(".portable"), Create::EmptyFile);
        assert_eq!(create_kind("portable_mode.txt"), Create::EmptyFile);
        assert_eq!(create_kind("rufus.ini"), Create::EmptyFile);
        assert_eq!(create_kind("hashcat.potfile"), Create::EmptyFile);
        assert_eq!(create_kind("config.xml"), Create::Skip, "empty XML would be a parse error");
    }

    #[test]
    fn fresh_install_switches_on_portable_mode() {
        let app = tmp("fresh");
        fs::write(app.join("portable.ini.template"), "storage.dir = storage").unwrap();
        let persist = s(&["data", ".portable", "portable.ini", "config.xml", "release\\x64\\x64dbg.ini"]);
        prepare(&app, None, &persist, &["bin\\gpgconf.ctl"]).unwrap();
        assert!(app.join("data").is_dir());
        assert!(app.join(".portable").is_file());
        assert_eq!(fs::read_to_string(app.join("portable.ini")).unwrap(), "storage.dir = storage");
        assert!(!app.join("config.xml").exists());
        assert!(app.join("release").join("x64").join("x64dbg.ini").is_file());
        assert!(app.join("bin").join("gpgconf.ctl").is_file());
        fs::remove_dir_all(&app).unwrap();
    }

    #[test]
    fn update_keeps_user_data_over_shipped_defaults() {
        let t = tmp("update");
        let (old, new) = (t.join("old"), t.join("new"));
        fs::create_dir_all(old.join("profile")).unwrap();
        fs::write(old.join("profile").join("prefs.js"), "user").unwrap();
        fs::write(old.join("app.ini"), "user settings").unwrap(); // persisted file
        fs::write(old.join("settings.ini"), "mine").unwrap(); // not in persist, not shipped
        fs::write(old.join("old.dll"), "code").unwrap(); // program file: must not move
        fs::create_dir_all(&new).unwrap();
        fs::write(new.join("app.ini"), "shipped default").unwrap();
        let carried = prepare(&new, Some(&old), &s(&["profile", "app.ini"]), &[]).unwrap();
        assert_eq!(fs::read_to_string(new.join("profile").join("prefs.js")).unwrap(), "user");
        assert_eq!(fs::read_to_string(new.join("app.ini")).unwrap(), "user settings");
        assert_eq!(fs::read_to_string(new.join("settings.ini")).unwrap(), "mine");
        assert!(!new.join("old.dll").exists());
        assert_eq!(carried.len(), 3);
        // A failed swap puts everything back.
        restore(&carried, &new, &old);
        assert!(old.join("profile").join("prefs.js").is_file() && old.join("settings.ini").is_file());
        assert_eq!(fs::read_to_string(old.join("app.ini")).unwrap(), "user settings");
        fs::remove_dir_all(&t).unwrap();
    }

    #[test]
    fn shipped_folders_get_new_files_and_keep_user_additions() {
        // Notepad++/OBS: `plugins` ships with the app; the user added one.
        let t = tmp("merge");
        let (old, new) = (t.join("old"), t.join("new"));
        fs::create_dir_all(old.join("plugins").join("MyPlugin")).unwrap();
        fs::write(old.join("plugins").join("MyPlugin").join("my.dll"), "user").unwrap();
        fs::write(old.join("plugins").join("mimeTools.dll"), "v1").unwrap();
        fs::create_dir_all(new.join("plugins")).unwrap();
        fs::write(new.join("plugins").join("mimeTools.dll"), "v2").unwrap();
        let carried = prepare(&new, Some(&old), &s(&["plugins"]), &[]).unwrap();
        assert_eq!(fs::read_to_string(new.join("plugins").join("mimeTools.dll")).unwrap(), "v2");
        assert_eq!(fs::read_to_string(new.join("plugins").join("MyPlugin").join("my.dll")).unwrap(), "user");
        restore(&carried, &new, &old);
        assert!(old.join("plugins").join("MyPlugin").join("my.dll").is_file());
        assert_eq!(fs::read_to_string(old.join("plugins").join("mimeTools.dll")).unwrap(), "v1");
        fs::remove_dir_all(&t).unwrap();
    }

    #[test]
    fn persist_paths_cannot_escape_the_app_dir() {
        let app = tmp("escape");
        assert!(prepare(&app, None, &s(&["..\\..\\Windows"]), &[]).is_err());
        fs::remove_dir_all(&app).unwrap();
    }
}
