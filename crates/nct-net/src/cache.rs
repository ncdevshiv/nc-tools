// net.fetch cache: URL-keyed, content-hashed, ETag/Last-Modified revalidated.
// Stored under <root>/.nc-tools/net-cache/<hash>.json so fetches are
// reproducible and auditable; TTL 0 = always revalidate.
use std::fs;
use std::path::PathBuf;

use serde_json::{json, Value};

use nct_core::errors::ToolError;
use nct_core::sha256_hex;

pub struct CacheEntry {
    pub url: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: String,
    pub body: String,
    pub fetched_at: String,
}

fn cache_dir(root: &std::path::Path) -> PathBuf {
    root.join(".nc-tools").join("net-cache")
}

fn key_for(url: &str) -> String {
    // sha256 of the normalized URL — filesystem-safe, collision-free enough
    sha256_hex(url.as_bytes())[..32].to_string()
}

pub fn load(root: &std::path::Path, url: &str) -> Option<CacheEntry> {
    let path = cache_dir(root).join(format!("{}.json", key_for(url)));
    let raw = fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    Some(CacheEntry {
        url: v["url"].as_str()?.to_string(),
        etag: v["etag"].as_str().map(String::from),
        last_modified: v["lastModified"].as_str().map(String::from),
        content_type: v["contentType"].as_str().unwrap_or_default().to_string(),
        body: v["body"].as_str()?.to_string(),
        fetched_at: v["fetchedAt"].as_str()?.to_string(),
    })
}

pub fn store(root: &std::path::Path, entry: &CacheEntry) -> Result<(), ToolError> {
    let dir = cache_dir(root);
    fs::create_dir_all(dir)?;
    let v = json!({
        "url": entry.url,
        "etag": entry.etag,
        "lastModified": entry.last_modified,
        "contentType": entry.content_type,
        "body": entry.body,
        "fetchedAt": entry.fetched_at,
    });
    let path = cache_dir(root).join(format!("{}.json", key_for(&entry.url)));
    let tmp = path.with_extension("part");
    fs::write(&tmp, serde_json::to_string(&v)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}
