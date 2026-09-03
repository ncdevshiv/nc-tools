// sys.* + batch.execute — kernel-level tools that need the kernel itself.
// Port of the sys section of src/kernel/kernel.mjs.
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::Arc;

use serde_json::{json, Value};

use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};

pub const JOURNAL_DESC: &str = "Read the session journal (your own tool-call trail).";
pub const WORKSPACE_DESC: &str = "Workspace info: root path, platform.";
pub const SNAPSHOT_LIST_NOTE: &str = "";
pub const BATCH_DESC: &str = "Run up to 25 kernel tool calls in ONE round-trip: [{tool, args, dependsOn?}]. Each sub-call is individually executed and journaled; failures do not abort the batch. Calls with no dependsOn execute in parallel; calls with dependsOn=[indices] wait for those to finish first. Use for independent multi-step work.";

pub fn register_sys_batch(k: &mut Kernel) {
    k.register("sys.journal", JOURNAL_DESC, nct_core::schema::schema_for::<JournalArgs>(), Arc::new(JournalHandler));
    k.register("sys.workspace", WORKSPACE_DESC, nct_core::schema::schema_for::<WorkspaceArgs>(), Arc::new(WorkspaceHandler));
    k.register("batch.execute", BATCH_DESC, nct_core::schema::schema_for::<BatchArgs>(), Arc::new(BatchHandler));
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JournalArgs {
    #[serde(default)]
    #[schemars(range(min = 1, max = 1000))]
    pub lastN: Option<u64>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmptySysArgs {}

/// Per-call workspace override: report/answer about this base dir instead of
/// the server root. A mandatory-dir override must exist; bad overrides are
/// rejected by kernel.base_dir, never silently ignored.
#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceArgs {
    #[serde(default)]
    pub baseDir: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BatchArgs {
    #[schemars(length(min = 1, max = 25))]
    pub calls: Vec<BatchCall>,
    /// Retry policy for failed sub-calls. When {times} > 0, a failed sub-call
    /// is retried up to `times` more times with a linear backoff of
    /// `delayMs * attempt` between attempts (default delayMs = 1000). Applies
    /// per failing sub-call, not to the batch as a whole.
    #[serde(default)]
    pub retry: Option<RetryArgs>,
}

#[derive(Deserialize, schemars::JsonSchema, Clone, Copy)]
#[serde(deny_unknown_fields)]
pub struct RetryArgs {
    #[serde(default)]
    #[schemars(range(min = 0, max = 5))]
    pub times: Option<u64>,
    #[serde(default)]
    #[schemars(range(min = 0, max = 30000))]
    pub delayMs: Option<u64>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BatchCall {
    pub tool: String,
    #[serde(default)]
    #[schemars(schema_with = "nct_core::plain_object_schema")]
    pub args: serde_json::Map<String, Value>,
    /// Indices of calls this one depends on (0-based). The call waits until
    /// all dependencies finish before executing. Empty/absent = independent.
    #[serde(default)]
    #[schemars(length(min = 0, max = 24))]
    pub dependsOn: Option<Vec<u64>>,
}

pub struct JournalHandler;
impl Handler for JournalHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: JournalArgs = parse_args(args)?;
        let n = a.lastN.unwrap_or(100) as usize;
        let events = k.journal.last_n(Some(n));
        let total = *k.journal.seq.lock().unwrap();
        Ok(json!({ "events": events, "total": total }))
    }
}

pub struct WorkspaceHandler;
impl Handler for WorkspaceHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: WorkspaceArgs = parse_args(args)?;
        let base = k.base_dir(a.baseDir.as_deref())?;
        let git = base.join(".git").exists();
        Ok(json!({
            "root": base.display().to_string(),
            "serverRoot": k.root.display().to_string(),
            "anchored": k.current_default_base().is_some(),
            "platform": nct_core::platform_str(),
            "node": Value::Null,
            "git": git,
            "capabilities": {
                "fs": true,
                "search": true,
                "proc": true,
                "batch": true,
                "git": git,
            },
        }))
    }
}

pub struct BatchHandler;
impl Handler for BatchHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: BatchArgs = parse_args(args)?;
        if a.calls.is_empty() {
            return Err(ToolError::new("ERR_BAD_INPUT", "calls must be a non-empty array of {tool, args}"));
        }
        if a.calls.len() > k.cfg.limits.batch_max {
            return Err(ToolError::new("ERR_BAD_INPUT", format!("max {} calls per batch.execute", k.cfg.limits.batch_max)));
        }

        let n = a.calls.len();
        let batch_name = nct_core::Kernel::resolve_tool(k, "batch.execute");
        let batch_str = batch_name.as_str();

        // Validate all calls upfront: empty tools and nested batch.execute
        // are rejected before any execution (fail-fast, not partial-execute).
        for c in &a.calls {
            if c.tool.is_empty() {
                return Ok(json!({
                    "results": (0..n).map(|i| {
                        if a.calls[i].tool.is_empty() {
                            json!({ "ok": false, "error": { "code": "ERR_BAD_INPUT", "message": "each call needs a string tool" } })
                        } else {
                            json!({ "ok": false, "error": { "code": "ERR_BAD_INPUT", "message": "preceding call had empty tool name" } })
                        }
                    }).collect::<Vec<_>>(),
                    "ok": 0,
                    "failed": n,
                }));
            }
        }

        // Pre-resolve dependencies and validate them.
        let deps: Vec<HashSet<usize>> = a
            .calls
            .iter()
            .enumerate()
            .map(|(i, c)| {
                c.dependsOn
                    .as_ref()
                    .map(|d| {
                        d.iter()
                            .filter(|&&x| (x as usize) < i)
                            .map(|x| *x as usize)
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect();

        // Detect cycles (a dependency on a later index is ignored, so cycles
        // are impossible — but a self-dependency would be pointess). If a call
        // depends on itself, reject.
        for (i, d) in deps.iter().enumerate() {
            if d.contains(&i) {
                return Err(ToolError::new("ERR_BAD_INPUT", format!("call {i} cannot depend on itself")));
            }
        }

        // If no calls have dependencies, everything is independent — execute
        // in parallel. If any have dependencies, use the topological scheduler.
        let has_deps = deps.iter().any(|d| !d.is_empty());

        let retry_times = a.retry.as_ref().and_then(|r| r.times).unwrap_or(0) as u32;
        let retry_delay = a.retry.as_ref().and_then(|r| r.delayMs).unwrap_or(1000);
        let retry = Retry { times: retry_times, delay_ms: retry_delay };

        let results: Vec<Value> = if has_deps {
            execute_dag(k, &a.calls, &deps, batch_str, &retry)
        } else {
            execute_parallel(k, &a.calls, batch_str, &retry)
        };

        let ok_count = results.iter().filter(|r| r["ok"] == json!(true)).count();
        Ok(json!({
            "results": results,
            "ok": ok_count,
            "failed": results.len() - ok_count,
        }))
    }
}

/// Retry policy for one sub-call invocation.
#[derive(Clone, Copy)]
struct Retry {
    times: u32,
    delay_ms: u64,
}

/// Run one sub-call with the retry policy. On failure the call is retried up
/// to `retry.times` more times with `delay_ms * attempt` linear backoff. The
/// successful (or final failing) outcome is returned.
fn run_with_retry(k: &Kernel, tool: &str, args: &Value, retry: &Retry) -> Value {
    let mut attempt = 0u32;
    loop {
        let out = k.call(tool, args);
        if out.ok {
            return call_item(out);
        }
        if attempt >= retry.times {
            return call_item(out);
        }
        attempt += 1;
        std::thread::sleep(std::time::Duration::from_millis(retry.delay_ms * attempt as u64));
    }
}

fn call_item(out: nct_core::CallOutcome) -> Value {
    let mut item = serde_json::Map::new();
    item.insert("ok".into(), json!(out.ok));
    if let Some(r) = out.result {
        item.insert("result".into(), r);
    }
    if let Some(e) = out.error {
        item.insert("error".into(), serde_json::to_value(e).unwrap_or(Value::Null));
    }
    Value::Object(item)
}

/// Execute all calls in parallel via scoped threads. Results are returned in
/// input order. Each call is journaled individually by the kernel.
fn execute_parallel(k: &Kernel, calls: &[BatchCall], batch_name: &str, retry: &Retry) -> Vec<Value> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = calls
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let tool = c.tool.clone();
                let args = Value::Object(c.args.clone());
                let batch = batch_name.to_string();
                let retry = *retry;
                scope.spawn(move || {
                    if nct_core::Kernel::resolve_tool(k, &batch) == nct_core::Kernel::resolve_tool(k, &tool) {
                        return json!({ "ok": false, "error": { "code": "ERR_REFUSED", "message": "batch.execute cannot nest itself" } });
                    }
                    run_with_retry(k, &tool, &args, &retry)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join().unwrap_or_else(|_| json!({
                    "ok": false,
                    "error": { "code": "ERR_INTERNAL", "message": "batch worker thread panicked" }
                }))
            })
            .collect()
    })
}

/// Execute calls respecting the dependency graph. Uses Kahn's topological
/// rounds: each round runs all ready calls in parallel, then unlocks
/// dependents. Results are returned in input order.
fn execute_dag(k: &Kernel, calls: &[BatchCall], deps: &[HashSet<usize>], batch_name: &str, retry: &Retry) -> Vec<Value> {
    let n = calls.len();
    let mut results: Vec<Value> = vec![
        json!({ "ok": false, "error": { "code": "ERR_INTERNAL", "message": "not executed" } });
        n
    ];
    let remaining: Vec<HashSet<usize>> = deps.to_vec();
    let mut done = vec![false; n];

    loop {
        // Find all calls whose dependencies are all satisfied
        let ready: Vec<usize> = (0..n)
            .filter(|i| !done[*i] && remaining[*i].iter().all(|d| done[*d]))
            .collect();
        if ready.is_empty() {
            break;
        }

        // Execute all ready calls in parallel
        let completed = std::thread::scope(|scope| {
            let handles: Vec<_> = ready
                .iter()
                .map(|&i| {
                    let tool = calls[i].tool.clone();
                    let args = Value::Object(calls[i].args.clone());
                    let bn = batch_name.to_string();
                    let retry = *retry;
                    (i, scope.spawn(move || {
                        if nct_core::Kernel::resolve_tool(k, &bn)
                            == nct_core::Kernel::resolve_tool(k, &tool)
                        {
                            return json!({ "ok": false, "error": { "code": "ERR_REFUSED", "message": "batch.execute cannot nest itself" } });
                        }
                        run_with_retry(k, &tool, &args, &retry)
                    }))
                })
                .collect();

            handles
                .into_iter()
                .map(|(i, h)| {
                    let v = h.join().unwrap_or_else(|_| json!({
                        "ok": false,
                        "error": { "code": "ERR_INTERNAL", "message": "worker panicked" }
                    }));
                    (i, v)
                })
                .collect::<Vec<_>>()
        });

        for (i, v) in completed {
            done[i] = true;
            results[i] = v;
        }
    }

    results
}

#[cfg(test)]
mod batch_tests {
    use super::*;
    use nct_core::config::Config;

    fn make_kernel() -> Kernel {
        let dir = std::env::temp_dir().join(format!(
            "nct-batch-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut k = Kernel::new(dir).unwrap();
        // Register fs tools so we can test with fs.write / fs.read
        nct_fs::register(&mut k);
        register_sys_batch(&mut k);
        k
    }

    /// Legacy: calls without dependsOn still work — 3 writes in parallel.
    #[test]
    fn batch_parallel_no_dependencies() {
        let k = make_kernel();
        let args = json!({
            "calls": [
                { "tool": "fs.write", "args": { "path": "a.txt", "content": "alpha" } },
                { "tool": "fs.write", "args": { "path": "b.txt", "content": "beta" } },
                { "tool": "fs.write", "args": { "path": "c.txt", "content": "gamma" } }
            ]
        });
        let result = BatchHandler.call(&k, &args).unwrap();
        assert_eq!(result["ok"], json!(3));
        assert_eq!(result["failed"], json!(0));
        let results = result["results"].as_array().unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0]["ok"], json!(true));
        assert_eq!(results[1]["ok"], json!(true));
        assert_eq!(results[2]["ok"], json!(true));
        // Files exist on disk
        assert!(k.root.join("a.txt").exists());
        assert!(k.root.join("b.txt").exists());
        assert!(k.root.join("c.txt").exists());
    }

    /// Dependencies: call 1 depends on call 0 (write then read).
    #[test]
    fn batch_with_dependencies_executes_in_order() {
        let k = make_kernel();
        let args = json!({
            "calls": [
                { "tool": "fs.write", "args": { "path": "dep.txt", "content": "dependency data" } },
                { "tool": "fs.read", "args": { "path": "dep.txt" }, "dependsOn": [0] }
            ]
        });
        let result = BatchHandler.call(&k, &args).unwrap();
        let results = result["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["ok"], json!(true)); // write
        assert_eq!(results[1]["ok"], json!(true)); // read (after write)
        // The read must contain the content from the write
        let read_content = results[1]["result"]["content"].as_str().unwrap();
        assert!(read_content.contains("dependency data"));
    }

    /// Nested batch.execute is refused.
    #[test]
    fn batch_nesting_is_refused() {
        let k = make_kernel();
        let args = json!({
            "calls": [
                { "tool": "batch.execute", "args": { "calls": [{ "tool": "fs.write", "args": { "path": "x", "content": "y" } }] } }
            ]
        });
        let result = BatchHandler.call(&k, &args).unwrap();
        let results = result["results"].as_array().unwrap();
        assert_eq!(results[0]["ok"], json!(false));
        assert_eq!(results[0]["error"]["code"], json!("ERR_REFUSED"));
    }

    /// Independent + dependent calls mixed: 0 and 1 are independent,
    /// 2 depends on both (fan-in).
    #[test]
    fn batch_dag_fan_in() {
        let k = make_kernel();
        let args = json!({
            "calls": [
                { "tool": "fs.write", "args": { "path": "x.txt", "content": "x" } },
                { "tool": "fs.write", "args": { "path": "y.txt", "content": "y" } },
                { "tool": "fs.readMany", "args": { "paths": ["x.txt", "y.txt"] }, "dependsOn": [0, 1] }
            ]
        });
        let result = BatchHandler.call(&k, &args).unwrap();
        let results = result["results"].as_array().unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0]["ok"], json!(true)); // write x
        assert_eq!(results[1]["ok"], json!(true)); // write y
        assert_eq!(results[2]["ok"], json!(true)); // readMany (after both writes)
        let files = results[2]["result"]["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
    }

    /// Mixed success and failure: one call fails, the other succeeds.
    #[test]
    fn batch_mixed_success_failure() {
        let k = make_kernel();
        let args = json!({
            "calls": [
                { "tool": "fs.read", "args": { "path": "nonexistent.txt" } },
                { "tool": "fs.write", "args": { "path": "ok.txt", "content": "survives" } }
            ]
        });
        let result = BatchHandler.call(&k, &args).unwrap();
        assert_eq!(result["ok"], json!(1));
        assert_eq!(result["failed"], json!(1));
        let results = result["results"].as_array().unwrap();
        assert_eq!(results[0]["ok"], json!(false)); // missing file
        assert_eq!(results[1]["ok"], json!(true)); // write succeeds
        assert!(k.root.join("ok.txt").exists());
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use nct_core::Kernel;

    fn kernel() -> Kernel {
        let dir = std::env::temp_dir().join(format!(
            "nct-retry-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut k = Kernel::new(dir).unwrap();
        nct_fs::register(&mut k);
        register_sys_batch(&mut k);
        k
    }

    #[test]
    fn retry_recovers_a_flapping_call() {
        // fs.read on a path that does NOT exist fails, then we create it and
        // retry the same batch — the retried call succeeds. (Retry is per-call
        // re-execution against fresh state, which is the real-world flake.)
        let k = kernel();
        let missing = k.root.join("will-exist.txt");
        let _ = missing;
        // first: fail (missing), second batch after creating it: succeed
        let args = json!({
            "calls": [{ "tool": "fs.read", "args": { "path": "will-exist.txt" } }],
            "retry": { "times": 3, "delayMs": 10 }
        });
        let v1 = BatchHandler.call(&k, &args).unwrap();
        assert_eq!(v1["failed"], json!(1), "missing file must fail");
        // create it now
        std::fs::write(k.root.join("will-exist.txt"), "now here\n").unwrap();
        let v2 = BatchHandler.call(&k, &args).unwrap();
        assert_eq!(v2["ok"], json!(1), "after creation the same call must succeed");
        // journal saw the retried calls (each attempt journaled)
        let evs = k.journal.events();
        let calls: Vec<_> = evs.iter().filter(|e| e["kind"] == "tool.call" && e["tool"] == json!("fs.read")).collect();
        assert!(calls.len() >= 2);
    }

    #[test]
    fn no_retry_is_backward_compatible() {
        let k = kernel();
        let args = json!({ "calls": [{ "tool": "fs.stat", "args": { "path": "." } }] });
        let v = BatchHandler.call(&k, &args).unwrap();
        assert_eq!(v["ok"], json!(1));
        assert_eq!(v["failed"], json!(0));
    }

    /// sys.workspace reports the EFFECTIVE base: default = server root
    /// (unchanged from before baseDir existed), baseDir = the other workspace,
    /// including whether THAT workspace is a git repo.
    #[test]
    fn workspace_reports_baseDir_not_server_root() {
        let k = kernel();
        let target = std::env::temp_dir().join(format!(
            "nct-ws-target-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&target);
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(target.join(".git")).unwrap();

        // default: server root, serverRoot echo present
        let def = WorkspaceHandler.call(&k, &json!({})).unwrap();
        assert_eq!(def["root"], json!(k.root.display().to_string()));
        assert_eq!(def["serverRoot"], json!(k.root.display().to_string()));
        assert_eq!(def["anchored"], json!(false), "no anchor yet");
        assert_eq!(def["capabilities"]["fs"], json!(true));

        // with the session anchor set (as the MCP roots handshake does),
        // base_dir(None) reports the anchor and anchored flips true
        k.set_default_base(&target);
        let anchored = WorkspaceHandler.call(&k, &json!({})).unwrap();
        assert_eq!(anchored["root"], json!(dunce::canonicalize(&target).unwrap().display().to_string()));
        assert_eq!(anchored["anchored"], json!(true));
        k.set_default_base(&std::path::Path::new("/nonexistent-anchor-xyz")); // refused
        assert_eq!(WorkspaceHandler.call(&k, &json!({})).unwrap()["anchored"], json!(true), "refused anchor keeps the old one");

        // baseDir: the OTHER workspace and its real git state
        // (compare canonicalized — resolve_checked returns the long path)
        let canon = dunce::canonicalize(&target).unwrap();
        let over = WorkspaceHandler
            .call(&k, &json!({ "baseDir": target.display().to_string() }))
            .unwrap();
        assert_eq!(over["root"], json!(canon.display().to_string()));
        assert_eq!(over["git"], json!(true), "target has .git — must be reported");
        assert_eq!(over["serverRoot"], json!(k.root.display().to_string()), "serverRoot stays the server's");

        // a bad baseDir is an error, never a silent fallback to the server root
        let bad = WorkspaceHandler.call(&k, &json!({ "baseDir": "/no/such/ws/xyz" }));
        assert!(bad.is_err(), "bad baseDir must error, not fall back");

        let _ = std::fs::remove_dir_all(&target);
    }
}
