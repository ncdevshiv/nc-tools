// fs.* tools — typed file operations, machine-wide (no workspace jail).
// Behavior-parity port of src/kernel/fs.mjs: same result shapes, same error
// codes, same hint payloads.
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};

use nct_core::errors::ToolError;
use nct_core::helpers::rel_slash;
use nct_core::kernel::{parse_args, Handler, Kernel};
use nct_core::paths::{is_reparse_point, resolve_checked};
use nct_core::sha256_hex;

pub const READ_DESC: &str = "Read a text file with line numbers. Supports offset/limit paging. Returns a digest (hash+mtime) — use it to avoid re-reading unchanged files.";
pub const READ_MANY_DESC: &str = "Read up to 50 files in ONE call. Each item returns ok/content or a structured error. Prefer this over N separate fs.read calls.";
pub const WRITE_DESC: &str = "Write a file (creates parent dirs). Returns created/overwrote.";
pub const WRITE_MANY_DESC: &str = "Write up to 50 files in ONE call: [{path, content}]. Per-item ok/error results.";
pub const APPEND_DESC: &str = "Append content to a file (creates it and parent dirs if missing).";
pub const COPY_DESC: &str = "Copy a file or directory (recursive default). Overwrites existing destinations (journal records the event).";
pub const LIST_DESC: &str = "List directory entries.";
pub const STAT_DESC: &str = "Stat a path (exists, type, size).";
pub const MKDIR_DESC: &str = "Create a directory.";
pub const DELETE_DESC: &str = "Delete a file or directory (needs recursive for dirs).";
pub const MOVE_DESC: &str = "Move/rename a file or directory.";


/// Generic adapter: deserialize typed args, then run the body. Args are
/// validated against the same struct that generated the schema.
pub struct Typed<T, F> {
    handler: F,
    _marker: std::marker::PhantomData<T>,
}

impl<T, F> Handler for Typed<T, F>
where
    T: serde::de::DeserializeOwned + Send + Sync + 'static,
    F: Fn(&Kernel, T) -> Result<Value, ToolError> + Send + Sync,
{
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let typed: T = parse_args(args)?;
        (self.handler)(k, typed)
    }
}

/// Convenience: wrap a closure into an Arc<dyn Handler> with typed args.
pub fn handler<T, F>(f: F) -> std::sync::Arc<dyn Handler>
where
    T: serde::de::DeserializeOwned + Send + Sync + 'static,
    F: Fn(&Kernel, T) -> Result<Value, ToolError> + Send + Sync + 'static,
{
    std::sync::Arc::new(Typed { handler: f, _marker: std::marker::PhantomData })
}

// ---- shared helpers --------------------------------------------------------

/// Nearest existing siblings/files for NOT_FOUND hints — mirrors fs.mjs
/// nearestSiblings: up to 5 sorted non-hidden siblings of the missing dir,
/// then the same basename found while walking up (max 8 total).
pub fn nearest_siblings(root: &Path, abs_missing: &Path) -> Vec<String> {
    let mut hints: Vec<String> = Vec::new();
    let dir = match abs_missing.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => root.to_path_buf(),
    };
    let base = abs_missing
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if let Ok(rd) = fs::read_dir(&dir) {
        let mut names: Vec<String> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        for n in names {
            if hints.len() >= 5 {
                break;
            }
            if n != base && !n.starts_with('.') {
                hints.push(rel_slash(root, &dir.join(&n)));
            }
        }
    }
    let mut probe = dir;
    for _ in 0..3 {
        if hints.len() >= 8 {
            break;
        }
        match fs::read_dir(&probe) {
            Ok(rd) => {
                let mut names: Vec<String> = rd
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect();
                names.sort();
                for n in names {
                    if n == base {
                        hints.push(rel_slash(root, &probe.join(&n)));
                        break;
                    }
                }
                match probe.parent() {
                    Some(p) if !p.as_os_str().is_empty() => probe = p.to_path_buf(),
                    _ => break,
                }
            }
            Err(_) => break,
        }
    }
    hints.truncate(8);
    hints
}

pub fn err_no_file(path: &str, abs: &Path) -> ToolError {
    ToolError::with_hint(
        "ERR_NOT_FOUND",
        format!("No such file: {path}"),
        json!({ "path": path, "nearestExisting": nearest_siblings(abs.parent().unwrap_or(Path::new("/")), abs) }),
    )
}

pub fn err_no_path_with_siblings(path: &str, abs: &Path) -> ToolError {
    ToolError::with_hint(
        "ERR_NOT_FOUND",
        format!("No such path: {path}"),
        json!({ "path": path, "nearestExisting": nearest_siblings(abs.parent().unwrap_or(Path::new("/")), abs) }),
    )
}

pub fn err_no_path(path: &str) -> ToolError {
    ToolError::with_hint("ERR_NOT_FOUND", format!("No such path: {path}"), json!({ "path": path }))
}

/// sha256-16 + rounded mtime (ms) — fs.mjs digestOf.
pub fn digest_of(abs: &Path) -> Result<Value, ToolError> {
    let meta = fs::metadata(abs)?;
    let bytes = fs::read(abs)?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Ok(json!({ "sha256_16": &sha256_hex(&bytes)[..16], "mtimeMs": mtime }))
}

pub fn mtime_ms(meta: &fs::Metadata) -> Option<u64> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
}

// ---- typed args (each generates its input schema via schemars) --------------

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadArgs {
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    pub path: String,
    #[doc = "1-based line to start from"]
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub offset: Option<u64>,
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub limit: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadManyArgs {
    #[schemars(length(min = 1, max = 50))]
    pub paths: Vec<String>,
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub limit: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteArgs {
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    pub path: String,
    pub content: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteManyArgs {
    #[schemars(length(min = 1, max = 50))]
    pub files: Vec<WriteFileArgs>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteFileArgs {
    pub path: String,
    pub content: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CopyArgs {
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    pub from: String,
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    pub to: String,
    #[serde(default)]
    pub recursive: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListArgs {
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub recursive: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PathArgs {
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    pub path: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MkdirArgs {
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    pub path: String,
    #[serde(default)]
    pub recursive: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteArgs {
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    pub path: String,
    #[serde(default)]
    pub recursive: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MoveArgs {
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    pub from: String,
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    pub to: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmptyArgs {}

// ---- handlers ---------------------------------------------------------------

pub struct ReadHandler;
impl Handler for ReadHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: ReadArgs = parse_args(args)?;
        read_impl(k, &a.path, a.offset, a.limit)
    }
}

pub fn read_impl(k: &Kernel, path: &str, offset: Option<u64>, limit: Option<u64>) -> Result<Value, ToolError> {
    let abs = resolve_checked(&k.root, path)?;
    let meta = fs::metadata(&abs).map_err(|_| err_no_file(path, &abs))?;
    if meta.is_dir() {
        return Err(ToolError::with_hint(
            "ERR_IS_DIRECTORY",
            format!("{path} is a directory; use fs.list"),
            json!({ "path": path }),
        ));
    }
    let raw = fs::read_to_string(&abs).map_err(|e| ToolError::new("ERR_INTERNAL", e.to_string()))?;
    let lines: Vec<&str> = raw.split('\n').collect();
    let total_lines = lines.len() as u64;
    let off = offset.unwrap_or(1).max(1).saturating_sub(1);
    let lim = limit.unwrap_or(k.cfg.limits.read_limit as u64);
    let start = (off as usize).min(lines.len());
    let end = (start + lim as usize).min(lines.len());
    let slice = &lines[start..end];
    let start_num = start as u64 + 1;
    let numbered: Vec<String> = slice
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{:>6}\t{}", start_num + i as u64, l))
        .collect();
    let truncated = (start as u64 + slice.len() as u64) < total_lines;
    let next_offset = if truncated { Some(start_num + slice.len() as u64) } else { None };
    Ok(json!({
        "content": numbered.join("\n"),
        "totalLines": total_lines,
        "truncated": truncated,
        "nextOffset": next_offset,
        "digest": digest_of(&abs)?,
    }))
}

pub struct ReadManyHandler;
impl Handler for ReadManyHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: ReadManyArgs = parse_args(args)?;
        if a.paths.is_empty() {
            return Err(ToolError::new("ERR_BAD_INPUT", "paths must be a non-empty array"));
        }
        if a.paths.len() > k.cfg.limits.fs_many {
            return Err(ToolError::new(
                "ERR_BAD_INPUT",
                format!("max {} paths per fs.readMany call", k.cfg.limits.fs_many),
            ));
        }
        let mut files = Vec::new();
        for p in &a.paths {
            match read_impl(k, p, None, a.limit) {
                Ok(mut v) => {
                    if let Value::Object(m) = &mut v {
                        m.insert("path".into(), json!(p));
                        m.insert("ok".into(), json!(true));
                    }
                    files.push(v);
                }
                Err(e) => files.push(json!({ "path": p, "ok": false, "error": e })),
            }
        }
        Ok(json!({ "files": files, "total": files.len() }))
    }
}

/// write body shared with fs.writeMany — every write is validated + journaled
/// individually by the kernel.
pub fn write_resolved(path: &str, abs: &Path, content: &str) -> Result<Value, ToolError> {
    let existed = abs.exists();
    if let Some(parent) = abs.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(abs, content).map_err(|e| ToolError::new("ERR_INTERNAL", e.to_string()))?;
    Ok(json!({
        "path": path,
        "bytes": content.len(),
        "created": !existed,
        "overwrote": existed,
    }))
}

pub struct WriteHandler;
impl Handler for WriteHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: WriteArgs = parse_args(args)?;
        let abs = resolve_checked(&k.root, &a.path)?;
        write_resolved(&a.path, &abs, &a.content)
    }
}

pub struct WriteManyHandler;
impl Handler for WriteManyHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: WriteManyArgs = parse_args(args)?;
        if a.files.is_empty() {
            return Err(ToolError::new("ERR_BAD_INPUT", "files must be a non-empty array of {path, content}"));
        }
        if a.files.len() > k.cfg.limits.fs_many {
            return Err(ToolError::new(
                "ERR_BAD_INPUT",
                format!("max {} files per fs.writeMany call", k.cfg.limits.fs_many),
            ));
        }
        let mut results = Vec::new();
        for f in &a.files {
            match resolve_checked(&k.root, &f.path).and_then(|abs| write_resolved(&f.path, &abs, &f.content)) {
                Ok(mut v) => {
                    if let Value::Object(m) = &mut v {
                        m.insert("path".into(), json!(f.path));
                        m.insert("ok".into(), json!(true));
                    }
                    results.push(v);
                }
                Err(e) => results.push(json!({ "path": f.path, "ok": false, "error": e })),
            }
        }
        let written = results.iter().filter(|r| r["ok"] == json!(true)).count();
        let failed = results.len() - written;
        Ok(json!({ "results": results, "written": written, "failed": failed }))
    }
}

pub struct AppendHandler;
impl Handler for AppendHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: WriteArgs = parse_args(args)?;
        let abs = resolve_checked(&k.root, &a.path)?;
        let existed = abs.exists();
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)?;
        }
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&abs)
            .map_err(|e| ToolError::new("ERR_INTERNAL", e.to_string()))?;
        f.write_all(a.content.as_bytes())?;
        Ok(json!({ "path": a.path, "bytes": a.content.len(), "created": !existed, "appended": true }))
    }
}

pub struct CopyHandler;
impl Handler for CopyHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: CopyArgs = parse_args(args)?;
        let from_abs = resolve_checked(&k.root, &a.from)?;
        let to_abs = resolve_checked(&k.root, &a.to)?;
        let meta = fs::metadata(&from_abs).map_err(|_| err_no_path_with_siblings(&a.from, &from_abs))?;
        if meta.is_dir() && !a.recursive.unwrap_or(true) {
            return Err(ToolError::with_hint(
                "ERR_IS_DIRECTORY",
                format!("{} is a directory; pass recursive=true to copy it", a.from),
                json!({ "path": a.from }),
            ));
        }
        if let Some(parent) = to_abs.parent() {
            fs::create_dir_all(parent)?;
        }
        copy_any(&from_abs, &to_abs)?;
        Ok(json!({ "from": a.from, "to": a.to, "copied": true }))
    }
}

/// Recursive copy mirroring Node cpSync {recursive:true} (probed): merge into
/// existing dirs, overwrite existing files, never follow reparse points.
fn copy_any(from: &Path, to: &Path) -> Result<(), ToolError> {
    let meta = fs::symlink_metadata(from).map_err(|e| ToolError::new("ERR_INTERNAL", e.to_string()))?;
    if meta.is_symlink() || is_reparse_point(from) {
        return Ok(()); // skipped, as Node cpSync skips junctions
    }
    if meta.is_dir() {
        fs::create_dir_all(to)?;
        let mut entries: Vec<PathBuf> = fs::read_dir(from)
            .map_err(|e| ToolError::new("ERR_INTERNAL", e.to_string()))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        entries.sort();
        for e in entries {
            let name = e.file_name().unwrap().to_os_string();
            copy_any(&e, &to.join(name))?;
        }
        Ok(())
    } else {
        if to.is_dir() {
            return Err(ToolError::new(
                "ERR_INTERNAL",
                format!("ERR_FS_CP_NON_DIR_TO_DIR: cannot copy {} onto a directory", from.display()),
            ));
        }
        fs::copy(from, to).map_err(|e| ToolError::new("ERR_INTERNAL", e.to_string()))?;
        Ok(())
    }
}

pub struct ListHandler;
impl Handler for ListHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: ListArgs = parse_args(args)?;
        let path_str = a.path.clone().unwrap_or_else(|| ".".to_string());
        let abs = resolve_checked(&k.root, &path_str)?;
        if !abs.exists() {
            return Err(err_no_path(&path_str));
        }
        let mut entries = Vec::new();
        list_walk(&k.root, &abs, 0, a.recursive.unwrap_or(false), k.cfg.limits.list_depth, &mut entries)?;
        Ok(json!({ "entries": entries, "total": entries.len() }))
    }
}

fn list_walk(
    root: &Path,
    dir: &Path,
    depth: usize,
    recursive: bool,
    max_depth: usize,
    out: &mut Vec<Value>,
) -> Result<(), ToolError> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| ToolError::new("ERR_INTERNAL", e.to_string()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    entries.sort();
    for full in entries {
        let name = full.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if name == ".nc-tools" || name == ".git" || name == "node_modules" {
            continue;
        }
        let meta = match fs::metadata(&full) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let is_dir = meta.is_dir();
        out.push(json!({
            "name": name,
            "path": rel_slash(root, &full),
            "type": if is_dir { "dir" } else { "file" },
            "size": if is_dir { Value::Null } else { json!(meta.len()) },
        }));
        if is_dir && recursive && depth < max_depth {
            if is_reparse_point(&full) {
                continue;
            }
            list_walk(root, &full, depth + 1, recursive, max_depth, out)?;
        }
    }
    Ok(())
}

pub struct StatHandler;
impl Handler for StatHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: PathArgs = parse_args(args)?;
        let abs = resolve_checked(&k.root, &a.path)?;
        if !abs.exists() {
            return Ok(json!({ "exists": false, "path": a.path }));
        }
        let meta = fs::metadata(&abs)?;
        let is_dir = meta.is_dir();
        Ok(json!({
            "exists": true,
            "path": a.path,
            "type": if is_dir { "dir" } else { "file" },
            "size": meta.len(),
            "mtimeMs": mtime_ms(&meta),
        }))
    }
}

pub struct MkdirHandler;
impl Handler for MkdirHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: MkdirArgs = parse_args(args)?;
        let abs = resolve_checked(&k.root, &a.path)?;
        let existed = abs.is_dir();
        if a.recursive.unwrap_or(true) {
            fs::create_dir_all(&abs)?;
        } else {
            fs::create_dir(&abs).map_err(|e| ToolError::new("ERR_INTERNAL", e.to_string()))?;
        }
        Ok(json!({ "path": a.path, "created": !existed }))
    }
}

pub struct DeleteHandler;
impl Handler for DeleteHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: DeleteArgs = parse_args(args)?;
        let abs = resolve_checked(&k.root, &a.path)?;
        if abs == k.root || abs == abs_root(&abs) {
            return Err(ToolError::new(
                "ERR_REFUSED",
                "Refusing to delete the base workspace dir or a filesystem root",
            ));
        }
        if !abs.exists() {
            return Err(err_no_path_with_siblings(&a.path, &abs));
        }
        let is_dir = fs::metadata(&abs)?.is_dir();
        if is_dir && !a.recursive.unwrap_or(false) {
            return Err(ToolError::with_hint(
                "ERR_IS_DIRECTORY",
                format!("{} is a directory; pass recursive=true", a.path),
                json!({ "path": a.path }),
            ));
        }
        if is_dir {
            fs::remove_dir_all(&abs)?;
        } else {
            fs::remove_file(&abs)?;
        }
        Ok(json!({ "path": a.path, "deleted": true }))
    }
}

/// Filesystem root of an absolute path (parse(abs).root): "C:\" / "/" etc.
fn abs_root(p: &Path) -> PathBuf {
    let mut root = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                root.push(comp.as_os_str());
            }
            _ => break,
        }
    }
    root
}

pub struct MoveHandler;
impl Handler for MoveHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: MoveArgs = parse_args(args)?;
        let from_abs = resolve_checked(&k.root, &a.from)?;
        let to_abs = resolve_checked(&k.root, &a.to)?;
        if !from_abs.exists() {
            return Err(ToolError::with_hint(
                "ERR_NOT_FOUND",
                format!("No such path: {}", a.from),
                json!({ "path": a.from }),
            ));
        }
        if let Some(parent) = to_abs.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&from_abs, &to_abs).map_err(|e| ToolError::new("ERR_INTERNAL", e.to_string()))?;
        Ok(json!({ "from": a.from, "to": a.to, "moved": true }))
    }
}
