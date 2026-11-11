//! Drive check: finds tools that can no longer start and what cancelled or
//! crashed installs left behind, so the app can offer to repair them.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::catalog::{self, LAUNCH_FILE, RUNTIME_CAT};
use crate::fsutil::WORK_PREFIX;
use crate::resolver::{resolve_stick, STICK_TOKEN};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// A work folder of an install that never finished (`.maxtrix-…`).
    Leftover,
    /// An empty tool folder.
    Empty,
    /// The launch profile is not valid JSON.
    BadProfile,
    /// The launch profile points to a start file that is gone.
    MissingStart(String),
    /// A folder the tool needs (e.g. the shared JDK under `_runtime`) is gone.
    MissingPath(String),
    /// Neither a launch profile nor any program file to start.
    NoStart,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub category: String,
    pub name: String,
    pub kind: Kind,
    /// The folder concerned (tool folder or leftover).
    pub path: PathBuf,
}

impl Finding {
    /// Part of the curated catalog: a reinstall brings it back (settings are kept).
    pub fn curated(&self) -> bool {
        catalog::find(&self.name).is_some()
    }

    /// Fixed by deleting the folder (nothing of value is in it).
    pub fn removable(&self) -> bool {
        matches!(self.kind, Kind::Leftover | Kind::Empty)
    }

    /// Fixed by installing the tool again.
    pub fn reinstall(&self) -> bool {
        !self.removable() && self.curated()
    }

    pub fn fixable(&self) -> bool {
        self.removable() || self.reinstall()
    }

    /// One line for the user.
    pub fn text(&self) -> String {
        let who = if self.removable() { format!("{}\\{}", self.category, self.name) } else { catalog::display_name(&self.name) };
        match &self.kind {
            Kind::Leftover => crate::tr!("{who}: leftover of a cancelled install", "{who}: Rest einer abgebrochenen Installation"),
            Kind::Empty => crate::tr!("{who}: empty folder", "{who}: leerer Ordner"),
            Kind::BadProfile => crate::tr!("{who}: start settings are damaged", "{who}: Starteinstellungen beschädigt"),
            Kind::MissingStart(f) => crate::tr!("{who}: start file '{f}' is missing", "{who}: Startdatei '{f}' fehlt"),
            Kind::MissingPath(p) => crate::tr!("{who}: needs '{p}', which is missing", "{who}: braucht '{p}', das fehlt"),
            Kind::NoStart => crate::tr!("{who}: no program file to start", "{who}: keine startbare Programmdatei"),
        }
    }
}

/// The result of a check.
pub struct Report {
    /// Tool folders that were looked at.
    pub tools: usize,
    pub findings: Vec<Finding>,
}

fn root_string(root: &Path) -> String {
    let abs = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    abs.to_string_lossy().trim_start_matches(r"\\?\").trim_end_matches(['\\', '/']).to_string()
}

/// `${STICK}\Tools\_runtime\temurin21-jdk\bin` → `_runtime\temurin21-jdk`
fn runtime_label(value: &str) -> String {
    let rest = value.trim_start_matches(STICK_TOKEN).trim_start_matches(['\\', '/']);
    let parts: Vec<&str> = rest.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
    parts.iter().skip(1).take(2).copied().collect::<Vec<_>>().join("\\")
}

/// Check one tool folder.
fn check_tool(root_str: &str, dir: &Path) -> Option<Kind> {
    let profile = dir.join(LAUNCH_FILE);
    let Ok(text) = fs::read_to_string(&profile) else {
        return catalog::find_run_target(dir).is_none().then_some(Kind::NoStart);
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Some(Kind::BadProfile);
    };
    if let Some(run) = json["run"].as_str().filter(|r| !r.is_empty()) {
        let resolved = resolve_stick(run, root_str);
        let p = if Path::new(&resolved).is_absolute() { PathBuf::from(&resolved) } else { dir.join(&resolved) };
        if !p.is_file() {
            return Some(Kind::MissingStart(run.to_string()));
        }
    }
    // Shared runtimes (JDK …) live in `Tools\_runtime\` and can be deleted on
    // their own. Only these are checked: other folders in a profile may be
    // created by the tool itself on first start.
    let marker = format!("{STICK_TOKEN}\\Tools\\{RUNTIME_CAT}\\");
    let mut values: Vec<&str> = json["path_prepend"].as_array().map(|a| a.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
    if let Some(env) = json["env"].as_object() {
        values.extend(env.values().filter_map(|v| v.as_str()));
    }
    for v in values {
        if v.replace('/', "\\").starts_with(&marker) && !Path::new(&resolve_stick(v, root_str)).exists() {
            return Some(Kind::MissingPath(runtime_label(v)));
        }
    }
    None
}

/// Work folders younger than this are left alone.
const LEFTOVER_AGE: Duration = Duration::from_secs(300);

fn older_than(path: &Path, age: Duration) -> bool {
    fs::metadata(path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|e| e >= age)
}

/// Look at every folder under `<root>\Tools\`.
pub fn check(root: &Path) -> Report {
    check_with(root, LEFTOVER_AGE)
}

fn check_with(root: &Path, leftover_age: Duration) -> Report {
    let root_str = root_string(root);
    let mut report = Report { tools: 0, findings: Vec::new() };
    let Ok(cats) = fs::read_dir(root.join("Tools")) else { return report };
    let mut cats: Vec<_> = cats.flatten().filter(|c| c.file_type().map(|t| t.is_dir()).unwrap_or(false)).collect();
    cats.sort_by_key(|c| c.file_name());
    for cat in cats {
        let category = cat.file_name().to_string_lossy().to_string();
        if category.starts_with('.') {
            continue;
        }
        let Ok(items) = fs::read_dir(cat.path()) else { continue };
        let mut items: Vec<_> = items.flatten().filter(|i| i.file_type().map(|t| t.is_dir()).unwrap_or(false)).collect();
        items.sort_by_key(|i| i.file_name());
        for it in items {
            let name = it.file_name().to_string_lossy().to_string();
            let path = it.path();
            let found = |kind| Finding { category: category.clone(), name: name.clone(), kind, path: path.clone() };
            if name.starts_with(WORK_PREFIX) {
                // A fresh one may belong to an install that is still running.
                if older_than(&path, leftover_age) {
                    report.findings.push(found(Kind::Leftover));
                }
                continue;
            }
            if name.starts_with('.') {
                continue;
            }
            let empty = fs::read_dir(&path).map(|mut d| d.next().is_none()).unwrap_or(false);
            if empty {
                report.findings.push(found(Kind::Empty));
                continue;
            }
            // Runtimes are checked through the tools that use them.
            if category.starts_with('_') {
                continue;
            }
            report.tools += 1;
            if let Some(kind) = check_tool(&root_str, &path) {
                report.findings.push(found(kind));
            }
        }
    }
    report
}

/// Delete the folders of removable findings. Only folders inside `<root>\Tools\`
/// are touched. Returns how many were removed.
pub fn remove_folders(root: &Path, findings: &[Finding]) -> usize {
    let tools = root.join("Tools");
    findings
        .iter()
        .filter(|f| f.removable() && f.path.starts_with(&tools) && f.path != tools)
        .filter(|f| fs::remove_dir_all(&f.path).is_ok() || !f.path.exists())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_broken_tools_and_leftovers() {
        let root = std::env::temp_dir().join(format!("maxtrix-health-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let t = root.join("Tools");
        let mk = |p: &str| fs::create_dir_all(t.join(p)).unwrap();
        let put = |p: &str, s: &str| fs::write(t.join(p), s).unwrap();

        mk("Files\\good");
        put("Files\\good\\good.exe", "x");
        put("Files\\good\\.maxtrix-launch.json", r#"{"run":"good.exe"}"#);
        mk("Files\\nostart");
        put("Files\\nostart\\.maxtrix-launch.json", r#"{"run":"gone.exe"}"#);
        mk("Files\\badjson");
        put("Files\\badjson\\.maxtrix-launch.json", "{oops");
        mk("Files\\notes");
        put("Files\\notes\\readme.txt", "x");
        mk("Files\\empty");
        mk("Files\\.maxtrix-new-1-2");
        mk("Reverse Engineering\\ghidra");
        put("Reverse Engineering\\ghidra\\ghidraRun.bat", "x");
        put(
            "Reverse Engineering\\ghidra\\.maxtrix-launch.json",
            r#"{"run":"ghidraRun.bat","env":{"JAVA_HOME":"${STICK}\\Tools\\_runtime\\jdk"},"path_prepend":["${STICK}\\Tools\\Files\\good"]}"#,
        );

        // Fresh work folders count only past the age limit.
        assert!(!check(&root).findings.iter().any(|f| f.kind == Kind::Leftover));
        let r = check_with(&root, Duration::ZERO);
        let kinds: Vec<(String, Kind)> = r.findings.iter().map(|f| (f.name.clone(), f.kind.clone())).collect();
        assert_eq!(r.tools, 5);
        assert!(kinds.contains(&(".maxtrix-new-1-2".into(), Kind::Leftover)));
        assert!(kinds.contains(&("empty".into(), Kind::Empty)));
        assert!(kinds.contains(&("badjson".into(), Kind::BadProfile)));
        assert!(kinds.contains(&("nostart".into(), Kind::MissingStart("gone.exe".into()))));
        assert!(kinds.contains(&("notes".into(), Kind::NoStart)));
        assert!(kinds.contains(&("ghidra".into(), Kind::MissingPath("_runtime\\jdk".into()))));
        assert!(!kinds.iter().any(|(n, _)| n == "good"));
        let ghidra = r.findings.iter().find(|f| f.name == "ghidra").unwrap();
        assert!(ghidra.reinstall() && ghidra.fixable());
        assert!(!r.findings.iter().find(|f| f.name == "notes").unwrap().fixable());

        assert_eq!(remove_folders(&root, &r.findings), 2);
        assert!(!t.join("Files\\empty").exists() && !t.join("Files\\.maxtrix-new-1-2").exists());
        assert!(t.join("Files\\notes").exists());
        let _ = fs::remove_dir_all(&root);
    }
}
