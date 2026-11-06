//! maxtrix — CLI that exercises the engine (shares the catalog/orchestration
//! layer with the GUI).
//!
//! Usage:
//!   maxtrix selftest | which7z
//!   maxtrix audit    [--install] [--keep <dir>]      catalog health check (optionally test-installs)
//!   maxtrix manifest <name>
//!   maxtrix plan     <name> [--root <dir>]           resolve deps/plugins + env (no download)
//!   maxtrix install  <name> [--root <dir>]           download + verify + extract (incl. plugins)
//!   maxtrix profile  <name> [--root <dir>]           (re)derive the launch profile (no download)
//!   maxtrix launch   <name> [--root <dir>] [--as R]  resolve a launch profile against a stick root

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

use maxtrix_engine::catalog;
use maxtrix_engine::install::InstallEvent;
use std::sync::atomic::AtomicBool;

/// The CLI has no cancel button (Ctrl+C ends the process).
static NO_CANCEL: AtomicBool = AtomicBool::new(false);
use maxtrix_engine::manifest::Manifest;
use maxtrix_engine::resolver::{self, LaunchProfile};

/// Default install/stick root: the directory the app runs from (its own dir).
fn default_root() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_string_lossy().into_owned()))
        .unwrap_or_else(|| ".".into())
}

fn strip_unc(p: PathBuf) -> String {
    let s = p.to_string_lossy().to_string();
    s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
}

/// Print progress events to stdout; download byte-progress is left to the GUI.
fn cli_events(ev: InstallEvent) {
    if let InstallEvent::Phase(s) = ev {
        println!("{s}");
    }
}

fn print_profile(profile: &LaunchProfile) {
    println!("Start        {}", profile.run.as_deref().unwrap_or("(kein bin)"));
    if let Some(a) = &profile.args {
        println!("Argumente    {a}");
    }
    if profile.env.is_empty() && profile.path_prepend.is_empty() {
        println!("Umgebung     (keine)");
    } else {
        println!("Umgebung:");
        for (k, v) in &profile.env {
            println!("  {k} = {v}");
        }
        for p in &profile.path_prepend {
            println!("  PATH  += {p}");
        }
    }
}

/// Fetch main + plugin manifests and compute their install dirs (no download).
fn resolve_plugins(main: &Manifest, root: &Path) -> Vec<(Manifest, PathBuf)> {
    let mut plugins = Vec::new();
    for need in resolver::plugin_needs(main) {
        let (_, pname) = resolver::parse_ref(&need.reference);
        match catalog::fetch_ref(&need.reference) {
            Ok(pm) => plugins.push((pm, catalog::install_dir(root, catalog::RUNTIME_CAT, &pname))),
            Err(e) => println!("Plugin {} übersprungen: {e}", need.reference),
        }
    }
    plugins
}

fn cmd_manifest(name: &str) -> Result<()> {
    let (m, entry) = catalog::fetch_named(name)?;
    println!("Name        {}", m.name);
    println!("Version     {}", m.version);
    println!("Kategorie   {}", entry.category);
    println!("Beschreibung {}", m.description);
    println!("URL         {}", m.url);
    println!("Datei       {}  (.{})", m.filename, m.ext);
    println!("Hash        {}:{}", m.hash_alg, m.hash_hex);
    if let Some(ed) = &m.extract_dir {
        println!("extract_dir {ed}");
    }
    if let Some(bin) = &m.bin {
        println!("bin/start   {bin}");
    }
    for (feat, apps) in &m.suggest {
        println!("suggest     {feat}: {apps}");
    }
    for (k, v) in &m.env_set {
        println!("env_set     {k} = {v}");
    }
    Ok(())
}

fn cmd_plan(name: &str, root: &str) -> Result<()> {
    let (main, entry) = catalog::fetch_named(name)?;
    let root = Path::new(root);
    let plugins = resolve_plugins(&main, root);

    println!("Plan für {name} v{}:", main.version);
    for (pm, pdir) in &plugins {
        println!("  Plugin       {} v{}  ->  {}", pm.name, pm.version, pdir.display());
    }
    let main_dir = catalog::install_dir(root, entry.category, name);
    println!("  Haupt-Tool   {} v{}  ->  {}", main.name, main.version, main_dir.display());

    println!("\nAbgeleitete Startumgebung (stick-relativ):");
    let profile = resolver::build_launch_profile(&main, &main_dir, &plugins, root, catalog::overlay(name).run, catalog::overlay(name).args);
    print_profile(&profile);
    println!("\n(Nur Auflösung – es wurde nichts heruntergeladen.)");
    Ok(())
}

fn cmd_profile(name: &str, root: &str) -> Result<()> {
    let (main, entry) = catalog::fetch_named(name)?;
    let root_path = Path::new(root);
    let plugins = resolve_plugins(&main, root_path);
    let main_dir = catalog::install_dir(root_path, entry.category, name);
    let profile = resolver::build_launch_profile(&main, &main_dir, &plugins, root_path, catalog::overlay(name).run, catalog::overlay(name).args);

    std::fs::create_dir_all(&main_dir)?;
    let path = main_dir.join(".maxtrix-launch.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&profile.to_json())?)?;
    println!("Launch-Profil geschrieben: {}\n", path.display());
    print_profile(&profile);
    Ok(())
}

fn cmd_launch(name: &str, root: &str, as_root: Option<&str>) -> Result<()> {
    let entry = catalog::find(name).ok_or_else(|| anyhow!("'{name}' nicht im Katalog"))?;
    let profile_path =
        catalog::install_dir(Path::new(root), entry.category, name).join(".maxtrix-launch.json");
    let text = std::fs::read_to_string(&profile_path)
        .with_context(|| format!("kein Launch-Profil gefunden: {}", profile_path.display()))?;
    let json: serde_json::Value = serde_json::from_str(&text)?;

    let mount_raw = match as_root {
        Some(a) => a.to_string(),
        None => strip_unc(std::fs::canonicalize(root).unwrap_or_else(|_| PathBuf::from(root))),
    };
    let mount = mount_raw.trim_end_matches(['\\', '/']).to_string();

    println!("Stick-Wurzel : {mount}");
    println!("Start        : {}", json["run"].as_str().unwrap_or("(kein bin)"));
    if let Some(a) = json["args"].as_str() {
        println!("Argumente    : {}", resolver::resolve_stick(a, &mount));
    }
    println!("Aufgelöste Umgebung:");
    if let Some(env) = json["env"].as_object() {
        for (k, v) in env {
            if let Some(s) = v.as_str() {
                println!("  {k} = {}", resolver::resolve_stick(s, &mount));
            }
        }
    }
    if let Some(paths) = json["path_prepend"].as_array() {
        for p in paths {
            if let Some(s) = p.as_str() {
                println!("  PATH  += {}", resolver::resolve_stick(s, &mount));
            }
        }
    }
    Ok(())
}

fn cmd_install(name: &str, root: &str) -> Result<()> {
    let summary = catalog::install_curated(name, Path::new(root), &cli_events, &NO_CANCEL)?;
    let r = &summary.report;

    println!("\n── Ergebnis ─────────────────────────────");
    println!("{} v{}", r.name, r.version);
    println!("verifiziert : {} ({})", r.verified, r.hash_alg);
    println!("Format      : {}", r.archive_kind);
    println!("Zielordner  : {}", r.target.display());
    match &r.pending_reason {
        Some(reason) => println!("Status      : Download+Hash OK — {reason}"),
        None => println!("Status      : installiert, {} Dateien", r.files),
    }
    if !summary.plugins.is_empty() {
        println!("Plugins     : {}", summary.plugins.join(", "));
    }
    println!();
    print_profile(&summary.profile);
    Ok(())
}

/// Catalog health check: can every curated tool be installed portably?
/// Static by default (manifests only); `--install` additionally test-installs
/// each tool into a scratch root and relies on the engine's post-install check.
fn cmd_audit(test_install: bool, keep_root: Option<String>) -> Result<()> {
    let scratch = keep_root
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("maxtrix-audit-{}", std::process::id())));
    if test_install {
        std::fs::create_dir_all(&scratch)?;
        println!("Test-Installationen nach {}\n", scratch.display());
    }
    let quiet = |_: InstallEvent| {};
    let (mut fails, mut warns) = (0usize, 0usize);
    println!("{:18} {:6} {:30} {:6} Hinweise", "Tool", "Typ", "Startziel", "Urteil");
    println!("{}", "-".repeat(100));
    for entry in catalog::curated() {
        let (verdict, kind, target, notes) = match catalog::fetch_named(entry.name) {
            Err(e) => ("FAIL", "?".to_string(), "-".to_string(), vec![format!("Manifest: {e}")]),
            Ok((m, _)) => {
                let mut notes = Vec::new();
                let target = m.launch_target();
                let mut verdict = "OK";
                // A downloaded .exe that isn't itself the program is an installer.
                let exe_is_installer = m.ext == "exe"
                    && target.as_deref().map(|t| !t.eq_ignore_ascii_case(&m.filename)).unwrap_or(true);
                if exe_is_installer || m.innosetup {
                    verdict = "FAIL";
                    notes.push("braucht ein Setup-Programm".to_string());
                } else if m.needs_installer {
                    verdict = "WARN";
                    notes.push("Installer-Skript wird übersprungen".to_string());
                }
                if m.artifacts.len() > 1 {
                    notes.push(format!("{} Downloads", m.artifacts.len()));
                }
                if !m.persist.is_empty() {
                    notes.push(format!("persist: {}", m.persist.join(", ")));
                }
                if test_install && verdict != "FAIL" {
                    match catalog::install_resolved(&m, &scratch, entry.category, &quiet, &NO_CANCEL) {
                        Ok(s) if s.report.pending_reason.is_none() => notes.insert(0, "Test-Install ok".into()),
                        Ok(s) => {
                            verdict = "FAIL";
                            notes.insert(0, format!("Entpacken: {}", s.report.pending_reason.unwrap_or_default()));
                        }
                        Err(e) => {
                            verdict = "FAIL";
                            notes.insert(0, format!("Test-Install: {e}"));
                        }
                    }
                    if keep_root.is_none() {
                        let _ = catalog::remove_tool(&scratch, entry.category, entry.name);
                    }
                }
                (verdict, m.ext.clone(), target.unwrap_or_else(|| "(nur PATH/CLI)".into()), notes)
            }
        };
        match verdict {
            "FAIL" => fails += 1,
            "WARN" => warns += 1,
            _ => {}
        }
        let target: String = target.chars().take(30).collect();
        println!("{:18} {:6} {:30} {:6} {}", entry.name, kind, target, verdict, notes.join(" · "));
    }
    if test_install && keep_root.is_none() {
        let _ = std::fs::remove_dir_all(&scratch);
    }
    let total = catalog::curated().len();
    println!("\n{total} Tools: {} ok, {warns} Warnungen, {fails} Fehler", total - warns - fails);
    if fails > 0 {
        Err(anyhow!("{fails} Tool(s) nicht portabel installierbar"))
    } else {
        Ok(())
    }
}

fn cmd_selftest() -> Result<()> {
    use maxtrix_engine::compute_hash;
    let cases = [
        ("sha256", "abc", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
        ("sha1", "abc", "a9993e364706816aba3e25717850c26c9cd0d89d"),
        ("md5", "abc", "900150983cd24fb0d6963f7d28e17f72"),
        ("sha512", "abc", "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"),
    ];
    let mut ok = true;
    for (alg, input, expected) in cases {
        let got = compute_hash(alg, input.as_bytes())?;
        let pass = got == expected;
        ok &= pass;
        println!("[{}] {} = {}", if pass { "OK" } else { "FAIL" }, alg, got);
    }
    if ok {
        println!("\nSelftest bestanden — Hash-Schicht korrekt.");
        Ok(())
    } else {
        Err(anyhow!("Selftest fehlgeschlagen"))
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    let name = || args.get(1).cloned().ok_or_else(|| anyhow!("Name fehlt"));
    let root = || flag(&args, "--root").unwrap_or_else(default_root);

    match cmd {
        "selftest" => cmd_selftest(),
        "audit" => cmd_audit(args.iter().any(|a| a == "--install"), flag(&args, "--keep")),
        "which7z" => {
            match maxtrix_engine::sevenzip::find_7z() {
                Some(p) => println!("7-Zip: {}", p.display()),
                None => println!("kein 7-Zip gefunden"),
            }
            Ok(())
        }
        "manifest" => cmd_manifest(&name()?),
        "plan" => cmd_plan(&name()?, &root()),
        "profile" => cmd_profile(&name()?, &root()),
        "launch" => cmd_launch(&name()?, &root(), flag(&args, "--as").as_deref()),
        "search" => {
            let q = name()?;
            let hits = maxtrix_engine::search::search(&q, 15)?;
            println!("{} Treffer (Top {}):", hits.len(), hits.len());
            for h in &hits {
                println!(
                    "  {:24} v{:16} {:>6}★  {:<10} {}",
                    h.name, h.version, h.stars,
                    if h.official { "offiziell" } else { "community" },
                    h.bucket_label()
                );
            }
            Ok(())
        }
        "installrepo" => {
            let n = name()?;
            let repo = flag(&args, "--repo").ok_or_else(|| anyhow!("--repo fehlt"))?;
            let file = flag(&args, "--file").ok_or_else(|| anyhow!("--file fehlt"))?;
            let cat = flag(&args, "--cat").unwrap_or_else(|| "Weitere".to_string());
            let summary =
                catalog::install_from_repo(&n, &repo, &file, &cat, Path::new(&root()), &cli_events, &NO_CANCEL)?;
            let r = &summary.report;
            println!("\n{} v{} → {} ({} Dateien)", r.name, r.version, r.target.display(), r.files);
            Ok(())
        }
        "term" => {
            let n = name()?;
            let r = root();
            let cat = category_for(&r, &n)?;
            maxtrix_engine::cli::open_tool_terminal(Path::new(&r), &cat, &n)?;
            println!("Terminal geöffnet: {n}");
            Ok(())
        }
        "console" => {
            maxtrix_engine::cli::open_console(Path::new(&root()))?;
            println!("IT-Konsole geöffnet");
            Ok(())
        }
        "install" => cmd_install(&name()?, &root()),
        "installed" => {
            let r = root();
            let list = catalog::scan_installed(Path::new(&r));
            if list.is_empty() {
                println!("Keine installierten Tools unter {r}\\Tools");
            }
            for t in &list {
                let ver = if t.version.is_empty() { "?" } else { t.version.as_str() };
                let kind = if t.curated { "kuratiert" } else { "fremd" };
                println!(
                    "  {:22} {:22} v{:10} [{}]{}",
                    t.name, t.category, ver, kind,
                    if t.has_profile { " +Profil" } else { "" }
                );
            }
            Ok(())
        }
        "start" => {
            let n = name()?;
            let r = root();
            let cat = category_for(&r, &n)?;
            catalog::launch_tool(Path::new(&r), &cat, &n)?;
            println!("Gestartet: {n}");
            Ok(())
        }
        "remove" => {
            let n = name()?;
            let r = root();
            let cat = category_for(&r, &n)?;
            catalog::remove_tool(Path::new(&r), &cat, &n)?;
            println!("Entfernt: {n}");
            Ok(())
        }
        _ => {
            println!("maxtrix — Portable-Tool-Engine (PoC)\n");
            println!("  maxtrix selftest              Hash-Schicht offline prüfen");
            println!("  maxtrix audit [--install]     Katalog prüfen: portabel installierbar? [--keep <dir>]");
            println!("  maxtrix which7z               gefundenes 7-Zip anzeigen");
            println!("  maxtrix manifest <name>       Manifest abrufen und anzeigen");
            println!("  maxtrix plan     <name>       Abhängigkeiten/Plugins + Startumgebung auflösen");
            println!("  maxtrix install  <name>       herunterladen + verifizieren + entpacken (inkl. Plugins)");
            println!("  maxtrix profile  <name>       Launch-Profil neu ableiten (ohne Download)");
            println!("  maxtrix launch   <name>       Launch-Profil gegen Laufwerk auflösen [--as <root>]");
            println!("  maxtrix term     <name>       Terminal mit dem Tool im PATH öffnen");
            println!("  maxtrix console               IT-Konsole: alle CLI-Tools gleichzeitig im PATH");
            println!("        [--root <dir>]");
            println!("\n  Katalog: {}", catalog::known());
            Ok(())
        }
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

/// Resolve a tool's category from the catalog, else from a disk scan.
fn category_for(root: &str, name: &str) -> Result<String> {
    if let Some(e) = catalog::find(name) {
        return Ok(e.category.to_string());
    }
    catalog::scan_installed(Path::new(root))
        .into_iter()
        .find(|t| t.name.eq_ignore_ascii_case(name))
        .map(|t| t.category)
        .ok_or_else(|| anyhow!("'{name}' weder im Katalog noch installiert gefunden"))
}
