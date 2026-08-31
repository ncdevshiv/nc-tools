// Tool registry + kernel — mirrors src/kernel/kernel.mjs. One entry per tool
// carrying its descriptor (name, description, input schema) and a handler
// that receives the kernel (for root/cfg/session-env/journal/registry access).
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Value};

use crate::config::Config;
use crate::errors::ToolError;
use crate::journal::Journal;
use crate::session::SessionEnv;

/// One tool: descriptor (advertised over MCP) + executable handler.
pub struct ToolEntry {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub handler: Arc<dyn Handler>,
}

/// Handlers are synchronous; long operations use threads (proc.start) or
/// bounded waits (proc.spawn). The kernel serializes calls per connection,
/// matching the JS server's request-at-a-time behavior.
pub trait Handler: Send + Sync {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError>;
}

/// Typed deserialization helper for handlers: args must match the tool's
/// declared input schema; failures surface as ERR_BAD_INPUT with the
/// serde path-precise message.
pub fn parse_args<T: serde::de::DeserializeOwned>(args: &Value) -> Result<T, ToolError> {
    serde_json::from_value::<T>(args.clone())
        .map_err(|e| ToolError::new("ERR_BAD_INPUT", format!("invalid arguments: {e}")))
}

/// Fault-injection hook (chaos harness): returns Some(error) to inject a
/// journaled failure, None to proceed. Injected failures are journaled like
/// real ones so recovery from them is measurable.
pub type Hook = Arc<dyn Fn(&str, &Value) -> Option<ToolError> + Send + Sync>;

#[derive(Debug, Clone, Serialize)]
pub struct CallOutcome {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolError>,
    pub duration_ms: u64,
    pub seq: u64,
}

pub struct Kernel {
    pub root: PathBuf,
    pub cfg: Config,
    pub journal: Journal,
    pub session_env: SessionEnv,
    /// Session id: groups journal events from one server process.
    pub sid: String,
    tools: BTreeMap<String, ToolEntry>,
    hooks: Mutex<Vec<Hook>>,
}

impl Kernel {
    pub fn new(root: PathBuf) -> Result<Kernel, ToolError> {
        let cfg = Config::load(&root);
        let journal = Journal::new(root.join(".nc-tools").join("journal.jsonl"))?;
        Ok(Kernel {
            root,
            cfg,
            journal,
            session_env: SessionEnv::new(),
            sid: session_id(),
            tools: BTreeMap::new(),
            hooks: Mutex::new(Vec::new()),
        })
    }

    pub fn register(&mut self, name: &str, description: &str, input_schema: Value, handler: Arc<dyn Handler>) {
        self.tools.insert(name.to_string(), ToolEntry {
            name: name.to_string(),
            description: description.to_string(),
            input_schema,
            handler,
        });
    }

    pub fn add_hook(&mut self, hook: Hook) {
        self.hooks.lock().unwrap().push(hook);
    }

    pub fn list_tools(&self) -> Vec<String> {
        self.tools.keys().cloned().collect() // BTreeMap = sorted, like JS listTools()
    }

    /// MCP clients expose kernel names with underscores (fs_stat); kernel
    /// names are dotted (fs.stat). Accept both.
    pub fn resolve_tool(&self, name: &str) -> String {
        if self.tools.contains_key(name) {
            return name.to_string();
        }
        // Wire aliases: single underscore (fs_stat) and the double-underscore
        // wire form (git__status, what agent loops emit for providers that
        // forbid dotted names). Try __ -> . first, then _ -> .
        let double = name.replace("__", ".");
        if self.tools.contains_key(&double) {
            return double;
        }
        let dotted = name.replace('_', ".");
        if self.tools.contains_key(&dotted) {
            dotted
        } else {
            name.to_string()
        }
    }

    pub fn descriptors(&self) -> Vec<Value> {
        self.tools
            .values()
            .map(|t| json!({ "name": t.name, "description": t.description, "inputSchema": t.input_schema }))
            .collect()
    }

    /// Execute one tool call with journaling (kernel.mjs call()).
    pub fn call(&self, tool: &str, args: &Value) -> CallOutcome {
        let started = Instant::now();
        let tool_name = self.resolve_tool(tool);
        let call_seq = match self.journal.append("tool.call", json!({
            "tool": tool_name,
            "args": args,
            "sid": self.sid,
        })) {
            Ok(ev) => ev.seq,
            Err(e) => {
                return CallOutcome {
                    ok: false,
                    result: None,
                    error: Some(e),
                    duration_ms: started.elapsed().as_millis() as u64,
                    seq: 0,
                }
            }
        };
        let out = self.dispatch(&tool_name, args);
        let duration_ms = started.elapsed().as_millis() as u64;
        let (ok, result, error) = match out {
            Ok(v) => (true, Some(v), None),
            Err(e) => (false, None, Some(e)),
        };
        let seq = match self.journal.append("tool.result", json!({
            "tool": tool_name,
            "ok": ok,
            "result": result,
            "error": error.as_ref().map(|e| serde_json::to_value(e).unwrap_or(Value::Null)),
            "durationMs": duration_ms,
            "callSeq": call_seq,
            "sid": self.sid,
        })) {
            Ok(ev) => ev.seq,
            Err(_) => 0,
        };
        CallOutcome { ok, result, error, duration_ms, seq }
    }

    fn dispatch(&self, tool_name: &str, args: &Value) -> Result<Value, ToolError> {
        let Some(entry) = self.tools.get(tool_name) else {
            return Err(ToolError::with_hint(
                "ERR_UNKNOWN_TOOL",
                format!("Unknown tool: {tool_name}"),
                json!({ "available": self.list_tools() }),
            ));
        };
        for hook in self.hooks.lock().unwrap().iter() {
            if let Some(injected) = hook(tool_name, args) {
                return Err(injected);
            }
        }
        entry.handler.call(self, args)
    }
}

/// Short session id from time+pid — stable per server process, no external rng.
fn session_id() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{:x}-{:x}", ms, std::process::id())
}
