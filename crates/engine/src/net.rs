//! Network fetch + checksum verification.

use anyhow::{anyhow, bail, Context, Result};
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use crate::Cancelled;

const UA: &str = "MaxtrixPortableManager/0.1 (+https://maxtrix)";

/// Shared HTTP agent with timeouts, so a stalled server can't block an install
/// forever (the read timeout applies per read, not to the whole download).
fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(20))
            .timeout_read(Duration::from_secs(60))
            .user_agent(UA)
            .build()
    })
}

/// Referer for a download, exactly like Scoop: the URL without its file name.
/// Some hosts (e.g. irfanview.info) answer with an HTML page instead of the file
/// when it is missing — which the hash check then rightly rejects.
fn referer_for(url: &str) -> String {
    let base = url.split('#').next().unwrap_or(url);
    let base = base.split('?').next().unwrap_or(base);
    match base.rfind('/') {
        Some(i) => base[..=i].to_string(),
        None => base.to_string(),
    }
}

/// Scoop's `handle_special_urls` for SourceForge: manifests often link
/// `sourceforge.net/project(s)/<p>/…`, which answers 404 or a web page — the
/// file itself comes from `downloads.sourceforge.net/project/<p>/<file>`.
pub(crate) fn special_url(url: &str) -> String {
    let base = url.split('#').next().unwrap_or(url);
    let Some(i) = base.find("sourceforge.net/project") else { return base.to_string() };
    let rest = &base[i + "sourceforge.net/project".len()..];
    let Some(rest) = rest.strip_prefix("s/").or_else(|| rest.strip_prefix('/')) else { return base.to_string() };
    let Some((project, file)) = rest.split_once('/') else { return base.to_string() };
    let file = file.strip_prefix("files/").unwrap_or(file);
    let file = file.split('?').next().unwrap_or(file);
    let file = file.strip_suffix("/download").unwrap_or(file);
    format!("https://downloads.sourceforge.net/project/{project}/{file}")
}

/// Fetch a URL as text (used for manifests).
pub fn fetch_text(url: &str) -> Result<String> {
    agent()
        .get(url)
        .call()
        .with_context(|| format!("Abruf fehlgeschlagen: {url}"))?
        .into_string()
        .context("Antwort ist kein gültiger Text")
}

/// Stream `url` into the file `dest`, hashing on the fly — nothing is held in RAM,
/// so a 500 MB package costs no more memory than a 5 MB one. Reports
/// `(bytes_done, total)` per chunk and checks `cancel` between chunks (a stalled
/// read still ends at the 60 s read timeout). Returns (bytes, hex digest).
pub fn download_to_file(
    url: &str,
    dest: &Path,
    hash_alg: &str,
    on_progress: &dyn Fn(u64, Option<u64>),
    cancel: &AtomicBool,
) -> Result<(u64, String)> {
    // Unknown algorithm: fail before spending time on the download.
    let mut hasher = crate::StreamHasher::new(hash_alg)?;
    let url = &special_url(url);
    let get = |referer: bool| {
        let req = agent().get(url);
        let req = if referer { req.set("Referer", &referer_for(url)) } else { req };
        req.call().map_err(|e| download_error(e, url))
    };
    let mut resp = get(true)?;
    // Some hosts need the Referer (irfanview.info), SourceForge answers it with
    // its "your download will start" page — a web page instead of the file means:
    // try once without.
    if is_html(&resp) && !names_html(url) {
        resp = get(false)?;
    }
    let total = resp
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok());

    let mut reader = resp.into_reader();
    let file = File::create(dest)
        .with_context(|| format!("Zwischenspeicher nicht anlegbar: {}", dest.display()))?;
    let mut out = BufWriter::with_capacity(1 << 20, file);
    let mut chunk = vec![0u8; 256 * 1024];
    let mut done: u64 = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        let n = reader
            .read(&mut chunk)
            .context("Fehler beim Lesen des Download-Streams")?;
        if n == 0 {
            break;
        }
        hasher.update(&chunk[..n]);
        out.write_all(&chunk[..n])
            .context("Download konnte nicht gespeichert werden (Datenträger voll?)")?;
        done += n as u64;
        on_progress(done, total);
    }
    out.flush()?;
    if let Some(t) = total {
        if done < t {
            bail!("Download unvollständig ({done} von {t} Bytes)");
        }
    }
    Ok((done, hasher.finish_hex()))
}

/// A download error the card can show: the HTTP status and what it means come
/// first (the card clips long lines), the URL last.
fn download_error(e: ureq::Error, url: &str) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, _) => {
            let why = match code {
                404 | 410 => ", Datei beim Hersteller nicht mehr vorhanden – Paketquelle veraltet",
                403 => ", Server verweigert den Download",
                500..=599 => ", Server-Störung – später erneut versuchen",
                _ => "",
            };
            anyhow!("Download fehlgeschlagen (HTTP {code}{why}): {url}")
        }
        other => anyhow::Error::new(other).context(format!("Download fehlgeschlagen (keine Verbindung?): {url}")),
    }
}

fn is_html(resp: &ureq::Response) -> bool {
    resp.content_type().eq_ignore_ascii_case("text/html")
}

/// The artifact itself is a web page (then HTML is the expected answer).
fn names_html(url: &str) -> bool {
    let name = url.rsplit("#/").next().unwrap_or(url).split(['?', '#']).next().unwrap_or(url);
    let name = name.to_ascii_lowercase();
    name.ends_with(".html") || name.ends_with(".htm")
}

/// Best-effort content length of a URL via a HEAD request (follows redirects).
pub fn head_size(url: &str) -> Option<u64> {
    let url = &special_url(url);
    let head = |referer: bool| {
        let req = agent().head(url);
        let req = if referer { req.set("Referer", &referer_for(url)) } else { req };
        req.call().ok()
    };
    let mut resp = head(true)?;
    // Same as the download: a web page instead of the file -> once without Referer.
    if is_html(&resp) && !names_html(url) {
        resp = head(false)?;
        if is_html(&resp) {
            return None;
        }
    }
    resp.header("Content-Length").and_then(|v| v.parse::<u64>().ok())
}

/// Verify `data` against `expected_hex` using `alg`. Errors on mismatch.
pub fn verify(alg: &str, expected_hex: &str, data: &[u8]) -> Result<()> {
    check_hash(alg, expected_hex, &crate::compute_hash(alg, data)?)
}

/// Compare a computed digest with the manifest's. Errors on mismatch.
pub fn check_hash(alg: &str, expected_hex: &str, actual: &str) -> Result<()> {
    let expected = expected_hex.to_ascii_lowercase();
    if actual == expected {
        Ok(())
    } else {
        Err(anyhow!(
            "Hash-Mismatch ({alg}): erwartet {expected}, berechnet {actual}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{referer_for, special_url};

    #[test]
    fn sourceforge_links_go_to_the_download_host() {
        assert_eq!(
            special_url("https://sourceforge.net/project/eraser/Eraser%206/6.2/Eraser%206.2.0.2996.exe#/dl.7z"),
            "https://downloads.sourceforge.net/project/eraser/Eraser%206/6.2/Eraser%206.2.0.2996.exe"
        );
        assert_eq!(
            special_url("https://sourceforge.net/projects/x/files/v1/x.zip/download"),
            "https://downloads.sourceforge.net/project/x/v1/x.zip"
        );
        assert_eq!(special_url("https://e.org/a.zip"), "https://e.org/a.zip");
    }

    #[test]
    fn referer_is_url_without_file_name() {
        assert_eq!(referer_for("https://www.irfanview.info/files/iview476_x64.zip"), "https://www.irfanview.info/files/");
        assert_eq!(referer_for("https://e.org/a/Setup.exe#/dl.7z"), "https://e.org/a/");
        assert_eq!(referer_for("https://e.org/dl/tool.zip?x=1"), "https://e.org/dl/");
    }
}
