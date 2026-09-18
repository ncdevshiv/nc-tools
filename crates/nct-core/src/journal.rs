// Append-only JSONL journal — mirrors src/kernel/journal.mjs.
// Cross-process safe: parallel agents share one journal file; writes are
// serialized with an exclusive lock file so no line is ever torn interleaved.
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::errors::ToolError;
use crate::filock::{FileLock, DEFAULT_TIMEOUT_MS};

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
        let lock_path = file_path
            .parent()
            .unwrap_or(Path::new("."))
            .join(".journal.lock");
        Ok(Journal {
            file_path,
            seq: std::sync::Arc::new(std::sync::Mutex::new(seq)),
            lock_path,
        })
    }

    /// Append one event; returns the event as written. Serialized across
    /// processes via the exclusive lock file.
    pub fn append(&self, kind: &str, fields: Value) -> Result<JournalEvent, ToolError> {
        // The next seq is derived from the file's last line while holding the
        // append lock: several server processes share one journal, and each
        // process resumes its counter at open time, so a counter alone
        // collides (the journal once recorded 92 duplicate seqs). The file is
        // the only shared truth — re-read it under the lock per append.
        let _lock = FileLock::acquire(&self.lock_path, DEFAULT_TIMEOUT_MS)?;
        let seq = self.last_seq_locked()?.saturating_add(1);
        let event = JournalEvent {
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            seq,
            kind: kind.to_string(),
            fields,
        };
        let line = serde_json::to_string(&event)? + "\n";
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file_path)?;
        f.write_all(line.as_bytes())?;
        *self.seq.lock().unwrap() = seq;
        Ok(event)
    }

    /// Seq of the journal file's last complete line (caller holds the append
    /// lock). A torn final line — a crash mid-write — is skipped by walking
    /// backwards to the nearest parseable line.
    fn last_seq_locked(&self) -> Result<u64, ToolError> {
        let mut f = match File::open(&self.file_path) {
            Ok(f) => f,
            Err(_) => return Ok(0),
        };
        let len = match f.metadata() {
            Ok(m) => m.len(),
            Err(_) => return Ok(0),
        };
        if len == 0 {
            return Ok(0);
        }
        // Tail window grows until it contains the last line (a single line can
        // be megabytes when a tool result embeds file content).
        let mut window = 64 * 1024u64;
        loop {
            let start = len.saturating_sub(window);
            let mut buf = vec![0u8; (len - start) as usize];
            f.seek(SeekFrom::Start(start))?;
            if f.read_exact(&mut buf).is_err() {
                return Ok(0);
            }
            if let Some(seq) = last_complete_seq(&buf) {
                return Ok(seq);
            }
            if start == 0 {
                return Ok(0);
            }
            window = (window * 2).min(len);
        }
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
        buf.lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect()
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

/// seq of the last complete JSON event in a window, walking the window
/// backwards so a torn final line never resets numbering to 0 (which would
/// restart collisions). Returns None when no line parses in the window.
fn last_complete_seq(buf: &[u8]) -> Option<u64> {
    let s = String::from_utf8_lossy(buf);
    for line in s.rsplit('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(line) {
            if let Some(seq) = map.get("seq").and_then(|v| v.as_u64()) {
                return Some(seq);
            }
        } else if line.parse::<u64>().is_ok() {
            // legacy single-number lines (older journal.mjs records)
            return line.parse::<u64>().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_journal(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nct-journal-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("journal.jsonl")
    }

    // Regression: two server processes share one journal; both construct
    // their Journal before either appends, so each resumed the same counter —
    // interleaved appends produced duplicate seqs (92 dups on record). With
    // seqs derived from the file under the append lock, they stay unique.
    #[test]
    fn concurrent_instances_never_collide() {
        let path = temp_journal("race");
        let a = Journal::new(path.clone()).unwrap();
        let b = Journal::new(path.clone()).unwrap(); // same file, independent counters
        let mut seqs = Vec::new();
        for i in 0..8 {
            let j = if i % 2 == 0 { &a } else { &b };
            seqs.push(
                j.append("tool.call", serde_json::json!({ "tool": "t" }))
                    .unwrap()
                    .seq,
            );
        }
        for w in seqs.windows(2) {
            assert!(w[0] < w[1], "seqs must be strictly increasing: {seqs:?}");
        }
        let uniq: std::collections::BTreeSet<_> = seqs.iter().collect();
        assert_eq!(uniq.len(), seqs.len(), "duplicate seqs: {seqs:?}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn restart_continues_numbering() {
        let path = temp_journal("restart");
        {
            let j = Journal::new(path.clone()).unwrap();
            j.append("tool.call", serde_json::json!({ "tool": "t" }))
                .unwrap();
        }
        let j2 = Journal::new(path.clone()).unwrap();
        assert_eq!(
            j2.append("tool.call", serde_json::json!({ "tool": "t" }))
                .unwrap()
                .seq,
            2
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn torn_final_line_does_not_reset_seq() {
        let path = temp_journal("torn");
        {
            let j = Journal::new(path.clone()).unwrap();
            j.append("tool.call", serde_json::json!({ "tool": "t" }))
                .unwrap();
        }
        // simulate a crash mid-write: a partial line at the end
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"ts\":\"x\",\"seq\":")
            .unwrap();
        assert_eq!(last_complete_seq(&std::fs::read(&path).unwrap()), Some(1));
        let j = Journal::new(path.clone()).unwrap();
        assert_eq!(
            j.append("tool.call", serde_json::json!({ "tool": "t" }))
                .unwrap()
                .seq,
            2
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
