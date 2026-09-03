// MCP stdio server binary — the Rust port of src/mcp/server.mjs.
// Protocol: MCP 2024-11-05 (initialize, tools/list, tools/call, ping), one
// JSON message per line, structured tool results as text content, isError on
// failures, idle auto-exit (NCTOOLS_MCP_IDLE_MS) so dormant agents free the
// process until the next call.
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nct_mcp::{build_kernel, SERVER_NAME, SERVER_VERSION};
use serde_json::{json, Value};

fn rpc_result(id: Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn rpc_error(id: Value, code: i64, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

/// tools/call result envelope (server.mjs handle): structured JSON rendered
/// as text content; isError marks failures.
fn call_envelope(out: &nct_core::CallOutcome) -> Value {
    let text = if out.ok {
        serde_json::to_string_pretty(&out.result.clone().unwrap_or(Value::Null)).unwrap_or_default()
    } else if let Some(e) = &out.error {
        serde_json::to_string_pretty(&json!({ "error": e })).unwrap_or_default()
    } else {
        "{}".to_string()
    };
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": !out.ok,
    })
}

/// Current wall-clock time in milliseconds since the epoch (for the idle timer).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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

    let kernel = match build_kernel(workspace.clone()) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("[nc-tools-mcp] fatal: failed to initialize kernel: {e}");
            std::process::exit(1);
        }
    };

    // idle auto-exit: 0 (or garbage) disables the timer — a misconfigured env
    // must never kill the process (server.mjs idleMsFromEnv).
    if let Some(dump) = dump_path {
        let descriptors = kernel.descriptors();
        let golden = json!({
            "generatedFrom": "rust/nct-mcp (build_kernel)",
            "toolCount": descriptors.len(),
            "tools": descriptors,
        });
        if dump.is_dir() {
            eprintln!("[nc-tools-mcp] --dump-tools target is a directory: {}", dump.display());
            std::process::exit(1);
        }
        if let Some(parent) = dump.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&dump, serde_json::to_string_pretty(&golden).unwrap_or_default() + "\n")
            .expect("write golden spec");
        eprintln!("[nc-tools-mcp] golden spec written: {} ({} tools)", dump.display(), descriptors.len());
        std::process::exit(0);
    }

    let idle_ms = std::env::var("NCTOOLS_MCP_IDLE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(30 * 60 * 1000);
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
    // of no requests, so a dormant agent frees the process even while the main
    // loop is blocked on stdin (server.mjs idleMsFromEnv semantics). idle_ms of
    // 0/garbage was already filtered above to the 30-min default, so a mis-
    // configured env never kills the process.
    let last_activity = std::sync::Arc::new(AtomicU64::new(now_ms()));
    {
        let la = std::sync::Arc::clone(&last_activity);
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(200));
            if now_ms().saturating_sub(la.load(Ordering::Relaxed)) >= idle_ms {
                eprintln!("[nc-tools-mcp] idle {idle_ms}ms — exiting; clients restart on demand");
                std::process::exit(0);
            }
        });
    }

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // a request arrived: mark activity so the idle watcher resets its clock
        last_activity.store(now_ms(), Ordering::Relaxed);
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(m) => m,
            Err(_) => {
                writeln!(stdout, "{}", rpc_error(Value::Null, -32700, "Parse error")).ok();
                let _ = stdout.flush();
                continue;
            }
        };
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or_default().to_string();
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
                    let _ = kernel.call(
                        "agent.register",
                        &json!({ "agentId": agent_id }),
                    );
                }
                rpc_result(
                    id,
                    json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": { "tools": { "listChanged": false } },
                        "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
                    }),
                )
            }
            m if m.starts_with("notifications/") => continue, // no response for notifications
            "tools/list" => rpc_result(id, json!({ "tools": kernel.descriptors() })),
            "tools/call" => {
                let name = params.get("name").and_then(|n| n.as_str()).unwrap_or_default().to_string();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let out = kernel.call(&name, &args);
                rpc_result(id, call_envelope(&out))
            }
            "ping" => rpc_result(id, json!({})),
            other => rpc_error(id, -32601, &format!("Method not found: {other}")),
        };
        writeln!(stdout, "{resp}").ok();
        let _ = stdout.flush();
    }
}
