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
    /// names are dotted (fs.stat). Several agent loops additionally render
    /// MCP tools as "<server-name>-<tool>" (Qwen sent `nc-tools-fs.read` for
    /// server "nc-tools", tool "fs.read") or as `mcp__<server>__<tool>__<name>`.
    /// Accept every wire form: exact match first, then underscore groups
    /// normalized to dots (fs_stat, git__status), then — universally — try
    /// progressively shorter prefixes (any head ending at `-` or `.`) until
    /// the remainder is a registered tool, so `anything-fs.read` /
    /// `nc_tools-fs.read` / `Nc_tool-net.http` all resolve while unknown
    /// tools still fall through to the ERR_UNKNOWN_TOOL path unchanged.
    pub fn resolve_tool(&self, name: &str) -> String {
        if self.tools.contains_key(name) {
            return name.to_string();
        }
        let normalized = name.replace("__", ".").replace('_', ".");
        if self.tools.contains_key(&normalized) {
            return normalized;
        }
        let bytes = normalized.as_bytes();
        for i in 0..bytes.len() {
            if bytes[i] == b'-' || bytes[i] == b'.' {
                let rest = &normalized[i + 1..];
                if self.tools.contains_key(rest) {
                    return rest.to_string();
                }
            }
        }
        name.to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

    struct Noop;
    impl Handler for Noop {
        fn call(&self, _k: &Kernel, _args: &Value) -> Result<Value, ToolError> {
            Ok(Value::Null)
        }
    }

    fn kernel_with_tools() -> Kernel {
        let dir = std::env::temp_dir().join(format!("nct-kernel-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut k = Kernel::new(dir).unwrap();
        for t in ["fs.read", "git.status", "net.http", "search.grep"] {
            k.register(t, "d", json!({}), Arc::new(Noop));
        }
        k
    }

    // Wire forms observed in real MCP clients: canonical dotted names, the
    // underscore variants, and a live Qwen session sending
    // "<server>-<tool>" (nc-tools-fs.read) plus mcp__-style renders.
    #[test]
    fn resolve_tool_accepts_observed_wire_forms() {
        let k = kernel_with_tools();
        for (sent, expected) in [
            ("fs.read", "fs.read"),               // canonical
            ("fs_read", "fs.read"),               // single underscore
            ("git__status", "git.status"),        // double underscore
            ("nc-tools-fs.read", "fs.read"),      // Qwen real: <server>-<tool>
            ("nctools-fs.read", "fs.read"),
            ("tools-fs.read", "fs.read"),
            ("tool-fs.read", "fs.read"),
            ("nc_tools-fs.read", "fs.read"),
            ("Nc_tool-fs.read", "fs.read"),
            ("nc-tools.fs.read", "fs.read"),      // <server>.<tool>
            ("mcp__nc-tools__fs__read", "fs.read"),
            ("mcp__other__git__status", "git.status"),
            ("nc-tools-net.http", "net.http"),
            ("anything-search.grep", "search.grep"),
        ] {
            assert_eq!(k.resolve_tool(sent), expected, "wire form failed: {sent}");
        }
    }

    #[test]
    fn resolve_tool_still_rejects_unknown_names() {
        let k = kernel_with_tools();
        for sent in ["fs.unknown", "read", "fs.readx", "nc-tool-read", "grep"] {
            let resolved = k.resolve_tool(sent);
            assert_eq!(resolved, sent, "unknown name must not be mis-resolved: {sent}");
        }
    }
}
