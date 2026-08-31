// Append-only JSONL journal — mirrors src/kernel/journal.mjs.
// Cross-process safe: parallel agents share one journal file; writes are
// serialized with an exclusive lock file so no line is ever torn interleaved.
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::errors::ToolError;

/// Exclusive-create lock file held for the duration of one append; bounded
/// retry (5s) exactly like the JS `withLock`.
struct JournalLock<'a> {
    path: &'a Path,
}

impl<'a> JournalLock<'a> {
    fn acquire(path: &'a Path) -> Result<JournalLock<'a>, ToolError> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(5000);
        loop {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(_) => return Ok(JournalLock { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if std::time::Instant::now() > deadline {
                        return Err(ToolError::new(
                            "ERR_INTERNAL",
                            format!("journal lock timeout: {} held too long", path.display()),
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(_) => return Ok(JournalLock { path }), // lock is best-effort
            }
        }
    }
}

impl Drop for JournalLock<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.path);
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct JournalEvent {
    pub ts: String,
    pub seq: u64,
    pub kind: String,
    #[serde(flatten)]
    pub fields: Value,
}

pub struct Journal {
    pub file_path: PathBuf,
    /// Monotonic event sequence, shared across &Kernel callers.
    pub seq: std::sync::Arc<std::sync::Mutex<u64>>,
    lock_path: PathBuf,
}

impl Journal {
    pub fn new(file_path: PathBuf) -> Result<Journal, ToolError> {
        if let Some(dir) = file_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Resume sequence numbering if a journal already exists (continuity).
        let seq = Journal::read_all(&file_path)
            .last()
            .and_then(|l| serde_json::from_str::<Value>(l).ok())
            .and_then(|v| v.get("seq").and_then(|s| s.as_u64()))
            .unwrap_or(0);
        let lock_path = file_path.parent().unwrap_or(Path::new(".")).join(".journal.lock");
        Ok(Journal { file_path, seq: std::sync::Arc::new(std::sync::Mutex::new(seq)), lock_path })
    }

    /// Append one event; returns the event as written. Serialized across
    /// processes via the exclusive lock file.
    pub fn append(&self, kind: &str, fields: Value) -> Result<JournalEvent, ToolError> {
        // interior mutability: the kernel hands out &Kernel; seq advances
        // atomically under the same mutex that guards concurrent callers
        let mut seq = self.seq.lock().unwrap();
        *seq += 1;
        let event = JournalEvent {
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            seq: *seq,
            kind: kind.to_string(),
            fields,
        };
        let line = serde_json::to_string(&event)? + "\n";
        let _lock = JournalLock::acquire(&self.lock_path)?;
        let mut f = OpenOptions::new().create(true).append(true).open(&self.file_path)?;
        f.write_all(line.as_bytes())?;
        Ok(event)
    }

    pub fn read_all(file_path: &Path) -> Vec<String> {
        let mut f = match File::open(file_path) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };
        let mut buf = String::new();
        if f.read_to_string(&mut buf).is_err() {
            return Vec::new();
        }
        buf.lines().filter(|l| !l.is_empty()).map(|l| l.to_string()).collect()
    }

    /// Parsed events, newest last. Corrupt lines are skipped (JSON journal reader).
    pub fn events(&self) -> Vec<Value> {
        Journal::read_all(&self.file_path)
            .iter()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect()
    }

    /// Last N events (reads the whole file — same behavior as journal.mjs lastN;
    /// the file is a session log, not a dataset).
    pub fn last_n(&self, n: Option<usize>) -> Vec<Value> {
        let all = self.events();
        match n {
            Some(n) => all.into_iter().rev().take(n).rev().collect(),
            None => all,
        }
    }

    /// Seek-backed tail read: the last `n` parsed events without building the
    /// full vector — used by the MCP server's sys.journal.
    pub fn tail(&self, n: usize) -> Vec<Value> {
        let mut f = match File::open(&self.file_path) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };
        let len = match f.metadata() {
            Ok(m) => m.len(),
            Err(_) => return Vec::new(),
        };
        // read last 256 KiB window — enough for 1000 events in practice
        let window = len.min(256 * 1024);
        let start = len - window;
        if f.seek(SeekFrom::Start(start)).is_err() {
            return Vec::new();
        }
        let mut buf = String::new();
        if f.read_to_string(&mut buf).is_err() {
            return Vec::new();
        }
        let lines: Vec<&str> = buf.lines().filter(|l| !l.is_empty()).collect();
        let take_from = lines.len().saturating_sub(n);
        lines[take_from..]
            .iter()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect()
    }
}
