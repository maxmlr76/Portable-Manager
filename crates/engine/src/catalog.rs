//! Curated catalog + install orchestration shared by the CLI and the GUI.
//!
//! The curated list is the "Maxtrix picks" front row; each entry maps to a Scoop
//! manifest fetched on demand. Full-index search is a later addition.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::install::{install, InstallEvent, InstallReport};
use crate::manifest::{parse_manifest, Manifest};
use crate::resolver::{self, LaunchProfile};
use crate::{fsutil, net, persist};

/// Name of the per-tool install-metadata file written into each tool dir.
const META_FILE: &str = ".maxtrix.json";
/// Name of the per-tool launch profile.
const LAUNCH_FILE: &str = ".maxtrix-launch.json";

/// Shared install root for auto-installed plugins/runtimes (e.g. a JDK).
pub const RUNTIME_CAT: &str = "_runtime";

/// A curated catalog entry (display metadata; the manifest is fetched on demand).
pub struct CatalogEntry {
    /// Scoop app id (folder name on the stick, manifest name).
    pub name: &'static str,
    /// Display name with the maker's spelling ("7-Zip", "Notepad++").
    pub title: &'static str,
    /// Scoop bucket name ("main", "extras", …).
    pub bucket: &'static str,
    pub category: &'static str,
    pub summary: &'static str,
}

/// The curated front-row catalog. Every entry is proven-installable by the engine
/// (`maxtrix audit --install` checks this). Packages that only work through a
/// Scoop setup script (Python, GIMP, PowerToys, FastCopy, HxD, AutoHotkey,
/// FileZilla) are deliberately left out — they can't be installed portably.
/// A catalog entry's one-line description in the current language.
pub fn summary(e: &CatalogEntry) -> &'static str {
    if !crate::i18n::is_german() {
        return e.summary;
    }
    match e.name {
        "7zip" => "Entpackt & packt nahezu jedes Archivformat.",
        "firefox" => "Datenschutzfreundlicher Standard-Browser.",
        "brave" => "Chromium-Browser mit Werbe- & Tracker-Blocker.",
        "thunderbird" => "Mail-, Kalender- & Newsreader-Client.",
        "qbittorrent" => "Werbefreier BitTorrent-Client.",
        "yt-dlp" => "Video/Audio von 1000+ Seiten laden (CLI).",
        "everything" => "Sofort-Dateisuche in Echtzeit.",
        "winmerge" => "Datei- & Ordnervergleich mit Merge.",
        "peazip" => "Archivmanager mit starker Verschlüsselung.",
        "hwinfo" => "Tiefe Hardware-Infos & Live-Sensoren.",
        "cpu-z" => "CPU-, RAM- & Mainboard-Details.",
        "gpu-z" => "Detailinfos zur Grafikkarte.",
        "systeminformer" => "Prozess- & Handle-Explorer (ex Process Hacker).",
        "bleachbit" => "System aufräumen & Spuren löschen.",
        "keepassxc" => "Offline-Passwortmanager, plattformübergreifend.",
        "cryptomator" => "Transparente Cloud-Ordner-Verschlüsselung.",
        "gnupg" => "OpenPGP-Signatur & -Verschlüsselung (CLI).",
        "hashcat" => "GPU-beschleunigte Passwort-Recovery.",
        "eraser" => "Dateien sicher überschreiben & löschen.",
        "nmap" => "Netzwerk- & Portscanner mit Service-Erkennung.",
        "wireshark" => "Netzwerkprotokoll-Analyse.",
        "putty" => "SSH-, Telnet- & Serial-Client.",
        "winscp" => "SFTP-, SCP- & FTP-Dateitransfer.",
        "rufus" => "Bootfähige USB-Sticks erstellen.",
        "ventoy" => "Multi-ISO-Boot-Stick.",
        "etcher" => "Images sicher auf USB/SD schreiben.",
        "wiztree" => "Sehr schnelle Speicherplatz-Analyse.",
        "testdisk" => "Partitionen retten & Dateien wiederherstellen.",
        "ghidra" => "NSA Reverse-Engineering-Suite – zieht das JDK automatisch mit.",
        "imhex" => "Moderner Hex-Editor mit Pattern-Language.",
        "x64dbg" => "Open-Source x64/x32-Debugger für Windows.",
        "cutter" => "GUI für Rizin (Reverse Engineering).",
        "dnspy" => ".NET-Decompiler & -Debugger.",
        "git" => "Versionsverwaltung.",
        "nodejs" => "JavaScript-Runtime.",
        "ripgrep" => "Blitzschnelle rekursive Codesuche.",
        "fzf" => "Fuzzy-Finder für die Kommandozeile.",
        "vscode" => "Erweiterbarer Code-Editor.",
        "notepadplusplus" => "Text- & Quellcode-Editor.",
        "windows-terminal" => "Moderner Terminal-Emulator.",
        "vlc" => "Spielt praktisch jedes Medienformat.",
        "mpv" => "Schlanker, skriptbarer Player.",
        "audacity" => "Audio aufnehmen & schneiden.",
        "obs-studio" => "Bildschirmaufnahme & Streaming.",
        "ffmpeg" => "Audio/Video-Konvertierung (CLI).",
        "handbrake" => "Video transkodieren & rippen.",
        "inkscape" => "Vektorgrafik-Editor.",
        "irfanview" => "Schneller Bildbetrachter & -konverter.",
        "sharex" => "Screenshots, Aufnahmen & Upload.",
        "sumatrapdf" => "Schneller, schlanker PDF-Reader.",
        _ => e.summary,
    }
}

/// Category folders under `Tools\` were German before 0.1.2: (old, new).
pub const LEGACY_CATEGORIES: &[(&str, &str)] = &[
    ("Dateien", "Files"),
    ("Sicherheit", "Security"),
    ("Netzwerk", "Network"),
    ("Datenträger", "Disks"),
    ("Entwicklung", "Development"),
    ("Grafik", "Graphics"),
    ("Weitere", "More"),
];

/// Display name of a category. Folder names are English; German shows the
/// translation. Unknown (user-made) folders keep their own name.
pub fn category_label(cat: &str) -> String {
    if crate::i18n::is_german() {
        if let Some((de, _)) = LEGACY_CATEGORIES.iter().find(|(_, en)| *en == cat) {
            return de.to_string();
        }
    }
    cat.to_string()
}

/// Move tools from the old German category folders into the English ones, once
/// per drive. A tool whose new folder already exists, or that is in use, stays
/// where it is (and is tried again at the next start). Launch profiles hold
/// drive-relative paths with the category in them, so they are rewritten too.
/// Returns how many tools were moved.
pub fn migrate_legacy_categories(root: &Path) -> usize {
    let tools = root.join("Tools");
    let mut moved = 0;
    for (de, en) in LEGACY_CATEGORIES {
        let old_dir = tools.join(de);
        let Ok(entries) = std::fs::read_dir(&old_dir) else { continue };
        let new_dir = tools.join(en);
        for entry in entries.flatten() {
            let name = entry.file_name();
            let target = new_dir.join(&name);
            if target.exists() || std::fs::create_dir_all(&new_dir).is_err() {
                continue;
            }
            if std::fs::rename(entry.path(), &target).is_err() {
                continue;
            }
            moved += 1;
            let profile = target.join(LAUNCH_FILE);
            if let Ok(text) = std::fs::read_to_string(&profile) {
                let fixed = rewrite_category(&text, de, en);
                if fixed != text {
                    let _ = std::fs::write(&profile, fixed);
                }
            }
        }
        // Remove the old folder once it is empty.
        let _ = std::fs::remove_dir(&old_dir);
    }
    moved
}

/// Replace the category segment of `Tools\<old>\` paths (as written in JSON, with
/// escaped or plain backslashes and with forward slashes).
fn rewrite_category(text: &str, old: &str, new: &str) -> String {
    let mut out = text.to_string();
    for sep in ["\\\\", "\\", "/"] {
        out = out.replace(&format!("Tools{sep}{old}{sep}"), &format!("Tools{sep}{new}{sep}"));
    }
    out
}

pub fn curated() -> &'static [CatalogEntry] {
    &[
        // ── Recommended (hero of the Discover page = first entry) ──
        CatalogEntry { name: "7zip", title: "7-Zip", bucket: "main", category: "Files", summary: "Unpacks and packs nearly every archive format." },

        // ── Browser & Internet ──
        CatalogEntry { name: "firefox", title: "Firefox", bucket: "extras", category: "Browser", summary: "Privacy-friendly standard browser." },
        CatalogEntry { name: "brave", title: "Brave", bucket: "extras", category: "Browser", summary: "Chromium browser with ad and tracker blocker." },
        CatalogEntry { name: "thunderbird", title: "Thunderbird", bucket: "extras", category: "Browser", summary: "Mail, calendar and news client." },
        CatalogEntry { name: "qbittorrent", title: "qBittorrent", bucket: "extras", category: "Browser", summary: "Ad-free BitTorrent client." },
        CatalogEntry { name: "yt-dlp", title: "yt-dlp", bucket: "main", category: "Browser", summary: "Download video/audio from 1000+ sites (CLI)." },

        // ── Dateien & Archive ──
        CatalogEntry { name: "everything", title: "Everything", bucket: "extras", category: "Files", summary: "Instant real-time file search." },
        CatalogEntry { name: "winmerge", title: "WinMerge", bucket: "extras", category: "Files", summary: "File and folder comparison with merge." },
        CatalogEntry { name: "peazip", title: "PeaZip", bucket: "extras", category: "Files", summary: "Archive manager with strong encryption." },

        // ── System-Info & Wartung ──
        CatalogEntry { name: "hwinfo", title: "HWiNFO", bucket: "extras", category: "System", summary: "Deep hardware info and live sensors." },
        CatalogEntry { name: "cpu-z", title: "CPU-Z", bucket: "extras", category: "System", summary: "CPU, RAM and mainboard details." },
        CatalogEntry { name: "gpu-z", title: "GPU-Z", bucket: "extras", category: "System", summary: "Detailed graphics card info." },
        CatalogEntry { name: "systeminformer", title: "System Informer", bucket: "extras", category: "System", summary: "Process and handle explorer (formerly Process Hacker)." },
        CatalogEntry { name: "bleachbit", title: "BleachBit", bucket: "extras", category: "System", summary: "Clean up the system and erase traces." },

        // ── Security & encryption ──
        CatalogEntry { name: "keepassxc", title: "KeePassXC", bucket: "extras", category: "Security", summary: "Offline password manager, cross-platform." },
        CatalogEntry { name: "cryptomator", title: "Cryptomator", bucket: "extras", category: "Security", summary: "Transparent cloud folder encryption." },
        CatalogEntry { name: "gnupg", title: "GnuPG", bucket: "main", category: "Security", summary: "OpenPGP signing and encryption (CLI)." },
        CatalogEntry { name: "hashcat", title: "hashcat", bucket: "main", category: "Security", summary: "GPU-accelerated password recovery." },
        CatalogEntry { name: "eraser", title: "Eraser", bucket: "extras", category: "Security", summary: "Securely overwrite and delete files." },

        // ── Netzwerk & Fernzugriff ──
        CatalogEntry { name: "nmap", title: "Nmap", bucket: "main", category: "Network", summary: "Network and port scanner with service detection." },
        CatalogEntry { name: "wireshark", title: "Wireshark", bucket: "extras", category: "Network", summary: "Network protocol analysis." },
        CatalogEntry { name: "putty", title: "PuTTY", bucket: "extras", category: "Network", summary: "SSH, Telnet and serial client." },
        CatalogEntry { name: "winscp", title: "WinSCP", bucket: "extras", category: "Network", summary: "SFTP, SCP and FTP file transfer." },

        // ── Disks, boot & recovery ──
        CatalogEntry { name: "rufus", title: "Rufus", bucket: "extras", category: "Disks", summary: "Create bootable USB drives." },
        CatalogEntry { name: "ventoy", title: "Ventoy", bucket: "extras", category: "Disks", summary: "Multi-ISO boot drive." },
        CatalogEntry { name: "etcher", title: "balenaEtcher", bucket: "extras", category: "Disks", summary: "Write images safely to USB/SD." },
        CatalogEntry { name: "wiztree", title: "WizTree", bucket: "extras", category: "Disks", summary: "Very fast disk space analysis." },
        CatalogEntry { name: "testdisk", title: "TestDisk & PhotoRec", bucket: "extras", category: "Disks", summary: "Rescue partitions and recover files." },

        // ── Reverse Engineering & Forensik ──
        CatalogEntry { name: "ghidra", title: "Ghidra", bucket: "extras", category: "Reverse Engineering", summary: "NSA reverse engineering suite, brings the JDK along automatically." },
        CatalogEntry { name: "imhex", title: "ImHex", bucket: "extras", category: "Reverse Engineering", summary: "Modern hex editor with a pattern language." },
        CatalogEntry { name: "x64dbg", title: "x64dbg", bucket: "extras", category: "Reverse Engineering", summary: "Open-source x64/x32 debugger for Windows." },
        CatalogEntry { name: "cutter", title: "Cutter", bucket: "extras", category: "Reverse Engineering", summary: "GUI for Rizin (reverse engineering)." },
        CatalogEntry { name: "dnspy", title: "dnSpy", bucket: "extras", category: "Reverse Engineering", summary: ".NET decompiler and debugger." },

        // ── Entwicklung, Editoren & CLI ──
        CatalogEntry { name: "git", title: "Git", bucket: "main", category: "Development", summary: "Version control." },
        CatalogEntry { name: "nodejs", title: "Node.js", bucket: "main", category: "Development", summary: "JavaScript runtime." },
        CatalogEntry { name: "ripgrep", title: "ripgrep", bucket: "main", category: "Development", summary: "Lightning-fast recursive code search." },
        CatalogEntry { name: "fzf", title: "fzf", bucket: "main", category: "Development", summary: "Fuzzy finder for the command line." },
        CatalogEntry { name: "vscode", title: "VS Code", bucket: "extras", category: "Development", summary: "Extensible code editor." },
        CatalogEntry { name: "notepadplusplus", title: "Notepad++", bucket: "extras", category: "Development", summary: "Text and source code editor." },
        CatalogEntry { name: "windows-terminal", title: "Windows Terminal", bucket: "extras", category: "Development", summary: "Modern terminal emulator." },

        // ── Multimedia & Audio/Video ──
        CatalogEntry { name: "vlc", title: "VLC", bucket: "extras", category: "Multimedia", summary: "Plays practically every media format." },
        CatalogEntry { name: "mpv", title: "mpv", bucket: "extras", category: "Multimedia", summary: "Lean, scriptable player." },
        CatalogEntry { name: "audacity", title: "Audacity", bucket: "extras", category: "Multimedia", summary: "Record and edit audio." },
        CatalogEntry { name: "obs-studio", title: "OBS Studio", bucket: "extras", category: "Multimedia", summary: "Screen recording and streaming." },
        CatalogEntry { name: "ffmpeg", title: "FFmpeg", bucket: "main", category: "Multimedia", summary: "Audio/video conversion (CLI)." },
        CatalogEntry { name: "handbrake", title: "HandBrake", bucket: "extras", category: "Multimedia", summary: "Transcode and rip video." },

        // ── Graphics & productivity ──
        CatalogEntry { name: "inkscape", title: "Inkscape", bucket: "extras", category: "Graphics", summary: "Vector graphics editor." },
        CatalogEntry { name: "irfanview", title: "IrfanView", bucket: "extras", category: "Graphics", summary: "Fast image viewer and converter." },
        CatalogEntry { name: "sharex", title: "ShareX", bucket: "extras", category: "Graphics", summary: "Screenshots, recordings and upload." },
        CatalogEntry { name: "sumatrapdf", title: "SumatraPDF", bucket: "extras", category: "Graphics", summary: "Fast, lean PDF reader." },
    ]
}

pub fn find(name: &str) -> Option<&'static CatalogEntry> {
    curated().iter().find(|e| e.name == name)
}

/// Name to show for a tool: the curated title, else the id as-is (search hits and
/// hand-placed folders keep their own spelling).
pub fn display_name(name: &str) -> String {
    find(name).map(|e| e.title.to_string()).unwrap_or_else(|| name.to_string())
}

pub fn known() -> String {
    curated().iter().map(|e| e.name).collect::<Vec<_>>().join(", ")
}

/// Raw manifest URL for a `bucket/name` on github.com/ScoopInstaller.
pub fn raw_url(bucket: &str, name: &str) -> Result<String> {
    let repo = resolver::bucket_repo(bucket)
        .ok_or_else(|| anyhow!("unbekannter Bucket '{bucket}'"))?;
    Ok(format!("https://raw.githubusercontent.com/ScoopInstaller/{repo}/master/bucket/{name}.json"))
}

fn fetch_manifest(url: &str, name: &str) -> Result<Manifest> {
    let text = net::fetch_text(url)?;
    let json: serde_json::Value =
        serde_json::from_str(&text).context(crate::tr!("manifest is not valid JSON", "Manifest ist kein gültiges JSON"))?;
    parse_manifest(name, &json)
}

/// Fetch a curated tool's manifest -> (manifest, catalog entry).
pub fn fetch_named(name: &str) -> Result<(Manifest, &'static CatalogEntry)> {
    let entry = find(name).ok_or_else(|| anyhow!(crate::tr!("'{name}' is not in the catalog: {}", "'{name}' ist nicht im Katalog: {}", known())))?;
    let m = fetch_manifest(&raw_url(entry.bucket, name)?, name)?;
    Ok((m, entry))
}

/// Fetch a dependency manifest by Scoop "bucket/name" reference.
pub fn fetch_ref(reference: &str) -> Result<Manifest> {
    let (bucket, name) = resolver::parse_ref(reference);
    if !resolver::valid_tool_name(&name) {
        return Err(anyhow!(crate::tr!("invalid tool name in reference: {reference}", "Ungültiger Tool-Name im Verweis: {reference}")));
    }
    fetch_manifest(&raw_url(&bucket, &name)?, &name)
}

/// Last line of defence: whatever `name` holds, the result stays one folder below the
/// category. Names from outside are rejected earlier by `resolver::valid_tool_name`;
/// folder names found on disk may contain spaces and stay as they are.
pub fn install_dir(root: &Path, category: &str, name: &str) -> PathBuf {
    let safe = name.replace(['/', '\\', ':'], "_");
    let safe = if safe.trim_matches(['.', ' ']).is_empty() { "_".to_string() } else { safe };
    root.join("Tools").join(category).join(safe)
}

fn dir_nonempty(p: &Path) -> bool {
    std::fs::read_dir(p).map(|mut it| it.next().is_some()).unwrap_or(false)
}

/// Maxtrix's own additions where a Scoop manifest relies on a PowerShell script
/// (which Maxtrix never runs) to make a tool portable.
#[derive(Default)]
pub struct Overlay {
    /// Start target inside the tool folder; replaces the manifest's.
    pub run: Option<&'static str>,
    /// Start arguments (may use `$dir`); replace the manifest's.
    pub args: Option<&'static str>,
    /// Files that must exist for the portable mode (created empty).
    pub markers: &'static [&'static str],
    /// An `.msi` (file name) inside the unpacked package to lay out as well, and
    /// the folder of it to keep — what Scoop's `Expand-MsiArchive` helper does in
    /// a script. Found by name: where 7-Zip puts it differs between versions.
    pub nested_msi: Option<(&'static str, &'static str)>,
    /// Rename the program file starting with `.0` (and ending in `.exe`) to `.1` —
    /// for packages that ship a versioned file name (`SumatraPDF-3.6.1-64.exe`).
    pub rename_exe: Option<(&'static str, &'static str)>,
}

pub fn overlay(name: &str) -> Overlay {
    match name {
        // Scoop runs `-CreateProfile`, which registers the profile on the host PC;
        // `-profile` keeps it entirely in the tool folder instead.
        "firefox" | "thunderbird" => Overlay { args: Some(r#"-profile "$dir\profile""#), ..Default::default() },
        // Scoop's post_install creates it; it switches GnuPG to portable mode (`home\`).
        "gnupg" => Overlay { markers: &[r"bin\gpgconf.ctl"], ..Default::default() },
        // Scoop writes a `hashcat.cmd` that only changes into the tool folder
        // before running `hashcat.exe` — Maxtrix starts every tool in its folder.
        "hashcat" => Overlay { run: Some("hashcat.exe"), ..Default::default() },
        // The installer only wraps the real package.
        // Scoop's script renames the versioned exe; its settings file = portable mode.
        "sumatrapdf" => Overlay { rename_exe: Some(("SumatraPDF-", "SumatraPDF.exe")), ..Default::default() },
        "eraser" => Overlay { nested_msi: Some(("Eraser (x64).msi", r"PFiles64\Eraser")), ..Default::default() },
        _ => Overlay::default(),
    }
}

/// Result of a full curated install (main tool + resolved plugins).
pub struct InstallSummary {
    pub report: InstallReport,
    pub plugins: Vec<String>,
    pub profile: LaunchProfile,
}

/// Install an already-fetched manifest into `<root>/Tools/<category>/`: resolve +
/// install plugins (shared runtime dir, deduped), then the main tool. Every new
/// version is built in a staging folder, gets the user's data (`persist`) from the
/// old version and is only then swapped in — a failed or cancelled update leaves
/// the installed version exactly as it was. `on` receives progress events;
/// setting `cancel` aborts with a [`crate::Cancelled`] error.
pub fn install_resolved(
    main: &Manifest,
    root: &Path,
    category: &str,
    on: &dyn Fn(InstallEvent),
    cancel: &AtomicBool,
) -> Result<InstallSummary> {
    if !resolver::valid_tool_name(&main.name) {
        return Err(anyhow!(crate::tr!("invalid tool name: {}", "Ungültiger Tool-Name: {}", main.name)));
    }
    let main_dir = install_dir(root, category, &main.name);
    // A running tool can't be swapped — say so before a long download.
    fsutil::ensure_not_in_use(&main_dir, &main.name)?;

    let needs = resolver::plugin_needs(main);
    let mut plugins: Vec<(Manifest, PathBuf)> = Vec::new();
    for need in &needs {
        let (_, pname) = resolver::parse_ref(&need.reference);
        let pdir = install_dir(root, RUNTIME_CAT, &pname);
        let pm = match fetch_ref(&need.reference) {
            Ok(pm) => pm,
            Err(e) => {
                on(InstallEvent::Phase(&crate::tr!("Plugin {} skipped: {e}", "Plugin {} übersprungen: {e}", need.reference)));
                continue;
            }
        };
        if dir_nonempty(&pdir) {
            on(InstallEvent::Phase(&crate::tr!("Plugin {} already present, skipping.", "Plugin {} bereits vorhanden – überspringe.", pm.name)));
        } else {
            on(InstallEvent::Phase(&crate::tr!("Plugin for '{}': {}", "Plugin für '{}': {}", need.feature, pm.name)));
            let r = install(&pm, root, RUNTIME_CAT, on, cancel)?;
            if let Some(reason) = &r.pending_reason {
                on(InstallEvent::Phase(&crate::tr!("Plugin {} cannot be unpacked: {reason}", "Plugin {} nicht entpackbar: {reason}", pm.name)));
                continue;
            }
            if let Err(e) = fsutil::commit_dir(&r.staging, &pdir, &pm.name) {
                let _ = std::fs::remove_dir_all(&r.staging);
                return Err(e);
            }
        }
        plugins.push((pm, pdir));
    }

    let ov = overlay(&main.name);
    let mut report = install(main, root, category, on, cancel)?;
    let profile = resolver::build_launch_profile(main, &main_dir, &plugins, root, ov.run, ov.args);
    let plugin_names: Vec<String> = plugins.iter().map(|(m, _)| m.name.clone()).collect();
    if report.pending_reason.is_some() {
        // Nothing usable was staged (and the staging folder is gone already).
        return Ok(InstallSummary { report, plugins: plugin_names, profile });
    }

    let stage = report.staging.clone();
    let old = main_dir.is_dir().then_some(main_dir.as_path());
    let prepared = (|| -> Result<persist::Carried> {
        // Unpacking can't be interrupted — a cancel during it still ends here,
        // before anything of the installed version is touched.
        if cancel.load(Ordering::Relaxed) {
            return Err(crate::Cancelled.into());
        }
        if let Some((msi_name, keep)) = ov.nested_msi {
            on(InstallEvent::Phase(&crate::tr!("  Unpacking inner MSI …", "  Entpacke inneres MSI …")));
            let msi = find_file(&stage, msi_name, 0)
                .ok_or_else(|| anyhow!(crate::tr!("'{msi_name}' not found in the package", "'{msi_name}' nicht im Paket gefunden")))?;
            crate::msi::extract_msi_file(&msi, &stage, Some(Path::new(keep)))?;
            // The wrapper only carried the MSI packages — they aren't part of the tool.
            remove_wrapper_msis(&stage);
        }
        if let Some((prefix, to)) = ov.rename_exe {
            if !stage.join(to).exists() {
                let versioned = std::fs::read_dir(&stage)?.flatten().find(|e| {
                    let n = e.file_name().to_string_lossy().to_ascii_lowercase();
                    n.starts_with(&prefix.to_ascii_lowercase()) && n.ends_with(".exe")
                });
                if let Some(e) = versioned {
                    std::fs::rename(e.path(), stage.join(to))?;
                }
            }
        }
        // Never report "installiert" for something that can't be started.
        if let Err(problem) = validate_install(&stage, &profile, report.files) {
            let hint = if main.needs_installer || main.innosetup || main.ext == "exe" {
                crate::tr!(". The package needs a setup program, which Maxtrix deliberately does not run", " – das Paket braucht ein Setup-Programm, das Maxtrix bewusst nicht ausführt")
            } else {
                String::new()
            };
            return Err(anyhow!(crate::tr!("{} cannot be installed portably: {problem}{hint}", "{} ist nicht portabel installierbar: {problem}{hint}", main.name)));
        }
        // The tool may have been started during the download.
        fsutil::ensure_not_in_use(&main_dir, &main.name)?;
        let carried = persist::prepare(&stage, old, &main.persist, ov.markers)?;
        if !carried.is_empty() {
            on(InstallEvent::Phase(&crate::tr!("  {} user data entries carried over", "  {} Benutzerdaten-Einträge übernommen", carried.len())));
        }
        std::fs::write(stage.join(LAUNCH_FILE), serde_json::to_vec_pretty(&profile.to_json())?)?;
        write_meta(&stage, &main.name, &main.version, category);
        Ok(carried)
    })();
    let carried = match prepared {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(e);
        }
    };
    if ov.nested_msi.is_some() {
        report.files = fsutil::count_files(&stage).unwrap_or(report.files);
    }
    if let Err(e) = fsutil::commit_dir(&stage, &main_dir, &main.name) {
        if let Some(old) = old {
            persist::restore(&carried, &stage, old);
        }
        let _ = std::fs::remove_dir_all(&stage);
        return Err(e);
    }
    Ok(InstallSummary { report, plugins: plugin_names, profile })
}
/// A file named `name` (any case) in `dir` or a few levels below.
fn find_file(dir: &Path, name: &str, depth: usize) -> Option<PathBuf> {
    let entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().collect();
    if let Some(e) = entries.iter().find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(name)) {
        return Some(e.path());
    }
    if depth >= 3 {
        return None;
    }
    entries
        .iter()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .find_map(|e| find_file(&e.path(), name, depth + 1))
}

/// Drop the `.msi` files (and 7-Zip's `_EXTRACTED` folder) of an installer wrapper.
fn remove_wrapper_msis(stage: &Path) {
    let _ = std::fs::remove_dir_all(stage.join("_EXTRACTED"));
    for e in std::fs::read_dir(stage).into_iter().flatten().flatten() {
        if e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("msi")) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Check a finished extraction: the launch target must exist, and a tool without
/// one (CLI/PATH-only) must at least have produced files.
fn validate_install(dir: &Path, profile: &LaunchProfile, files: usize) -> std::result::Result<(), String> {
    match &profile.run {
        Some(run) => {
            let rel = crate::install::safe_rel(run).map_err(|e| e.to_string())?;
            if dir.join(rel).is_file() {
                Ok(())
            } else {
                Err(crate::tr!("start file '{run}' is missing after unpacking", "Startdatei '{run}' fehlt nach dem Entpacken"))
            }
        }
        None if files > 0 => Ok(()),
        None => Err(crate::tr!("no files were unpacked", "es wurden keine Dateien entpackt")),
    }
}

/// For an existing install: `Some(missing)` if its launch profile points to a
/// file that isn't there (e.g. an old install that only saved a setup `.msi`).
pub fn broken_launch_target(root: &Path, category: &str, name: &str) -> Option<String> {
    let dir = install_dir(root, category, name);
    let text = std::fs::read_to_string(dir.join(LAUNCH_FILE)).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let run = json["run"].as_str().filter(|r| !r.is_empty())?;
    let p = if Path::new(run).is_absolute() { PathBuf::from(run) } else { dir.join(run) };
    (!p.is_file()).then(|| run.to_string())
}

/// Install a curated tool by name (fetches its manifest from the catalog).
pub fn install_curated(name: &str, root: &Path, on: &dyn Fn(InstallEvent), cancel: &AtomicBool) -> Result<InstallSummary> {
    let (main, entry) = fetch_named(name)?;
    install_resolved(&main, root, entry.category, on, cancel)
}

/// Build raw manifest URLs for a bucket repo, trying `master` then `main`.
fn repo_raw_urls(repo: &str, file_path: &str) -> Vec<String> {
    let path = repo.trim_end_matches('/').trim_start_matches("https://github.com/");
    ["main", "master"]
        .iter()
        .map(|br| format!("https://raw.githubusercontent.com/{path}/{br}/{file_path}"))
        .collect()
}

/// Fetch a manifest from an arbitrary bucket repo + file path (search results).
pub fn fetch_manifest_from_repo(repo: &str, file_path: &str, name: &str) -> Result<Manifest> {
    for url in repo_raw_urls(repo, file_path) {
        if let Ok(text) = net::fetch_text(&url) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                return parse_manifest(name, &json);
            }
        }
    }
    Err(anyhow!(crate::tr!("manifest not available: {repo}/{file_path}", "Manifest nicht abrufbar: {repo}/{file_path}")))
}

/// Install a tool from any Scoop bucket (a full-text search result).
pub fn install_from_repo(
    name: &str,
    repo: &str,
    file_path: &str,
    category: &str,
    root: &Path,
    on: &dyn Fn(InstallEvent),
    cancel: &AtomicBool,
) -> Result<InstallSummary> {
    if !resolver::valid_tool_name(name) {
        return Err(anyhow!(crate::tr!("invalid tool name: {name}", "Ungültiger Tool-Name: {name}")));
    }
    let main = fetch_manifest_from_repo(repo, file_path, name)?;
    install_resolved(&main, root, category, on, cancel)
}

/// Write per-tool install metadata (used to detect installed state + version).
fn write_meta(dir: &Path, name: &str, version: &str, category: &str) {
    let meta = serde_json::json!({ "name": name, "version": version, "category": category });
    if let Ok(bytes) = serde_json::to_vec_pretty(&meta) {
        let _ = std::fs::write(dir.join(META_FILE), bytes);
    }
}

/// Installed version of a curated tool: `None` if not installed, `Some(version)`
/// if installed (version may be empty for a tool present without metadata).
pub fn installed_version(root: &Path, category: &str, name: &str) -> Option<String> {
    let dir = install_dir(root, category, name);
    let meta = dir.join(META_FILE);
    if let Ok(text) = std::fs::read_to_string(&meta) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
            return Some(json["version"].as_str().unwrap_or("").to_string());
        }
    }
    // No metadata but a non-empty dir still counts as installed.
    if std::fs::read_dir(&dir).map(|mut it| it.next().is_some()).unwrap_or(false) {
        Some(String::new())
    } else {
        None
    }
}

/// Latest upstream version of a curated tool (from its manifest).
pub fn latest_version(name: &str) -> Result<String> {
    Ok(fetch_named(name)?.0.version)
}

/// What the catalog card shows before an install: latest version, download size
/// (best effort, via HEAD) and the host it comes from.
pub struct LatestInfo {
    pub version: String,
    pub size: Option<u64>,
    pub host: String,
}

pub fn latest_info(name: &str) -> Result<LatestInfo> {
    let (m, _) = fetch_named(name)?;
    let sizes: Vec<Option<u64>> = m.artifacts.iter().map(|a| net::head_size(&a.url)).collect();
    let size = sizes.iter().all(Option::is_some).then(|| sizes.iter().flatten().sum());
    Ok(LatestInfo { version: m.version, size, host: host_of(&m.url) })
}

/// `github.com` from `https://github.com/…` (without `www.`).
pub fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    host.trim_start_matches("www.").to_string()
}

/// Total size of the files below `dir` (what a tool occupies on the stick).
pub fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
            Err(_) => 0,
        })
        .sum()
}

/// Windows subsystem of an `.exe` (2 = GUI, 3 = console), from its PE header.
fn exe_subsystem(path: &Path) -> Option<u16> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; 4];
    f.seek(SeekFrom::Start(0x3c)).ok()?;
    f.read_exact(&mut buf).ok()?;
    let pe = u32::from_le_bytes(buf) as u64;
    f.seek(SeekFrom::Start(pe)).ok()?;
    f.read_exact(&mut buf).ok()?;
    if &buf != b"PE\0\0" {
        return None;
    }
    // Signature (4) + COFF header (20) + offset of `Subsystem` in the optional header (68).
    f.seek(SeekFrom::Start(pe + 24 + 68)).ok()?;
    let mut sub = [0u8; 2];
    f.read_exact(&mut sub).ok()?;
    Some(u16::from_le_bytes(sub))
}

/// A command-line tool: "Starten" would only flash a console window (or do
/// nothing), so the app offers "Terminal" instead. True when the start target is
/// a console program, or when the tool only puts commands on PATH (GnuPG, Node.js).
pub fn is_console_tool(root: &Path, category: &str, name: &str) -> bool {
    let dir = install_dir(root, category, name);
    let run = std::fs::read_to_string(dir.join(LAUNCH_FILE))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .map(|j| j["run"].as_str().unwrap_or("").to_string());
    let target = match run {
        Some(r) if r.is_empty() => return true,
        Some(r) if Path::new(&r).is_absolute() => PathBuf::from(r),
        Some(r) => dir.join(r),
        None => match find_run_target(&dir) {
            Some(p) => p,
            None => return false,
        },
    };
    let is_exe = target.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe"));
    is_exe && exe_subsystem(&target) == Some(3)
}

/// Remove an installed tool (its own directory only; shared runtimes are kept).
pub fn remove_tool(root: &Path, category: &str, name: &str) -> Result<()> {
    let dir = install_dir(root, category, name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| crate::tr!("remove failed: {}", "Entfernen fehlgeschlagen: {}", dir.display()))?;
    }
    Ok(())
}

/// Environment variables of a launch profile (name, value).
type EnvVars = Vec<(String, String)>;

/// Launch an installed tool. If it has a launch profile (from a curated install),
/// resolve `${STICK}`, set env + PATH and run `run` in the tool dir. Otherwise
/// fall back to a heuristic best-guess executable (for manually placed tools).
pub fn launch_tool(root: &Path, category: &str, name: &str) -> Result<()> {
    let dir = install_dir(root, category, name);
    let profile = dir.join(LAUNCH_FILE);

    let (run, args, env, path_prepend): (String, Option<String>, EnvVars, Vec<String>) = if profile.exists() {
        let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&profile)?)?;
        let root_abs = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let root_str = root_abs
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .trim_end_matches(['\\', '/'])
            .to_string();
        let run = json["run"].as_str().unwrap_or("").to_string();
        let args = json["args"].as_str().filter(|a| !a.is_empty()).map(|a| resolver::resolve_stick(a, &root_str));
        let env = json["env"]
            .as_object()
            .map(|o| o.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), resolver::resolve_stick(s, &root_str)))).collect())
            .unwrap_or_default();
        let path_prepend = json["path_prepend"]
            .as_array()
            .map(|a| a.iter().filter_map(|p| p.as_str()).map(|s| resolver::resolve_stick(s, &root_str)).collect())
            .unwrap_or_default();
        (run, args, env, path_prepend)
    } else {
        // No profile: best-guess an executable and use its absolute path.
        let target = find_run_target(&dir)
            .ok_or_else(|| anyhow!(crate::tr!("no launchable file found in {}", "keine startbare Datei in {} gefunden", dir.display())))?;
        let abs = std::fs::canonicalize(&target).unwrap_or(target);
        let abs = abs.to_string_lossy().trim_start_matches(r"\\?\").to_string();
        (abs, None, Vec::new(), Vec::new())
    };

    // Profiles from older versions may lack a target (GUI apps that only list
    // `shortcuts`) — fall back to the best-guess executable.
    let run = if run.is_empty() {
        let target = find_run_target(&dir).ok_or_else(|| anyhow!(crate::tr!("nothing to launch for {name}", "nichts Startbares für {name}")))?;
        let abs = std::fs::canonicalize(&target).unwrap_or(target);
        abs.to_string_lossy().trim_start_matches(r"\\?\").to_string()
    } else {
        run
    };
    let lower = run.to_ascii_lowercase();
    let is_script = lower.ends_with(".bat") || lower.ends_with(".cmd");
    let mut cmd = if is_script {
        // cmd doesn't find a quoted *relative* script name ("run me.bat") — use the full path.
        let script = if Path::new(&run).is_absolute() { PathBuf::from(&run) } else { dir.join(&run) };
        let script = script.to_string_lossy().trim_start_matches(r"\\?\").to_string();
        let mut c = Command::new("cmd");
        match &args {
            // cmd strips the outer quotes: `/C ""x.bat" args"` keeps both intact.
            Some(a) => raw_args(&mut c, &format!("/C \"\"{script}\" {a}\"")),
            None => {
                c.arg("/C").arg(&script);
            }
        }
        c
    } else if Path::new(&run).is_absolute() {
        Command::new(&run)
    } else {
        Command::new(dir.join(&run))
    };
    if let (false, Some(a)) = (is_script, &args) {
        raw_args(&mut cmd, a);
    }
    cmd.current_dir(&dir);
    for (k, v) in env {
        cmd.env(k, v);
    }
    if !path_prepend.is_empty() {
        let mut parts = path_prepend;
        parts.push(std::env::var("PATH").unwrap_or_default());
        cmd.env("PATH", parts.join(";"));
    }
    cmd.spawn().with_context(|| crate::tr!("failed to start {name}", "Start von {name} fehlgeschlagen"))?;
    Ok(())
}

/// Append Scoop-style arguments verbatim: they carry their own quoting
/// (`--user-data-dir="…\User Data"`), which per-argument quoting would break.
fn raw_args(cmd: &mut Command, args: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.raw_arg(args);
    }
    #[cfg(not(windows))]
    {
        cmd.args(args.split_whitespace());
    }
}

/// Resolve an installed tool's launch-profile env + PATH prepends, with
/// `${STICK}` expanded to the concrete `root`. Empty when there is no profile.
pub fn resolved_launch_env(root: &Path, category: &str, name: &str) -> (Vec<(String, String)>, Vec<String>) {
    let dir = install_dir(root, category, name);
    let profile = dir.join(LAUNCH_FILE);
    if !profile.exists() {
        return (Vec::new(), Vec::new());
    }
    let text = match std::fs::read_to_string(&profile) {
        Ok(t) => t,
        Err(_) => return (Vec::new(), Vec::new()),
    };
    let json: serde_json::Value = match serde_json::from_str(&text) {
        Ok(j) => j,
        Err(_) => return (Vec::new(), Vec::new()),
    };
    let root_abs = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let root_str = root_abs
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .trim_end_matches(['\\', '/'])
        .to_string();
    let env = json["env"]
        .as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), resolver::resolve_stick(s, &root_str))))
                .collect()
        })
        .unwrap_or_default();
    let paths = json["path_prepend"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| p.as_str())
                .map(|s| resolver::resolve_stick(s, &root_str))
                .collect()
        })
        .unwrap_or_default();
    (env, paths)
}

/// Resolve an installed tool's primary `.exe` (for icon extraction): the launch
/// profile's `run` when it points to an exe, else a best-guess exe in the dir.
pub fn tool_executable(root: &Path, category: &str, name: &str) -> Option<PathBuf> {
    let dir = install_dir(root, category, name);
    if !dir.exists() {
        return None;
    }
    if let Ok(text) = std::fs::read_to_string(dir.join(LAUNCH_FILE)) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(run) = json["run"].as_str() {
                if run.to_ascii_lowercase().ends_with(".exe") {
                    let p = if Path::new(run).is_absolute() {
                        PathBuf::from(run)
                    } else {
                        dir.join(run)
                    };
                    if p.is_file() {
                        return Some(p);
                    }
                }
            }
        }
    }
    find_run_target(&dir)
        .filter(|p| p.extension().map(|e| e.eq_ignore_ascii_case("exe")).unwrap_or(false))
}

/// Open a tool's folder in the OS file explorer.
pub fn open_in_explorer(root: &Path, category: &str, name: &str) -> Result<()> {
    let dir = install_dir(root, category, name);
    Command::new("explorer")
        .arg(&dir)
        .spawn()
        .context(crate::tr!("could not open Explorer", "Explorer konnte nicht geöffnet werden"))?;
    Ok(())
}

/// A tool found on disk under `Tools\`.
pub struct InstalledTool {
    pub name: String,
    pub category: String,
    pub version: String,
    /// True if the tool is part of the curated catalog.
    pub curated: bool,
    /// True if it has a launch profile (installed via the library).
    pub has_profile: bool,
}

fn meta_version(dir: &Path) -> String {
    std::fs::read_to_string(dir.join(META_FILE))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|j| j["version"].as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Scan `<root>\Tools\` for every installed tool (curated or manually placed),
/// skipping the shared `_runtime` and dotfolders.
pub fn scan_installed(root: &Path) -> Vec<InstalledTool> {
    let mut out = Vec::new();
    let cats = match std::fs::read_dir(root.join("Tools")) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for cat in cats.flatten() {
        if !cat.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let category = cat.file_name().to_string_lossy().to_string();
        if category.starts_with('_') || category.starts_with('.') {
            continue;
        }
        let subs = match std::fs::read_dir(cat.path()) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for tool in subs.flatten() {
            if !tool.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let dir = tool.path();
            if !std::fs::read_dir(&dir).map(|mut it| it.next().is_some()).unwrap_or(false) {
                continue;
            }
            let name = tool.file_name().to_string_lossy().to_string();
            // Work folders of a running or interrupted install (`.maxtrix-new-…`).
            if name.starts_with('.') {
                continue;
            }
            out.push(InstalledTool {
                curated: find(&name).is_some(),
                has_profile: dir.join(LAUNCH_FILE).exists(),
                version: meta_version(&dir),
                name,
                category: category.clone(),
            });
        }
    }
    out
}

const RUN_EXCLUDE: &[&str] = &[
    "unins", "setup", "update", "installer", "vcredist", "vc_redist",
    "crashpad", "crashreport", "crashhandler", "helper", "elevate",
];

/// Best-guess the executable to launch inside `dir` (for tools without a profile).
pub fn find_run_target(dir: &Path) -> Option<PathBuf> {
    let folder = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let mut best: Option<(i32, PathBuf)> = None;
    collect_run_candidates(dir, 0, &folder, &mut best);
    best.map(|(_, p)| p)
}

/// True when `word` appears as a whole token inside `name` (bounded by `_`, `-`,
/// `.`, or string start/end), avoiding false positives like "update" matching
/// "updatechecker.exe".
fn is_excluded_word(name: &str, word: &str) -> bool {
    // Fast path: name starts with the word and the next char is a boundary.
    if let Some(after) = name.strip_prefix(word) {
        if after.is_empty() || after.starts_with('_') || after.starts_with('-') || after.starts_with('.') {
            return true;
        }
    }
    // Check for word preceded by a boundary character.
    let mut search_from = 0;
    while let Some(pos) = name[search_from..].find(word) {
        let abs = search_from + pos;
        let before_ok = abs == 0
            || name.as_bytes()[abs - 1] == b'_'
            || name.as_bytes()[abs - 1] == b'-'
            || name.as_bytes()[abs - 1] == b'.';
        let after = &name[abs + word.len()..];
        let after_ok = after.is_empty()
            || after.starts_with('_')
            || after.starts_with('-')
            || after.starts_with('.');
        if before_ok && after_ok {
            return true;
        }
        search_from = abs + 1;
    }
    false
}

fn collect_run_candidates(dir: &Path, depth: usize, folder: &str, best: &mut Option<(i32, PathBuf)>) {
    if depth > 3 {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for e in entries.flatten() {
        let path = e.path();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            collect_run_candidates(&path, depth + 1, folder, best);
            continue;
        }
        let name = e.file_name().to_string_lossy().to_ascii_lowercase();
        let ext_exe = name.ends_with(".exe");
        let ext_bat = name.ends_with(".bat") || name.ends_with(".cmd");
        if !ext_exe && !ext_bat {
            continue;
        }
        if RUN_EXCLUDE.iter().any(|x| is_excluded_word(&name, x)) {
            continue;
        }
        let stem = name.trim_end_matches(".exe").trim_end_matches(".bat").trim_end_matches(".cmd");
        // Lower score = better.
        let mut score = depth as i32 * 20 + (name.len() as i32) / 4;
        if stem == folder {
            score -= 1000;
        } else if stem.contains(folder) || folder.contains(stem) {
            score -= 200;
        }
        if ext_exe {
            score -= 5;
        }
        if best.as_ref().map(|(s, _)| score < *s).unwrap_or(true) {
            *best = Some((score, path));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_german_folders_move_and_profiles_follow() {
        let root = std::env::temp_dir().join(format!("mx-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let tool = root.join("Tools").join("Dateien").join("7zip");
        std::fs::create_dir_all(&tool).unwrap();
        std::fs::write(tool.join(LAUNCH_FILE), r#"{"run":"${STICK}\Tools\Dateien\7zip\7zFM.exe"}"#).unwrap();
        // A tool that already exists in the new folder is left alone.
        std::fs::create_dir_all(root.join("Tools").join("Grafik").join("inkscape")).unwrap();
        std::fs::create_dir_all(root.join("Tools").join("Graphics").join("inkscape")).unwrap();

        assert_eq!(migrate_legacy_categories(&root), 1);
        let moved = root.join("Tools").join("Files").join("7zip");
        let profile = std::fs::read_to_string(moved.join(LAUNCH_FILE)).unwrap();
        assert!(profile.contains(r"Tools\Files\7zip"), "{profile}");
        assert!(!root.join("Tools").join("Dateien").exists());
        assert!(root.join("Tools").join("Grafik").join("inkscape").exists());
        assert_eq!(migrate_legacy_categories(&root), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn titles_and_cli_entries_match_the_catalog() {
        for e in curated() {
            assert!(!e.title.is_empty(), "{}", e.name);
        }
        // The CLI view takes its folders from the catalog — every entry must be in it
        // (stale hard-coded categories once broke "Open terminal" for hashcat/ripgrep).
        for t in crate::cli::cli_tools() {
            assert!(find(t.name).is_some(), "CLI tool {} is missing from the catalog", t.name);
        }
        assert_eq!(display_name("notepadplusplus"), "Notepad++");
        assert_eq!(display_name("7zip"), "7-Zip");
        // Not curated: the id stays as it is.
        assert_eq!(display_name("some-tool"), "some-tool");
    }

    #[test]
    fn host_is_taken_from_the_download_url() {
        assert_eq!(host_of("https://www.irfanview.info/files/x.zip"), "irfanview.info");
        assert_eq!(host_of("https://github.com/a/b/releases/x.7z#/dl.7z"), "github.com");
    }

    #[cfg(windows)]
    #[test]
    fn console_programs_are_told_apart_from_windowed_ones() {
        assert_eq!(exe_subsystem(Path::new(r"C:\Windows\System32\PING.EXE")), Some(3));
        assert_eq!(exe_subsystem(Path::new(r"C:\Windows\System32\notepad.exe")), Some(2));
        assert_eq!(exe_subsystem(Path::new(r"C:\Windows\win.ini")), None);
    }

    #[cfg(windows)]
    #[test]
    fn a_tool_without_start_target_or_with_a_console_exe_is_a_cli_tool() {
        let root = std::env::temp_dir().join(format!("maxtrix-cli-{}", std::process::id()));
        let dir = install_dir(&root, "Test", "clitool");
        std::fs::create_dir_all(&dir).unwrap();
        // PATH-only tool (GnuPG, Node.js): no `run` at all.
        std::fs::write(dir.join(LAUNCH_FILE), r#"{"run": null, "env": {}, "path_prepend": []}"#).unwrap();
        assert!(is_console_tool(&root, "Test", "clitool"));
        // A console exe as start target.
        std::fs::copy(r"C:\Windows\System32\PING.EXE", dir.join("tool.exe")).unwrap();
        std::fs::write(dir.join(LAUNCH_FILE), r#"{"run": "tool.exe"}"#).unwrap();
        assert!(is_console_tool(&root, "Test", "clitool"));
        // A windowed exe.
        std::fs::copy(r"C:\Windows\System32\notepad.exe", dir.join("tool.exe")).unwrap();
        assert!(!is_console_tool(&root, "Test", "clitool"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
