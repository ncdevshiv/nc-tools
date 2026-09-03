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

/// The root the net-cache reads/writes under: `NCTOOLS_NET_CACHE` wins
/// (shared, machine-wide cache — mirrors NCTOOLS_MODEL_CACHE), otherwise the
/// kernel's EFFECTIVE base (session anchor / server root), so a session
/// anchored to the client's workspace keeps its fetch cache in THAT
/// workspace's .nc-tools instead of polluting the server root's.
pub fn root_for(k: &nct_core::Kernel) -> std::path::PathBuf {
    if let Ok(d) = std::env::var("NCTOOLS_NET_CACHE") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    k.base_dir(None).unwrap_or_else(|_| k.root.clone())
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

#[cfg(test)]
mod root_for_tests {
    use super::*;

    /// The net-cache root follows the session anchor (default base) so a
    /// session working on another workspace caches into THAT workspace, and
    /// NCTOOLS_NET_CACHE overrides everything (shared machine-wide cache).
    #[test]
    fn net_cache_root_routes_to_session_anchor_and_env() {
        let server_root = std::env::temp_dir().join(format!(
            "nct-netcache-srv-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let anchored = std::env::temp_dir().join(format!(
            "nct-netcache-anchor-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&server_root).unwrap();
        fs::create_dir_all(&anchored).unwrap();
        let mut k = nct_core::Kernel::new(server_root.clone()).unwrap();
        let _ = fs::remove_dir_all(join_cache(&server_root));
        let _ = fs::remove_dir_all(join_cache(&anchored));

        // no anchor, no env: server root
        let was_set = std::env::var("NCTOOLS_NET_CACHE").ok();
        std::env::remove_var("NCTOOLS_NET_CACHE");
        assert_eq!(root_for(&k), server_root);

        // session anchor set: cache root follows it (canonicalized)
        assert!(k.set_default_base(&anchored));
        assert_eq!(root_for(&k), dunce::canonicalize(&anchored).unwrap());

        // env wins over the anchor
        std::env::set_var("NCTOOLS_NET_CACHE", anchored.join("shared-cache").display().to_string());
        assert_eq!(root_for(&k), anchored.join("shared-cache"));

        // store/load actually land under the env-overridden dir
        store(&root_for(&k), &CacheEntry {
            url: "https://example.com/x".into(),
            etag: None,
            last_modified: None,
            content_type: "text/plain".into(),
            body: "hi".into(),
            fetched_at: "2026-01-01T00:00:00Z".into(),
        })
        .unwrap();
        assert!(join_cache(&anchored.join("shared-cache")).exists(), "cache file must be under the env dir");
        assert!(load(&root_for(&k), "https://example.com/x").is_some());

        // restore
        match was_set {
            Some(v) => std::env::set_var("NCTOOLS_NET_CACHE", v),
            None => std::env::remove_var("NCTOOLS_NET_CACHE"),
        }
        let _ = fs::remove_dir_all(&server_root);
        let _ = fs::remove_dir_all(&anchored);
    }

    fn join_cache(p: &std::path::Path) -> std::path::PathBuf {
        p.join(".nc-tools").join("net-cache")
    }
}
