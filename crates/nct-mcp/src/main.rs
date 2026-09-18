// MCP stdio server binary — protocol 2024-11-05 .. 2025-11-25 (negotiated).
//
// Design (v2):
//   * one reader (main thread) keeps the stdin loop free while tools run, so
//     `notifications/cancelled` and interleaved requests are handled promptly;
//   * every `tools/call` runs on its own worker thread — a long proc.spawn no
//     longer blocks every other call (the old request-at-a-time loop did);
//   * version negotiation mirrors the TS SDK server (echo a supported
//     requested revision, else answer with the latest);
//   * tools/call results carry `structuredContent` for revisions >=
//     2025-06-18, text content for all;
//   * progress notifications flow when the client minted a progressToken;
//   * idle auto-exit (NCTOOLS_MCP_IDLE_MS) drains managed children first.
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nct_mcp::{
    build_kernel, pick_protocol_version, roots, structured_output_supported, SERVER_NAME,
    SERVER_VERSION,
};
use serde_json::{json, Value};

fn rpc_result(id: Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn rpc_error(id: Value, code: i64, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

/// Serialized stdout writer: worker threads and the reader share one handle and
/// one line-write mutex, so responses never interleave.
struct Conn {
    out: Mutex<std::io::Stdout>,
}

impl Conn {
    fn send(&self, msg: &str) {
        if let Ok(mut o) = self.out.lock() {
            let _ = writeln!(o, "{msg}");
            let _ = o.flush();
        }
    }

    fn send_value(&self, msg: &Value) {
        self.send(&msg.to_string());
    }
}

/// Keeps the active-worker count and the cancel registry in sync with a
/// worker's lifetime. A close-time panic (before or outside the kernel's own
/// catch_unwind) would otherwise leak a cancel entry and pin the idle watcher
/// at `workers > 0` forever.
struct CallGuard {
    workers: Arc<AtomicU64>,
    cancels: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    key: String,
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = self.cancels.lock() {
            map.remove(&self.key);
        }
        self.workers.fetch_sub(1, Ordering::SeqCst);
    }
}

/// tools/call result envelope: structured JSON rendered as text content;
/// `isError` marks failures. `structured` adds the machine-readable
/// `structuredContent` field (2025-06-18+ revisions only).
fn call_envelope(out: &nct_core::CallOutcome, structured: bool) -> Value {
    let text = if out.ok {
        serde_json::to_string_pretty(&out.result.clone().unwrap_or(Value::Null)).unwrap_or_default()
    } else if let Some(e) = &out.error {
        serde_json::to_string_pretty(&json!({ "error": e })).unwrap_or_default()
    } else {
        "{}".to_string()
    };
    let mut env = json!({
        "content": [{ "type": "text", "text": text }],
        "isError": !out.ok,
    });
    if structured {
        if let Some(result) = &out.result {
            if result.is_object() {
                env["structuredContent"] = result.clone();
            }
        }
    }
    env
}

/// Current wall-clock time in milliseconds since the epoch (for the idle timer).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Registry key for a JSON-RPC id: both sides stringify the parsed Value, so
/// `1` and `"1"` never collide.
fn id_key(id: &Value) -> String {
    id.to_string()
}

const USAGE: &str = concat!(
    "usage: nc-tools-mcp [--dump-tools <path>] [workspace-dir]\n",
    "\n",
    "options:\n",
    "  --dump-tools <path>   write the tool descriptor golden JSON and exit\n",
    "  --help, -h            show this help and exit\n",
    "  --version, -V         print server name + version and exit\n",
    "\n",
    "workspace-dir is the root the tools operate on (default: $NCTOOLS_WORKSPACE,\n",
    "then the current working directory). MCP stdio protocol on stdin/stdout."
);

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut workspace_arg: Option<String> = None;
    let mut dump_path: Option<std::path::PathBuf> = None;
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("{SERVER_NAME} {SERVER_VERSION}");
                std::process::exit(0);
            }
            "--dump-tools" => {
                i += 1;
                match raw.get(i) {
                    Some(p) => {
                        dump_path = Some(std::path::PathBuf::from(p));
                        i += 1;
                    }
                    None => {
                        eprintln!("[nc-tools-mcp] --dump-tools requires an output path\n{USAGE}");
                        std::process::exit(2);
                    }
                }
            }
            s if s.starts_with('-') => {
                eprintln!("[nc-tools-mcp] unknown option: {s}\n{USAGE}");
                std::process::exit(2);
            }
            s => {
                workspace_arg = Some(s.to_string());
                i += 1;
            }
        }
    }
    let workspace = match workspace_arg {
        Some(r) if !r.is_empty() => std::path::PathBuf::from(r),
        _ => match std::env::var("NCTOOLS_WORKSPACE") {
            Ok(w) if !w.is_empty() => std::path::PathBuf::from(w),
            _ => std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        },
    };
    let workspace = dunce::canonicalize(&workspace).unwrap_or(workspace);

    let kernel_inner = match build_kernel(workspace.clone()) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("[nc-tools-mcp] fatal: failed to initialize kernel: {e}");
            std::process::exit(1);
        }
    };
    // Arc: the idle-exit watcher runs on its own thread and must be able to
    // run the kernel's shutdown hooks before it calls std::process::exit.
    let kernel = std::sync::Arc::new(kernel_inner);

    if let Some(dump) = dump_path {
        let descriptors = kernel.descriptors();
        let golden = json!({
            "generatedFrom": "rust/nct-mcp (build_kernel)",
            "toolCount": descriptors.len(),
            "tools": descriptors,
        });
        if dump.is_dir() {
            eprintln!(
                "[nc-tools-mcp] --dump-tools target is a directory: {}",
                dump.display()
            );
            std::process::exit(1);
        }
        if let Some(parent) = dump.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(
            &dump,
            serde_json::to_string_pretty(&golden).unwrap_or_default() + "\n",
        )
        .expect("write golden spec");
        eprintln!(
            "[nc-tools-mcp] golden spec written: {} ({} tools)",
            dump.display(),
            descriptors.len()
        );
        std::process::exit(0);
    }

    // idle auto-exit: a POSITIVE NCTOOLS_MCP_IDLE_MS sets the timer; 0 (or a
    // negative) disables it entirely — the documented contract (config.rs /
    // AUDIT.md "0 disables") and how an embedding client keeps the server —
    // and its proc.start handles — alive for its own lifetime. Unset/garbage
    // keeps the 30-minute default so a misconfigured env still frees the
    // process eventually.
    let idle_ms: Option<u64> = match std::env::var("NCTOOLS_MCP_IDLE_MS") {
        Ok(v) => match v.trim().parse::<i64>() {
            Ok(n) if n <= 0 => None,
            Ok(n) => Some(n as u64),
            Err(_) => Some(30 * 60 * 1000),
        },
        Err(_) => Some(30 * 60 * 1000),
    };
    match idle_ms {
        Some(ms) => eprintln!("[nc-tools-mcp] idle auto-exit after {ms}ms (0 disables)"),
        None => eprintln!("[nc-tools-mcp] idle auto-exit disabled"),
    }
    eprintln!("[nc-tools-mcp] serving workspace: {}", workspace.display());

    // Panic hook: any escape from the boundary (or from the loop itself)
    // leaves a located breadcrumb on stderr — timestamp, panic site, message
    // — instead of Rust's default thread dump. Panics inside tool handlers
    // never reach this; kernel.rs converts them to ERR_PANIC results.
    std::panic::set_hook(Box::new(|info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "opaque panic payload".to_string());
        eprintln!("[nc-tools-mcp] PANIC at {loc}: {msg}");
    }));

    // Idle auto-exit: a background watcher exits the process after `idle_ms`
    // of no REQUESTS (activity is reset by the reader loop). A live worker
    // defers the exit — it owns a running child and will observe the
    // shutdown flag when the client actually leaves. idle_ms of 0 was already
    // consumed above as "disabled".
    let last_activity = std::sync::Arc::new(AtomicU64::new(now_ms()));
    let active_workers = Arc::new(AtomicU64::new(0));
    if let Some(idle_ms) = idle_ms {
        let la = std::sync::Arc::clone(&last_activity);
        let k = std::sync::Arc::clone(&kernel);
        let workers = std::sync::Arc::clone(&active_workers);
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(200));
            if now_ms().saturating_sub(la.load(Ordering::Relaxed)) >= idle_ms {
                if workers.load(Ordering::SeqCst) > 0 {
                    continue; // a tool call is still running; do not exit under it
                }
                // Arm shutdown BEFORE the final worker check: a request
                // accepted in the gap between check and exit sees the flag, so
                // any child it spawns is killed with the process instead of
                // being orphaned.
                nct_core::request_shutdown();
                if workers.load(Ordering::SeqCst) > 0 {
                    // A call slipped in after arming; keep serving it and undo
                    // the arm so the process is not left in a shutdown state.
                    nct_core::clear_shutdown();
                    continue;
                }
                eprintln!("[nc-tools-mcp] idle {idle_ms}ms — exiting; clients restart on demand");
                // std::process::exit skips destructors, so managed background
                // children would be orphaned. Drain them first.
                k.run_shutdown_hooks();
                std::process::exit(0);
            }
        });
    }

    let conn = Arc::new(Conn {
        out: Mutex::new(std::io::stdout()),
    });
    // Per-request cancellation flags: notifications/cancelled flips the flag
    // while the worker thread running that call observes it from its wait loop.
    let cancels: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> = Default::default();
    // Negotiated revision feature gate: set once at initialize, read by tool
    // workers to decide whether to emit structuredContent.
    let structured_output = Arc::new(AtomicBool::new(false));

    let stdin = std::io::stdin();
    let mut stdin_lock = stdin.lock();
    loop {
        let mut line = String::new();
        match stdin_lock.read_line(&mut line) {
            Ok(0) | Err(_) => break, // EOF or read error
            Ok(_) => {}
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // a request arrived: mark activity so the idle watcher resets its clock
        last_activity.store(now_ms(), Ordering::Relaxed);
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(m) => m,
            Err(_) => {
                conn.send(&rpc_error(Value::Null, -32700, "Parse error"));
                continue;
            }
        };
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = msg
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_string();
        let params = msg.get("params").cloned().unwrap_or(json!({}));

        let resp = match method.as_str() {
            "initialize" => {
                // Identity binding: a client that carries a durable agentId
                // (survives crash/compaction/restart) tells us who it is so it
                // keeps its identity, history, and locks across sessions.
                // Registration is best-effort — the handshake must never fail
                // because the coordination layer had a problem, so a client
                // that omits clientInfo is still fully functional (it gets a
                // fresh sid-bound identity on first agent.register).
                let client_info = params.get("clientInfo").cloned().unwrap_or(json!({}));
                if let Some(agent_id) = client_info.get("agentId").and_then(|a| a.as_str()) {
                    let _ = kernel.call("agent.register", &json!({ "agentId": agent_id }));
                }
                // Version negotiation, byte-compatible with the TS SDK server:
                // echo the requested revision when we support it, otherwise
                // answer with our latest and let the client decide.
                let requested = params
                    .get("protocolVersion")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let negotiated = pick_protocol_version(requested);
                structured_output.store(structured_output_supported(negotiated), Ordering::SeqCst);
                // Spec-correct workspace anchoring (MCP roots): when the
                // client declares the roots capability, ask IT for its
                // workspace roots and bind the first filesystem directory as
                // the session default base. This is what makes an agent
                // working on F:\ncfs get ncfs results even though the server
                // was rooted on F:\nc-tools — without mutating Kernel.root,
                // so journal/coordination side-channels keep their home.
                // Any failure here is reported in the handshake result
                // (anchoring.requested/anchored) and NEVER fatal.
                let client_caps = params.get("capabilities").cloned().unwrap_or(json!({}));
                let mut anchoring = json!({ "requested": false });
                if roots::client_supports_roots(&client_caps) {
                    anchoring["requested"] = json!(true);
                    match server_roots_list(&mut stdin_lock, &conn) {
                        Some(result) => {
                            if let Some(target) = roots::first_root_from_result(&result) {
                                let accepted = kernel.set_default_base(&target);
                                anchoring["anchored"] = json!(accepted);
                                anchoring["base"] = json!(target.display().to_string());
                                if !accepted {
                                    anchoring["reason"] =
                                        json!("first root is not an existing directory");
                                }
                            } else {
                                anchoring["anchored"] = json!(false);
                                anchoring["reason"] =
                                    json!("no filesystem directory in roots/list");
                            }
                        }
                        None => {
                            anchoring["anchored"] = json!(false);
                            anchoring["reason"] = json!("client did not answer roots/list");
                        }
                    }
                }
                let mut result = json!({
                    "protocolVersion": negotiated,
                    "capabilities": {
                        "tools": { "listChanged": false },
                    },
                    "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
                });
                result["anchoring"] = anchoring;
                rpc_result(id, result)
            }
            "notifications/roots/list_changed" => {
                // The client moved/changed workspaces mid-session: re-ask for
                // roots and re-anchor the session default base.
                if let Some(result) = server_roots_list(&mut stdin_lock, &conn) {
                    if let Some(target) = roots::first_root_from_result(&result) {
                        kernel.set_default_base(&target);
                    }
                }
                continue; // notification: no response
            }
            "notifications/cancelled" => {
                // Flip the flag the worker running that request polls. Unknown
                // ids are a no-op (the call may have finished already).
                if let Some(req_id) = params.get("requestId") {
                    if let Some(flag) = cancels.lock().unwrap().get(&id_key(req_id)) {
                        flag.store(true, Ordering::SeqCst);
                    }
                }
                continue; // notification: no response
            }
            m if m.starts_with("notifications/") => continue, // no response for notifications
            "tools/list" => rpc_result(id, json!({ "tools": kernel.descriptors() })),
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or_default()
                    .to_string();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                // progressToken lives in `_meta` per the MCP spec; only a call
                // that carries one can emit progress notifications.
                let progress_token = params
                    .get("_meta")
                    .and_then(|meta| meta.get("progressToken"))
                    .cloned();
                let key = id_key(&id);
                let flag = Arc::new(AtomicBool::new(false));
                cancels.lock().unwrap().insert(key.clone(), flag.clone());
                let k = Arc::clone(&kernel);
                let conn_w = Arc::clone(&conn);
                let structured = Arc::clone(&structured_output);
                let id_w = id.clone();
                active_workers.fetch_add(1, Ordering::SeqCst);
                let guard = CallGuard {
                    workers: Arc::clone(&active_workers),
                    cancels: Arc::clone(&cancels),
                    key,
                };
                std::thread::spawn(move || {
                    let _guard = guard;
                    nct_core::set_cancel_flag(Some(flag));
                    if let Some(token) = progress_token {
                        let sink_conn = Arc::clone(&conn_w);
                        let sink: nct_core::ProgressSink = Arc::new(move |p: Value| {
                            sink_conn.send_value(&json!({
                                "jsonrpc": "2.0",
                                "method": "notifications/progress",
                                "params": p,
                            }));
                        });
                        nct_core::set_progress_ctx(Some(nct_core::ProgressCtx { token, sink }));
                    }
                    let out = k.call(&name, &args);
                    nct_core::set_cancel_flag(None);
                    nct_core::set_progress_ctx(None);
                    let env = call_envelope(&out, structured.load(Ordering::SeqCst));
                    conn_w.send(&rpc_result(id_w, env));
                });
                continue; // the worker writes the response
            }
            "ping" => rpc_result(id, json!({})),
            other => rpc_error(id, -32601, &format!("Method not found: {other}")),
        };
        conn.send(&resp);
    }
    // stdin closed: the client is gone. Stop in-flight work so process exit
    // cannot orphan a synchronous child — proc.spawn/run_bounded/watch scan
    // loops observe the shutdown flag through is_cancelled() and kill their
    // children. Then give workers a bounded window to unwind before leaving.
    nct_core::request_shutdown();
    let drain_deadline = std::time::Instant::now() + Duration::from_secs(10);
    while active_workers.load(Ordering::SeqCst) > 0 && std::time::Instant::now() < drain_deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    kernel.run_shutdown_hooks();
}

/// Server → client request over stdio: send `roots/list`, then read lines
/// until the response with our id arrives. Client REQUESTS interleaved before
/// the response are answered inline (only ping/tools are safe to auto-serve
/// during a handshake; anything else gets a deferred-method error rather than
/// being dropped); notifications are ignored. Returns the result object, or
/// None on EOF/garbage (never panics, never hangs forever — the caller treats
/// None as "client does not support roots").
fn server_roots_list(stdin: &mut std::io::StdinLock<'static>, conn: &Conn) -> Option<Value> {
    const SERVER_REQ_ID: i64 = -1_000_001; // negative: cannot collide with client ids
    let req = json!({
        "jsonrpc": "2.0",
        "id": SERVER_REQ_ID,
        "method": "roots/list",
        "params": {},
    });
    conn.send_value(&req);
    for _ in 0..64 {
        // bounded: a client that floods 64 non-response lines is broken
        let mut line = String::new();
        match stdin.read_line(&mut line) {
            Ok(0) => return None, // EOF
            Ok(_) => {}
            Err(_) => return None,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
            continue; // garbage line — skip
        };
        let msg_id = msg.get("id").cloned().unwrap_or(Value::Null);
        if msg.get("method").is_some() {
            // interleaved client message while our request is in flight
            let method = msg["method"].as_str().unwrap_or_default();
            let resp = if method == "ping" {
                rpc_result(msg_id, json!({}))
            } else if method.starts_with("notifications/") {
                continue;
            } else {
                rpc_error(
                    msg_id,
                    -32601,
                    "server busy with roots/list; resend after handshake",
                )
            };
            conn.send(&resp);
            continue;
        }
        if msg_id == json!(SERVER_REQ_ID) {
            if msg.get("error").is_some() {
                return None; // client explicitly refused (capability lied)
            }
            return Some(msg.get("result").cloned().unwrap_or(Value::Null));
        }
        // unrelated response — ignore
    }
    None
}
