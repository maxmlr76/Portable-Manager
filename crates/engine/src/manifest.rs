//! Parsing of Scoop-style app manifests (a subset sufficient for install).
//!
//! Scoop lets most keys live either at the top level or inside
//! `architecture.64bit`; the architecture value wins when present. We resolve
//! every key *individually* that way — a 64-bit block that only carries `bin`
//! (while `url`/`hash` sit at the top level) is common and must still install.

use anyhow::{anyhow, Result};
use serde_json::Value;

/// One downloadable artifact of a manifest (Scoop allows several `url`s).
#[derive(Debug, Clone)]
pub struct Artifact {
    pub url: String,
    /// Filename to save the download as (from a `#/name` fragment, else the URL basename).
    pub filename: String,
    /// Lowercase file extension of `filename` (e.g. "zip", "7z", "msi", "exe").
    pub ext: String,
    /// Checksum algorithm ("sha256", "sha512", "sha1", "md5").
    pub hash_alg: String,
    /// Expected checksum, lowercase hex.
    pub hash_hex: String,
    /// Folder inside the archive whose contents are taken (Scoop `extract_dir`).
    pub extract_dir: Option<String>,
    /// Subfolder of the app dir to extract into (Scoop `extract_to`).
    pub extract_to: Option<String>,
}

/// A normalized package definition distilled from a Scoop manifest.
#[derive(Debug, Clone)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub description: String,
    pub homepage: String,
    /// Primary (first) artifact — mirrored here for callers that show one source.
    pub url: String,
    pub filename: String,
    pub ext: String,
    pub hash_alg: String,
    pub hash_hex: String,
    pub extract_dir: Option<String>,
    /// All artifacts in install order (the primary one is `artifacts[0]`).
    pub artifacts: Vec<Artifact>,
    /// Runtime dependencies (Scoop `depends`), e.g. "extras/vcredist2022".
    pub depends: Vec<String>,
    /// Feature suggestions (Scoop `suggest`): (feature, candidate app(s)).
    pub suggest: Vec<(String, String)>,
    /// Environment variables the app sets (Scoop `env_set`); values may contain `$dir`.
    pub env_set: Vec<(String, String)>,
    /// Path fragments (relative to `$dir`) to prepend to PATH (Scoop `env_add_path`).
    pub env_add_path: Vec<String>,
    /// First runnable entry from `bin` (often a console tool).
    pub bin: Option<String>,
    /// Arguments of that `bin` entry (`[exe, alias, args]`), may contain `$dir`.
    pub bin_args: Option<String>,
    /// First `.exe` target from `shortcuts` (the GUI entry point, if any).
    pub shortcut: Option<String>,
    /// Arguments of that shortcut (`[target, name, args]`), may contain `$dir`.
    pub shortcut_args: Option<String>,
    /// Scoop `persist` sources (paths inside the app dir that hold user data).
    pub persist: Vec<String>,
    /// True if the manifest relies on an `installer` script/file to finish the install.
    pub needs_installer: bool,
    /// True if the manifest runs `pre_install` / `post_install` PowerShell steps.
    pub has_scripts: bool,
    /// True for Inno-Setup packages (Scoop unpacks those with innounp).
    pub innosetup: bool,
}

impl Manifest {
    /// What "Starten" should run: the GUI shortcut if there is one, else `bin`.
    pub fn launch_target(&self) -> Option<String> {
        self.shortcut.clone().or_else(|| self.bin.clone())
    }

    /// The start arguments belonging to [`launch_target`](Self::launch_target).
    pub fn launch_args(&self) -> Option<String> {
        if self.shortcut.is_some() { self.shortcut_args.clone() } else { self.bin_args.clone() }
    }
}

/// Non-empty third element of a Scoop tuple (the arguments).
fn tuple_args(t: &[Value]) -> Option<String> {
    t.get(2).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

/// Extract the first runnable command (and its args) from a Scoop `bin` value
/// (string, array of strings, or array of `[exe, alias, args]` tuples).
fn first_bin(v: &Value) -> Option<(String, Option<String>)> {
    match v {
        Value::String(s) => Some((s.clone(), None)),
        Value::Array(a) => a.iter().find_map(|e| match e {
            Value::String(s) => Some((s.clone(), None)),
            Value::Array(inner) => inner.first().and_then(Value::as_str).map(|s| (s.to_string(), tuple_args(inner))),
            _ => None,
        }),
        _ => None,
    }
}

/// First `.exe` target (and its args) of a Scoop `shortcuts` value
/// (`[[target, name, args, icon], …]`).
fn first_shortcut_exe(v: &Value) -> Option<(String, Option<String>)> {
    v.as_array()?.iter().find_map(|e| {
        let t = e.as_array()?;
        let target = t.first()?.as_str()?;
        target.to_ascii_lowercase().ends_with(".exe").then(|| (target.to_string(), tuple_args(t)))
    })
}

/// Split a Scoop hash string into (algorithm, hex). Bare values are sha256.
fn split_hash(raw: &str) -> (String, String) {
    match raw.split_once(':') {
        Some((alg, hex)) => (alg.to_ascii_lowercase(), hex.to_ascii_lowercase()),
        None => ("sha256".to_string(), raw.to_ascii_lowercase()),
    }
}

/// Derive the saved filename and extension from a Scoop URL.
/// Scoop encodes an override filename in a `#/name` fragment (used to make an
/// NSIS installer look like an archive, e.g. `...setup.exe#/dl.7z`).
fn filename_and_ext(url: &str) -> (String, String) {
    let name = if let Some(idx) = url.find("#/") {
        url[idx + 2..].to_string()
    } else {
        let path = url.split(['#', '?']).next().unwrap_or(url);
        path.rsplit('/').next().unwrap_or("download").to_string()
    };
    let ext = name
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    (name, ext)
}

/// Read a string-or-array JSON value into a `Vec<String>`.
fn as_string_list(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    }
}

/// Scoop `persist` entries: a string, or an array of strings / `[source, target]` pairs.
fn persist_sources(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a
            .iter()
            .filter_map(|e| match e {
                Value::String(s) => Some(s.clone()),
                Value::Array(pair) => pair.first().and_then(Value::as_str).map(str::to_string),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Parse a manifest JSON document into a `Manifest`, selecting the 64-bit artifact(s).
pub fn parse_manifest(name: &str, v: &Value) -> Result<Manifest> {
    let version = v["version"].as_str().unwrap_or("").to_string();
    let description = v["description"].as_str().unwrap_or("").to_string();
    let homepage = v["homepage"].as_str().unwrap_or("").to_string();

    // Resolve a key field-by-field: architecture.64bit first, then the top level.
    let arch64 = v.get("architecture").and_then(|a| a.get("64bit"));
    let field = |key: &str| -> Option<&Value> {
        arch64.and_then(|a| a.get(key)).or_else(|| v.get(key))
    };

    let urls = field("url").map(as_string_list).unwrap_or_default();
    if urls.is_empty() {
        return Err(anyhow!("Manifest '{name}' hat keine Download-URL für 64-bit"));
    }
    let hashes = field("hash").map(as_string_list).unwrap_or_default();
    if hashes.len() < urls.len() {
        return Err(anyhow!("Manifest '{name}' hat keinen Hash für jede Download-URL"));
    }
    // extract_dir / extract_to may be a string (applies to the first URL) or an
    // array aligned with the URLs — exactly like Scoop.
    let extract_dirs = field("extract_dir").map(as_string_list).unwrap_or_default();
    let extract_tos = field("extract_to").map(as_string_list).unwrap_or_default();
    let non_empty = |s: Option<&String>| s.filter(|s| !s.is_empty()).cloned();

    let artifacts: Vec<Artifact> = urls
        .iter()
        .enumerate()
        .map(|(i, url)| {
            let (hash_alg, hash_hex) = split_hash(&hashes[i]);
            let (filename, ext) = filename_and_ext(url);
            Artifact {
                url: url.clone(),
                filename,
                ext,
                hash_alg,
                hash_hex,
                extract_dir: non_empty(extract_dirs.get(i)),
                extract_to: non_empty(extract_tos.get(i)),
            }
        })
        .collect();
    let primary = artifacts[0].clone();

    let depends = v.get("depends").map(as_string_list).unwrap_or_default();
    let suggest = v
        .get("suggest")
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .map(|(feat, apps)| (feat.clone(), as_string_list(apps).join(", ")))
                .collect()
        })
        .unwrap_or_default();

    let env_set = field("env_set")
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let env_add_path = field("env_add_path").map(as_string_list).unwrap_or_default();
    let (bin, bin_args) = field("bin").and_then(first_bin).unzip();
    let (shortcut, shortcut_args) = field("shortcuts").and_then(first_shortcut_exe).unzip();
    let persist = v.get("persist").map(persist_sources).unwrap_or_default();

    let needs_installer = field("installer")
        .map(|i| i.get("script").is_some() || i.get("file").is_some())
        .unwrap_or(false);
    let has_scripts = field("pre_install").is_some() || field("post_install").is_some();
    let innosetup = v.get("innosetup").and_then(Value::as_bool).unwrap_or(false);

    Ok(Manifest {
        name: name.to_string(),
        version,
        description,
        homepage,
        url: primary.url,
        filename: primary.filename,
        ext: primary.ext,
        hash_alg: primary.hash_alg,
        hash_hex: primary.hash_hex,
        extract_dir: primary.extract_dir,
        artifacts,
        depends,
        suggest,
        env_set,
        env_add_path,
        bin,
        bin_args: bin_args.flatten(),
        shortcut,
        shortcut_args: shortcut_args.flatten(),
        persist,
        needs_installer,
        has_scripts,
        innosetup,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn arch64_block_without_url_falls_back_to_top_level() {
        // HWiNFO-style: url/hash on top, bin/shortcuts only per architecture.
        let v = json!({
            "version": "8.52",
            "url": "https://example.org/hwi_852.zip",
            "hash": "abc",
            "architecture": { "64bit": { "bin": [["HWiNFO64.exe", "HWiNFO"]],
                                         "shortcuts": [["HWiNFO64.exe", "HWiNFO"]] } }
        });
        let m = parse_manifest("hwinfo", &v).unwrap();
        assert_eq!(m.url, "https://example.org/hwi_852.zip");
        assert_eq!(m.hash_hex, "abc");
        assert_eq!(m.bin.as_deref(), Some("HWiNFO64.exe"));
        assert_eq!(m.launch_target().as_deref(), Some("HWiNFO64.exe"));
    }

    #[test]
    fn arch64_values_win_over_top_level() {
        // 7-Zip-style: MSI + extract_dir per arch, bin/shortcuts on top.
        let v = json!({
            "version": "26.03",
            "bin": ["7z.exe", "7zG.exe", "7zFM.exe"],
            "shortcuts": [["7zFM.exe", "7-Zip\\7-Zip File Manager"], ["7-zip.chm", "Help"]],
            "architecture": { "64bit": { "url": "https://e.org/7z2603-x64.msi",
                                         "hash": "c0ff", "extract_dir": "Files\\7-Zip" } }
        });
        let m = parse_manifest("7zip", &v).unwrap();
        assert_eq!(m.ext, "msi");
        assert_eq!(m.extract_dir.as_deref(), Some("Files\\7-Zip"));
        // The GUI shortcut wins over the console `bin` for "Starten".
        assert_eq!(m.launch_target().as_deref(), Some("7zFM.exe"));
    }

    #[test]
    fn multiple_urls_with_aligned_extract_to() {
        // IrfanView-style: program + plugins, the latter into `Plugins\`.
        let v = json!({
            "version": "4.76",
            "extract_to": ["", "Plugins"],
            "architecture": { "64bit": {
                "url": ["https://e.org/iview476_x64.zip", "https://e.org/iview476_plugins_x64.zip"],
                "hash": ["aa", "bb"],
                "shortcuts": [["i_view64.exe", "IrfanView"]] } }
        });
        let m = parse_manifest("irfanview", &v).unwrap();
        assert_eq!(m.artifacts.len(), 2);
        assert_eq!(m.artifacts[0].extract_to, None);
        assert_eq!(m.artifacts[1].extract_to.as_deref(), Some("Plugins"));
        assert_eq!(m.artifacts[1].hash_hex, "bb");
    }

    #[test]
    fn missing_hash_is_an_error() {
        let v = json!({ "version": "1", "url": ["https://e.org/a.zip", "https://e.org/b.zip"], "hash": "aa" });
        assert!(parse_manifest("x", &v).is_err());
    }

    #[test]
    fn installer_scripts_and_persist_are_flagged() {
        let v = json!({
            "version": "1", "url": "https://e.org/setup.exe", "hash": "aa",
            "installer": { "script": ["Start-Process setup.exe"] },
            "persist": ["data", ["conf.ini", "conf.ini"]]
        });
        let m = parse_manifest("x", &v).unwrap();
        assert!(m.needs_installer);
        assert_eq!(m.persist, vec!["data".to_string(), "conf.ini".to_string()]);
    }

    #[test]
    fn launch_args_follow_the_chosen_target() {
        // Brave-style: args with `$dir` on the shortcut; git-style on a later tuple field.
        let v = json!({
            "version": "1", "url": "https://e.org/b.7z", "hash": "aa",
            "bin": [["brave.exe", "brave", "--user-data-dir=\"$dir\\User Data\""]],
            "shortcuts": [["brave.exe", "Brave", "--user-data-dir=\"$dir\\User Data\""]]
        });
        let m = parse_manifest("brave", &v).unwrap();
        assert_eq!(m.launch_args().as_deref(), Some("--user-data-dir=\"$dir\\User Data\""));
        // No shortcut: the bin tuple's args apply; plain strings carry none.
        let v = json!({ "version": "1", "url": "https://e.org/x.zip", "hash": "aa",
                        "bin": [["tool.exe", "tool", "--portable"]] });
        assert_eq!(parse_manifest("x", &v).unwrap().launch_args().as_deref(), Some("--portable"));
        let v = json!({ "version": "1", "url": "https://e.org/x.zip", "hash": "aa", "bin": "tool.exe",
                        "shortcuts": [["tool.exe", "Tool"]] });
        assert_eq!(parse_manifest("x", &v).unwrap().launch_args(), None);
    }

    #[test]
    fn filename_fragment_overrides_extension() {
        let (name, ext) = filename_and_ext("https://e.org/Setup.exe#/dl.7z");
        assert_eq!((name.as_str(), ext.as_str()), ("dl.7z", "7z"));
    }
}
