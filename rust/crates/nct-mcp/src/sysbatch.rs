// sys.* + batch.execute — kernel-level tools that need the kernel itself.
// Port of the sys section of src/kernel/kernel.mjs.
use std::sync::Arc;

use serde_json::{json, Value};

use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};

pub const JOURNAL_DESC: &str = "Read the session journal (your own tool-call trail).";
pub const WORKSPACE_DESC: &str = "Workspace info: root path, platform.";
pub const SNAPSHOT_LIST_NOTE: &str = "";
pub const BATCH_DESC: &str = "Run up to 25 kernel tool calls in ONE round-trip: [{tool, args}]. Each sub-call is individually executed and journaled; failures do not abort the batch. Use for independent multi-step work.";

pub fn register_sys_batch(k: &mut Kernel) {
    k.register("sys.journal", JOURNAL_DESC, nct_core::schema::schema_for::<JournalArgs>(), Arc::new(JournalHandler));
    k.register("sys.workspace", WORKSPACE_DESC, nct_core::schema::schema_for::<EmptySysArgs>(), Arc::new(WorkspaceHandler));
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

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BatchArgs {
    #[schemars(length(min = 1, max = 25))]
    pub calls: Vec<BatchCall>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BatchCall {
    pub tool: String,
    #[serde(default)]
    #[schemars(schema_with = "nct_core::plain_object_schema")]
    pub args: serde_json::Map<String, Value>,
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
    fn call(&self, k: &Kernel, _args: &Value) -> Result<Value, ToolError> {
        let git = k.root.join(".git").exists();
        Ok(json!({
            "root": k.root.display().to_string(),
            "platform": nct_core::platform_str(),
            "node": Value::Null, // Rust kernel: no node runtime; field kept for shape parity
            "git": git,
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
        let mut results = Vec::new();
        for c in &a.calls {
            if c.tool.is_empty() {
                results.push(json!({ "ok": false, "error": { "code": "ERR_BAD_INPUT", "message": "each call needs a string tool" } }));
                continue;
            }
            if nct_core::Kernel::resolve_tool(k, "batch.execute") == nct_core::Kernel::resolve_tool(k, &c.tool) {
                results.push(json!({ "ok": false, "error": { "code": "ERR_REFUSED", "message": "batch.execute cannot nest itself" } }));
                continue;
            }
            let out = k.call(&c.tool, &Value::Object(c.args.clone()));
            let mut item = serde_json::Map::new();
            item.insert("ok".into(), json!(out.ok));
            if let Some(r) = out.result {
                item.insert("result".into(), r);
            }
            if let Some(e) = out.error {
                item.insert("error".into(), serde_json::to_value(e).unwrap_or(Value::Null));
            }
            results.push(Value::Object(item));
        }
        let ok_count = results.iter().filter(|r| r["ok"] == json!(true)).count();
        Ok(json!({
            "results": results,
            "ok": ok_count,
            "failed": results.len() - ok_count,
        }))
    }
}
