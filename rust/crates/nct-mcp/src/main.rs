// MCP stdio server binary — the Rust port of src/mcp/server.mjs.
// Protocol: MCP 2024-11-05 (initialize, tools/list, tools/call, ping), one
// JSON message per line, structured tool results as text content, isError on
// failures, idle auto-exit (NCTOOLS_MCP_IDLE_MS) so dormant agents free the
// process until the next call.
use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

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

fn main() {
    let mut args = std::env::args().skip(1);
    let arg_root = args.next();
    let workspace = match arg_root {
        Some(r) if !r.is_empty() => std::path::PathBuf::from(&r),
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
    let idle_ms = std::env::var("NCTOOLS_MCP_IDLE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(30 * 60 * 1000);
    eprintln!("[nc-tools-mcp] serving workspace: {}", workspace.display());

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let last_activity = Instant::now();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
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
            "initialize" => rpc_result(
                id,
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
                }),
            ),
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
        if last_activity.elapsed() > Duration::from_millis(idle_ms) {
            eprintln!("[nc-tools-mcp] idle {idle_ms}ms — exiting; clients restart on demand");
            std::process::exit(0);
        }
    }
}
