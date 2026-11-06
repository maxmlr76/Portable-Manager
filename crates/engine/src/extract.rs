//! ZIP extraction (native). Other formats go through 7-Zip (`sevenzip.rs`) or
//! `msiexec /a` (`msi.rs`).

use anyhow::{Context, Result};
use std::fs;
use std::io::{BufReader, Read, Seek};
use std::path::{Component, Path, PathBuf};

/// Extract the ZIP file `archive` into `target`, optionally keeping only the
/// contents of a top-level `extract_dir`. Returns the number of files written.
pub fn extract_zip_file(archive: &Path, target: &Path, extract_dir: Option<&Path>) -> Result<usize> {
    let f = fs::File::open(archive)
        .with_context(|| format!("Archiv nicht lesbar: {}", archive.display()))?;
    extract_zip(BufReader::new(f), target, extract_dir)
}

/// Uses `enclosed_name` to reject path traversal (zip-slip).
fn extract_zip<R: Read + Seek>(reader: R, target: &Path, extract_dir: Option<&Path>) -> Result<usize> {
    let mut archive = zip::ZipArchive::new(reader).context("ZIP-Archiv unlesbar")?;
    fs::create_dir_all(target)?;
    let mut written = 0usize;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let safe = match entry.enclosed_name() {
            Some(p) => p.to_path_buf(),
            None => continue, // unsafe / absolute path -> skip
        };

        // Strip the leading folder Scoop's extract_dir points at.
        let rel = match extract_dir {
            Some(ed) => match strip_prefix_ci(&safe, ed) {
                Some(r) => r,
                None => continue, // outside the extract_dir -> skip
            },
            None => safe,
        };
        if rel.as_os_str().is_empty() {
            continue;
        }

        let out = target.join(&rel);
        if entry.is_dir() {
            fs::create_dir_all(&out)?;
        } else {
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut f = fs::File::create(&out)
                .with_context(|| format!("Kann Datei nicht schreiben: {}", out.display()))?;
            std::io::copy(&mut entry, &mut f)?;
            written += 1;
        }
    }
    Ok(written)
}

/// Strip `prefix` from `path`, comparing folder names case-insensitively like
/// Windows (and Scoop) do — BleachBit's manifest says `BleachBit-portable`, the
/// ZIP contains `BleachBit-Portable\`.
fn strip_prefix_ci(path: &Path, prefix: &Path) -> Option<PathBuf> {
    let mut rest = path.components();
    for p in prefix.components().filter(|c| !matches!(c, Component::CurDir)) {
        let c = rest.next()?;
        if c.as_os_str().to_string_lossy().to_lowercase() != p.as_os_str().to_string_lossy().to_lowercase() {
            return None;
        }
    }
    Some(rest.as_path().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    #[test]
    fn extract_dir_matches_case_insensitively() {
        let p = Path::new("BleachBit-Portable/bleachbit.exe");
        assert_eq!(strip_prefix_ci(p, Path::new("BleachBit-portable")), Some(PathBuf::from("bleachbit.exe")));
        assert_eq!(strip_prefix_ci(Path::new("a/b/c.txt"), Path::new(r"A\B")), Some(PathBuf::from("c.txt")));
        assert_eq!(strip_prefix_ci(Path::new("other/x.exe"), Path::new("BleachBit-portable")), None);
    }

    #[test]
    fn extract_dir_keeps_only_that_folder() {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let opt = zip::write::SimpleFileOptions::default();
            z.start_file("tool-1.0/app.exe", opt).unwrap();
            z.write_all(b"exe").unwrap();
            z.start_file("tool-1.0/lib/x.dll", opt).unwrap();
            z.write_all(b"dll").unwrap();
            z.start_file("README.txt", opt).unwrap();
            z.write_all(b"outside").unwrap();
            z.finish().unwrap();
        }
        let t = std::env::temp_dir().join(format!("maxtrix-zip-{}", std::process::id()));
        let _ = fs::remove_dir_all(&t);
        buf.set_position(0);
        let n = extract_zip(buf, &t, Some(Path::new("tool-1.0"))).unwrap();
        assert_eq!(n, 2);
        assert!(t.join("app.exe").is_file() && t.join("lib").join("x.dll").is_file());
        assert!(!t.join("README.txt").exists());
        fs::remove_dir_all(&t).unwrap();
    }
}
