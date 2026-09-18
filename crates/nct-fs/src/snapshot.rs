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
pub const ROLLBACK_DESC: &str = "Restore the workspace to a snapshot: restores manifest files, removes files created after the snapshot. Pass paths to restore only those files and leave the rest untouched (partial rollback).";
pub const LIST_SNAPSHOTS_DESC: &str = "List available snapshots.";
pub const SNAPSHOT_DIFF_DESC: &str = "Diff two snapshots: what files were added, removed, or modified between them, plus byte totals. Answers 'what changed since I snapshotted before the deploy?' — the review gate before a rollback.";

const EXCLUDED: &[&str] = &[".git", "node_modules", ".nc-tools"];

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SnapshotArgs {
    #[serde(default)]
    pub label: Option<String>,
    /// Per-call workspace override: snapshot THIS base dir (its snapshot
    /// store lives under <baseDir>/.nc-tools/snapshots). Default: the session
    /// workspace the server was rooted on.
    #[serde(default)]
    pub baseDir: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RollbackArgs {
    pub id: String,
    /// Restore only these paths from the snapshot; other files are left as-is.
    /// This makes a bad edit undoable WITHOUT losing unrelated work.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    /// Per-call workspace override: restore into THIS base dir (same one the
    /// snapshot was taken from). Default: the session workspace.
    #[serde(default)]
    pub baseDir: Option<String>,
    /// Preview only: report what a full rollback would restore and delete
    /// without touching the tree. A full rollback DELETES every file created
    /// after the snapshot — including unrelated work — so preview first.
    #[serde(default)]
    pub dryRun: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SnapshotDiffArgs {
    pub a: String,
    pub b: String,
    /// Per-call workspace override: diff snapshots from THIS base dir's
    /// snapshot store. Default: the session workspace.
    #[serde(default)]
    pub baseDir: Option<String>,
}

/// Per-call workspace override for sys.listSnapshots (was an empty-args tool).
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListSnapshotsArgs {
    #[serde(default)]
    pub baseDir: Option<String>,
}

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
        let name = abs
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if EXCLUDED.contains(&name.as_str()) {
            continue;
        }
        let rel_path = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let Ok(lm) = fs::symlink_metadata(&abs) else {
            continue;
        };
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
        let base = k.base_dir(a.baseDir.as_deref())?;
        let label = a.label.unwrap_or_else(|| "auto".to_string());
        let safe: String = label
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let id = format!("{}-{}", now_ms(), safe);
        let dir = snapshots_root(&base).join(&id);
        let mut files: Vec<String> = Vec::new();
        walk_files(&base, "", &mut files);
        for rel in &files {
            let src = base.join(rel);
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
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: ListSnapshotsArgs = parse_args(args)?;
        let base = k.base_dir(a.baseDir.as_deref())?;
        let snapshots = list_impl(&base);
        Ok(json!({ "snapshots": snapshots, "total": snapshots.len() }))
    }
}

fn list_impl(root: &Path) -> Vec<Value> {
    let sroot = snapshots_root(root);
    let Ok(rd) = fs::read_dir(&sroot) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names.reverse(); // JS readdirSync().sort().reverse()
    let mut out = Vec::new();
    for name in names {
        let dir = sroot.join(&name);
        match fs::metadata(&dir) {
            Ok(m) if m.is_dir() => {}
            _ => continue,
        }
        let Ok(raw) = fs::read_to_string(dir.join("manifest.json")) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
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
        let base = k.base_dir(a.baseDir.as_deref())?;
        let dir = snapshots_root(&base).join(&a.id);
        let manifest_path = dir.join("manifest.json");
        if !manifest_path.exists() {
            let available: Vec<String> = list_impl(&base)
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
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        // Partial rollback: restore ONLY the requested paths (and their
        // descendants) from the snapshot, leaving all else untouched. A bad
        // edit to config/ is undone without losing unrelated work in src/.
        if let Some(select) = &a.paths {
            let selected: Vec<String> = manifest_files
                .iter()
                .filter(|rel| {
                    select
                        .iter()
                        .any(|p| rel.as_str() == p.as_str() || rel.starts_with(&format!("{p}/")))
                })
                .cloned()
                .collect();
            let mut restored = Vec::new();
            for rel in &selected {
                if rel.is_empty() {
                    continue;
                }
                let src = dir.join(rel);
                if !src.exists() {
                    continue;
                }
                let dst = base.join(rel);
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(&src, &dst)?;
                restored.push(rel.clone());
            }
            return Ok(json!({
                "id": a.id,
                "partial": true,
                "restored": restored.len(),
                "restoredPaths": &restored[..restored.len().min(20)],
                "note": "partial rollback — only the selected paths were restored; unrelated files untouched",
            }));
        }

        // Files absent from the manifest are what a full rollback DELETES —
        // every file created or touched after the snapshot, including
        // unrelated work. Compute the list before writing anything so dryRun
        // can show it without touching the tree.
        let manifest_set: std::collections::HashSet<&String> = manifest_files.iter().collect();
        let mut current: Vec<String> = Vec::new();
        walk_files(&base, "", &mut current);
        let would_remove: Vec<String> = current
            .iter()
            .filter(|rel| !manifest_set.contains(rel))
            .cloned()
            .collect();
        if a.dryRun.unwrap_or(false) {
            return Ok(json!({
                "id": a.id,
                "dryRun": true,
                "wouldRestore": manifest_files.len(),
                "wouldRemove": would_remove.len(),
                "wouldRemoveFiles": &would_remove[..would_remove.len().min(50)],
                "note": "dry run — nothing was written. Re-run with dryRun=false to apply. The delete phase removes every file created after the snapshot, including unrelated work; pass paths to restore a subset instead.",
            }));
        }
        // 1. restore every file in the manifest
        for rel in &manifest_files {
            let src = dir.join(rel);
            let dst = base.join(rel);
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&src, &dst)?;
        }
        // 2. remove files created after the snapshot (not in manifest, not excluded)
        let mut removed: Vec<String> = Vec::new();
        for rel in &would_remove {
            let abs = base.join(rel);
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
            "removedTruncated": removed.len() > 20,
        }))
    }
}

pub struct SnapshotDiffHandler;
impl Handler for SnapshotDiffHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: SnapshotDiffArgs = parse_args(args)?;
        let base = k.base_dir(a.baseDir.as_deref())?;
        let sroot = snapshots_root(&base);
        let dir_a = sroot.join(&a.a);
        let dir_b = sroot.join(&a.b);
        if !dir_a.join("manifest.json").exists() || !dir_b.join("manifest.json").exists() {
            let available: Vec<String> = list_impl(&base)
                .iter()
                .filter_map(|s| s["id"].as_str().map(|s| s.to_string()))
                .collect();
            return Err(ToolError::with_hint(
                "ERR_UNKNOWN_SNAPSHOT",
                format!("snapshot diff needs two known ids (got {} / {})", a.a, a.b),
                json!({ "available": available }),
            ));
        }
        let files_a = snapshot_files(&dir_a);
        let files_b = snapshot_files(&dir_b);
        let set_a: std::collections::HashSet<&String> = files_a.iter().collect();
        let set_b: std::collections::HashSet<&String> = files_b.iter().collect();

        // added = in B but not A; removed = in A but not B; modified = in both
        // but different bytes (compare actual file content via length + hash).
        let mut added: Vec<String> = Vec::new();
        let mut removed: Vec<String> = Vec::new();
        let mut modified: Vec<Value> = Vec::new();
        for f in &files_b {
            if !set_a.contains(f) {
                added.push(f.clone());
            }
        }
        for f in &files_a {
            if !set_b.contains(f) {
                removed.push(f.clone());
            }
        }
        for f in &files_a {
            if set_b.contains(f) {
                let pa = dir_a.join(f);
                let pb = dir_b.join(f);
                if file_changed(&pa, &pb) {
                    modified.push(
                        json!({ "file": f, "aBytes": file_len(&pa), "bBytes": file_len(&pb) }),
                    );
                }
            }
        }
        added.sort();
        removed.sort();
        modified.sort_by(|x, y| {
            x["file"]
                .as_str()
                .unwrap_or("")
                .cmp(y["file"].as_str().unwrap_or(""))
        });

        // "changed" = everything that differs between the two snapshots:
        // added + removed + modified. This is the number an agent reads to
        // decide whether a rollback is warranted.
        let changed_files: Vec<String> = modified
            .iter()
            .map(|m| m["file"].as_str().unwrap_or("").to_string())
            .collect();
        let changed_count = added.len() + removed.len() + modified.len();
        let mut total_bytes_a: u64 = 0;
        let mut total_bytes_b: u64 = 0;
        for f in &files_a {
            total_bytes_a += file_len(&dir_a.join(f));
        }
        for f in &files_b {
            total_bytes_b += file_len(&dir_b.join(f));
        }

        Ok(json!({
            "from": a.a,
            "to": a.b,
            "added": &added[..added.len().min(50)],
            "removed": &removed[..removed.len().min(50)],
            "modified": &modified[..modified.len().min(50)],
            "changed": changed_count,
            "changedFiles": &changed_files[..changed_files.len().min(50)],
            "bytesFrom": total_bytes_a,
            "bytesTo": total_bytes_b,
            "netBytes": total_bytes_b as i64 - total_bytes_a as i64,
        }))
    }
}

fn snapshot_files(dir: &Path) -> Vec<String> {
    let Ok(raw) = fs::read_to_string(dir.join("manifest.json")) else {
        return Vec::new();
    };
    let Ok(manifest) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    manifest["files"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn file_changed(a: &Path, b: &Path) -> bool {
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(ma), Ok(mb)) => {
            if ma.len() != mb.len() {
                return true;
            }
            // Same length — compare content hashes to catch same-size edits.
            match (fs::read(a), fs::read(b)) {
                (Ok(ba), Ok(bb)) => ba != bb,
                _ => true,
            }
        }
        _ => true,
    }
}

fn file_len(p: &Path) -> u64 {
    fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

#[cfg(test)]
mod snapshot_diff_tests {
    use super::*;
    use nct_core::kernel::Kernel;
    use std::fs;

    fn make_kernel() -> Kernel {
        let dir = std::env::temp_dir().join(format!(
            "nct-snap-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut k = Kernel::new(dir).unwrap();
        crate::register(&mut k);
        k
    }

    #[test]
    fn snapshot_roundtrip_restores_all() {
        let k = make_kernel();
        fs::create_dir_all(k.root.join("sub")).unwrap();
        fs::write(k.root.join("a.txt"), "alpha\n").unwrap();
        fs::write(k.root.join("sub/b.txt"), "beta\n").unwrap();
        let s1 = SnapshotHandler
            .call(&k, &json!({ "label": "before" }))
            .unwrap();
        let id = s1["id"].as_str().unwrap().to_string();
        assert!(s1["files"].as_u64().unwrap() >= 2);
        // mutate
        fs::write(k.root.join("a.txt"), "CHANGED\n").unwrap();
        // rollback
        let rb = RollbackHandler.call(&k, &json!({ "id": id })).unwrap();
        assert!(rb["restored"].as_u64().unwrap() >= 2);
        assert_eq!(fs::read_to_string(k.root.join("a.txt")).unwrap(), "alpha\n");
    }

    #[test]
    fn partial_rollback_restores_only_selected() {
        let k = make_kernel();
        fs::write(k.root.join("a.txt"), "alpha\n").unwrap();
        fs::write(k.root.join("b.txt"), "beta\n").unwrap();
        let s1 = SnapshotHandler
            .call(&k, &json!({ "label": "before" }))
            .unwrap();
        let id = s1["id"].as_str().unwrap().to_string();
        // mutate both
        fs::write(k.root.join("a.txt"), "CHANGED-A\n").unwrap();
        fs::write(k.root.join("b.txt"), "CHANGED-B\n").unwrap();
        // rollback only a.txt
        let rb = RollbackHandler
            .call(&k, &json!({ "id": id, "paths": ["a.txt"] }))
            .unwrap();
        assert_eq!(rb["partial"], json!(true));
        assert_eq!(rb["restored"], json!(1));
        assert_eq!(fs::read_to_string(k.root.join("a.txt")).unwrap(), "alpha\n");
        // b.txt is untouched — the unrelated change survives
        assert_eq!(
            fs::read_to_string(k.root.join("b.txt")).unwrap(),
            "CHANGED-B\n"
        );
    }

    #[test]
    fn rollback_dry_run_lists_deletes_without_writing() {
        let k = make_kernel();
        fs::write(k.root.join("a.txt"), "alpha\n").unwrap();
        let s1 = SnapshotHandler
            .call(&k, &json!({ "label": "before" }))
            .unwrap();
        let id = s1["id"].as_str().unwrap().to_string();
        // Created AFTER the snapshot — this is what a full rollback would delete.
        fs::write(k.root.join("new.txt"), "created after the snapshot\n").unwrap();

        let dry = RollbackHandler
            .call(&k, &json!({ "id": id, "dryRun": true }))
            .unwrap();
        assert_eq!(dry["dryRun"], json!(true));
        assert!(
            dry["wouldRemove"].as_u64().unwrap() >= 1,
            "new.txt not listed: {dry}"
        );
        let listed = dry["wouldRemoveFiles"].as_array().unwrap();
        assert!(
            listed.iter().any(|v| v.as_str() == Some("new.txt")),
            "missing new.txt: {dry}"
        );
        // Nothing was touched.
        assert!(k.root.join("new.txt").exists(), "dry run must not delete");
        assert_eq!(fs::read_to_string(k.root.join("a.txt")).unwrap(), "alpha\n");

        // ...and a real rollback still does the work.
        let real = RollbackHandler.call(&k, &json!({ "id": id })).unwrap();
        assert!(real["dryRun"].is_null());
        assert!(real["removed"].as_u64().unwrap() >= 1);
        assert!(
            !k.root.join("new.txt").exists(),
            "real rollback should delete new.txt"
        );
    }

    #[test]
    fn snapshot_diff_reports_added_removed_modified() {
        let k = make_kernel();
        fs::write(k.root.join("keep.txt"), "same\n").unwrap();
        fs::write(k.root.join("remove.txt"), "gone soon\n").unwrap();
        let s1 = SnapshotHandler.call(&k, &json!({ "label": "v1" })).unwrap();
        let id1 = s1["id"].as_str().unwrap().to_string();
        // change keep, remove remove.txt, add new.txt
        fs::write(k.root.join("keep.txt"), "changed\n").unwrap();
        fs::remove_file(k.root.join("remove.txt")).unwrap();
        fs::write(k.root.join("new.txt"), "brand new\n").unwrap();
        let s2 = SnapshotHandler.call(&k, &json!({ "label": "v2" })).unwrap();
        let id2 = s2["id"].as_str().unwrap().to_string();

        let diff = SnapshotDiffHandler
            .call(&k, &json!({ "a": id1, "b": id2 }))
            .unwrap();
        assert!(diff["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "new.txt"));
        assert!(diff["removed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "remove.txt"));
        assert!(diff["modified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["file"] == "keep.txt"));
        assert!(diff["changed"].as_u64().unwrap() >= 3);
        assert!(diff["bytesFrom"].as_u64().unwrap() > 0);
    }

    #[test]
    fn snapshot_diff_unknown_id_errors() {
        let k = make_kernel();
        let err =
            SnapshotDiffHandler.call(&k, &json!({ "a": "does-not-exist", "b": "also-missing" }));
        let e = err.unwrap_err();
        assert_eq!(e.code, "ERR_UNKNOWN_SNAPSHOT");
    }

    #[test]
    fn rollback_unknown_id_gives_available() {
        let k = make_kernel();
        let err = RollbackHandler
            .call(&k, &json!({ "id": "nope" }))
            .unwrap_err();
        assert_eq!(err.code, "ERR_UNKNOWN_SNAPSHOT");
        let hint = err.hint.expect("has available list");
        assert!(hint.get("available").is_some());
    }

    /// The whole snapshot family must route to baseDir: the store lives under
    /// <baseDir>/.nc-tools/snapshots and covers baseDir's files — NOT the
    /// server root's. Proven on all four tools (snapshot, list, diff, rollback).
    #[test]
    fn snapshot_family_routes_to_baseDir() {
        let k = make_kernel();
        let target = std::env::temp_dir().join(format!(
            "nct-snap-target-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&target);
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("work.txt"), "target content\n").unwrap();

        // snapshot the TARGET workspace via baseDir
        let snap = SnapshotHandler
            .call(
                &k,
                &json!({ "label": "cross", "baseDir": target.display().to_string() }),
            )
            .unwrap();
        let id = snap["id"].as_str().unwrap().to_string();
        assert!(snap["files"].as_u64().unwrap() >= 1);
        // the store is under the TARGET, not the server root
        assert!(target
            .join(".nc-tools")
            .join("snapshots")
            .join(&id)
            .join("manifest.json")
            .exists());
        assert!(!k
            .root
            .join(".nc-tools")
            .join("snapshots")
            .join(&id)
            .exists());

        // listSnapshots on the target sees it; the server root does not
        let listed = ListSnapshotsHandler
            .call(&k, &json!({ "baseDir": target.display().to_string() }))
            .unwrap();
        assert!(listed["snapshots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == json!(id)));
        let listed_root = ListSnapshotsHandler.call(&k, &json!({})).unwrap();
        assert!(!listed_root["snapshots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == json!(id)));

        // mutate the target, then diff + rollback through baseDir
        fs::write(target.join("work.txt"), "CHANGED\n").unwrap();
        fs::write(target.join("extra.txt"), "created after\n").unwrap();
        let snap2 = SnapshotHandler
            .call(
                &k,
                &json!({ "label": "after", "baseDir": target.display().to_string() }),
            )
            .unwrap();
        let id2 = snap2["id"].as_str().unwrap().to_string();
        let diff = SnapshotDiffHandler
            .call(
                &k,
                &json!({ "a": id, "b": id2, "baseDir": target.display().to_string() }),
            )
            .unwrap();
        assert!(diff["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "extra.txt"));

        let rb = RollbackHandler
            .call(
                &k,
                &json!({ "id": id, "baseDir": target.display().to_string() }),
            )
            .unwrap();
        assert_eq!(rb["id"], json!(id));
        assert_eq!(
            fs::read_to_string(target.join("work.txt")).unwrap(),
            "target content\n",
            "target file restored"
        );
        assert!(
            !target.join("extra.txt").exists(),
            "post-snapshot file removed from the target"
        );
        // the server root was never touched by any of this
        assert!(!k.root.join("work.txt").exists());

        let _ = fs::remove_dir_all(&target);
    }
}
