// sys.replay — replay journal entries as tool calls (time-machine debugging).
//
// PROBLEM: the journal is a complete instruction log — every tool.call records
// the exact {tool, args} and every tool.result the outcome. But it's treated as
// write-only. "Why did this pass last week but not today?" is unanswerable
// because there's no way to re-run a slice of the history and compare.
//
// sys.replay fixes that:
//   * reads the journal's tool.call events in a seq range (or all);
//   * by default (dryRun=true) shows what WOULD be re-executed — no side
//     effects — with the original args and the recorded result for comparison;
//   * with dryRun=false it actually re-runs each call through the kernel,
//     collecting fresh results side-by-side with the originals, so an agent
//     can diff the two and find the divergence.
//
// The journal can be weeks old, so re-executing a recorded call re-applies
// STALE arguments to the current tree. `dryRun=false` therefore refuses to
// run calls that can delete files, rewrite them, publish to a remote, or
// execute code unless the caller also passes confirmWrite=true — the same
// "state the intent explicitly" shape as sys.rollback's dryRun and
// agent.lock's force. Read-only calls always execute.
//
// It intentionally does NOT replay into the same journal/seq space (each replay
// call is itself journaled as sys.replay, so you get a trace of the replay).
use serde::Deserialize;
use serde_json::{json, Value};

use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};

pub const REPLAY_DESC: &str = "Replay journal entries as tool calls (time-machine debugging). Reads tool.call events in a seq range, and by default (dryRun=true) shows what WOULD be re-executed with the original args + recorded result — no side effects. dryRun=false actually re-runs each call through the kernel and returns fresh results next to the originals so you can diff and find the divergence. Irreversible calls (file writes/deletes, git mutations, proc/exec) are SKIPPED unless confirmWrite=true, because the journal can be weeks old and stale args re-applied to the current tree can destroy it. Restrict with tool= / from= / to= / lastN=. Each replay call is itself journaled.";

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplayArgs {
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub from: Option<u64>,
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub to: Option<u64>,
    /// Replay only calls to this tool (dotted name).
    #[serde(default)]
    pub tool: Option<String>,
    /// Dry-run (default true): show what would run, no execution.
    #[serde(default)]
    pub dryRun: Option<bool>,
    /// Keep only the most recent `lastN` matching calls (the tail of the range).
    #[serde(default)]
    #[schemars(range(min = 1, max = 500))]
    pub lastN: Option<u64>,
    /// Execute irreversible calls under dryRun=false. Without it they are
    /// skipped and reported, since the journal's args are stale.
    #[serde(default)]
    pub confirmWrite: Option<bool>,
}

pub struct ReplayHandler;
impl Handler for ReplayHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: ReplayArgs = parse_args(args)?;
        let events = k.journal.events();
        let tool_filter = a.tool.as_deref();
        let last_n = a.lastN.unwrap_or(100) as usize;

        // Collect tool.call events in range, oldest-last.
        let mut calls: Vec<Value> = events
            .iter()
            .filter(|e| e["kind"].as_str() == Some("tool.call"))
            .filter(|e| {
                if let Some(from) = a.from {
                    if e["seq"].as_u64().unwrap_or(0) < from {
                        return false;
                    }
                }
                if let Some(to) = a.to {
                    if e["seq"].as_u64().unwrap_or(0) > to {
                        return false;
                    }
                }
                if let Some(t) = tool_filter {
                    if e["tool"].as_str() != Some(t) {
                        return false;
                    }
                }
                true
            })
            .cloned()
            .collect();
        // lastN keeps the TAIL — the newest calls. `truncate` would have kept
        // the oldest and silently dropped the ones an agent is actually
        // debugging.
        if calls.len() > last_n {
            calls.drain(..calls.len() - last_n);
        }

        let dry = a.dryRun.unwrap_or(true);
        let confirm_write = a.confirmWrite.unwrap_or(false);
        let mut replayed: Vec<Value> = Vec::new();
        let mut skipped: Vec<Value> = Vec::new();

        for c in &calls {
            let seq = c["seq"].as_u64().unwrap_or(0);
            let tool = c["tool"].as_str().unwrap_or("").to_string();
            let call_args = if c["args"].is_null() {
                json!({})
            } else {
                c["args"].clone()
            };
            let risk = replay_risk(&tool);
            // Find the matching result event for comparison.
            let original = events
                .iter()
                .find(|e| {
                    e["kind"].as_str() == Some("tool.result") && e["callSeq"].as_u64() == Some(seq)
                })
                .cloned()
                .unwrap_or(json!({}));

            if dry {
                replayed.push(json!({
                    "seq": seq,
                    "tool": tool,
                    "args": call_args,
                    "dryRun": true,
                    "risk": if risk.is_empty() { Value::Null } else { json!(risk) },
                    "originalResult": original["ok"],
                    "recordedResult": original["result"],
                }));
                continue;
            }
            // Replaying a replay is meaningless — the recorded call already
            // re-ran whatever it replayed — and it recurses: a live replay of
            // sys.replay would call sys.replay, which would call it again.
            if tool == "sys.replay" {
                skipped.push(json!({
                    "seq": seq,
                    "tool": tool,
                    "reason": "self-replay is meaningless and recursive",
                    "hint": "replay the calls that sys.replay replayed, if any"
                }));
                continue;
            }
            // Live replay: refuse stale, irreversible calls unless confirmed.
            if !risk.is_empty() && !confirm_write {
                skipped.push(json!({
                    "seq": seq,
                    "tool": tool,
                    "reason": risk,
                    "hint": "re-apply with confirmWrite=true to execute; the journal's args are from when the call was first recorded"
                }));
                continue;
            }
            let out = k.call(&tool, &call_args);
            let fresh = out.result.clone().unwrap_or(Value::Null);
            let recorded = original["result"].clone();
            replayed.push(json!({
                "seq": seq,
                "tool": tool,
                "args": call_args,
                "ok": out.ok,
                "fresh": fresh,
                "freshError": out.error.map(|e| serde_json::to_value(e).unwrap_or(Value::Null)).unwrap_or(Value::Null),
                "originalOk": original["ok"],
                "recorded": recorded,
                "diverged": fresh != recorded || out.ok != original["ok"].as_bool().unwrap_or(false),
            }));
        }

        // How many replayed calls differ from what the journal recorded — on
        // BOTH ok-ness and payload, not just success. A tool that returns
        // ok:true with a different body is exactly the drift this tool exists
        // to surface.
        let diverged = replayed
            .iter()
            .filter(|r| r["diverged"].as_bool().unwrap_or(false))
            .count();
        let errors = replayed
            .iter()
            .filter(|r| r["ok"].as_bool() == Some(false))
            .count();

        Ok(json!({
            "replayed": replayed.len(),
            "skipped": skipped.len(),
            "diverged": diverged,
            "errors": errors,
            "dryRun": dry,
            "confirmWrite": confirm_write,
            "events": replayed,
            "skippedEvents": skipped,
            "note": if dry {
                "dry run — no side effects; pass dryRun=false to re-execute"
            } else if confirm_write {
                "live replay executed through the kernel, including irreversible calls"
            } else {
                "live replay; irreversible calls were skipped (pass confirmWrite=true to run them)"
            },
        }))
    }
}

/// What can go wrong if a recorded call of this tool is re-executed with its
/// original arguments. Empty string means read-only — safe to replay.
fn replay_risk(tool: &str) -> &'static str {
    match tool {
        "fs.delete" | "sys.rollback" => "deletes files",
        "fs.write" | "fs.append" | "fs.copy" | "fs.move" | "fs.mkdir" | "fs.writeMany"
        | "patch.apply" | "patch.applyMany" | "search.replace" => "rewrites files",
        "git.push" => "publishes to a remote",
        "git.add" | "git.commit" | "git.checkout" | "git.branch" | "git.cherryPick" | "git.tag"
        | "git.stash" | "git.pull" => "mutates the repository",
        "proc.kill" | "proc.stop" | "proc.start" | "proc.watch" => "controls processes",
        "proc.spawn" | "proc.runScript" | "proc.diff" | "test.run" => "executes code",
        "pkg.add" | "pkg.runScript" => "installs or runs packages",
        "env.set" => "mutates the session environment",
        "agent.register" | "agent.heartbeat" | "agent.post" | "agent.lock" | "agent.unlock"
        | "agent.compact" | "agent.resume" => "writes coordination state",
        "sys.doctor" | "sys.snapshot" => "writes kernel state",
        _ => "",
    }
}

pub fn register_replay(k: &mut Kernel) {
    k.register(
        "sys.replay",
        REPLAY_DESC,
        nct_core::schema::schema_for::<ReplayArgs>(),
        std::sync::Arc::new(ReplayHandler),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use nct_core::kernel::Kernel;

    fn make_kernel() -> Kernel {
        let dir = std::env::temp_dir().join(format!(
            "nct-replay-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut k = Kernel::new(dir).unwrap();
        register_replay(&mut k);
        k
    }

    #[test]
    fn lastN_keeps_the_tail_not_the_head() {
        let mut k = make_kernel();
        // a marker tool we can call N times to fill the journal
        k.register("marker.tool", "d", json!({}), std::sync::Arc::new(Noop));
        for i in 0..10 {
            let _ = k.call("marker.tool", &json!({ "n": i }));
        }
        let out = k.call("sys.replay", &json!({ "tool": "marker.tool", "lastN": 3 }));
        let r: Value = serde_json::from_value(out.result.unwrap()).unwrap();
        assert_eq!(r["replayed"], json!(3));
        // the tail must be the newest calls: the last one recorded was n=9
        let evs = r["events"].as_array().unwrap();
        let last_args = &evs.last().unwrap()["args"];
        assert!(
            last_args["n"].as_u64().is_some() && last_args["n"].as_u64().unwrap_or(0) >= 7,
            "lastN must keep the most recent calls, got: {evs:?}"
        );
    }

    #[test]
    fn irreversible_calls_are_skipped_unless_confirmed() {
        let mut k = make_kernel();
        // A handler registered under a DESTRUCTIVE name, so replay_risk flags
        // it without ever touching the real filesystem.
        k.register("fs.write", "d", json!({}), std::sync::Arc::new(Noop));
        let _ = k.call("fs.write", &json!({ "path": "x.txt" }));
        let out = k.call(
            "sys.replay",
            &json!({ "dryRun": false, "tool": "fs.write" }),
        );
        let r: Value = serde_json::from_value(out.result.unwrap()).unwrap();
        assert_eq!(
            r["skipped"],
            json!(1),
            "unconfirmed irreversible call must be skipped: {r}"
        );
        assert_eq!(r["replayed"], json!(0));
        assert_eq!(r["skippedEvents"][0]["tool"], json!("fs.write"));
        assert!(r["skippedEvents"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("rewrite"));
        // ...and executed when confirmed.
        let confirmed = k.call(
            "sys.replay",
            &json!({ "dryRun": false, "tool": "fs.write", "confirmWrite": true }),
        );
        let rc: Value = serde_json::from_value(confirmed.result.unwrap()).unwrap();
        assert_eq!(rc["skipped"], json!(0), "confirmWrite must execute: {rc}");
        assert_eq!(rc["replayed"], json!(1));
    }

    #[test]
    fn self_replay_is_refused_instead_of_recurse_ing() {
        let mut k = make_kernel();
        k.register("marker.tool", "d", json!({}), std::sync::Arc::new(Noop));
        let _ = k.call("marker.tool", &json!({}));
        // this call is itself journaled as sys.replay
        let out = k.call(
            "sys.replay",
            &json!({ "dryRun": false, "tool": "marker.tool" }),
        );
        let r: Value = serde_json::from_value(out.result.unwrap()).unwrap();
        assert_eq!(r["replayed"], json!(1));
        // now a later replay of the same range must NOT re-run sys.replay
        let out2 = k.call("sys.replay", &json!({ "dryRun": false }));
        let r2: Value = serde_json::from_value(out2.result.unwrap()).unwrap();
        let tools: Vec<String> = r2["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["tool"].as_str().unwrap_or("").to_string())
            .collect();
        assert!(
            !tools.contains(&"sys.replay".to_string()),
            "must not replay itself: {r2}"
        );
        let skipped_tools: Vec<String> = r2["skippedEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["tool"] == json!("sys.replay"))
            .map(|e| e["tool"].as_str().unwrap_or("").to_string())
            .collect();
        assert!(
            !skipped_tools.is_empty(),
            "sys.replay rows must be skipped explicitly: {r2}"
        );
    }

    #[test]
    fn read_only_calls_execute_without_confirmation() {
        let k = make_kernel();
        // sys.workspace is read-only; record it, then replay live.
        let _ = k.call("sys.workspace", &json!({}));
        let out = k.call(
            "sys.replay",
            &json!({ "dryRun": false, "tool": "sys.workspace" }),
        );
        let r: Value = serde_json::from_value(out.result.unwrap()).unwrap();
        assert_eq!(
            r["skipped"],
            json!(0),
            "read-only calls must not need confirmWrite: {r}"
        );
        assert_eq!(r["replayed"], json!(1));
    }

    #[test]
    fn divergence_counts_payload_diffs_not_just_okness() {
        let mut k = make_kernel();
        // A tool that returns ok:true but whose payload changes between
        // runs must be reported as diverged.
        struct Drift;
        impl Handler for Drift {
            fn call(&self, k: &Kernel, _args: &Value) -> Result<Value, ToolError> {
                Ok(json!({ "value": k.journal.events().len() }))
            }
        }
        k.register("drift.tool", "d", json!({}), std::sync::Arc::new(Drift));
        let _ = k.call("drift.tool", &json!({}));
        let out = k.call(
            "sys.replay",
            &json!({ "dryRun": false, "tool": "drift.tool" }),
        );
        let r: Value = serde_json::from_value(out.result.unwrap()).unwrap();
        assert_eq!(r["replayed"], json!(1));
        assert!(
            r["diverged"].as_u64().unwrap_or(0) >= 1,
            "payload drift must count as divergence: {r}"
        );
    }

    struct Noop;
    impl Handler for Noop {
        fn call(&self, _k: &Kernel, _args: &Value) -> Result<Value, ToolError> {
            Ok(json!({ "ok": true }))
        }
    }

    #[test]
    fn risk_classification_covers_the_destructive_surface() {
        for tool in [
            "fs.delete",
            "fs.write",
            "fs.writeMany",
            "fs.copy",
            "fs.move",
            "fs.append",
            "patch.apply",
            "patch.applyMany",
            "search.replace",
            "sys.rollback",
            "git.push",
            "git.commit",
            "git.checkout",
            "proc.kill",
            "proc.stop",
            "proc.spawn",
            "proc.runScript",
            "test.run",
            "pkg.add",
            "env.set",
        ] {
            assert!(!replay_risk(tool).is_empty(), "must be flagged: {tool}");
        }
        for tool in [
            "fs.read",
            "fs.stat",
            "fs.list",
            "fs.tree",
            "git.status",
            "git.log",
            "git.diff",
            "search.grep",
            "search.files",
            "search.semantic",
            "net.fetch",
            "net.search",
            "env.get",
            "env.list",
            "sys.journal",
            "sys.workspace",
            "sys.doctor",
        ] {
            // doctor is flagged (repair mode); everything else here is read-only
            let flagged = !replay_risk(tool).is_empty();
            assert!(
                !flagged || tool == "sys.doctor",
                "must be read-only: {tool}"
            );
        }
    }
}
