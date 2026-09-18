// Advisory-lock registry reader — shared by the fs.* mutation tools (for the
// optional hard-write-guard) and the coordination layer. Reads `.nc-tools/locks.jsonl`
// the way coordination.rs writes it, returning the LIVE (non-expired) lock on a
// path held by an agent. Cross-process: parallel servers sharing a workspace
// read the same file.
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

pub fn locks_path(root: &Path) -> PathBuf {
    root.join(".nc-tools").join("locks.jsonl")
}

fn read_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .map(|raw| {
            raw.lines()
                .filter(|l| !l.is_empty())
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Live (non-expired, not-released) locks across the registry. Last row per
/// path wins (a release re-appends an expired row). Returns raw lock rows with
/// an added `expired` bool.
pub fn live_locks(root: &Path) -> Vec<Value> {
    let rows = read_lines(&locks_path(root));
    let mut latest: Vec<Value> = Vec::new();
    for row in rows {
        let path = row["path"].as_str().unwrap_or("").to_string();
        if let Some(existing) = latest
            .iter_mut()
            .find(|e| e["path"].as_str() == Some(path.as_str()))
        {
            *existing = row;
        } else {
            latest.push(row);
        }
    }
    latest
        .into_iter()
        .map(|mut l| {
            let expiry = l["expiresAtMs"].as_u64().unwrap_or(0);
            let expired =
                expiry == 0 || l["released"].as_bool().unwrap_or(false) || now_ms() > expiry;
            l["expired"] = json!(expired);
            l
        })
        .collect()
}

fn json(v: impl serde::Serialize) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

/// The LIVE lock on `path` held by a DIFFERENT agent than `self_agent_id`.
/// Returns None when no live foreign lock exists. `path` should be the
/// workspace-relative path (same normalization coordination uses: rel_slash).
pub fn foreign_live_lock(root: &Path, path: &str, self_agent_id: &str) -> Option<Value> {
    live_locks(root).into_iter().find(|l| {
        l["path"].as_str() == Some(path)
            && l["expired"] != json(true)
            && l["agentId"].as_str() != Some(self_agent_id)
    })
}

/// Resolve a path to the workspace-relative form coordination.rs uses.
/// Note: this mirrors rel_slash so a write and its lock share the same key.
pub fn rel_path(root: &Path, abs: &Path) -> String {
    let rel = pathdiff::diff_paths(abs, root).unwrap_or_else(|| abs.to_path_buf());
    rel.to_string_lossy().replace('\\', "/")
}

/// The lock registry's path key: the workspace-relative form with BOTH the
/// root and the target canonicalized. pathdiff only matches when the two texts
/// share a prefix, and `resolve_checked` canonicalizes, so if either side is
/// left in the caller's raw form the diff fails, `rel_path` falls back to the
/// ABSOLUTE path, and no lock key ever matches — the foreign-lock guard
/// silently stops working. Windows canonicalization is the common trigger
/// (component-casing changes and 8.3 short names, e.g. NCDEVS~1 → Ncdevshiv),
/// but the failure is platform-independent whenever a path's text differs from
/// its canonical form. Use this on BOTH sides (agent.lock writer, fs.* guard
/// reader) so a write and its lock share one key regardless of how either side
/// was spelled. Canonicalizing a missing target resolves its deepest existing
/// ancestor and appends the rest, which is what a write target needs.
pub fn rel_key(root: &Path, abs: &Path) -> String {
    let canonical_root = match crate::paths::resolve_checked(root, ".") {
        Ok(r) => r,
        Err(_) => root.to_path_buf(),
    };
    let canonical_abs = match crate::paths::resolve_checked(abs, ".") {
        Ok(a) => a,
        Err(_) => abs.to_path_buf(),
    };
    rel_path(&canonical_root, &canonical_abs)
}

/// Append a lock line (as the test harness and coordination layer do). Exposed
/// so fs.* lock-guard tests can seed a lock deterministically without piping
/// through agent.lock.
pub fn append_lock_line(root: &Path, entry: &Value) {
    let path = locks_path(root);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all((serde_json::to_string(entry).unwrap_or_default() + "\n").as_bytes());
    }
}

#[cfg(test)]
mod lock_key_tests {
    use super::*;
    use std::fs;

    fn ws(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nct-lockkey-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The registry key must stay workspace-relative no matter how the root was
    /// spelled. resolve_checked canonicalizes the target, so when the root text
    /// differs from its canonical form pathdiff cannot match the prefix and
    /// rel_path falls back to the absolute path — which is the failure mode
    /// that silently disabled the foreign-lock guard.
    #[test]
    fn rel_key_stays_relative_from_a_raw_root() {
        let root = ws("rel");
        let abs = crate::paths::resolve_checked(&root, "src/a.rs").unwrap();
        let key = rel_key(&root, &abs);
        assert_eq!(key, "src/a.rs", "key must be workspace-relative: {key}");
        assert!(
            !key.contains(std::path::MAIN_SEPARATOR_STR),
            "key must be relative: {key}"
        );
        assert!(!key.starts_with('/'), "key must be relative: {key}");

        // A path outside the root must still round-trip to a stable key, not to
        // an absolute path keyed on the raw root spelling.
        let outside = crate::paths::resolve_checked(&root, "../other/a.rs").unwrap();
        let outer = rel_key(&root, &outside);
        assert_eq!(
            outer, "../other/a.rs",
            "outside key must stay relative: {outer}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Two agents spelling the same root differently must compute the same key —
    /// that is what makes a cross-process lock match a cross-process guard. The
    /// TARGET spelling must be normalized the same way: a writer that hands in
    /// a raw path and a reader that hands in a resolve_checked'd one are the same
    /// file, and their keys must agree.
    #[test]
    fn rel_key_is_invariant_under_path_normalization() {
        let root = ws("invar");
        let raw = root.clone();
        let dotted = root.join(".");
        let abs = crate::paths::resolve_checked(&root, "nested/b.txt").unwrap();
        let abs_raw = root.join("nested").join(".").join("b.txt");
        assert_eq!(
            rel_key(&raw, &abs),
            rel_key(&dotted, &abs),
            "root spellings must agree"
        );
        assert_eq!(
            rel_key(&raw, &abs_raw),
            rel_key(&raw, &abs),
            "target spellings must agree: {} vs {}",
            rel_key(&raw, &abs_raw),
            rel_key(&raw, &abs)
        );
        assert_eq!(
            rel_key(&dotted, &abs_raw),
            rel_key(&raw, &abs),
            "both sides must normalize"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
