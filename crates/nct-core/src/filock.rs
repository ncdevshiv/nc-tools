// Cross-process exclusive file lock by existence. The lock IS the presence of
// a lock file — no kernel handle needs to stay open, so it works between
// separate server processes the way a POSIX `flock` or a Windows named mutex
// would, on the plain `std::fs` available to this crate.
//
// Acquire is `create_new`: exactly one waiter succeeds, the rest poll until the
// deadline and then fail with a typed error. Release is `remove_file` in `Drop`.
// A crashed holder does NOT self-heal — `Drop` never runs, and the lock file
// stays on disk until something deletes it. The owner marker written into the
// lock file (pid + timestamp) is what makes a stale lock diagnosable; see the
// `holder` field of the timeout error.
//
// Every `Err` from `create_new` is treated as "not acquired". An OS error
// (a transient scan lock, a read-only parent, an 8.3 short-name clash) is
// retried through the deadline and then fails typed — never upgraded into a
// lock the caller does not hold, because callers treat `Ok(_)` as ownership of
// the critical section and a fake lock silently un-serializes the append.
//
// Used by the journal (append serialization) and by the coordination layer
// (roster/lock/message read-modify-append) — two places that previously each
// hand-rolled the same create-new + sleep loop.
use std::path::{Path, PathBuf};

use crate::errors::{codes, ToolError};

/// Default acquire budget: 5s, matching the journal's original timeout. Long
/// enough to cover a slow append, short enough to stay well inside the 30s MCP
/// call ceiling a calling agent is bounded by.
pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;

/// Poll interval while waiting on a contended lock. 5ms keeps the CPU cost of
/// a busy waiter negligible while still draining a fast handoff quickly.
const POLL_MS: u64 = 5;

#[derive(Debug)]
pub struct FileLock {
    path: PathBuf,
}

impl FileLock {
    /// Acquire the exclusive lock at `path`, waiting up to `timeout_ms`.
    ///
    /// Every `Ok` return is a real exclusive lock. A persistent OS error fails
    /// with `ERR_TIMEOUT` instead of pretending the critical section is ours —
    /// the callers (journal, coordination, doctor) append inside it and would
    /// otherwise race each other.
    pub fn acquire(path: &Path, timeout_ms: u64) -> Result<FileLock, ToolError> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(mut f) => {
                    // The lock is already held by the create; a marker failure
                    // only costs the diagnostic, never the critical section.
                    let _ = write_owner(&mut f, path);
                    return Ok(FileLock {
                        path: path.to_path_buf(),
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if std::time::Instant::now() > deadline {
                        return Err(lock_timeout(path, timeout_ms));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
                }
                Err(e) => {
                    if std::time::Instant::now() > deadline {
                        return Err(ToolError::with_hint(
                            codes::TIMEOUT,
                            format!(
                                "could not acquire lock {} after {timeout_ms}ms: {e}",
                                path.display()
                            ),
                            serde_json::json!({
                                "lock": path.display().to_string(),
                                "waitedMs": timeout_ms,
                                "osError": e.to_string(),
                                "hint": "the filesystem refused create_new repeatedly (a scan lock, a read-only parent, a short-name clash). The critical section was NOT entered, so nothing was written without a lock — retry, or fix the filesystem state"
                            }),
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
                }
            }
        }
    }

    /// Acquire without waiting — `Ok(None)` when someone already holds it.
    /// For callers that would rather see "busy" than block.
    pub fn try_acquire(path: &Path) -> Result<Option<FileLock>, ToolError> {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut f) => {
                let _ = write_owner(&mut f, path);
                Ok(Some(FileLock {
                    path: path.to_path_buf(),
                }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(e) => Err(ToolError::with_hint(
                codes::TIMEOUT,
                format!("could not acquire lock {}: {}", path.display(), e),
                serde_json::json!({
                    "lock": path.display().to_string(),
                    "hint": "the filesystem refused create_new; the lock was NOT taken"
                }),
            )),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Drop the lock file eagerly rather than at `Drop`. Lets a caller commit
    /// the lock release at a specific point instead of relying on scope exit.
    /// Idempotent — calling it twice is a no-op.
    pub fn release(&mut self) {
        self.remove_file_if_owned();
    }

    fn remove_file_if_owned(&mut self) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        let _ = std::fs::remove_file(&self.path);
        self.path = PathBuf::new();
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        self.remove_file_if_owned();
    }
}

/// The lock-file path paired with a data file: `foo.jsonl` -> `foo.jsonl.lock`.
/// Keeps every lock beside the file it guards, so a workspace directory is a
/// self-contained picture of its own critical sections.
pub fn lock_path_for(data_path: &Path) -> PathBuf {
    // `with_extension` inserts a separator dot, so a leading dot in the new
    // extension would double it (`plain` + `.lock` -> `plain..lock`). Extensionless
    // paths get a plain `lock` suffix instead.
    match data_path.extension().and_then(|e| e.to_str()) {
        Some(ext) => data_path.with_extension(format!("{ext}.lock")),
        None => data_path.with_extension("lock"),
    }
}

/// What the holder recorded in the lock file, so a lock left behind by a
/// crash is a diagnosis instead of a mystery timeout. Empty for locks taken
/// before this marker existed, or when the write itself failed.
fn holder_hint(path: &Path) -> serde_json::Value {
    match std::fs::read_to_string(path) {
        Ok(raw) if !raw.trim().is_empty() => serde_json::json!(raw.trim().to_string()),
        _ => serde_json::Value::Null,
    }
}

fn lock_timeout(path: &Path, timeout_ms: u64) -> ToolError {
    ToolError::with_hint(
        codes::TIMEOUT,
        format!(
            "lock timeout: {} held for over {timeout_ms}ms",
            path.display()
        ),
        serde_json::json!({
            "lock": path.display().to_string(),
            "waitedMs": timeout_ms,
            "holder": holder_hint(path),
            "hint": "another process holds this file's critical section. Retry; if that process crashed it left this lock file behind and deleting it is the recovery"
        }),
    )
}

/// Write the owner marker after winning `create_new`. The lock is already held
/// at that point, so a failure here only loses the diagnostic.
fn write_owner(f: &mut std::fs::File, path: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let body = format!(
        "pid={} acquiredAt={} guards={}\n",
        std::process::id(),
        crate::helpers::now_iso(),
        path.display()
    );
    f.write_all(body.as_bytes())?;
    f.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "nct-filock-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn lock_file_naming() {
        assert_eq!(
            lock_path_for(Path::new("agents.jsonl")),
            PathBuf::from("agents.jsonl.lock")
        );
        assert_eq!(
            lock_path_for(Path::new("plain")),
            PathBuf::from("plain.lock")
        );
        // nested paths keep the lock beside the data file
        assert_eq!(
            lock_path_for(Path::new(".nc-tools/agents.jsonl")),
            PathBuf::from(".nc-tools/agents.jsonl.lock")
        );
    }

    #[test]
    fn second_acquirer_blocks_until_release() {
        let dir = temp_dir("serial");
        let p = dir.join("f.jsonl");
        let a = FileLock::acquire(&p, 1000).unwrap();
        let marker = std::fs::read_to_string(&p).unwrap();
        assert!(
            marker.contains("pid="),
            "a winning acquire records its holder: {marker:?}"
        );
        assert!(
            marker.contains("guards="),
            "the marker names the guarded file: {marker:?}"
        );
        let blocked = std::thread::spawn({
            let p = p.clone();
            move || FileLock::acquire(&p, 1000)
        });
        // give the waiter a moment to observe the contention
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(p.exists(), "lock file must be present while held");
        drop(a);
        assert!(
            blocked.join().unwrap().is_ok(),
            "waiter must acquire after release"
        );
        assert!(!p.exists(), "release must remove the lock file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn acquire_times_out_with_a_typed_error() {
        let dir = temp_dir("timeout");
        let p = dir.join("f.jsonl");
        let _held = FileLock::acquire(&p, 1000).unwrap();
        let err = FileLock::acquire(&p, 30).unwrap_err();
        assert_eq!(err.code, "ERR_TIMEOUT");
        let hint = err.hint.as_ref().unwrap();
        assert!(
            hint["lock"].as_str().is_some(),
            "hint names the lock: {err:?}"
        );
        assert!(
            hint["holder"].as_str().is_some(),
            "hint carries the holder marker: {err:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A filesystem that persistently refuses `create_new` must fail the
    /// acquire. Returning a lock the caller does not hold is what let 8 threads
    /// append to one journal without serializing: they each believed they owned
    /// the critical section and two of them computed the same next sequence.
    #[test]
    fn persistent_os_error_fails_instead_of_granting_a_fake_lock() {
        let dir = temp_dir("refused");
        let block = dir.join("not-a-dir");
        std::fs::write(&block, b"blocker").unwrap();
        // The parent is a regular file, so create_new can never succeed and
        // never reports AlreadyExists — this is the non-contention branch.
        let p = block.join("sub.lock");
        let err = FileLock::acquire(&p, 30).unwrap_err();
        assert_eq!(err.code, "ERR_TIMEOUT");
        let hint = err.hint.as_ref().unwrap();
        assert!(
            hint["osError"].as_str().is_some(),
            "hint names the OS error: {err:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn try_acquire_reports_contention_without_waiting() {
        let dir = temp_dir("try");
        let p = dir.join("f.jsonl");
        let first = FileLock::try_acquire(&p).unwrap();
        assert!(first.is_some(), "free file -> lock");
        drop(first.unwrap());
        let held = FileLock::try_acquire(&p).unwrap().unwrap();
        assert!(
            FileLock::try_acquire(&p).unwrap().is_none(),
            "held file -> None"
        );
        drop(held);
        // release must be idempotent — releasing twice cannot blow up
        let mut g = FileLock::try_acquire(&p).unwrap().unwrap();
        g.release();
        g.release();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn release_is_eager_and_scope_safe() {
        let dir = temp_dir("eager");
        let p = dir.join("f.jsonl");
        {
            let mut lock = FileLock::acquire(&p, 1000).unwrap();
            lock.release();
            assert!(!p.exists(), "release must remove the file immediately");
            // still acquirable right after
            assert!(FileLock::try_acquire(&p).unwrap().is_some());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The invariant the journal and coordination layers both depend on:
    /// N contending threads appending under the lock never interleave a torn
    /// line, and every line stays parseable.
    #[test]
    fn concurrent_appends_under_lock_produce_no_torn_lines() {
        let dir = temp_dir("torn");
        let p = dir.join("data.jsonl");
        // The lock guards `p` but lives at `p.lock` — locking `p` itself would
        // make `Drop` delete the data file, which is exactly the failure this
        // test caught on its first run.
        let l = lock_path_for(&p);
        let results: std::sync::Arc<std::sync::Mutex<Vec<u64>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let p = p.clone();
            let l = l.clone();
            let results = results.clone();
            handles.push(std::thread::spawn(move || {
                let _lock = FileLock::acquire(&l, 5000).unwrap();
                let seq = std::fs::read_to_string(&p)
                    .map(|raw| {
                        raw.lines()
                            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                            .filter_map(|v| v["seq"].as_u64())
                            .max()
                            .unwrap_or(0)
                    })
                    .unwrap_or(0)
                    + 1;
                let line = format!(r#"{{"seq":{}, "payload":"{}"}}"#, seq, "x".repeat(200));
                use std::io::Write;
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&p)
                    .unwrap()
                    .write_all((line + "\n").as_bytes())
                    .unwrap();
                results.lock().unwrap().push(seq);
                seq
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let raw = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 8, "every append must land as exactly one line");
        for l in &lines {
            assert!(
                serde_json::from_str::<serde_json::Value>(l).is_ok(),
                "torn line: {l:?}"
            );
        }
        let seqs: std::collections::BTreeSet<_> = results.lock().unwrap().iter().copied().collect();
        assert_eq!(seqs.len(), 8, "seqs must be unique under the lock");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
