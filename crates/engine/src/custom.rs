//! Add a program that is not in the catalog: copy it to `Tools\<category>\<name>\`
//! and write a launch profile, so it starts like any other tool on the drive.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::catalog::{self, LAUNCH_FILE};
use crate::fsutil;

/// A folder is only copied as a whole when it looks like a program folder.
const MAX_FOLDER_FILES: usize = 20_000;
const MAX_FOLDER_BYTES: u64 = 8 << 30;
const MAX_NAME_CHARS: usize = 60;

/// What the dialog shows after an EXE was picked.
#[derive(Debug, Clone)]
pub struct Suggestion {
    pub name: String,
    /// The EXE's folder, if copying all of it makes sense.
    pub folder: Option<FolderInfo>,
}

#[derive(Debug, Clone)]
pub struct FolderInfo {
    pub name: String,
    pub files: usize,
    pub bytes: u64,
}

/// Folders that are never a program folder (whole user folders, drive roots …).
fn is_user_or_system_folder(dir: &Path) -> bool {
    if dir.parent().is_none() {
        return true; // a drive root
    }
    let name = dir.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    const NAMES: &[&str] = &[
        "desktop", "downloads", "documents", "dokumente", "pictures", "bilder", "music", "musik", "videos",
        "onedrive", "users", "benutzer", "windows", "system32", "syswow64", "program files", "program files (x86)",
        "programdata", "appdata", "temp", "tmp", "tools",
    ];
    if NAMES.contains(&name.as_str()) {
        return true;
    }
    // The user's own profile folder (C:\Users\<name>).
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        if Path::new(&profile) == dir {
            return true;
        }
    }
    false
}

/// Count files and bytes, giving up past the limits.
fn measure(dir: &Path) -> Option<(usize, u64)> {
    let (mut files, mut bytes) = (0usize, 0u64);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).ok()?.flatten() {
            let ft = e.file_type().ok()?;
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() {
                files += 1;
                bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
                if files > MAX_FOLDER_FILES || bytes > MAX_FOLDER_BYTES {
                    return None;
                }
            }
        }
    }
    Some((files, bytes))
}

/// A readable default name: "notepad++.exe" → "notepad++", "HxD64" → "HxD64".
fn default_name(exe: &Path) -> String {
    let stem = exe.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    clean_name(&stem)
}

/// The name as a folder name: no characters Windows rejects, no leading dot or
/// underscore (those are the app's own folders), not too long.
pub fn clean_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_control() || r#"\/:*?"<>|"#.contains(c) { ' ' } else { c })
        .take(MAX_NAME_CHARS)
        .collect();
    s.split_whitespace().collect::<Vec<_>>().join(" ").trim_start_matches(['.', '_']).trim_end_matches(['.', ' ']).to_string()
}

/// Look at a picked EXE.
pub fn suggest(exe: &Path) -> Suggestion {
    let folder = exe
        .parent()
        .filter(|d| !is_user_or_system_folder(d))
        .and_then(|d| {
            let (files, bytes) = measure(d)?;
            Some(FolderInfo { name: d.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), files, bytes })
        });
    // A program folder usually carries the better name ("Notepad++" vs "notepad++").
    let name = folder.as_ref().map(|f| clean_name(&f.name)).filter(|n| !n.is_empty()).unwrap_or_else(|| default_name(exe));
    Suggestion { name, folder }
}

/// Copy files from `src` into `dst`, reporting (done, total).
fn copy_tree(src: &Path, dst: &Path, done: &mut usize, total: usize, on: &dyn Fn(usize, usize)) -> Result<()> {
    fs::create_dir_all(dst)?;
    for e in fs::read_dir(src)? {
        let e = e?;
        let ft = e.file_type()?;
        let to = dst.join(e.file_name());
        if ft.is_dir() {
            copy_tree(&e.path(), &to, done, total, on)?;
        } else if ft.is_file() {
            fs::copy(e.path(), &to).with_context(|| e.path().display().to_string())?;
            *done += 1;
            on(*done, total);
        }
    }
    Ok(())
}

/// Copy the program to the drive. `whole_folder` copies the EXE's folder,
/// otherwise only the EXE. Returns the new tool folder.
pub fn add(root: &Path, category: &str, name: &str, exe: &Path, whole_folder: bool, on: &dyn Fn(usize, usize)) -> Result<PathBuf> {
    let name = clean_name(name);
    if name.is_empty() {
        bail!(crate::tr!("please enter a name", "Bitte einen Namen eingeben"));
    }
    if catalog::curated().iter().any(|e| e.name.eq_ignore_ascii_case(&name)) {
        bail!(crate::tr!("“{name}” is the name of a catalog tool, please choose another", "„{name}“ ist der Name eines Katalog-Tools, bitte einen anderen wählen"));
    }
    if category.is_empty() || category.starts_with(['_', '.']) || category.contains(['\\', '/', ':']) {
        bail!(crate::tr!("please choose a category", "Bitte eine Kategorie wählen"));
    }
    if !exe.is_file() {
        bail!(crate::tr!("the program file was not found", "Die Programmdatei wurde nicht gefunden"));
    }
    let target = catalog::install_dir(root, category, &name);
    if target.exists() {
        let cat = catalog::category_label(category);
        bail!(crate::tr!("“{name}” already exists in {cat}", "„{name}“ gibt es in {cat} schon"));
    }
    let tools = fs::canonicalize(root.join("Tools")).unwrap_or_else(|_| root.join("Tools"));
    let exe_abs = fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    if exe_abs.starts_with(&tools) {
        bail!(crate::tr!("this program is already in the Tools folder of the drive", "Dieses Programm liegt schon im Tools-Ordner des Sticks"));
    }
    let exe_name = exe.file_name().context("no file name")?.to_owned();
    let src_dir = exe.parent().context("no folder")?;

    let parent = target.parent().context("no parent")?;
    fs::create_dir_all(parent)?;
    let staging = fsutil::unique_dir(parent, "add")?;
    let result = (|| -> Result<()> {
        if whole_folder {
            let total = measure(src_dir).map(|(f, _)| f).unwrap_or(0);
            copy_tree(src_dir, &staging, &mut 0, total, on)?;
        } else {
            fs::copy(exe, staging.join(&exe_name))?;
            on(1, 1);
        }
        let profile = serde_json::json!({
            "run": exe_name.to_string_lossy(),
            "args": null,
            "env": {},
            "path_prepend": [],
        });
        fs::write(staging.join(LAUNCH_FILE), serde_json::to_string_pretty(&profile)?)?;
        fsutil::commit_dir(&staging, &target, &name)
    })();
    if let Err(e) = result {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name("  My:Tool?  v2 "), "My Tool v2");
        assert_eq!(clean_name("..hidden"), "hidden");
        assert_eq!(clean_name("_x."), "x");
        assert_eq!(clean_name("***"), "");
    }

    #[test]
    fn adds_a_folder_and_starts_from_it() {
        let base = std::env::temp_dir().join(format!("maxtrix-custom-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let src = base.join("src").join("Hex Tool");
        fs::create_dir_all(src.join("data")).unwrap();
        fs::write(src.join("hex.exe"), "x").unwrap();
        fs::write(src.join("data").join("a.cfg"), "y").unwrap();
        let root = base.join("stick");
        fs::create_dir_all(root.join("Tools")).unwrap();

        let s = suggest(&src.join("hex.exe"));
        assert_eq!(s.name, "Hex Tool");
        assert_eq!(s.folder.as_ref().map(|f| f.files), Some(2));

        let dir = add(&root, "Files", &s.name, &src.join("hex.exe"), true, &|_, _| {}).unwrap();
        assert!(dir.join("hex.exe").is_file() && dir.join("data").join("a.cfg").is_file());
        let tool = catalog::scan_installed(&root).into_iter().find(|t| t.name == "Hex Tool").unwrap();
        assert!(tool.has_profile && !tool.curated);
        assert_eq!(catalog::tool_executable(&root, "Files", "Hex Tool"), Some(dir.join("hex.exe")));

        // Same name again, a catalog name, or a program already on the drive: refused.
        assert!(add(&root, "Files", "Hex Tool", &src.join("hex.exe"), false, &|_, _| {}).is_err());
        assert!(add(&root, "Files", "7zip", &src.join("hex.exe"), false, &|_, _| {}).is_err());
        assert!(add(&root, "Files", "Again", &dir.join("hex.exe"), false, &|_, _| {}).is_err());
        // Only the EXE.
        let one = add(&root, "More", "Single", &src.join("hex.exe"), false, &|_, _| {}).unwrap();
        assert!(one.join("hex.exe").is_file() && !one.join("data").exists());
        let _ = fs::remove_dir_all(&base);
    }
}
