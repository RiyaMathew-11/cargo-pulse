//! On-disk ETag cache for GitHub API responses, keyed by URL. A 304 served
//! from here costs no rate-limit quota. Every read and write is
//! error-tolerant: a broken cache falls through to a plain request.

use std::hash::{Hash, Hasher};
use std::path::PathBuf;

pub struct Entry {
    pub etag: Option<String>,
    pub link: Option<String>,
    pub body: serde_json::Value,
}

/// XDG_CACHE_HOME, then ~/.cache, then the system temp dir.
fn dir() -> PathBuf {
    if let Ok(d) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(d).join("cargo-pulse");
    }
    if let Ok(h) = std::env::var("HOME") {
        return PathBuf::from(h).join(".cache").join("cargo-pulse");
    }
    std::env::temp_dir().join("cargo-pulse")
}

fn path_for(url: &str) -> PathBuf {
    // DefaultHasher::new() is keyed with fixed state, so stable across runs;
    // the stored URL guards against collisions and algorithm changes.
    let mut h = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut h);
    dir().join(format!("{:016x}.json", h.finish()))
}

pub fn load(url: &str) -> Option<Entry> {
    let text = std::fs::read_to_string(path_for(url)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    if v["url"] != url {
        return None;
    }
    Some(Entry {
        etag: v["etag"].as_str().map(String::from),
        link: v["link"].as_str().map(String::from),
        body: v["body"].clone(),
    })
}

pub fn store(url: &str, etag: Option<&str>, link: Option<&str>, body: &serde_json::Value) {
    let Some(etag) = etag else { return }; // nothing to validate with later
    let fetched_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let entry = serde_json::json!({
        "url": url, "etag": etag, "link": link, "fetched_at": fetched_at, "body": body,
    });
    let d = dir();
    let _ = std::fs::create_dir_all(&d);
    let _ = std::fs::write(path_for(url), entry.to_string());
}
