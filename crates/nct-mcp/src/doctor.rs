// sys.doctor — one-call health/diagnostics for the kernel: tool inventory,
// config limits, session env, journal stats, and (deep) runtime checks.
use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};
use serde::Deserialize;
use serde_json::{json, Value};
use std::process::Command;

pub const DOCTOR_DESC: &str = "Self-diagnostics: registered tools, config limits, session env, journal stats. deep=true also probes runtime (workspace writable, cargo on PATH).";

pub fn register_sys_doctor(k: &mut Kernel) {
    k.register(
        "sys.doctor",
        DOCTOR_DESC,
        nct_core::schema::schema_for::<DoctorArgs>(),
        std::sync::Arc::new(DoctorHandler),
    );
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DoctorArgs {
    #[doc = "Include runtime probes (writable root, cargo available)"]
    #[serde(default)]
    pub deep: Option<bool>,
    /// When true, auto-fix the healthy-but-degraded cases the doctor can repair:
    /// superseded rows in the coordination logs and expired lock rows. Runs under
    /// the same cross-process file locks the live writers take.
    #[serde(default)]
    pub repair: Option<bool>,
    /// Per-call workspace override: diagnose THIS base dir instead of the
    /// server root. When absent, the server-rooted session workspace is used
    /// (identical behavior to before).
    #[serde(default)]
    pub baseDir: Option<String>,
}

pub struct DoctorHandler;
impl Handler for DoctorHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: DoctorArgs = parse_args(args)?;
        let deep = a.deep.unwrap_or(false);
        let base = k.base_dir(a.baseDir.as_deref())?;
        let tools = k.list_tools();
        let events = k.journal.last_n(None);
        let ev_count = events.len();
        let last_seq = events
            .last()
            .and_then(|e| e.get("seq"))
            .and_then(|s| s.as_u64())
            .unwrap_or(0);
        let env = k.session_env.snapshot();
        let env_keys: Vec<String> = env.keys().cloned().collect();
        let limits = &k.cfg.limits;
        let mut out = json!({
            "version": env!("CARGO_PKG_VERSION"),
            "platform": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "sid": k.sid,
            "root": base.display().to_string(),
            "serverRoot": k.root.display().to_string(),
            "tools": {
                "count": tools.len(),
                "names": tools,
            },
            "limits": {
                "readLimit": limits.read_limit,
                "fsMany": limits.fs_many,
                "patchMany": limits.patch_many,
                "batchMax": limits.batch_max,
                "grepMaxResults": limits.grep_max_results,
                "spawnTimeoutMs": limits.spawn_timeout_ms,
                "spawnTimeoutMaxMs": limits.spawn_timeout_max_ms,
                "procOutputBytes": limits.proc_output_bytes,
                "procMaxDurationMs": limits.proc_max_duration_ms,
                "procHandleOutputBytes": limits.proc_handle_output_bytes,
                "netMaxBody": limits.net_max_body,
                "childTimeoutMs": limits.child_timeout_ms,
                "listDepth": limits.list_depth,
                "walkDepth": limits.walk_depth,
                "grepMaxScanFiles": limits.grep_max_scan_files,
                "grepMaxScanMs": limits.grep_max_scan_ms,
                "grepMaxFileBytes": limits.grep_max_file_bytes,
                "testTimeoutMs": limits.test_timeout_ms,
                "procListMax": limits.proc_list_max,
            },
            "env": {
                "count": env.len(),
                "keys": env_keys,
            },
            "journal": {
                "path": k.journal.file_path.display().to_string(),
                "eventCount": ev_count,
                "lastSeq": last_seq,
            },
        });
        if deep {
            let root_writable = probe_write(&base.join(".nc-tools-doctor-probe"));
            let temp_writable = probe_write(&std::env::temp_dir().join("nct-doctor-probe.tmp"));
            let cargo = Command::new("cargo")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            out["checks"] = json!({
                "baseWritable": root_writable,
                "tempWritable": temp_writable,
                "cargoAvailable": cargo,
            });
        }
        if a.repair.unwrap_or(false) {
            let repaired = repair_self(&base);
            out["repair"] = json!({
                "requested": true,
                "applied": repaired,
            });
        }
        Ok(out)
    }
}

/// Self-heal the degraded coordination state: a roster that carries the full
/// history of one agentId instead of one row, and lock rows whose expiry has
/// passed. Both logs are append-only files the live server keeps writing, so
/// each rewrite takes the same cross-process lock the writers take and
/// replaces the file atomically. Lines that do not parse, and lock rows that
/// still define a path's current state, are kept verbatim — repair never
/// erases data it cannot interpret.
fn repair_self(root: &std::path::Path) -> Value {
    let dir = root.join(".nc-tools");
    let roster_path = dir.join("agents.jsonl");
    let locks_path = dir.join("locks.jsonl");
    let mut repaired = json!({ "rosterCompacted": 0, "staleLocksDropped": 0 });

    repair_log(&roster_path, compact_roster, &mut repaired);
    repair_log(&locks_path, drop_stale_locks, &mut repaired);
    repaired
}

/// Apply one log repair under its cross-process lock. If another process is
/// mid-write on that log, leave it alone rather than fight the lock.
fn repair_log(path: &std::path::Path, fix: fn(&std::path::Path, &mut Value), repaired: &mut Value) {
    // The lock must outlive `fix`, so the result is bound rather than tested.
    let Ok(_lock) = nct_core::FileLock::acquire(
        &nct_core::lock_path_for(path),
        nct_core::filock::DEFAULT_TIMEOUT_MS,
    ) else {
        return;
    };
    fix(path, repaired);
}

/// Rewrite a log file atomically: temp file in the same directory, then
/// rename. File::create truncates the target FIRST, so a concurrent append
/// landing between truncate and write is lost forever, and a crash mid-rewrite
/// leaves a half-written log in place of a good one.
fn replace_log_atomic(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    let tmp = dir.join(format!(
        ".{}-repair-tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("log")
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.flush()?;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Compact the roster to one row per agentId (last-seen wins), keeping the
/// first-seen order.
fn compact_roster(path: &std::path::Path, repaired: &mut Value) {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return;
    };
    let mut latest: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut unparseable: Vec<&str> = Vec::new();
    let mut rows = 0u64;
    for line in raw.lines().filter(|l| !l.is_empty()) {
        rows += 1;
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            let id = v["agentId"].as_str().unwrap_or("").to_string();
            if id.is_empty() {
                unparseable.push(line);
                continue;
            }
            if !latest.contains_key(&id) {
                order.push(id.clone());
            }
            latest.insert(id, v);
        } else {
            unparseable.push(line);
        }
    }
    // The old code guarded this on `latest.len() < order.len()`, which can
    // never hold: `order` grows by exactly one entry each time a new key
    // enters `latest`, so the compaction was dead code and always reported 0.
    if rows as usize == order.len() + unparseable.len() {
        return; // no duplicate rows to fold
    }
    let mut rebuilt = String::new();
    for id in &order {
        if let Some(v) = latest.get(id) {
            rebuilt.push_str(&serde_json::to_string(v).unwrap_or_default());
            rebuilt.push('\n');
        }
    }
    // Preserve unparseable lines verbatim: they are data we cannot interpret,
    // not data we are entitled to erase.
    for line in &unparseable {
        rebuilt.push_str(line);
        rebuilt.push('\n');
    }
    if replace_log_atomic(path, &rebuilt).is_ok() {
        repaired["rosterCompacted"] = json!(order.len());
    }
}

/// Whether a lock row is droppable garbage: superseded AND already dead.
/// Unparseable rows are kept, and so is the last row for its path — the latter
/// because dropping it would make an EARLIER row the last row for that path,
/// resurrecting a lock the agent already released.
fn stale_lock_row(row: Option<&Value>, last_for_path: bool, now_ms: u64) -> bool {
    let Some(v) = row else { return false };
    if last_for_path {
        return false;
    }
    let expiry = v["expiresAtMs"].as_u64().unwrap_or(0);
    let released = v["released"].as_bool().unwrap_or(false);
    released || (expiry != 0 && now_ms > expiry)
}

/// Drop lock rows that are superseded AND already dead.
fn drop_stale_locks(path: &std::path::Path, repaired: &mut Value) {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return;
    };
    let now_ms = nct_core::now_ms();
    let lines: Vec<&str> = raw.lines().filter(|l| !l.is_empty()).collect();
    let vals: Vec<Option<Value>> = lines.iter().map(|l| serde_json::from_str(l).ok()).collect();

    // last row per path = the current state
    let mut last_for_path: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (i, v) in vals.iter().enumerate() {
        if let Some(v) = v {
            let p = v["path"].as_str().unwrap_or("").to_string();
            if !p.is_empty() {
                last_for_path.insert(p, i);
            }
        }
    }

    let last_rows: std::collections::HashSet<usize> = last_for_path.values().cloned().collect();
    let mut kept = String::new();
    let mut dropped = 0u64;
    for (i, (line, v)) in lines.iter().zip(vals.iter()).enumerate() {
        if stale_lock_row(v.as_ref(), last_rows.contains(&i), now_ms) {
            dropped += 1;
        } else {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    if dropped == 0 {
        return;
    }
    if replace_log_atomic(path, &kept).is_ok() {
        repaired["staleLocksDropped"] = json!(dropped);
    }
}

fn probe_write(path: &std::path::Path) -> bool {
    std::fs::write(path, b"ok").is_ok() && std::fs::remove_file(path).is_ok()
}

#[cfg(test)]
mod doctor_tests {
    use super::*;

    fn doctor_kernel() -> Kernel {
        let dir = std::env::temp_dir().join(format!(
            "nct-doctor-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Kernel::new(dir).unwrap()
    }

    /// sys.doctor reports the EFFECTIVE base: default = server root
    /// (unchanged), baseDir = the other workspace, whose writability the deep
    /// probe then actually checks. A bad baseDir errors — never a silent
    /// fallback to the server root.
    #[test]
    fn doctor_reports_baseDir_not_server_root() {
        let k = doctor_kernel();
        let target = std::env::temp_dir().join(format!(
            "nct-doctor-target-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&target);
        std::fs::create_dir_all(&target).unwrap();

        let def = DoctorHandler.call(&k, &json!({ "deep": true })).unwrap();
        assert_eq!(def["root"], json!(k.root.display().to_string()));
        assert_eq!(def["serverRoot"], json!(k.root.display().to_string()));

        // (compare canonicalized — resolve_checked returns the long path)
        let canon = dunce::canonicalize(&target).unwrap();
        let over = DoctorHandler
            .call(
                &k,
                &json!({ "deep": true, "baseDir": target.display().to_string() }),
            )
            .unwrap();
        assert_eq!(over["root"], json!(canon.display().to_string()));
        assert_eq!(over["serverRoot"], json!(k.root.display().to_string()));
        assert_eq!(
            over["checks"]["baseWritable"],
            json!(true),
            "the probe must check the TARGET's writability"
        );

        let bad = DoctorHandler.call(&k, &json!({ "baseDir": "/no/such/ws/xyz" }));
        assert!(bad.is_err(), "bad baseDir must error, not fall back");

        let _ = std::fs::remove_dir_all(&target);
    }

    fn ws(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nct-doctor-repair-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".nc-tools")).unwrap();
        dir
    }

    fn write_lines(path: &std::path::Path, lines: &[&str]) {
        std::fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    fn read_lines(path: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect()
    }

    /// The compaction used to be dead code: the guard was `latest.len() <
    /// order.len()`, but `order` grows by exactly one entry each time a new
    /// key enters `latest`, so it could never be true and `rosterCompacted`
    /// was always 0. Duplicates must actually be folded.
    #[test]
    fn repair_compacts_duplicate_roster_rows() {
        let root = ws("compact");
        let roster = root.join(".nc-tools").join("agents.jsonl");
        write_lines(
            &roster,
            &[
                r#"{"agentId":"a","lastSeen":"t1"}"#,
                r#"{"agentId":"b","lastSeen":"t2"}"#,
                r#"{"agentId":"a","lastSeen":"t3"}"#,
                r#"{"agentId":"a","lastSeen":"t4"}"#,
            ],
        );

        let r = repair_self(&root);
        assert_eq!(
            r["rosterCompacted"],
            json!(2),
            "expected one row per agent: {r}"
        );

        let lines = read_lines(&roster);
        assert_eq!(lines.len(), 2, "duplicates must be folded: {lines:?}");
        let ids: Vec<&str> = lines
            .iter()
            .map(|l| l.split('"').nth(3).unwrap_or(""))
            .collect();
        assert_eq!(ids, vec!["a", "b"], "first-seen order: {lines:?}");
        // last-seen wins: the surviving row for "a" is the newest one
        assert!(
            lines[0].contains("t4"),
            "last-seen row must survive: {lines:?}"
        );
        assert!(lines[1].contains("t2"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Repair must not turn a healthy roster into a rewrite.
    #[test]
    fn repair_leaves_an_already_compact_roster_alone() {
        let root = ws("noop");
        let roster = root.join(".nc-tools").join("agents.jsonl");
        let before = r#"{"agentId":"a","lastSeen":"t1"}
{"agentId":"b","lastSeen":"t2"}
"#;
        std::fs::write(&roster, before).unwrap();

        let r = repair_self(&root);
        assert_eq!(r["rosterCompacted"], json!(0), "no rewrite expected: {r}");
        assert_eq!(std::fs::read_to_string(&roster).unwrap(), before);
        // no leftover temp from a rewrite that never happened
        let leftovers = std::fs::read_dir(root.join(".nc-tools"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("-repair-tmp"))
            .collect::<Vec<_>>();
        assert!(
            leftovers.is_empty(),
            "no temp file should exist: {leftovers:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Repair never erases a line it cannot interpret, in either log.
    #[test]
    fn repair_preserves_unparseable_lines() {
        let root = ws("unparseable");
        let roster = root.join(".nc-tools").join("agents.jsonl");
        let locks = root.join(".nc-tools").join("locks.jsonl");
        write_lines(
            &roster,
            &[
                r#"{"agentId":"a","lastSeen":"t1"}"#,
                "not json",
                r#"{"agentId":"a","lastSeen":"t2"}"#,
            ],
        );
        write_lines(
            &locks,
            &[
                r#"{"path":"x.rs","agentId":"a","expiresAtMs":0}"#,
                "garbage line",
            ],
        );

        repair_self(&root);
        let rl = read_lines(&roster);
        assert!(
            rl.iter().any(|l| l == "not json"),
            "unparseable roster line lost: {rl:?}"
        );
        let ll = read_lines(&locks);
        assert!(
            ll.iter().any(|l| l == "garbage line"),
            "unparseable lock line lost: {ll:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Dropping the released marker for a path would make an EARLIER live row
    /// the last row for that path — resurrecting a lock the agent already
    /// released. The last row per path is state, never garbage.
    #[test]
    fn repair_does_not_resurrect_an_already_released_lock() {
        let root = ws("resurrect");
        let locks = root.join(".nc-tools").join("locks.jsonl");
        write_lines(
            &locks,
            &[
                r#"{"path":"x.rs","agentId":"agent-1","expiresAtMs":18446744073709551615,"released":false}"#,
                r#"{"path":"x.rs","agentId":"agent-1","expiresAtMs":18446744073709551615,"released":true}"#,
            ],
        );

        let r = repair_self(&root);
        assert_eq!(
            r["staleLocksDropped"],
            json!(0),
            "the release marker is state: {r}"
        );

        let lines = read_lines(&locks);
        assert_eq!(lines.len(), 2, "both rows must survive: {lines:?}");
        assert!(
            lines[1].contains("\"released\":true"),
            "the release must still be last: {lines:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An expired row that is NOT the last row for its path is real garbage and
    /// gets dropped — the last row for the path (still live) is kept, and so is
    /// the last row of another path even when it is dead: each is state, not
    /// history.
    #[test]
    fn repair_drops_superseded_expired_lock_rows() {
        let root = ws("expired");
        let locks = root.join(".nc-tools").join("locks.jsonl");
        write_lines(
            &locks,
            &[
                r#"{"path":"x.rs","agentId":"agent-1","expiresAtMs":1,"released":false}"#,
                r#"{"path":"x.rs","agentId":"agent-1","expiresAtMs":18446744073709551615,"released":false}"#,
                r#"{"path":"y.rs","agentId":"agent-2","expiresAtMs":1,"released":true}"#,
            ],
        );

        let r = repair_self(&root);
        assert_eq!(
            r["staleLocksDropped"],
            json!(1),
            "only the superseded row is garbage: {r}"
        );

        let lines = read_lines(&locks);
        assert_eq!(
            lines.len(),
            2,
            "one state row per path must remain: {lines:?}"
        );
        assert!(
            lines[0].contains("x.rs") && lines[0].contains("18446744073709551615"),
            "the live x.rs row must survive: {lines:?}"
        );
        assert!(
            lines[1].contains("y.rs"),
            "y.rs's last row is state: {lines:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The repair also runs under the normal sys.doctor call, reporting what it
    /// changed. The workspace here is `C:\Users\NCDEVS~1\...` — an 8.3 short
    /// name — which is exactly the spelling that broke lock-key matching before
    /// rel_key canonicalized the root.
    #[test]
    fn doctor_repair_reports_what_it_dropped() {
        let root = ws("handler");
        let mut k = Kernel::new(root.clone()).unwrap();
        register_sys_doctor(&mut k);

        let locks = root.join(".nc-tools").join("locks.jsonl");
        write_lines(
            &locks,
            &[
                r#"{"path":"x.rs","agentId":"a","expiresAtMs":1,"released":false}"#,
                r#"{"path":"x.rs","agentId":"a","expiresAtMs":18446744073709551615,"released":false}"#,
            ],
        );

        let out = DoctorHandler.call(&k, &json!({ "repair": true })).unwrap();
        assert_eq!(
            out["repair"]["applied"]["staleLocksDropped"],
            json!(1),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Atomic replace: the target is rewritten in one rename, and the temp
    /// file is never left behind.
    #[test]
    fn replace_log_atomic_leaves_no_temp_and_writes_content() {
        let root = ws("atomic");
        let dir = root.join(".nc-tools");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("agents.jsonl");
        std::fs::write(&p, "old").unwrap();

        replace_log_atomic(&p, "new-content\n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new-content\n");
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("-repair-tmp"))
            .collect::<Vec<_>>();
        assert!(
            leftovers.is_empty(),
            "temp must be renamed away: {leftovers:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
