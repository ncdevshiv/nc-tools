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
/// serde path-precise message plus a field-level hint (`missing` /
/// `unknownField` / `expected`) so the agent can fix the call in one turn.
pub fn parse_args<T: serde::de::DeserializeOwned>(args: &Value) -> Result<T, ToolError> {
    serde_json::from_value::<T>(args.clone()).map_err(|e| {
        let msg = e.to_string();
        let mut hint = serde_json::Map::new();
        for (marker, key) in
            [("missing field `", "missing"), ("unknown field `", "unknownField")]
        {
            if let Some(i) = msg.find(marker) {
                let rest = &msg[i + marker.len()..];
                if let Some(end) = rest.find('`') {
                    hint.insert(key.to_string(), json!(rest[..end].to_string()));
                    break;
                }
            }
        }
        if let Some(i) = msg.find("expected ") {
            let expected = msg[i + "expected ".len()..].trim_end();
            if !expected.is_empty() {
                hint.insert("expected".to_string(), json!(expected));
            }
        }
        let err = ToolError::new(crate::errors::codes::BAD_INPUT, format!("invalid arguments: {msg}"));
        if hint.is_empty() { err } else { err.with_value_hint(hint) }
    })
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
        self.hooks.lock().unwrap_or_else(|p| p.into_inner()).push(hook);
    }

    pub fn list_tools(&self) -> Vec<String> {
        self.tools.keys().cloned().collect() // BTreeMap = sorted, like JS listTools()
    }

    /// Resolve the effective base directory for a path-resolving tool. When a
    /// caller passes an explicit `baseDir` (per-call workspace override) that
    /// resolves, it wins over the session root — so an agent bound to one
    /// workspace can still read/observe another without re-rooting the server.
    /// The override must exist; a bad override is an error, not a silent
    /// fallback to the session root (that is precisely the "results come back
    /// relative to the wrong workspace" bug).
    pub fn base_dir(&self, override_dir: Option<&str>) -> Result<PathBuf, ToolError> {
        match override_dir {
            Some(d) if !d.is_empty() => crate::paths::resolve_checked(&self.root, d),
            _ => Ok(self.root.clone()),
        }
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
        // Case only slipped by the caller (FS_READ): tool names are lowercase.
        let lower = name.to_lowercase();
        if self.tools.contains_key(&lower) {
            return lower;
        }
        let normalized = name.replace("__", ".").replace('_', ".");
        if self.tools.contains_key(&normalized) {
            return normalized;
        }
        // Case slipped through the underscore normalization too (FS_READ).
        let norm_lower = normalized.to_lowercase();
        if self.tools.contains_key(&norm_lower) {
            return norm_lower;
        }
        let bytes = normalized.as_bytes();
        for i in 0..bytes.len() {
            if bytes[i] == b'-' || bytes[i] == b'.' {
                let rest = &normalized[i + 1..];
                if self.tools.contains_key(rest) {
                    return rest.to_string();
                }
                let rest_lower = rest.to_lowercase();
                if self.tools.contains_key(&rest_lower) {
                    return rest_lower;
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
            return Err(self.unknown_tool_error(tool_name));
        };
        // The hooks guard MUST be dropped before the handler runs: handlers
        // legitimately re-enter dispatch (batch sub-calls, composite tools) —
        // holding the lock across the call deadlocks them.
        {
            let hooks = self.hooks.lock().unwrap_or_else(|p| p.into_inner());
            for hook in hooks.iter() {
                if let Some(injected) = hook(tool_name, args) {
                    return Err(injected);
                }
            }
        }
        // Panic boundary: a handler bug must surface as a structured,
        // retryable error (ERR_PANIC) — never kill the server process, which
        // over stdio would strand the agent with a dead pipe and no code.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| entry.handler.call(self, args))) {
            Ok(result) => result,
            Err(payload) => Err(panic_error(tool_name, payload.as_ref())),
        }
    }

    /// Unknown-tool error with remediation: the full surface, the closest
    /// name when the sent one looks like a typo, and the accepted wire forms
    /// — enough for a calling agent to fix its next attempt in one turn.
    fn unknown_tool_error(&self, sent: &str) -> ToolError {
        let mut hint = json!({ "available": self.list_tools() });
        let mut message = format!("Unknown tool: {sent}");
        let normalized = sent.to_lowercase().replace("__", ".").replace(['_', '-'], ".");
        let mut best: Option<(&str, usize)> = None;
        for name in self.tools.keys() {
            let d = levenshtein(&normalized, name);
            if best.is_none_or(|(_, bd)| d < bd) {
                best = Some((name, d));
            }
        }
        if let Some((name, d)) = best {
            let threshold = std::cmp::max(2, name.len() / 3);
            if d <= threshold {
                let mcp = format!("mcp__{}__{}", crate::MCP_SERVER_NAME, name.replace('.', "_"));
                message.push_str(&format!(
                    " — did you mean '{name}'? Accepted wire forms: '{name}', '{}', '{mcp}'",
                    name.replace('.', "_")
                ));
                hint["didYouMean"] = json!(name);
                hint["retryWith"] = json!(mcp);
            }
        }
        // Server-prefixed wire form with no tool after the prefix (the
        // `mcp__nc-tools=` class): the fix is the format itself, not a guess.
        // Any non-alphanumeric is a separator here — stray `=`/`:` must not
        // fuse with the last token.
        const SERVER_HEADS: [&str; 7] = ["mcp", "nc", "tools", "tool", "nctools", "kernel", "server"];
        let clean: String = normalized
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '.' })
            .collect();
        let tokens: Vec<&str> = clean.split('.').filter(|t| !t.is_empty()).collect();
        let server_only = !tokens.is_empty() && tokens.iter().all(|t| SERVER_HEADS.contains(t));
        if server_only {
            message.push_str(&format!(
                " — no tool name after the server prefix; use mcp__{}__<toolname> \
                 (dots become underscores), e.g. mcp__{}__fs_read or mcp__{}__net_fetch",
                crate::MCP_SERVER_NAME, crate::MCP_SERVER_NAME, crate::MCP_SERVER_NAME
            ));
            hint["wireForm"] = json!(format!("mcp__{}__<toolname>", crate::MCP_SERVER_NAME));
            hint["examples"] = json!([
                format!("mcp__{}__fs_read", crate::MCP_SERVER_NAME),
                format!("mcp__{}__net_fetch", crate::MCP_SERVER_NAME),
                format!("mcp__{}__batch_execute", crate::MCP_SERVER_NAME),
            ]);
        } else if hint.get("didYouMean").is_none() {
            message.push_str(
                " — pick a name from hint.available; MCP clients may render it as \
                 mcp__<server>__<name> or <name> with dots as underscores",
            );
        }
        ToolError::with_hint(crate::errors::codes::UNKNOWN_TOOL, message, hint)
    }
}

/// A caught panic becomes a first-class ToolError: code, the panic message
/// itself (the exact root cause), and a note that the server survived.
fn panic_error(tool: &str, payload: &(dyn std::any::Any + Send)) -> ToolError {
    let msg = if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "opaque panic payload".to_string()
    };
    ToolError::with_hint(
        crate::errors::codes::PANIC,
        format!("tool '{tool}' panicked: {msg}"),
        json!({
            "tool": tool,
            "note": "internal defect, server recovered; stderr log has the location; retry or use another tool"
        }),
    )
}

/// Levenshtein edit distance, two-row DP, for did-you-mean suggestions.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let sub = prev[j - 1] + usize::from(a[i - 1] != b[j - 1]);
            cur[j] = usize::min(usize::min(prev[j] + 1, cur[j - 1] + 1), sub);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Short session id from time+pid+a monotonic counter, stable per server
/// process. The counter guarantees two kernels constructed in the same process
/// at the same millisecond still get DISTINCT sids (time+pid alone collides —
/// which would wrongly resume one agent's identity onto another server).
fn session_id() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let c = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    format!("{:x}-{:x}-{:x}", ms, std::process::id(), c)
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

    #[test]
    fn resolve_tool_accepts_case_slips() {
        let k = kernel_with_tools();
        assert_eq!(k.resolve_tool("FS_READ"), "fs.read");
        assert_eq!(k.resolve_tool("Fs.Read"), "fs.read");
        assert_eq!(k.resolve_tool("fs_READ"), "fs.read");
        // mcp render + case combined: head stripped, remainder case-fixed
        assert_eq!(k.resolve_tool("MCP__NC-TOOLS__GIT__STATUS"), "git.status");
        // resolving must be exact-tool, not fuzzy: readx ≠ read, any case
        assert_eq!(k.resolve_tool("FS_READX"), "FS_READX");
    }

    struct Panicky;
    impl Handler for Panicky {
        fn call(&self, _k: &Kernel, _args: &Value) -> Result<Value, ToolError> {
            panic!("injected boom: root cause proof");
        }
    }

    #[test]
    fn panicking_handler_becomes_err_panic_and_kernel_survives() {
        let mut k = kernel_with_tools();
        k.register("boom.tool", "d", json!({}), Arc::new(Panicky));
        let out = k.call("boom.tool", &json!({}));
        assert!(!out.ok);
        let e = out.error.expect("panic must produce an error");
        assert_eq!(e.code, "ERR_PANIC", "got: {e:?}");
        assert!(e.message.contains("injected boom: root cause proof"), "panic message must be the root cause: {}", e.message);
        // the kernel must still be fully functional afterwards
        assert!(k.call("fs.read", &json!({})).result.is_some() || k.call("fs.read", &json!({})).error.is_some());
        let names = k.list_tools();
        assert!(names.contains(&"boom.tool".to_string()));
    }

    #[test]
    fn unknown_tool_error_carries_did_you_mean_and_retry() {
        let k = kernel_with_tools();
        let out = k.call("fs.rea", &json!({}));
        let e = out.error.expect("must error");
        assert_eq!(e.code, "ERR_UNKNOWN_TOOL");
        assert!(e.message.contains("did you mean 'fs.read'"), "message: {}", e.message);
        assert_eq!(e.hint.as_ref().unwrap()["didYouMean"], json!("fs.read"));
        assert_eq!(e.hint.as_ref().unwrap()["retryWith"], json!("mcp__nc-tools__fs_read"));
        // garbage far from any name must NOT guess
        let junk = k.call("zzzzzz", &json!({}));
        assert!(junk.error.unwrap().hint.as_ref().unwrap().get("didYouMean").is_none());
    }

    #[test]
    fn server_prefixed_name_without_tool_teaches_wire_form() {
        let k = kernel_with_tools();
        for sent in ["mcp__nc-tools=", "mcp__nc-tools", "nc-tools", "mcp__nc-tools:"] {
            let out = k.call(sent, &json!({}));
            let e = out.error.expect("must error");
            assert_eq!(e.code, "ERR_UNKNOWN_TOOL");
            let hint = e.hint.as_ref().unwrap();
            assert_eq!(hint["wireForm"], json!("mcp__nc-tools__<toolname>"), "sent: {sent}");
            assert!(hint["examples"].as_array().map_or(false, |a| !a.is_empty()), "sent: {sent}");
            assert!(hint.get("didYouMean").is_none(), "must not guess a tool: {sent}");
        }
        // a real tool behind the prefix is unaffected (resolves, no error)
        assert_eq!(k.resolve_tool("mcp__nc-tools__fs__read"), "fs.read");
    }
}
