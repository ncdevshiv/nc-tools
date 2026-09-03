// proc.diff — runtime process-tree diff across two snapshots.
//
// PROBLEM: proc.list is a snapshot. "What did `npm install` spawn?" is a
// diffable question — capture the process table, run the command, capture
// again, diff. But nothing does that, so an agent answers "what changed?" by
// re-listing and eyeballing, which is error-prone and loses the before/after.
//
// proc.diff captures the process table twice (optionally running a command in
// between) and diffs by pid: processes that appeared, disappeared, or changed
// (memKb / same-pid command name). This makes a runtime change visible and
// attributable.
//
// Two modes:
//   * snapshot-to-snapshot: run once (label A), and again (label B).
//   * run-command-between: capture, spawn a command, wait, capture again.
//
// It reuses system_processes() — the same parser proc.list uses — so it shares
// its platform logic. No mock, no reimplementation.
use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{json, Value};

use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};

pub const DIFF_DESC: &str = "Diff the OS process table across two points in time (runtime process-tree diff). With {command} it captures the table, runs the command, captures again, and reports what started / stopped / changed (pid, name, memKb). Without it, pass before=/after= for two stored snapshots. Answers 'what did this command spawn?' — proc.list is a snapshot, this is the diff.";

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiffArgs {
    /// Command to run between the two captures (argv-safe: cmd + args).
    #[serde(default)]
    pub cmd: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    /// Optional stored "before" snapshot (the output of a prior proc.diff
    /// {capture} run). Used without a command.
    #[serde(default)]
    pub before: Option<String>,
    /// Optional stored "after" snapshot to diff against `before`.
    #[serde(default)]
    pub after: Option<String>,
    /// Capture the current table and return it (a snapshot id) with no diff.
    #[serde(default)]
    pub capture: Option<bool>,
    /// Timeout for the in-between command (default 60s).
    #[serde(default)]
    #[schemars(range(min = 1000, max = 600000))]
    pub timeoutMs: Option<u64>,
    /// Working dir for the in-between command.
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub baseDir: Option<String>,
}

pub struct DiffHandler;
impl Handler for DiffHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: DiffArgs = parse_args(args)?;

        // capture mode: return a snapshot id
        if a.capture.unwrap_or(false) {
            let env = k.session_env.snapshot();
            let procs = system_processes(&env, &k.root)?;
            let snapshot = json!({ "ts": nct_core::now_iso(), "processes": procs });
            let id = nct_core::sha256_hex(serde_json::to_string(&snapshot).unwrap_or_default().as_bytes())[..8].to_string();
            let _ = k.journal.append("proc.diff", json!({ "mode": "capture", "id": id, "count": procs.len(), "sid": k.sid }));
            return Ok(json!({ "capture": true, "id": id, "count": procs.len(), "snapshot": snapshot }));
        }

        // Determine before/after tables.
        let before_json: Vec<Value> = if let Some(b) = &a.before {
            parse_snapshot(b)?
        } else if a.cmd.is_some() {
            // no before given -> capture now, then run command, then capture again
            let env = k.session_env.snapshot();
            system_processes(&env, &k.root)?
        } else {
            return Err(ToolError::with_hint(
                "ERR_BAD_INPUT",
                "proc.diff needs a command (capture-run-capture) or before=/after= snapshots",
                json!({ "hint": "proc.diff {cmd, args}   OR   proc.diff {before, after}" }),
            ));
        };

        let after_json: Vec<Value> = if let Some(a) = &a.after {
            parse_snapshot(a)?
        } else if a.cmd.is_some() {
            // run the command, then capture
            let cmd = a.cmd.as_ref().unwrap();
            let args_v = a.args.clone().unwrap_or_default();
            let root = k.base_dir(a.baseDir.as_deref())?;
            let cwd_abs = nct_core::paths::resolve_checked(&root, a.cwd.as_deref().unwrap_or("."))?;
            let timeout = a.timeoutMs.unwrap_or(k.cfg.limits.spawn_timeout_ms);
            let _ = k.call("proc.spawn", &json!({ "cmd": cmd, "args": args_v, "cwd": cwd_abs.display().to_string(), "timeoutMs": timeout }));
            let env = k.session_env.snapshot();
            system_processes(&env, &root)?
        } else {
            Vec::new()
        };

        let before = before_json;
        let after = after_json;
        if before.is_empty() && after.is_empty() {
            return Ok(json!({ "started": [], "stopped": [], "changed": [], "error": "empty both sides — snapshots not usable" }));
        }

        let diff = diff_tables(&before, &after);

        let _ = k.journal.append("proc.diff", json!({
            "mode": if a.cmd.is_some() { "run-command" } else { "snapshots" },
            "started": diff.started.len(),
            "stopped": diff.stopped.len(),
            "changed": diff.changed.len(),
            "sid": k.sid,
        }));

        Ok(json!({
            "beforeCount": before.len(),
            "afterCount": after.len(),
            "started": diff.started,
            "stopped": diff.stopped,
            "changed": diff.changed,
        }))
    }
}

struct DiffOut {
    started: Vec<Value>,
    stopped: Vec<Value>,
    changed: Vec<Value>,
}

/// Diff two process tables by pid. started = pid in after but not before;
/// stopped = pid in before but not after; changed = same pid, different memKb
/// or name.
fn diff_tables(before: &[Value], after: &[Value]) -> DiffOut {
    let before_map: HashMap<u64, &Value> = index_by_pid(before);
    let after_map: HashMap<u64, &Value> = index_by_pid(after);

    let mut started = Vec::new();
    let mut stopped = Vec::new();
    let mut changed = Vec::new();

    for (pid, v) in &after_map {
        if !before_map.contains_key(pid) {
            started.push((*v).clone());
        }
    }
    for (pid, v) in &before_map {
        if !after_map.contains_key(pid) {
            stopped.push((*v).clone());
        }
    }
    for (pid, b) in &before_map {
        if let Some(a) = after_map.get(pid) {
            let b_mem = b["memKb"].as_u64().unwrap_or(0);
            let a_mem = a["memKb"].as_u64().unwrap_or(0);
            let b_name = b["name"].as_str().unwrap_or("");
            let a_name = a["name"].as_str().unwrap_or("");
            if b_mem != a_mem || b_name != a_name {
                changed.push(json!({ "pid": pid, "beforeName": b_name, "afterName": a_name, "beforeMemKb": b_mem, "afterMemKb": a_mem }));
            }
        }
    }
    started.sort_by_key(|v| v["pid"].as_u64().unwrap_or(0));
    stopped.sort_by_key(|v| v["pid"].as_u64().unwrap_or(0));
    changed.sort_by_key(|v| v["pid"].as_u64().unwrap_or(0));
    DiffOut { started, stopped, changed }
}

fn index_by_pid(table: &[Value]) -> HashMap<u64, &Value> {
    table.iter().filter_map(|v| v["pid"].as_u64().map(|p| (p, v))).collect()
}

fn parse_snapshot(s: &str) -> Result<Vec<Value>, ToolError> {
    let v: Value = serde_json::from_str(s).unwrap_or(Value::Null);
    if let Some(arr) = v["processes"].as_array() {
        Ok(arr.clone())
    } else if let Some(arr) = v.as_array() {
        Ok(arr.clone())
    } else {
        Err(ToolError::with_hint("ERR_BAD_INPUT", "snapshot is not a valid proc.diff capture", json!({})))
    }
}

// Reuse the exact platform parser proc.list uses. Imported from the sibling
// function in this crate so the diff shares the same OS logic.
use crate::system_processes;

pub fn register_proc_diff(k: &mut Kernel) {
    k.register("proc.diff", DIFF_DESC, nct_core::schema::schema_for::<DiffArgs>(), std::sync::Arc::new(DiffHandler));
}

#[cfg(test)]
mod diff_tests {
    use super::*;

    #[test]
    fn detects_started_stopped_changed() {
        let before = vec![
            json!({ "pid": 1, "name": "bash.exe", "memKb": 500 }),
            json!({ "pid": 2, "name": "node.exe", "memKb": 1000 }),
            json!({ "pid": 3, "name": "sleep.exe", "memKb": 100 }),
        ];
        let after = vec![
            json!({ "pid": 1, "name": "bash.exe", "memKb": 520 }), // changed mem
            json!({ "pid": 3, "name": "sleep.exe", "memKb": 100 }), // unchanged
            json!({ "pid": 4, "name": "git.exe", "memKb": 9000 }),  // started
        ];
        let diff = diff_tables(&before, &after);
        assert_eq!(diff.started.len(), 1);
        assert_eq!(diff.started[0]["pid"], json!(4));
        assert_eq!(diff.stopped.len(), 1);
        assert_eq!(diff.stopped[0]["pid"], json!(2));
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0]["pid"], json!(1));
    }

    #[test]
    fn empty_tables_produce_empty_diff() {
        let diff = diff_tables(&[], &[]);
        assert!(diff.started.is_empty());
        assert!(diff.stopped.is_empty());
        assert!(diff.changed.is_empty());
    }

    #[test]
    fn parse_snapshot_accepts_capture_and_array_forms() {
        let snap = json!({ "ts": "x", "processes": [ { "pid": 9, "name": "a" } ] });
        let arr = parse_snapshot(&serde_json::to_string(&snap).unwrap()).unwrap();
        assert_eq!(arr.len(), 1);
        let arr2 = parse_snapshot("[{\"pid\":7,\"name\":\"b\"}]").unwrap();
        assert_eq!(arr2.len(), 1);
        assert!(parse_snapshot("garbage").is_err());
    }
}
