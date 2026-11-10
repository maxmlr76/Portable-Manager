//! CLI-Werkzeuge: open a portable shell with a command-line tool (or all of them)
//! already on `PATH`, with the tool's launch env (e.g. `JAVA_HOME`) applied and the
//! working directory set to the tool folder. Also copies suggested commands to the
//! clipboard. Windows-first; the terminal is a fresh PowerShell console.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::catalog;

/// Static CLI metadata for a curated command-line tool. Its folder category comes
/// from the catalog (`category()`), so the two can't drift apart again.
pub struct CliTool {
    pub name: &'static str,
    /// Extra environment lines beyond the PATH entry (display only, shown as chips).
    pub env: &'static [&'static str],
    /// Suggested command snippets (copied to the clipboard on click).
    pub commands: &'static [&'static str],
}

impl CliTool {
    /// The tool's catalog category (= its folder under `Tools\`).
    pub fn category(&self) -> &'static str {
        catalog::find(self.name).map(|e| e.category).unwrap_or("More")
    }
}

/// The curated command-line tools surfaced in the CLI view.
pub fn cli_tools() -> &'static [CliTool] {
    &[
        CliTool {
            name: "nmap",
            env: &[],
            commands: &[
                "nmap -sn 192.168.1.0/24",
                "nmap -sT -sV -T4 <target>",
                "nmap -p- -Pn <target>",
            ],
        },
        CliTool {
            name: "hashcat",
            env: &[],
            commands: &[
                "hashcat -b",
                "hashcat -m 1000 hash.txt rockyou.txt",
                "hashcat -m 22000 wpa.hc22000 rockyou.txt",
            ],
        },
        CliTool {
            name: "gnupg",
            env: &["GNUPGHOME  =  Tools\\Security\\gnupg\\home"],
            commands: &["gpg --list-keys", "gpg --full-generate-key", "gpg -c <file>"],
        },
        CliTool {
            name: "yt-dlp",
            env: &[],
            commands: &["yt-dlp <url>", "yt-dlp -x --audio-format mp3 <url>", "yt-dlp -F <url>"],
        },
        CliTool {
            name: "ffmpeg",
            env: &[],
            commands: &["ffmpeg -i <input> <output.mp4>", "ffmpeg -i <video> -vn -c:a copy <audio.m4a>", "ffprobe <file>"],
        },
        CliTool {
            name: "ripgrep",
            env: &[],
            commands: &["rg <pattern>", "rg -i -n <pattern> <path>", "rg --files -g \"*.txt\""],
        },
        CliTool {
            name: "fzf",
            env: &[],
            commands: &["fzf", "Get-ChildItem -Recurse | fzf"],
        },
        CliTool {
            name: "nodejs",
            env: &[],
            commands: &["node --version", "npx <package>", "node <script.js>"],
        },
        CliTool {
            name: "ghidra",
            env: &["JAVA_HOME  =  Tools\\_runtime\\<jdk>", "PATH  +=  %JAVA_HOME%\\bin"],
            commands: &["analyzeHeadless <project-folder> Proj -import <file>"],
        },
    ]
}

pub fn find_cli(name: &str) -> Option<&'static CliTool> {
    cli_tools().iter().find(|t| t.name == name)
}

/// A concrete shell session: what to put on PATH and in the environment.
pub struct CliSession {
    pub tool_dir: PathBuf,
    pub path_dirs: Vec<String>,
    pub env: Vec<(String, String)>,
}

fn abs_string(p: &Path) -> String {
    let a = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    a.to_string_lossy().trim_start_matches(r"\\?\").to_string()
}

fn push_unique(dirs: &mut Vec<String>, d: String) {
    if !d.is_empty() && !dirs.iter().any(|x| x.eq_ignore_ascii_case(&d)) {
        dirs.push(d);
    }
}

/// Resolve the PATH dirs + env for one installed CLI tool.
pub fn cli_session(root: &Path, name: &str) -> Result<CliSession> {
    let t = find_cli(name).ok_or_else(|| anyhow!(crate::tr!("'{name}' is not a CLI tool", "'{name}' ist kein CLI-Werkzeug")))?;
    tool_session(root, t.category(), name)
}

/// PATH dirs + env for any installed tool in `Tools\<category>\<name>`.
pub fn tool_session(root: &Path, category: &str, name: &str) -> Result<CliSession> {
    let dir = catalog::install_dir(root, category, name);
    if !dir.exists() {
        return Err(anyhow!(crate::tr!("{name} is not installed", "{name} ist nicht installiert")));
    }

    let (env, mut path_dirs) = catalog::resolved_launch_env(root, category, name);
    // Always expose the tool folder and, if found, the folder holding its binary.
    if let Some(target) = catalog::find_run_target(&dir) {
        if let Some(parent) = target.parent() {
            push_unique(&mut path_dirs, abs_string(parent));
        }
    }
    push_unique(&mut path_dirs, abs_string(&dir));

    Ok(CliSession { tool_dir: dir, path_dirs, env })
}

/// Escape a value for a PowerShell single-quoted string literal.
fn ps_lit(s: &str) -> String {
    // PowerShell also ends a string at the typographic quotes, so double those too.
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// Environment variable names come from manifests and go into the script as code.
fn valid_env_name(k: &str) -> bool {
    let mut chars = k.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Spawn a fresh PowerShell console running `startup`, then stay interactive.
fn spawn_powershell(startup: &str) -> Result<()> {
    let mut cmd = Command::new("powershell");
    cmd.args(["-NoLogo", "-NoExit", "-Command", startup]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        cmd.creation_flags(CREATE_NEW_CONSOLE);
    }
    cmd.spawn().context(crate::tr!("could not open the terminal (PowerShell)", "Terminal (PowerShell) konnte nicht geöffnet werden"))?;
    Ok(())
}

/// Open a terminal in `working_dir` with `path_dirs` prepended to PATH, `env`
/// applied, and a header + example commands printed.
pub fn open_terminal(
    working_dir: &Path,
    path_dirs: &[String],
    env: &[(String, String)],
    header: &str,
    examples: &[String],
) -> Result<()> {
    let mut s = String::new();
    for (k, v) in env {
        if !valid_env_name(k) {
            continue;
        }
        s.push_str(&format!("$env:{} = {}; ", k, ps_lit(v)));
    }
    if !path_dirs.is_empty() {
        s.push_str(&format!("$env:PATH = {} + ';' + $env:PATH; ", ps_lit(&path_dirs.join(";"))));
    }
    s.push_str(&format!("Set-Location -LiteralPath {}; ", ps_lit(&working_dir.to_string_lossy())));
    s.push_str(&format!("Write-Host {} -ForegroundColor Cyan; ", ps_lit(header)));
    if !examples.is_empty() {
        s.push_str("Write-Host ''; Write-Host 'Beispiele (anpassen & ausfuehren):' -ForegroundColor DarkGray; ");
        for ex in examples {
            s.push_str(&format!("Write-Host {} -ForegroundColor Gray; ", ps_lit(&format!("  {ex}"))));
        }
        s.push_str("Write-Host ''; ");
    }
    spawn_powershell(&s)
}

/// Open a terminal for a single installed tool (curated examples if there are any).
pub fn open_tool_terminal(root: &Path, category: &str, name: &str) -> Result<()> {
    let sess = tool_session(root, category, name)?;
    let examples: Vec<String> = find_cli(name)
        .map(|t| t.commands.iter().map(|c| c.to_string()).collect())
        .unwrap_or_default();
    let header = format!("Maxtrix CLI  -  {} liegt im PATH.", catalog::display_name(name));
    open_terminal(&sess.tool_dir, &sess.path_dirs, &sess.env, &header, &examples)
}

/// Open one terminal ("IT-Konsole") with every installed CLI tool on PATH.
pub fn open_console(root: &Path) -> Result<()> {
    let mut path_dirs: Vec<String> = Vec::new();
    let mut env: Vec<(String, String)> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for t in cli_tools() {
        if let Ok(sess) = cli_session(root, t.name) {
            names.push(t.name.to_string());
            for p in sess.path_dirs {
                push_unique(&mut path_dirs, p);
            }
            for (k, v) in sess.env {
                if !env.iter().any(|(ek, _)| ek == &k) {
                    env.push((k, v));
                }
            }
        }
    }
    if names.is_empty() {
        return Err(anyhow!(crate::tr!("no CLI tools installed", "keine CLI-Werkzeuge installiert")));
    }
    let header = format!("Maxtrix IT-Konsole  -  im PATH: {}", names.join(", "));
    open_terminal(root, &path_dirs, &env, &header, &[])
}

/// Copy text to the Windows clipboard via `clip`, without flashing a window.
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    use std::io::Write;
    let mut cmd = Command::new("cmd");
    cmd.args(["/C", "clip"]);
    cmd.stdin(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().context(crate::tr!("clipboard not available", "Zwischenablage nicht erreichbar"))?;
    if let Some(mut si) = child.stdin.take() {
        si.write_all(text.as_bytes()).ok();
    }
    child.wait().ok();
    Ok(())
}

#[cfg(test)]
mod injection_tests {
    use super::*;

    #[test]
    fn script_text_cannot_be_broken_out_of() {
        assert_eq!(ps_lit("a'b"), "'a''b'");
        assert_eq!(ps_lit("a\u{2019}b"), "'a\u{2019}\u{2019}b'");
        assert!(valid_env_name("JAVA_HOME"));
        assert!(!valid_env_name("X; calc"));
        assert!(!valid_env_name("1A"));
        assert!(!valid_env_name(""));
    }
}
