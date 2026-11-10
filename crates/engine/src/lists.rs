//! Share a user list as a small JSON file. Only tool names travel: on import a
//! tool is installed from the curated catalog, never from a source the file names,
//! so a list from someone else cannot point the app at an arbitrary download.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::resolver::valid_tool_name;

/// Marks the file as a Portable Manager list.
pub const FORMAT: &str = "maxtrix-portable-manager-list";
const VERSION: u64 = 1;
/// Plenty for any real list, small enough that a bad file can't hurt.
const MAX_TOOLS: usize = 500;
pub const MAX_FILE_BYTES: u64 = 1 << 20;
const MAX_NAME_CHARS: usize = 60;

/// The file content for a list.
pub fn to_json(name: &str, tools: &[String]) -> String {
    let tools: Vec<Value> = tools.iter().map(|t| json!({ "name": t })).collect();
    let doc = json!({ "format": FORMAT, "version": VERSION, "name": name, "tools": tools });
    serde_json::to_string_pretty(&doc).unwrap_or_default()
}

/// Read a list file: its name and the tool names, checked and without duplicates.
pub fn parse(text: &str) -> Result<(String, Vec<String>)> {
    let doc: Value = serde_json::from_str(text)
        .context(crate::tr!("the file is not a valid list (no JSON)", "Die Datei ist keine gültige Liste (kein JSON)"))?;
    if doc["format"].as_str() != Some(FORMAT) {
        bail!(crate::tr!("the file is not a Portable Manager list", "Die Datei ist keine Portable-Manager-Liste"));
    }
    if doc["version"].as_u64().unwrap_or(0) > VERSION {
        bail!(crate::tr!("the list was made by a newer version of the app", "Die Liste stammt aus einer neueren Version der App"));
    }
    let name: String = doc["name"]
        .as_str()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_CHARS)
        .collect::<String>()
        .trim()
        .to_string();
    if name.is_empty() {
        bail!(crate::tr!("the list has no name", "Die Liste hat keinen Namen"));
    }
    let mut tools: Vec<String> = Vec::new();
    for entry in doc["tools"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
        // Accept {"name": "nmap"} and plain "nmap".
        let Some(tool) = entry["name"].as_str().or_else(|| entry.as_str()) else { continue };
        let tool = tool.trim().to_ascii_lowercase();
        if valid_tool_name(&tool) && !tools.contains(&tool) {
            tools.push(tool);
        }
        if tools.len() >= MAX_TOOLS {
            break;
        }
    }
    Ok((name, tools))
}

/// `name`, or `name (2)`, `name (3)`, … if a list of that name already exists.
pub fn unique_name(name: &str, taken: &[String]) -> String {
    let free = |n: &str| !taken.iter().any(|t| t.eq_ignore_ascii_case(n));
    if free(name) {
        return name.to_string();
    }
    (2..).map(|i| format!("{name} ({i})")).find(|n| free(n)).unwrap_or_default()
}

/// A file name for the export, without characters Windows rejects.
pub fn file_name(list: &str) -> String {
    let clean: String = list
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.') { c } else { '_' })
        .collect();
    let clean = clean.trim().trim_matches('.');
    format!("{}.json", if clean.is_empty() { "list" } else { clean })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_survives_the_round_trip() {
        let text = to_json("Pentest", &["nmap".into(), "wireshark".into()]);
        assert_eq!(parse(&text).unwrap(), ("Pentest".to_string(), vec!["nmap".to_string(), "wireshark".to_string()]));
    }

    #[test]
    fn bad_entries_are_dropped_and_bad_files_rejected() {
        let text = format!(
            r#"{{"format":"{FORMAT}","version":1,"name":"  X\u0007 ","tools":[{{"name":"NMAP"}},"nmap","..\\evil","a/b",{{"name":"7zip"}},{{"repo":"x"}}]}}"#
        );
        assert_eq!(parse(&text).unwrap(), ("X".to_string(), vec!["nmap".to_string(), "7zip".to_string()]));
        assert!(parse("{}").is_err());
        assert!(parse("no json").is_err());
        assert!(parse(&format!(r#"{{"format":"{FORMAT}","name":"","tools":[]}}"#)).is_err());
        assert!(parse(&format!(r#"{{"format":"{FORMAT}","version":9,"name":"A","tools":[]}}"#)).is_err());
    }

    #[test]
    fn names_stay_unique_and_file_names_safe() {
        let taken = vec!["Pentest".to_string(), "pentest (2)".to_string()];
        assert_eq!(unique_name("Pentest", &taken), "Pentest (3)");
        assert_eq!(unique_name("Other", &taken), "Other");
        assert_eq!(file_name("Daten/Rettung: 1"), "Daten_Rettung_ 1.json");
        assert_eq!(file_name("..."), "list.json");
    }
}
