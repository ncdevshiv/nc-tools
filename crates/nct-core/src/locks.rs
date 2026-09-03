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
        .map(|raw| raw.lines().filter(|l| !l.is_empty()).filter_map(|l| serde_json::from_str(l).ok()).collect())
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
        if let Some(existing) = latest.iter_mut().find(|e| e["path"].as_str() == Some(path.as_str())) {
            *existing = row;
        } else {
            latest.push(row);
        }
    }
    latest
        .into_iter()
        .map(|mut l| {
            let expiry = l["expiresAtMs"].as_u64().unwrap_or(0);
            let expired = expiry == 0 || l["released"].as_bool().unwrap_or(false) || now_ms() > expiry;
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
    live_locks(root)
        .into_iter()
        .find(|l| {
            l["path"].as_str() == Some(path) && l["expired"] != json(true) && l["agentId"].as_str() != Some(self_agent_id)
        })
}

/// Resolve a path to the workspace-relative form coordination.rs uses.
/// Note: this mirrors rel_slash so a write and its lock share the same key.
pub fn rel_path(root: &Path, abs: &Path) -> String {
    let rel = pathdiff::diff_paths(abs, root).unwrap_or_else(|| abs.to_path_buf());
    rel.to_string_lossy().replace('\\', "/")
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
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all((serde_json::to_string(entry).unwrap_or_default() + "\n").as_bytes());
    }
}
