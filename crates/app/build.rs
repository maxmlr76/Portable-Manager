/// Civil date from days since 1970-01-01 (Howard Hinnant's algorithm) — no
/// date crate needed just for the build stamp.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn main() {
    // Build stamp shown in the sidebar (UTC) — replaces the hand-edited tag.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let (hh, mm) = (secs.rem_euclid(86_400) / 3600, secs.rem_euclid(3600) / 60);
    println!("cargo:rustc-env=MAXTRIX_BUILD={y:04}-{m:02}-{d:02} {hh:02}:{mm:02} UTC");
    // Re-stamp whenever the app's code or UI changes.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed=build.rs");

    // Force the dark variant of Slint's std widgets (scrollbars etc.) — otherwise
    // they follow a light Windows theme and look out of place in the dark UI.
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint-UI konnte nicht kompiliert werden");

    // Embed the Maxtrix app icon + version metadata into the .exe (Windows only).
    // Needs rc.exe from the Windows SDK; if it's missing we warn and continue so the
    // build still works on a bare cargo setup.
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/maxtrix.ico");
        res.set("ProductName", "Maxtrix Portable Organizer");
        res.set("FileDescription", "Maxtrix Portable Organizer");
        res.set("CompanyName", "Maxtrix");
        res.set("LegalCopyright", "\u{00A9} Maxtrix");
        res.set("OriginalFilename", "PortableOrganizer.exe");
        res.set("InternalName", "PortableOrganizer");
        if let Err(e) = res.compile() {
            println!("cargo:warning=Icon/Metadaten nicht eingebettet (rc.exe fehlt?): {e}");
        }
    }
}
