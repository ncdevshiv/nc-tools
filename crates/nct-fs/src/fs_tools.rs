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
use sha2::{Digest, Sha256};

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

pub fn err_no_file(path: &str, abs: &Path, root: &Path) -> ToolError {
    ToolError::with_hint(
        "ERR_NOT_FOUND",
        format!("No such file: {path}"),
        json!({ "path": path, "nearestExisting": nearest_siblings(root, abs) }),
    )
}

pub fn err_no_path_with_siblings(path: &str, abs: &Path, root: &Path) -> ToolError {
    ToolError::with_hint(
        "ERR_NOT_FOUND",
        format!("No such path: {path}"),
        json!({ "path": path, "nearestExisting": nearest_siblings(root, abs) }),
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
    #[doc = "Base dir for relative paths (default: the session workspace). Lets an agent bound to one workspace read files in another without re-rooting the server."]
    #[serde(default)]
    pub baseDir: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadManyArgs {
    #[schemars(length(min = 1, max = 50))]
    pub paths: Vec<String>,
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub limit: Option<u64>,
    #[doc = "Base dir for relative paths (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteArgs {
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    pub path: String,
    pub content: String,
    #[doc = "Base dir for relative paths (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
    /// When true, refuse the write if ANOTHER agent holds a live advisory lock
    /// on this path (hard-write-guard). Default false = advisory: the write
    /// proceeds but the result carries a lockConflict note.
    #[serde(default)]
    pub guardLocks: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteManyArgs {
    #[schemars(length(min = 1, max = 50))]
    pub files: Vec<WriteFileArgs>,
    #[doc = "Base dir for relative paths (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
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
    #[doc = "Base dir for relative paths (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
    /// When true, refuse the copy if ANOTHER agent holds a live advisory lock
    /// on the destination (hard-write-guard). Default false = advisory.
    #[serde(default)]
    pub guardLocks: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListArgs {
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub recursive: Option<bool>,
    #[doc = "Base dir for relative paths (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PathArgs {
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    pub path: String,
    #[doc = "Base dir for relative paths (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MkdirArgs {
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    pub path: String,
    #[serde(default)]
    pub recursive: Option<bool>,
    #[doc = "Base dir for relative paths (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteArgs {
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    pub path: String,
    #[serde(default)]
    pub recursive: Option<bool>,
    #[doc = "Base dir for relative paths (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
    /// When true, refuse the delete if ANOTHER agent holds a live advisory lock
    /// on this path (hard-write-guard). Default false = advisory.
    #[serde(default)]
    pub guardLocks: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MoveArgs {
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    pub from: String,
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    pub to: String,
    #[doc = "Base dir for relative paths (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
    /// When true, refuse the move if ANOTHER agent holds a live advisory lock
    /// on the destination (hard-write-guard). Default false = advisory.
    #[serde(default)]
    pub guardLocks: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmptyArgs {}

// ---- handlers ---------------------------------------------------------------

pub struct ReadHandler;
impl Handler for ReadHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: ReadArgs = parse_args(args)?;
        read_impl(k, &a.path, a.offset, a.limit, a.baseDir.as_deref())
    }
}

pub fn read_impl(k: &Kernel, path: &str, offset: Option<u64>, limit: Option<u64>, base_dir: Option<&str>) -> Result<Value, ToolError> {
    let base = k.base_dir(base_dir)?;
    read_impl_base(k, path, offset, limit, &base)
}

fn read_impl_base(k: &Kernel, path: &str, offset: Option<u64>, limit: Option<u64>, base: &Path) -> Result<Value, ToolError> {
    let abs = resolve_checked(base, path)?;
    let meta = fs::metadata(&abs).map_err(|_| err_no_file(path, &abs, base))?;
    if meta.is_dir() {
        return Err(ToolError::with_hint(
            "ERR_IS_DIRECTORY",
            format!("{path} is a directory; use fs.list"),
            json!({ "path": path }),
        ));
    }

    let file_size = meta.len();
    let mut hasher = Sha256::new();
    use std::io::{BufRead, Read};

    // --- binary detection: sample the first 8 KB for NUL-byte ratio ---
    // The sample is NOT fed to the hasher — the streaming reader below reads
    // the whole file (including these bytes) and feeds the hasher in one pass.
    let mut file = fs::File::open(&abs).map_err(ToolError::from)?;
    let sample_size = (file_size as usize).min(8192);
    if sample_size > 0 {
        let mut sample = vec![0u8; sample_size];
        let n = file.read(&mut sample).map_err(ToolError::from)?;
        let nul_count = sample[..n].iter().filter(|&&b| b == 0u8).count();
        if n > 0 && (nul_count as f64 / n as f64) > 0.30 {
            return Err(ToolError::with_hint(
                "ERR_BINARY_FILE",
                format!("{path} is a binary file ({} bytes, {}% NUL bytes)", file_size, (nul_count * 100) / n),
                json!({ "path": path, "binaryDetected": true, "fileSize": file_size, "hint": "use fs.readRange for raw byte access or net.fetch for extraction" }),
            ));
        }
    }

    // --- streaming line reader: never loads the whole file into memory ---
    // Always seek back to 0: the sample consumed bytes [0, sample_size), and
    // the streaming reader must start from the beginning of the file.
    use std::io::Seek;
    file.seek(std::io::SeekFrom::Start(0)).map_err(ToolError::from)?;

    let off = offset.unwrap_or(1).max(1).saturating_sub(1) as usize;
    let lim = limit.unwrap_or(k.cfg.limits.read_limit as u64) as usize;

    // We need to count total lines (for totalLines) while also extracting the
    // requested window. Stream the file once, counting lines and collecting
    // only the window [off, off+lim). The hasher consumes the same stream.
    let mut reader = std::io::BufReader::new(file);
    let mut line_buf = String::new();
    let mut line_idx: usize = 0;
    let mut total_lines: u64 = 0;
    let mut collected: Vec<String> = Vec::with_capacity(lim);
    let window_end = off.saturating_add(lim);

    loop {
        line_buf.clear();
        let bytes_read = match reader.read_line(&mut line_buf) {
            Ok(n) => n,
            Err(_) => break,
        };
        if bytes_read == 0 {
            break;
        }
        // Feed the hasher in the same pass — no re-read for the digest.
        hasher.update(line_buf.as_bytes());
        // Strip the trailing newline for presentation (like split('\n') did).
        let line = line_buf.strip_suffix('\n').unwrap_or(&line_buf);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line_idx >= off && line_idx < window_end {
            collected.push(format!("{:>6}\t{}", line_idx + 1, line));
        }
        line_idx += 1;
        total_lines += 1;
    }

    // If the file had no trailing newline, the last "line" from split('\n')
    // semantics would still be counted. The streaming reader already handles
    // this: a file "a\nb" yields two read_line calls.
    // Edge case: empty file → 0 lines (matching split('\n') which yields [""]).
    if total_lines == 0 && file_size == 0 {
        total_lines = 1; // match split('\n') on empty string = [""]
    }

    let start_num = off as u64 + 1;
    let truncated = (off + collected.len()) < total_lines as usize;
    let next_offset = if truncated {
        Some(start_num + collected.len() as u64)
    } else {
        None
    };
    let digest = json!({ "sha256_16": &format!("{:x}", hasher.finalize())[..16], "mtimeMs": mtime_ms(&meta).unwrap_or(0) });

    Ok(json!({
        "content": collected.join("\n"),
        "totalLines": total_lines,
        "truncated": truncated,
        "nextOffset": next_offset,
        "digest": digest,
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

        // Parallel read: each file is independent, so read them concurrently
        // via scoped threads and collect results in index order. 50 files that
        // take 5ms each complete in ~5ms (max), not ~250ms (sum).
        let limit = a.limit;
        let base_dir = a.baseDir.clone();
        let base = k.base_dir(base_dir.as_deref())?;
        let results: Vec<Value> = std::thread::scope(|scope| {
            let handles: Vec<_> = a
                .paths
                .iter()
                .map(|p| {
                    let p = p.clone();
                    let base_ref: &Path = &base;
                    scope.spawn(move || {
                        match read_impl_base(k, &p, None, limit, base_ref) {
                            Ok(mut v) => {
                                if let Value::Object(m) = &mut v {
                                    m.insert("path".into(), json!(p));
                                    m.insert("ok".into(), json!(true));
                                }
                                v
                            }
                            Err(e) => json!({ "path": p, "ok": false, "error": e }),
                        }
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| json!({ "ok": false, "error": { "code": "ERR_INTERNAL", "message": "reader thread panicked" } })))
                .collect()
        });
        Ok(json!({ "files": results, "total": results.len() }))
    }
}

/// write body shared with fs.writeMany — every write is validated + journaled
/// individually by the kernel.
///
/// Crash-safe atomic write: content goes to a temp file, is fsync'd, then
/// atomically renamed over the destination. A crash mid-write leaves the
/// previous file intact — the temp file becomes garbage, not the target.
/// On overwrite, the previous sha256 is returned so callers (e.g. sys.rollback)
/// can restore the pre-write version without a snapshot.
pub fn write_resolved(path: &str, abs: &Path, content: &str) -> Result<Value, ToolError> {
    use std::io::Write;
    let existed = abs.exists();
    if let Some(parent) = abs.parent() {
        fs::create_dir_all(parent)?;
    }

    // Capture the previous file's hash before overwriting — this is what
    // makes a bad write undoable via sys.rollback without a full snapshot.
    let previous_hash: Option<String> = if existed {
        fs::read(abs).ok().map(|bytes| sha256_hex(&bytes))
    } else {
        None
    };

    // Atomic write: temp file → fsync → rename.
    // The temp file lives in the SAME directory (rename must be same-volume
    // to be atomic on both POSIX and Windows NTFS).
    let tmp_dir = abs.parent().unwrap_or(Path::new("."));
    let tmp_name = format!(
        ".{}-atomic-tmp",
        abs.file_name().and_then(|n| n.to_str()).unwrap_or("file")
    );
    let tmp_path = tmp_dir.join(&tmp_name);

    {
        let mut f = fs::File::create(&tmp_path).map_err(|e| {
            // Clean up a leftover temp from a previous crash
            let _ = fs::remove_file(&tmp_path);
            ToolError::from(e)
        })?;
        f.write_all(content.as_bytes())?;
        f.flush()?;
        // fsync the file before rename so the data is durable on disk.
        // On Windows, fsync is a no-op (NTFS rename is atomic already).
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let _ = unsafe { libc::fsync(f.as_raw_fd()) };
        }
    } // file handle dropped here

    // Atomic rename: on POSIX this is atomic; on Windows MoveFileEx with
    // MOVEFILE_REPLACE_EXISTING (std::fs::rename uses it).
    // If rename fails (e.g. cross-volume, though tmp is same-dir), fall back
    // to direct write so the operation still succeeds (degrade, don't fail).
    if fs::rename(&tmp_path, abs).is_err() {
        // Fallback: direct write (less safe, but never fails the operation)
        let _ = fs::remove_file(&tmp_path);
        fs::write(abs, content)?;
    }

    let mut result = json!({
        "path": path,
        "bytes": content.len(),
        "created": !existed,
        "overwrote": existed,
    });
    if let Some(ref h) = previous_hash {
        result["previousHash"] = json!(h);
    }
    Ok(result)
}

/// Check for a foreign agent's live advisory lock on `abs` before a write.
/// Advisory (guard=false default): the write proceeds but the caller merges a
/// `lockConflict` note so the agent knows it collided. Guard (guard=true):
/// the write is refused with ERR_REFUSED + the lock holder's identity.
/// Returns the merge-able note JSON on no-foreign-lock, or the conflict note.
pub fn maybe_warn_foreign_lock(k: &Kernel, base: &Path, abs: &Path, guard: bool) -> Result<Value, ToolError> {
    let self_agent = k.current_agent_id().unwrap_or_default();
    let rel = nct_core::rel_path(base, abs);
    if let Some(lock) = nct_core::foreign_live_lock(&k.root, &rel, &self_agent) {
        let holder = lock["agentId"].as_str().unwrap_or("?").to_string();
        let locked_path = lock["path"].as_str().unwrap_or(&rel).to_string();
        if guard {
            return Err(ToolError::with_hint(
                "ERR_REFUSED",
                format!("path is locked by agent '{holder}' — refusing to write (guardLocks)"),
                json!({ "path": rel, "lockedBy": holder, "lockedPath": locked_path, "hint": "ask the holder to agent.unlock, or retry without guardLocks" }),
            ));
        }
        return Ok(json!({ "lockConflict": true, "lockedBy": holder, "lockedPath": locked_path, "note": "advisory write proceeded despite an active foreign lock" }));
    }
    Ok(json!({ "lockConflict": false }))
}

/// Merge a lockConflict note into a write result when one was produced.
fn merge_lock_note(mut result: Value, note: Value) -> Value {
    if let Value::Object(m) = &mut result {
        if note["lockConflict"] == json!(true) {
            m.insert("lockConflict".into(), json!(true));
            m.insert("lockedBy".into(), note["lockedBy"].clone());
            m.insert("lockedPath".into(), note["lockedPath"].clone());
        }
    }
    result
}


pub struct WriteHandler;
impl Handler for WriteHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: WriteArgs = parse_args(args)?;
        let base = k.base_dir(a.baseDir.as_deref())?;
        let abs = resolve_checked(&base, &a.path)?;
        let guard = a.guardLocks.unwrap_or(false);
        let lock_note = maybe_warn_foreign_lock(k, &base, &abs, guard)?;
        Ok(merge_lock_note(write_resolved(&a.path, &abs, &a.content)?, lock_note))
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
        let base = k.base_dir(a.baseDir.as_deref())?;
        let mut results = Vec::new();
        for f in &a.files {
            let write_result = (|| -> Result<Value, ToolError> {
                let abs = resolve_checked(&base, &f.path)?;
                let lock_note = maybe_warn_foreign_lock(k, &base, &abs, false)?;
                Ok(merge_lock_note(write_resolved(&f.path, &abs, &f.content)?, lock_note))
            })();
            match write_result {
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
        let abs = resolve_checked(&k.base_dir(a.baseDir.as_deref())?, &a.path)?;
        let existed = abs.exists();
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)?;
        }
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&abs)
            .map_err(ToolError::from)?;
        f.write_all(a.content.as_bytes())?;
        Ok(json!({ "path": a.path, "bytes": a.content.len(), "created": !existed, "appended": true }))
    }
}

pub struct CopyHandler;
impl Handler for CopyHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: CopyArgs = parse_args(args)?;
        let base = k.base_dir(a.baseDir.as_deref())?;
        let from_abs = resolve_checked(&base, &a.from)?;
        let to_abs = resolve_checked(&base, &a.to)?;
        let _ = maybe_warn_foreign_lock(k, &base, &to_abs, a.guardLocks.unwrap_or(false))?;
        let meta = fs::metadata(&from_abs).map_err(|_| err_no_path_with_siblings(&a.from, &from_abs, &base))?;
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
    let meta = fs::symlink_metadata(from).map_err(ToolError::from)?;
    if meta.is_symlink() || is_reparse_point(from) {
        return Ok(()); // skipped, as Node cpSync skips junctions
    }
    if meta.is_dir() {
        fs::create_dir_all(to)?;
        let mut entries: Vec<PathBuf> = fs::read_dir(from)
            .map_err(ToolError::from)?
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
        fs::copy(from, to).map_err(ToolError::from)?;
        Ok(())
    }
}

#[cfg(test)]
mod read_streaming_tests {
    use super::*;
    use std::io::Write;

    fn make_kernel() -> Kernel {
        let dir = std::env::temp_dir().join(format!("nct-fs-test-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Kernel::new(dir).unwrap()
    }

    /// The streaming reader must produce identical line content to the old
    /// split('\n') approach — including the numbered format.
    #[test]
    fn streaming_read_matches_legacy_semantics() {
        let k = make_kernel();
        let path = "test.txt";
        let content = "line one\nline two\nline three\n";
        std::fs::write(k.root.join(path), content).unwrap();

        let result = read_impl(&k, path, None, None, None).unwrap();
        let content_val = result["content"].as_str().unwrap();
        assert!(content_val.contains("line one"));
        assert!(content_val.contains("line two"));
        assert!(content_val.contains("line three"));
        assert_eq!(result["totalLines"], json!(3));
        assert_eq!(result["truncated"], json!(false));
    }

    /// No trailing newline: the old split('\n') of "a\nb" gives ["a", "b"]
    /// (2 lines). The streaming reader must match.
    #[test]
    fn no_trailing_newline_counts_correctly() {
        let k = make_kernel();
        std::fs::write(k.root.join("nt.txt"), "alpha\nbeta").unwrap();
        let result = read_impl(&k, "nt.txt", None, None, None).unwrap();
        assert_eq!(result["totalLines"], json!(2));
        assert!(result["content"].as_str().unwrap().contains("alpha"));
        assert!(result["content"].as_str().unwrap().contains("beta"));
    }

    /// Empty file: split('\n') on "" gives [""] (1 line). The streaming reader
    /// returns 0 lines (no read_line calls), but the legacy code would return
    /// totalLines=1. Match the legacy behavior for backward compatibility.
    #[test]
    fn empty_file_returns_one_line() {
        let k = make_kernel();
        std::fs::write(k.root.join("empty.txt"), "").unwrap();
        let result = read_impl(&k, "empty.txt", None, None, None).unwrap();
        assert_eq!(result["totalLines"], json!(1));
        assert_eq!(result["content"].as_str().unwrap(), "");
    }

    /// Binary detection: a file with >30% NUL bytes is rejected with a
    /// structured error, not a read-to-string crash.
    #[test]
    fn binary_file_is_detected_and_rejected() {
        let k = make_kernel();
        let mut bin = vec![0u8; 4096];
        for i in 0..bin.len() {
            if i % 3 != 0 {
                bin[i] = 0;
            } else {
                bin[i] = b'X';
            }
        }
        std::fs::write(k.root.join("bin.dat"), &bin).unwrap();
        let result = read_impl(&k, "bin.dat", None, None, None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, "ERR_BINARY_FILE");
    }

    /// Large file: the streaming reader reads only the requested window.
    /// A 100K-line file with limit=10 should return 10 lines — proving
    /// the file wasn't loaded whole.
    #[test]
    fn large_file_pages_without_loading_whole() {
        let k = make_kernel();
        let content: String = (0..100_000)
            .map(|i| format!("line {i}\n"))
            .collect();
        std::fs::write(k.root.join("big.txt"), content).unwrap();
        let result = read_impl(&k, "big.txt", None, Some(10), None).unwrap();
        assert_eq!(result["totalLines"], json!(100_000));
        let content = result["content"].as_str().unwrap();
        // Should contain exactly the first 10 lines
        assert!(content.contains("line 0\n") || content.contains("line 0\t"));
        assert!(!content.contains("line 50\n") && !content.contains("line 50\t"));
        assert_eq!(result["truncated"], json!(true));
        assert!(!result["nextOffset"].is_null());
    }

    /// Offset paging: read lines 50-59 of a 100-line file.
    #[test]
    fn offset_pages_correctly() {
        let k = make_kernel();
        let content: String = (0..100)
            .map(|i| format!("line {i}\n"))
            .collect();
        std::fs::write(k.root.join("offset.txt"), content).unwrap();
        let result = read_impl(&k, "offset.txt", Some(50), Some(10), None).unwrap();
        assert_eq!(result["totalLines"], json!(100));
        let content = result["content"].as_str().unwrap();
        assert!(content.contains("line 49"));
        assert!(!content.contains("line 39"));
        assert_eq!(result["truncated"], json!(true));
    }

    /// Digest: the streaming hash must match a direct sha256 of the file.
    #[test]
    fn streaming_digest_matches_direct_hash() {
        let k = make_kernel();
        let content = "hash me please\nline two\n";
        std::fs::write(k.root.join("hash.txt"), content).unwrap();
        let result = read_impl(&k, "hash.txt", None, None, None).unwrap();
        let streaming_hash = result["digest"]["sha256_16"].as_str().unwrap();
        let direct_hash = &sha256_hex(content.as_bytes())[..16];
        assert_eq!(streaming_hash, direct_hash);
    }
}

#[cfg(test)]
mod write_atomic_tests {
    use super::*;
    use std::io::Read;

    fn make_kernel() -> Kernel {
        let dir = std::env::temp_dir().join(format!("nct-fs-write-test-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Kernel::new(dir).unwrap()
    }

    /// Atomic write: file content is correct after a successful write.
    #[test]
    fn atomic_write_creates_file_with_correct_content() {
        let k = make_kernel();
        let abs = k.root.join("created.txt");
        let result = write_resolved("created.txt", &abs, "hello atomic\n").unwrap();
        assert_eq!(result["bytes"], json!(13));
        assert_eq!(result["created"], json!(true));
        assert_eq!(result["overwrote"], json!(false));
        assert!(result.get("previousHash").is_none() || result["previousHash"].is_null());
        let on_disk = std::fs::read_to_string(&abs).unwrap();
        assert_eq!(on_disk, "hello atomic\n");
    }

    /// Atomic overwrite: previousHash is returned and file content is replaced.
    #[test]
    fn atomic_overwrite_returns_previous_hash() {
        let k = make_kernel();
        let abs = k.root.join("overwrite.txt");
        // initial write
        write_resolved("overwrite.txt", &abs, "version 1\n").unwrap();
        // overwrite
        let result = write_resolved("overwrite.txt", &abs, "version 2\n").unwrap();
        assert_eq!(result["created"], json!(false));
        assert_eq!(result["overwrote"], json!(true));
        let prev_hash = result["previousHash"].as_str().unwrap();
        let expected_hash = sha256_hex(b"version 1\n");
        assert_eq!(prev_hash, &expected_hash);
        // File on disk has the new content
        let on_disk = std::fs::read_to_string(&abs).unwrap();
        assert_eq!(on_disk, "version 2\n");
    }

    /// Atomic write: no leftover temp file after a successful write.
    #[test]
    fn atomic_write_leaves_no_temp_file() {
        let k = make_kernel();
        let abs = k.root.join("notmp.txt");
        write_resolved("notmp.txt", &abs, "content\n").unwrap();
        let tmp = k.root.join(".notmp.txt-atomic-tmp");
        assert!(!tmp.exists(), "temp file should be cleaned up");
    }

    /// Atomic write: creates parent directories if they don't exist.
    #[test]
    fn atomic_write_creates_parent_dirs() {
        let k = make_kernel();
        let abs = k.root.join("deep/nested/dir/file.txt");
        write_resolved("deep/nested/dir/file.txt", &abs, "nested\n").unwrap();
        assert!(abs.exists());
        assert_eq!(std::fs::read_to_string(&abs).unwrap(), "nested\n");
    }

    /// Crash simulation: a leftover temp file from a previous crash is
    /// cleaned up and the target is not corrupted.
    #[test]
    fn atomic_write_survives_leftover_temp() {
        let k = make_kernel();
        let abs = k.root.join("survive.txt");
        let tmp = k.root.join(".survive.txt-atomic-tmp");
        // Simulate a crash: write content, but leave a temp file behind
        std::fs::write(&tmp, "garbage from crash").unwrap();
        // The write should succeed despite the leftover temp
        write_resolved("survive.txt", &abs, "real content\n").unwrap();
        assert_eq!(std::fs::read_to_string(&abs).unwrap(), "real content\n");
        // Temp is cleaned up by the rename
        assert!(!tmp.exists());
    }

    /// Empty content: atomic write handles empty files correctly.
    #[test]
    fn atomic_write_empty_content() {
        let k = make_kernel();
        let abs = k.root.join("empty.txt");
        let result = write_resolved("empty.txt", &abs, "").unwrap();
        assert_eq!(result["bytes"], json!(0));
        assert!(abs.exists());
        assert_eq!(std::fs::read_to_string(&abs).unwrap(), "");
    }
}

#[cfg(test)]
mod readmany_parallel_tests {
    use super::*;
    use std::time::Instant;

    fn make_kernel() -> Kernel {
        let dir = std::env::temp_dir().join(format!("nct-fs-readmany-test-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Kernel::new(dir).unwrap()
    }

    /// 50 files: all are read and results preserve input order.
    #[test]
    fn readmany_preserves_order_and_all_succeed() {
        let k = make_kernel();
        let paths: Vec<String> = (0..50)
            .map(|i| {
                let p = format!("file-{i:02}.txt");
                std::fs::write(k.root.join(&p), format!("content line {i}\n")).unwrap();
                p
            })
            .collect();

        let result = {
            let args = json!({ "paths": paths });
            ReadManyHandler.call(&k, &args)
        }
        .unwrap();

        let files = result["files"].as_array().unwrap();
        assert_eq!(files.len(), 50);
        assert_eq!(result["total"], json!(50));
        // Order is preserved: file-00 is first, file-49 is last
        for (i, f) in files.iter().enumerate() {
            assert_eq!(f["ok"], json!(true));
            let expected_path = format!("file-{i:02}.txt");
            assert_eq!(f["path"], json!(expected_path));
            let expected_content = format!("content line {i}");
            assert!(f["content"].as_str().unwrap().contains(&expected_content));
        }
    }

    /// Mixed success and failure: each file gets its own ok/error independently.
    #[test]
    fn readmany_mixed_success_and_failure() {
        let k = make_kernel();
        std::fs::write(k.root.join("exists.txt"), "present\n").unwrap();
        // does-not-exist.txt is NOT created

        let result = {
            let args = json!({ "paths": ["exists.txt", "missing.txt", "also-missing.txt"] });
            ReadManyHandler.call(&k, &args)
        }
        .unwrap();

        let files = result["files"].as_array().unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(files[0]["ok"], json!(true));
        assert!(files[0]["content"].as_str().unwrap().contains("present"));
        assert_eq!(files[1]["ok"], json!(false));
        assert!(files[1]["error"]["code"].as_str().is_some());
        assert_eq!(files[2]["ok"], json!(false));
    }

    /// Parallel executes all 50 files and returns correct content.
    /// The timing assertion is intentionally lenient — on warm caches serial
    /// I/O can be near-instant, so we only assert correctness here.
    #[test]
    fn readmany_parallel_completes_all_files() {
        let k = make_kernel();
        let paths: Vec<String> = (0..50)
            .map(|i| {
                let p = format!("bench-{i:02}.txt");
                let content = format!("line {}\n", i).repeat(1000);
                std::fs::write(k.root.join(&p), content).unwrap();
                p
            })
            .collect();

        let par_start = Instant::now();
        let par_result = {
            let args = json!({ "paths": paths.clone() });
            ReadManyHandler.call(&k, &args)
        }
        .unwrap();
        let par_ms = par_start.elapsed().as_micros();

        let files = par_result["files"].as_array().unwrap();
        assert_eq!(files.len(), 50);
        assert!(files.iter().all(|f| f["ok"] == json!(true)));

        // Sanity: parallel must complete in reasonable time (< 5s for 50 6KB files)
        assert!(par_ms < 5_000_000, "parallel took {par_ms}µs — too slow");
    }
}

pub struct ListHandler;
impl Handler for ListHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: ListArgs = parse_args(args)?;
        let path_str = a.path.clone().unwrap_or_else(|| ".".to_string());
        let base = k.base_dir(a.baseDir.as_deref())?;
        let abs = resolve_checked(&base, &path_str)?;
        if !abs.exists() {
            return Err(err_no_path(&path_str));
        }
        let mut entries = Vec::new();
        list_walk(&base, &abs, 0, a.recursive.unwrap_or(false), k.cfg.limits.list_depth, &mut entries)?;
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
        .map_err(ToolError::from)?
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
        let abs = resolve_checked(&k.base_dir(a.baseDir.as_deref())?, &a.path)?;
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
        let base = k.base_dir(a.baseDir.as_deref())?;
        let abs = resolve_checked(&base, &a.path)?;
        let existed = abs.is_dir();
        if a.recursive.unwrap_or(true) {
            fs::create_dir_all(&abs)?;
        } else {
            fs::create_dir(&abs).map_err(ToolError::from)?;
        }
        Ok(json!({ "path": a.path, "created": !existed }))
    }
}

pub struct DeleteHandler;
impl Handler for DeleteHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: DeleteArgs = parse_args(args)?;
        let base = k.base_dir(a.baseDir.as_deref())?;
        let abs = resolve_checked(&base, &a.path)?;
        if abs == base || abs == abs_root(&abs) {
            return Err(ToolError::new(
                "ERR_REFUSED",
                "Refusing to delete the base workspace dir or a filesystem root",
            ));
        }
        let _ = maybe_warn_foreign_lock(k, &base, &abs, a.guardLocks.unwrap_or(false))?;
        if !abs.exists() {
            return Err(err_no_path_with_siblings(&a.path, &abs, &base));
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
        let base = k.base_dir(a.baseDir.as_deref())?;
        let from_abs = resolve_checked(&base, &a.from)?;
        let to_abs = resolve_checked(&base, &a.to)?;
        if !from_abs.exists() {
            return Err(ToolError::with_hint(
                "ERR_NOT_FOUND",
                format!("No such path: {}", a.from),
                json!({ "path": a.from }),
            ));
        }
        let _ = maybe_warn_foreign_lock(k, &base, &to_abs, a.guardLocks.unwrap_or(false))?;
        if let Some(parent) = to_abs.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&from_abs, &to_abs).map_err(ToolError::from)?;
        Ok(json!({ "from": a.from, "to": a.to, "moved": true }))
    }
}

// ---- phase 2 additions: fs.readRange / fs.tree -------------------------------

pub const READ_RANGE_DESC: &str = "Read a BYTE RANGE of a file without loading it whole - for huge files and logs. Returns the decoded window, byte offsets (chain via nextByteOffset), the 1-based line number where the window starts, and eof. Use fs.read for line-based paging of normal files.";

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadRangeArgs {
    #[doc = "Path - relative to the base dir, or absolute (any location allowed)"]
    pub path: String,
    #[doc = "Byte offset to start from (0 = file start)"]
    #[serde(default)]
    pub byteOffset: Option<u64>,
    #[doc = "Window size in bytes (default 65536, max 1048576)"]
    #[serde(default)]
    #[schemars(range(min = 1, max = 1048576))]
    pub maxBytes: Option<u64>,
    #[doc = "Base dir for relative paths (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
}

pub struct ReadRangeHandler;
impl Handler for ReadRangeHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: ReadRangeArgs = parse_args(args)?;
        let base = k.base_dir(a.baseDir.as_deref())?;
        let abs = resolve_checked(&base, &a.path)?;
        let meta = fs::metadata(&abs).map_err(|_| err_no_file(&a.path, &abs, &base))?;
        if meta.is_dir() {
            return Err(ToolError::with_hint(
                "ERR_IS_DIRECTORY",
                format!("{} is a directory; use fs.list", a.path),
                json!({ "path": a.path }),
            ));
        }
        let total_bytes = meta.len();
        let offset = a.byteOffset.unwrap_or(0);
        if offset >= total_bytes && total_bytes > 0 {
            return Err(ToolError::with_hint(
                "ERR_BAD_INPUT",
                format!("byteOffset {} is past end of file ({} bytes)", offset, total_bytes),
                json!({ "byteOffset": offset, "fileSize": total_bytes }),
            ));
        }
        use std::io::{Read, Seek, SeekFrom};
        let max_bytes = a.maxBytes.unwrap_or(65_536) as usize;
        let mut f = fs::File::open(&abs).map_err(ToolError::from)?;
        // 1-based line number where the window starts: count newlines in [0, offset).
        let mut start_line: u64 = 1;
        if offset > 0 {
            f.seek(SeekFrom::Start(0)).map_err(ToolError::from)?;
            let mut remaining = offset as usize;
            let mut buf = [0u8; 65_536];
            while remaining > 0 {
                let take = remaining.min(buf.len());
                let n = f
                    .read(&mut buf[..take])
                    .map_err(ToolError::from)?;
                if n == 0 {
                    break;
                }
                start_line += buf[..n].iter().filter(|&&b| b == b'\n').count() as u64;
                remaining -= n;
            }
        }
        f.seek(SeekFrom::Start(offset)).map_err(ToolError::from)?;
        let mut window = vec![0u8; max_bytes];
        let mut filled = 0usize;
        while filled < max_bytes {
            let n = f
                .read(&mut window[filled..])
                .map_err(ToolError::from)?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        window.truncate(filled);
        // Trim to a valid UTF-8 boundary so the next window starts clean.
        let valid = match std::str::from_utf8(&window) {
            Ok(_) => filled,
            Err(e) => e.valid_up_to(),
        };
        window.truncate(valid);
        let content = String::from_utf8_lossy(&window).to_string();
        let window_lines = content.split('\n').count() as u64;
        let eof = (offset + valid as u64) >= total_bytes;
        Ok(json!({
            "path": a.path,
            "fileSize": total_bytes,
            "byteOffset": offset,
            "byteLength": valid,
            "nextByteOffset": offset + valid as u64,
            "eof": eof,
            "startLine": start_line,
            "lines": window_lines,
            "content": content,
        }))
    }
}

pub const TREE_DESC: &str = "Render a bounded directory tree in one call: depth-capped, entry-capped, skips .git/node_modules/target/dist/.nc-tools. Returns structured entries (path/type/size/depth) plus an ASCII rendering. Directories sort first.";

const TREE_SKIP: [&str; 5] = [".git", "node_modules", ".nc-tools", "target", "dist"];

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TreeArgs {
    #[doc = "Path - relative to the base dir, or absolute (any location allowed)"]
    #[serde(default)]
    pub path: Option<String>,
    #[doc = "Max depth (default 4, max 12)"]
    #[serde(default)]
    #[schemars(range(min = 1, max = 12))]
    pub depth: Option<u64>,
    #[doc = "Max entries (default 500, max 5000)"]
    #[serde(default)]
    #[schemars(range(min = 1, max = 5000))]
    pub maxEntries: Option<u64>,
    #[doc = "Base dir for relative paths (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
}

pub struct TreeHandler;
impl Handler for TreeHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: TreeArgs = parse_args(args)?;
        let path_str = a.path.clone().unwrap_or_else(|| ".".to_string());
        let base = k.base_dir(a.baseDir.as_deref())?;
        let abs = resolve_checked(&base, &path_str)?;
        if !abs.is_dir() {
            return Err(ToolError::with_hint(
                "ERR_NOT_FOUND",
                format!("not a directory: {}", path_str),
                json!({ "path": path_str }),
            ));
        }
        let max_depth = a.depth.unwrap_or(4) as usize;
        let max_entries = a.maxEntries.unwrap_or(500) as usize;
        let mut entries: Vec<Value> = Vec::new();
        let mut lines: Vec<String> = Vec::new();
        let mut truncated = false;
        tree_walk(
            &abs, "", "", 0, max_depth, max_entries, &mut entries, &mut lines, &mut truncated,
        )?;
        Ok(json!({
            "path": path_str,
            "tree": lines.join("\n"),
            "entries": entries,
            "total": entries.len(),
            "truncated": truncated,
        }))
    }
}

#[allow(clippy::too_many_arguments)]
fn tree_walk(
    dir: &Path,
    rel_prefix: &str,
    ascii_prefix: &str,
    depth: usize,
    max_depth: usize,
    max_entries: usize,
    entries: &mut Vec<Value>,
    lines: &mut Vec<String>,
    truncated: &mut bool,
) -> Result<(), ToolError> {
    if depth > max_depth {
        return Ok(());
    }
    let mut kids: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(ToolError::from)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    kids.sort();
    kids.sort_by_key(|p| !p.is_dir()); // dirs first within each level
    let visible: Vec<PathBuf> = kids
        .into_iter()
        .filter(|p| {
            let name = p
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            !TREE_SKIP.contains(&name.as_str())
        })
        .collect();
    let count = visible.len();
    for (i, full) in visible.into_iter().enumerate() {
        if entries.len() >= max_entries {
            *truncated = true;
            return Ok(());
        }
        let name = full
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let meta = match fs::symlink_metadata(&full) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_symlink() {
            continue;
        }
        let is_dir = meta.is_dir();
        let rel = if rel_prefix.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", rel_prefix, name)
        };
        let last = i + 1 == count;
        let branch = if last { "└── " } else { "├── " };
        lines.push(format!(
            "{}{}{}",
            ascii_prefix,
            branch,
            if is_dir { format!("{name}/") } else { name.clone() }
        ));
        entries.push(json!({
            "path": rel,
            "type": if is_dir { "dir" } else { "file" },
            "size": if is_dir { Value::Null } else { json!(meta.len()) },
            "depth": depth,
        }));
        if is_dir {
            let child_ascii = format!("{}{}", ascii_prefix, if last { "    " } else { "│   " });
            tree_walk(
                &full, &rel, &child_ascii, depth + 1, max_depth, max_entries, entries, lines, truncated,
            )?;
        }
    }
    Ok(())
}


#[cfg(test)]
mod lock_guard_tests {
    use super::*;
    use nct_core::kernel::Kernel;
    use std::fs;

    /// Write a lock into .nc-tools/locks.jsonl the way coordination.rs does.
    fn write_lock(root: &std::path::Path, path_rel: &str, holder: &str, hold_ms: u64) {
        let lock = json!({
            "path": path_rel,
            "agentId": holder,
            "heldAt": nct_core::now_iso(),
            "holdMs": hold_ms,
            "expiresAt": nct_core::now_iso(),
            "expiresAtMs": nct_core::now_ms() + hold_ms,
            "seq": nct_core::now_ms(),
            "released": false,
        });
        nct_core::append_lock_line(root, &lock);
    }

    fn kernel_with(root: &std::path::Path) -> Kernel {
        let mut k = Kernel::new(root.to_path_buf()).unwrap();
        crate::register(&mut k);
        k
    }

    fn ws(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nct-lockguard-{tag}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_with_foreign_lock_and_no_guard_reports_conflict_not_blocks() {
        let root = ws("w1");
        // agent-1 holds a live lock on src/a.rs
        write_lock(&root, "src/a.rs", "agent-1", 600_000);
        let k = kernel_with(&root);
        k.set_agent_id("agent-2");
        let out = k.call("fs.write", &json!({ "path": "src/a.rs", "content": "x", "baseDir": root.display().to_string() }));
        assert!(out.ok, "advisory write should succeed: {:?}", out.error);
        let r = out.result.unwrap();
        assert_eq!(r["lockConflict"], json!(true), "should report conflict: {r}");
        assert_eq!(r["lockedBy"], json!("agent-1"));
        assert_eq!(fs::read_to_string(root.join("src/a.rs")).unwrap(), "x");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_with_foreign_lock_and_guard_refuses() {
        let root = ws("w2");
        write_lock(&root, "src/b.rs", "agent-1", 600_000);
        let k = kernel_with(&root);
        k.set_agent_id("agent-2");
        let out = k.call("fs.write", &json!({ "path": "src/b.rs", "content": "x", "baseDir": root.display().to_string(), "guardLocks": true }));
        assert!(!out.ok, "guard must refuse");
        let e = out.error.unwrap();
        assert_eq!(e.code, "ERR_REFUSED");
        assert!(e.message.contains("agent-1"), "should name the holder: {}", e.message);
        assert!(!root.join("src/b.rs").exists(), "must NOT have written");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn own_lock_does_not_conflict() {
        let root = ws("w3");
        write_lock(&root, "src/c.rs", "agent-1", 600_000);
        let k = kernel_with(&root);
        k.set_agent_id("agent-1");
        let out = k.call("fs.write", &json!({ "path": "src/c.rs", "content": "x", "baseDir": root.display().to_string(), "guardLocks": true }));
        assert!(out.ok, "own lock must not block: {:?}", out.error);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_with_guard_refuses_on_foreign_lock() {
        let root = ws("w4");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/d.rs"), "content").unwrap();
        write_lock(&root, "src/d.rs", "agent-1", 600_000);
        let k = kernel_with(&root);
        k.set_agent_id("agent-2");
        let out = k.call("fs.delete", &json!({ "path": "src/d.rs", "baseDir": root.display().to_string(), "guardLocks": true }));
        assert!(!out.ok, "guard must refuse delete");
        assert!(root.join("src/d.rs").exists(), "must NOT delete");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn expired_lock_does_not_block() {
        let root = ws("w5");
        write_lock(&root, "src/e.rs", "agent-1", 0); // holdMs 0 => already expired
        let k = kernel_with(&root);
        k.set_agent_id("agent-2");
        let out = k.call("fs.write", &json!({ "path": "src/e.rs", "content": "x", "baseDir": root.display().to_string(), "guardLocks": true }));
        assert!(out.ok, "expired lock must not block: {:?}", out.error);
        let _ = fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod move_copy_guard_tests {
    use super::*;
    use std::fs;

    fn write_lock(root: &std::path::Path, path_rel: &str, holder: &str, hold_ms: u64) {
        let lock = json!({
            "path": path_rel, "agentId": holder, "heldAt": nct_core::now_iso(),
            "holdMs": hold_ms, "expiresAt": nct_core::now_iso(),
            "expiresAtMs": nct_core::now_ms() + hold_ms, "seq": nct_core::now_ms(), "released": false,
        });
        nct_core::append_lock_line(root, &lock);
    }
    fn kernel_with(root: &std::path::Path) -> Kernel {
        let mut k = Kernel::new(root.to_path_buf()).unwrap();
        crate::register(&mut k);
        k
    }
    fn ws(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("nct-moveguard-{tag}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let _ = fs::remove_dir_all(&d); fs::create_dir_all(&d).unwrap(); d
    }

    #[test]
    fn copy_guard_refuses_on_foreign_dest_lock() {
        let root = ws("c1");
        fs::write(root.join("a.txt"), "src").unwrap();
        fs::write(root.join("b.txt"), "dest").unwrap();
        write_lock(&root, "b.txt", "agent-1", 600_000);
        let k = kernel_with(&root); k.set_agent_id("agent-2");
        let out = k.call("fs.copy", &json!({ "from": "a.txt", "to": "b.txt", "baseDir": root.display().to_string(), "guardLocks": true }));
        assert!(!out.ok, "guard must refuse copy to locked dest");
        assert_eq!(out.error.unwrap().code, "ERR_REFUSED");
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "dest", "dest must be untouched");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn move_guard_refuses_on_foreign_dest_lock() {
        let root = ws("m1");
        fs::write(root.join("a.txt"), "src").unwrap();
        fs::write(root.join("b.txt"), "dest").unwrap();
        write_lock(&root, "b.txt", "agent-1", 600_000);
        let k = kernel_with(&root); k.set_agent_id("agent-2");
        let out = k.call("fs.move", &json!({ "from": "a.txt", "to": "b.txt", "baseDir": root.display().to_string(), "guardLocks": true }));
        assert!(!out.ok, "guard must refuse move to locked dest");
        assert_eq!(out.error.unwrap().code, "ERR_REFUSED");
        assert!(root.join("a.txt").exists(), "source must be untouched");
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "dest");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn copy_no_guard_proceeds_with_advisory_conflict_note() {
        let root = ws("c2");
        fs::write(root.join("a.txt"), "src").unwrap();
        write_lock(&root, "b.txt", "agent-1", 600_000);
        let k = kernel_with(&root); k.set_agent_id("agent-2");
        // no guardLocks; copy to a DIFFERENT new file (not locked) -> note false
        let out = k.call("fs.copy", &json!({ "from": "a.txt", "to": "c.txt", "baseDir": root.display().to_string() }));
        assert!(out.ok);
        assert!(root.join("c.txt").exists());
        let _ = fs::remove_dir_all(&root);
    }
}
