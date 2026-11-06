//! Dependency / plugin resolution and launch-environment derivation.
//!
//! Scoop expresses hard deps via `depends` and soft ones via `suggest`. The
//! Maxtrix overlay *elevates* selected suggested features (e.g. a JDK for Ghidra)
//! into auto-installed "plugins" so a tool runs portably without a system runtime.

use std::path::{Path, PathBuf};

use crate::manifest::Manifest;

/// Suggested features we auto-install instead of merely recommending.
pub const AUTO_FEATURES: &[&str] = &["jdk", "jre", "java"];

/// A dependency to install before the main tool.
#[derive(Debug, Clone)]
pub struct PluginNeed {
    /// Scoop reference, e.g. "java/temurin21-jdk".
    pub reference: String,
    /// Feature label ("JDK") or "depends" for hard dependencies.
    pub feature: String,
    /// True for `depends` (hard), false for an elevated `suggest`.
    pub hard: bool,
}

/// Compute the plugins/dependencies to install for `m`:
/// all hard `depends`, plus `suggest` entries whose feature is auto-elevated.
pub fn plugin_needs(m: &Manifest) -> Vec<PluginNeed> {
    let mut needs = Vec::new();
    for d in &m.depends {
        needs.push(PluginNeed {
            reference: d.clone(),
            feature: "depends".into(),
            hard: true,
        });
    }
    for (feature, apps) in &m.suggest {
        if AUTO_FEATURES.iter().any(|f| f.eq_ignore_ascii_case(feature)) {
            if let Some(first) = apps.split(',').next() {
                let reference = first.trim().to_string();
                if !reference.is_empty() {
                    needs.push(PluginNeed {
                        reference,
                        feature: feature.clone(),
                        hard: false,
                    });
                }
            }
        }
    }
    needs
}

/// Split a Scoop reference "bucket/name" into its parts. A bare name yields an
/// empty bucket (caller decides the fallback).
pub fn parse_ref(reference: &str) -> (String, String) {
    match reference.split_once('/') {
        Some((bucket, name)) => (bucket.to_string(), name.to_string()),
        None => (String::new(), reference.to_string()),
    }
}

/// Map a Scoop bucket name to its GitHub repo under github.com/ScoopInstaller.
pub fn bucket_repo(bucket: &str) -> Option<&'static str> {
    Some(match bucket.to_ascii_lowercase().as_str() {
        "main" => "Main",
        "extras" => "Extras",
        "java" => "Java",
        "versions" => "Versions",
        "nirsoft" => "Nirsoft",
        "sysinternals" => "Sysinternals",
        _ => return None,
    })
}

/// Placeholder for the stick root in stored launch profiles; resolved to the
/// actual mount point at launch time so profiles are drive-letter independent.
pub const STICK_TOKEN: &str = "${STICK}";

/// Replace the stick-root placeholder with a concrete root path.
pub fn resolve_stick(value: &str, stick_root: &str) -> String {
    value.replace(STICK_TOKEN, stick_root)
}

/// Build the stick-relative `$dir` token for a tool installed at `dir` under `root`,
/// e.g. `${STICK}\Tools\_runtime\temurin21-jdk`.
pub fn dir_token(root: &Path, dir: &Path) -> String {
    let rel = dir.strip_prefix(root).unwrap_or(dir);
    let rel = rel.to_string_lossy().replace('/', std::path::MAIN_SEPARATOR_STR);
    if rel.is_empty() {
        STICK_TOKEN.to_string()
    } else {
        format!("{STICK_TOKEN}{}{rel}", std::path::MAIN_SEPARATOR)
    }
}

/// Replace Scoop's folder variables with `dir_token`. On the stick persisted data
/// lives inside the app folder itself, so `$persist_dir` and `$original_dir` are
/// the same folder as `$dir`.
pub fn subst_dir(s: &str, dir_token: &str) -> String {
    s.replace("$persist_dir", dir_token)
        .replace("$original_dir", dir_token)
        .replace("$dir", dir_token)
}

/// Derive the environment a manifest contributes, expressed against `dir_token`
/// (typically a `${STICK}`-relative path): its `env_set` (with `$dir` substituted)
/// and its `env_add_path` (joined onto the token).
pub fn env_contributions(m: &Manifest, dir_token: &str) -> (Vec<(String, String)>, Vec<String>) {
    let subst = |s: &str| subst_dir(s, dir_token);

    let vars = m
        .env_set
        .iter()
        .map(|(k, val)| (k.clone(), subst(val)))
        .collect();

    let paths = m
        .env_add_path
        .iter()
        .map(|p| {
            let p = subst(p);
            if Path::new(&p).is_absolute() || p.contains(STICK_TOKEN) {
                p
            } else {
                format!("{dir_token}{}{p}", std::path::MAIN_SEPARATOR)
            }
        })
        .collect();

    (vars, paths)
}

/// A resolved launch profile: what to run and the environment to run it in.
#[derive(Debug, Default)]
pub struct LaunchProfile {
    pub run: Option<String>,
    /// Command-line arguments for `run` (`${STICK}`-relative paths, passed verbatim).
    pub args: Option<String>,
    pub env: Vec<(String, String)>,
    pub path_prepend: Vec<String>,
}

impl LaunchProfile {
    /// Serialize to the on-stick `.maxtrix-launch.json` shape.
    pub fn to_json(&self) -> serde_json::Value {
        let env: serde_json::Map<String, serde_json::Value> = self
            .env
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        serde_json::json!({
            "run": self.run,
            "args": self.args,
            "env": env,
            "path_prepend": self.path_prepend,
        })
    }
}

/// Aggregate a drive-independent launch profile for a main tool and its plugins,
/// with all paths stored `${STICK}`-relative to `root`. `run_override` /
/// `args_override` replace the manifest's start target / arguments (Maxtrix overlay).
pub fn build_launch_profile(
    main: &Manifest,
    main_dir: &Path,
    plugins: &[(Manifest, PathBuf)],
    root: &Path,
    run_override: Option<&str>,
    args_override: Option<&str>,
) -> LaunchProfile {
    let main_token = dir_token(root, main_dir);
    let args = args_override.map(str::to_string).or_else(|| main.launch_args());
    let mut profile = LaunchProfile {
        run: run_override.map(str::to_string).or_else(|| main.launch_target()),
        args: args.map(|a| subst_dir(&a, &main_token)),
        ..Default::default()
    };
    // Plugins first (e.g. JAVA_HOME), then the main tool's own env.
    for (pm, pdir) in plugins {
        let token = dir_token(root, pdir);
        let (vars, paths) = env_contributions(pm, &token);
        profile.env.extend(vars);
        profile.path_prepend.extend(paths);
    }
    let (vars, paths) = env_contributions(main, &main_token);
    profile.env.extend(vars);
    profile.path_prepend.extend(paths);
    profile
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoop_dir_variables_map_to_the_app_folder() {
        let t = r"${STICK}\Tools\Browser\brave";
        assert_eq!(subst_dir(r#"--user-data-dir="$dir\User Data""#, t), r#"--user-data-dir="${STICK}\Tools\Browser\brave\User Data""#);
        assert_eq!(subst_dir(r"$persist_dir\profile", t), r"${STICK}\Tools\Browser\brave\profile");
        assert_eq!(subst_dir(r"$original_dir", t), t);
    }
}
