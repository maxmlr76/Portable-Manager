//! Full-text search across the whole Scoop ecosystem via the ScoopSearch API
//! (the same Azure Search index that powers scoop.sh).

use anyhow::{Context, Result};
use serde_json::Value;

const ENDPOINT: &str =
    "https://scoopsearch.search.windows.net/indexes/apps/docs/search?api-version=2020-06-30";
/// Public read-only query key used by the scoop.sh frontend. It may rotate
/// upstream; callers should treat a failed search as non-fatal.
const API_KEY: &str = "DC6D2BBE65FC7313F2C52BBD2B0286ED";

/// One search result (a manifest in some Scoop bucket).
#[derive(Clone, Debug)]
pub struct SearchHit {
    pub name: String,
    pub description: String,
    pub homepage: String,
    pub version: String,
    /// The bucket's GitHub URL, e.g. `https://github.com/ScoopInstaller/Extras`.
    pub repo: String,
    /// Manifest path within the bucket repo, e.g. `bucket/ghidra.json`.
    pub file_path: String,
    pub official: bool,
    pub stars: i64,
}

impl SearchHit {
    /// Short owner/repo label, e.g. `ScoopInstaller/Extras`.
    pub fn bucket_label(&self) -> String {
        self.repo
            .trim_end_matches('/')
            .trim_start_matches("https://github.com/")
            .to_string()
    }
}

/// One page of search results plus the server-reported total match count.
pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    pub total: usize,
}

/// Query ScoopSearch for a single page (`skip`..`skip+top`) in server relevance
/// order. No dedup/re-rank — the caller decides how to present the pool.
pub fn search_page(query: &str, skip: usize, top: usize) -> Result<SearchPage> {
    let body = serde_json::json!({
        "count": true,
        "search": query,
        "searchMode": "all",
        "top": top,
        "skip": skip,
        "select": "Name,Description,Homepage,Version,Metadata/Repository,Metadata/FilePath,Metadata/OfficialRepository,Metadata/RepositoryStars"
    })
    .to_string();

    let text = ureq::post(ENDPOINT)
        .set("api-key", API_KEY)
        .set("Content-Type", "application/json")
        .send_string(&body)
        .context("ScoopSearch-Anfrage fehlgeschlagen")?
        .into_string()?;

    let json: Value = serde_json::from_str(&text).context("ScoopSearch-Antwort ungültig")?;
    let total = json["@odata.count"].as_u64().unwrap_or(0) as usize;
    let mut hits = Vec::new();
    if let Some(arr) = json["value"].as_array() {
        for v in arr {
            let md = &v["Metadata"];
            hits.push(SearchHit {
                name: v["Name"].as_str().unwrap_or("").to_string(),
                description: v["Description"].as_str().unwrap_or("").to_string(),
                homepage: v["Homepage"].as_str().unwrap_or("").to_string(),
                version: v["Version"].as_str().unwrap_or("").to_string(),
                repo: md["Repository"].as_str().unwrap_or("").to_string(),
                file_path: md["FilePath"].as_str().unwrap_or("").to_string(),
                official: md["OfficialRepository"].as_bool().unwrap_or(false),
                stars: md["RepositoryStars"].as_i64().unwrap_or(0),
            });
        }
    }
    Ok(SearchPage { hits, total })
}

/// Convenience for the CLI: first page, deduped and ranked (official/stars first).
pub fn search(query: &str, top: usize) -> Result<Vec<SearchHit>> {
    let mut hits = search_page(query, 0, top)?.hits;
    let mut seen = std::collections::HashSet::new();
    hits.retain(|h| seen.insert(h.name.to_ascii_lowercase()));

    let ql = query.trim().to_ascii_lowercase();
    let rank = |h: &SearchHit| {
        let nl = h.name.to_ascii_lowercase();
        let exact = if nl == ql { 2 } else if nl.starts_with(&ql) { 1 } else { 0 };
        (exact, h.official as i32, h.stars)
    };
    hits.sort_by(|a, b| rank(b).cmp(&rank(a)));
    Ok(hits)
}
