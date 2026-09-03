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
// It intentionally does NOT replay into the same journal/seq space (each replay
// call is itself journaled as sys.replay, so you get a trace of the replay).
// Safe by default, opt-in to execute. Restrictable to one tool or a seq range.
use serde::Deserialize;
use serde_json::{json, Value};

use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};

pub const REPLAY_DESC: &str = "Replay journal entries as tool calls (time-machine debugging). Reads tool.call events in a seq range, and by default (dryRun=true) shows what WOULD be re-executed with the original args + recorded result — no side effects. dryRun=false actually re-runs each call through the kernel and returns fresh results next to the originals so you can diff and find the divergence. Restrict with tool= / from= / to=. Each replay call is itself journaled.";

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
    #[serde(default)]
    #[schemars(range(min = 1, max = 500))]
    pub lastN: Option<u64>,
}

pub struct ReplayHandler;
impl Handler for ReplayHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: ReplayArgs = parse_args(args)?;
        let events = k.journal.events();
        let tool_filter = a.tool.as_deref();
        let last_n = a.lastN.unwrap_or(100) as usize;

        // Collect tool.call events in range, newest-last.
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
        calls.truncate(last_n.max(calls.len().saturating_sub(last_n)).min(calls.len()));

        let dry = a.dryRun.unwrap_or(true);
        let mut replayed: Vec<Value> = Vec::new();
        for c in &calls {
            let seq = c["seq"].as_u64().unwrap_or(0);
            let tool = c["tool"].as_str().unwrap_or("").to_string();
            let call_args = if c["args"].is_null() { json!({}) } else { c["args"].clone() };
            // Find the matching result event for comparison.
            let original = events
                .iter()
                .find(|e| e["kind"].as_str() == Some("tool.result") && e["callSeq"].as_u64() == Some(seq))
                .cloned()
                .unwrap_or(json!({}));

            if dry {
                replayed.push(json!({
                    "seq": seq,
                    "tool": tool,
                    "args": call_args,
                    "dryRun": true,
                    "originalResult": original["ok"],
                    "recordedResult": original["result"],
                }));
            } else {
                let out = k.call(&tool, &call_args);
                replayed.push(json!({
                    "seq": seq,
                    "tool": tool,
                    "args": call_args,
                    "ok": out.ok,
                    "fresh": out.result.clone().unwrap_or(Value::Null),
                    "freshError": out.error.map(|e| serde_json::to_value(e).unwrap_or(Value::Null)).unwrap_or(Value::Null),
                    "originalOk": original["ok"],
                    "recorded": original["result"],
                }));
            }
        }

        // Compute a quick divergence summary when actually replayed.
        let mut diverged = 0usize;
        if !dry {
            for r in &replayed {
                let fresh_ok = r["ok"].as_bool().unwrap_or(false);
                let orig_ok = r["originalOk"].as_bool().unwrap_or(false);
                if fresh_ok != orig_ok {
                    divged(r["seq"].as_u64().unwrap_or(0), &mut diverged);
                }
            }
        }

        Ok(json!({
            "replayed": replayed.len(),
            "dryRun": dry,
            "events": replayed,
            "diverged": diverged,
            "note": if dry { "dry run — no side effects; pass dryRun=false to re-execute" } else { "live replay executed through the kernel" },
        }))
    }
}

fn divged(_seq: u64, count: &mut usize) {
    *count += 1;
}

pub fn register_replay(k: &mut Kernel) {
    k.register("sys.replay", REPLAY_DESC, nct_core::schema::schema_for::<ReplayArgs>(), std::sync::Arc::new(ReplayHandler));
}
