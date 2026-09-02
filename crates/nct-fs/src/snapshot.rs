// sys.snapshot / sys.rollback — workspace snapshots with manifest-based
// restore. Port of src/kernel/snapshot.mjs: the primitive that makes agent
// speculation safe — snapshot before a risky edit, roll back if verification
// fails.
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};

use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};
use nct_core::paths::is_reparse_point;
use nct_core::{now_iso, now_ms};

pub const SNAPSHOT_DESC: &str = "Capture a workspace snapshot (files + dirs, excludes .git/node_modules/.nc-tools). Returns an id usable with sys.rollback. Use before risky edits so you can undo.";
pub const ROLLBACK_DESC: &str = "Restore the workspace to a snapshot: restores manifest files, removes files created after the snapshot.";
pub const LIST_SNAPSHOTS_DESC: &str = "List available snapshots.";

const EXCLUDED: &[&str] = &[".git", "node_modules", ".nc-tools"];

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SnapshotArgs {
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RollbackArgs {
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmptySnapArgs {}

fn snapshots_root(root: &Path) -> PathBuf {
    root.join(".nc-tools").join("snapshots")
}

/// Relative-path file walk (slash-separated rels), skipping excluded dirs,
/// links and reparse points — snapshot.mjs walkFiles.
fn walk_files(dir: &Path, rel: &str, out: &mut Vec<String>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    entries.sort();
    for abs in entries {
        let name = abs.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if EXCLUDED.contains(&name.as_str()) {
            continue;
        }
        let rel_path = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        let Ok(lm) = fs::symlink_metadata(&abs) else { continue };
        if lm.is_symlink() {
            continue;
        }
        if lm.is_dir() {
            if is_reparse_point(&abs) {
                continue;
            }
            walk_files(&abs, &rel_path, out);
        } else {
            out.push(rel_path);
        }
    }
}

pub struct SnapshotHandler;
impl Handler for SnapshotHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: SnapshotArgs = parse_args(args)?;
        let label = a.label.unwrap_or_else(|| "auto".to_string());
        let safe: String = label
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .collect();
        let id = format!("{}-{}", now_ms(), safe);
        let dir = snapshots_root(&k.root).join(&id);
        let mut files: Vec<String> = Vec::new();
        walk_files(&k.root, "", &mut files);
        for rel in &files {
            let src = k.root.join(rel);
            let dst = dir.join(rel);
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&src, &dst)?;
        }
        let manifest = json!({ "id": id, "files": files, "ts": now_iso() });
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("manifest.json"), serde_json::to_string(&manifest)?)?;
        Ok(json!({ "id": id, "files": files.len(), "restoredOnRollback": files.len() }))
    }
}

pub struct ListSnapshotsHandler;
impl Handler for ListSnapshotsHandler {
    fn call(&self, k: &Kernel, _args: &Value) -> Result<Value, ToolError> {
        let snapshots = list_impl(&k.root);
        Ok(json!({ "snapshots": snapshots, "total": snapshots.len() }))
    }
}

fn list_impl(root: &Path) -> Vec<Value> {
    let sroot = snapshots_root(root);
    let Ok(rd) = fs::read_dir(&sroot) else { return Vec::new() };
    let mut names: Vec<String> = rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect();
    names.sort();
    names.reverse(); // JS readdirSync().sort().reverse()
    let mut out = Vec::new();
    for name in names {
        let dir = sroot.join(&name);
        match fs::metadata(&dir) {
            Ok(m) if m.is_dir() => {}
            _ => continue,
        }
        let Ok(raw) = fs::read_to_string(dir.join("manifest.json")) else { continue };
        let Ok(manifest) = serde_json::from_str::<Value>(&raw) else { continue };
        out.push(json!({
            "id": name,
            "files": manifest["files"].as_array().map(|a| a.len()).unwrap_or(0),
            "ts": manifest["ts"],
        }));
    }
    out
}

pub struct RollbackHandler;
impl Handler for RollbackHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: RollbackArgs = parse_args(args)?;
        if a.id.is_empty() {
            return Err(ToolError::new("ERR_BAD_INPUT", "snapshot id required"));
        }
        let dir = snapshots_root(&k.root).join(&a.id);
        let manifest_path = dir.join("manifest.json");
        if !manifest_path.exists() {
            let available: Vec<String> = list_impl(&k.root)
                .iter()
                .filter_map(|s| s["id"].as_str().map(|s| s.to_string()))
                .collect();
            return Err(ToolError::with_hint(
                "ERR_UNKNOWN_SNAPSHOT",
                format!("no snapshot {}", a.id),
                json!({ "available": available }),
            ));
        }
        let raw = fs::read_to_string(&manifest_path)?;
        let manifest: Value = serde_json::from_str(&raw)?;
        let manifest_files: Vec<String> = manifest["files"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        // 1. restore every file in the manifest
        for rel in &manifest_files {
            let src = dir.join(rel);
            let dst = k.root.join(rel);
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&src, &dst)?;
        }
        // 2. remove files created after the snapshot (not in manifest, not excluded)
        let manifest_set: std::collections::HashSet<&String> = manifest_files.iter().collect();
        let mut current: Vec<String> = Vec::new();
        walk_files(&k.root, "", &mut current);
        let mut removed: Vec<String> = Vec::new();
        for rel in &current {
            if manifest_set.contains(rel) {
                continue;
            }
            let abs = k.root.join(rel);
            let res = if fs::metadata(&abs).map(|m| m.is_dir()).unwrap_or(false) {
                fs::remove_dir_all(&abs)
            } else {
                fs::remove_file(&abs)
            };
            if res.is_ok() {
                removed.push(rel.clone());
            }
        }
        Ok(json!({
            "id": a.id,
            "restored": manifest_files.len(),
            "removed": removed.len(),
            "removedFiles": &removed[..removed.len().min(20)],
        }))
    }
}
