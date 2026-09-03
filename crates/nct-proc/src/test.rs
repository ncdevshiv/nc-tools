// test.* — typed test-runner driver. Returns structured results (counts +
// failing test identities), not stdout text. Port of src/kernel/test.mjs:
// node:test uses its built-in junit reporter; pytest uses --junitxml; one
// parser for both.
use serde::Deserialize;
use serde_json::{json, Value};

use nct_core::childenv::child_env;
use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};
use nct_core::paths::resolve_checked;

use super::schema;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const RUN_DESC: &str = "Run a test suite and get STRUCTURED results: passed/failed counts, failing test names + messages. Frameworks: node (node:test), pytest.";

pub fn register_test(k: &mut Kernel) {
    k.register("test.run", RUN_DESC, schema::<RunArgs>(), std::sync::Arc::new(RunHandler));
}

// Node's test runner expands glob patterns itself (v21+); a bare directory
// is NOT descended into (Node 24 treats it as a module path and fails), so
// directories passed by the agent are expanded here (test.mjs patternsFor).
const TEST_EXT_PATTERNS: &[&str] = &[
    "*.test.mjs", "*.test.js", "*.test.cjs", "*.test.ts", "*.test.tsx", "*.test.mts", "*.test.cts",
];
const DEFAULT_PATTERNS: &[&str] = &[
    "test/*.test.mjs", "test/*.test.js", "test/*.test.cjs",
    "tests/**/*.test.mjs", "tests/**/*.test.js", "tests/**/*.test.cjs",
    "src/**/*.test.mjs", "src/**/*.test.js", "src/**/*.test.cjs",
    "*.test.mjs", "*.test.js",
];

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunArgs {
    #[serde(default)]
    pub framework: Option<Framework>,
    #[doc = "Test file/dir path (absolute allowed); default = standard patterns in base dir"]
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    #[schemars(range(min = 1000, max = 600000))]
    pub timeoutMs: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Framework {
    Node,
    Pytest,
    #[serde(alias = "rust", alias = "cargotest")]
    Cargo,
}

pub struct RunHandler;
impl Handler for RunHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: RunArgs = parse_args(args)?;
        match a.framework.unwrap_or(Framework::Node) {
            Framework::Node => run_node(k, a.path.as_deref(), a.timeoutMs),
            Framework::Pytest => run_pytest(k, a.path.as_deref(), a.timeoutMs),
            Framework::Cargo => run_cargo(k, a.path.as_deref(), a.timeoutMs),
        }
    }
}

fn patterns_for(k: &Kernel, path: &str) -> Result<Vec<String>, ToolError> {
    let abs = resolve_checked(&k.root, path)?;
    if !abs.exists() {
        return Err(ToolError::with_hint("ERR_NOT_FOUND", format!("No such path: {path}"), json!({ "path": path })));
    }
    if abs.is_dir() {
        let base = abs.display().to_string().replace('\\', "/");
        Ok(TEST_EXT_PATTERNS.iter().map(|g| format!("{base}/**/{g}")).collect())
    } else {
        Ok(vec![path.to_string()])
    }
}

fn run_suite(k: &Kernel, cmd: &str, args: &[String], timeout_ms: u64) -> Result<std::process::Output, ToolError> {
    // Strip NODE_TEST_CONTEXT: node sets it for its own test children, and an
    // inherited value makes the spawned runner think it is nested and skip files.
    let mut env = child_env(&k.session_env.snapshot());
    env.remove("NODE_TEST_CONTEXT");
    let mut c = Command::new(cmd);
    c.args(args)
        .current_dir(&k.root)
        .stdin(Stdio::null())
        .env_clear()
        .envs(env);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(nct_core::CREATE_NO_WINDOW);
    }
    let mut child = c
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ToolError::new("ERR_CMD_NOT_FOUND", format!("{cmd} is not available"))
            } else {
                ToolError::new("ERR_SPAWN", format!("{cmd} failed: {e}"))
            }
        })?;
    // Drain BOTH pipes concurrently into their own buffers. Serial reads
    // (stdout to EOF, then stderr) deadlock once a runner writes enough to
    // fill the OS pipe buffer (~64KB) on the SECOND stream while the FIRST is
    // still held open: the child blocks on write, so the first never reaches
    // EOF and the whole call hangs. Two background pumps drain each stream as
    // it arrives, so the child can always write; we then poll for exit.
    let out_buf = Arc::new(Mutex::new(String::new()));
    let err_buf = Arc::new(Mutex::new(String::new()));
    let mut pump_handles = Vec::new();
    if let Some(mut s) = child.stdout.take() {
        let buf = out_buf.clone();
        pump_handles.push(std::thread::spawn(move || {
            use std::io::Read;
            let mut chunk = [0u8; 8192];
            loop {
                match s.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.lock().unwrap().push_str(&String::from_utf8_lossy(&chunk[..n])),
                }
            }
        }));
    }
    if let Some(mut s) = child.stderr.take() {
        let buf = err_buf.clone();
        pump_handles.push(std::thread::spawn(move || {
            use std::io::Read;
            let mut chunk = [0u8; 8192];
            loop {
                match s.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.lock().unwrap().push_str(&String::from_utf8_lossy(&chunk[..n])),
                }
            }
        }));
    }
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ToolError::new("ERR_SPAWN", format!("{cmd} timed out")));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => return Err(ToolError::new("ERR_SPAWN", format!("{cmd} failed: {e}"))),
        }
    };
    for h in pump_handles {
        let _ = h.join();
    }
    let out = out_buf.lock().unwrap().clone();
    let err = err_buf.lock().unwrap().clone();
    Ok(std::process::Output { status, stdout: out.into_bytes(), stderr: err.into_bytes() })
}

fn run_node(k: &Kernel, path: Option<&str>, timeout_ms: Option<u64>) -> Result<Value, ToolError> {
    let timeout = timeout_ms.unwrap_or(k.cfg.limits.test_timeout_ms);
    let patterns: Vec<String> = match path {
        Some(p) => patterns_for(k, p)?,
        None => DEFAULT_PATTERNS.iter().map(|s| s.to_string()).collect(),
    };
    let mut args: Vec<String> = vec!["--test".into(), "--test-reporter=junit".into()];
    args.extend(patterns.iter().cloned());
    let out = run_suite(k, "node", &args, timeout)?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let xml = format!("{stdout}\n{stderr}");
    if !xml.contains("<testsuites") && !xml.contains("<testcase") {
        return Err(ToolError::with_hint(
            "ERR_TEST_PARSE",
            "no junit report in output; tests may not exist",
            json!({ "exitCode": out.status.code(), "stderrTail": tail_str(&stderr, 300) }),
        ));
    }
    let parsed = parse_junit_xml(&xml, "node")?;
    if parsed["total"].as_u64() == Some(0) {
        return Err(ToolError::with_hint(
            "ERR_NO_TESTS",
            "no tests were discovered (0 test cases in junit report)",
            json!({ "patterns": patterns, "exitCode": out.status.code(), "hint": "patterns matched no files; check the test directory layout or pass an explicit path" }),
        ));
    }
    let mut result = parsed;
    if let Value::Object(m) = &mut result {
        m.insert("exitCode".into(), json!(out.status.code()));
    }
    Ok(result)
}

fn run_pytest(k: &Kernel, path: Option<&str>, timeout_ms: Option<u64>) -> Result<Value, ToolError> {
    let timeout = timeout_ms.unwrap_or(k.cfg.limits.test_timeout_ms);
    let junit = k.root.join(".nc-tools").join("junit.xml");
    let mut args: Vec<String> = vec!["-m".into(), "pytest".into(), path.unwrap_or(".").to_string(), "--junitxml".into(), junit.display().to_string(), "-q".into(), "--no-header".into()];
    let _ = &mut args;
    let out = run_suite(k, "python", &args, timeout)?;
    if !junit.exists() {
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        return Err(ToolError::with_hint(
            "ERR_TEST_PARSE",
            "pytest produced no junit report; are there any tests?",
            json!({ "exitCode": out.status.code(), "stderrTail": tail_str(&stderr, 300) }),
        ));
    }
    let xml = std::fs::read_to_string(&junit).map_err(ToolError::from)?;
    let _ = std::fs::remove_file(&junit);
    let parsed = parse_junit_xml(&xml, "pytest")?;
    if parsed["total"].as_u64() == Some(0) {
        return Err(ToolError::with_hint(
            "ERR_NO_TESTS",
            "pytest discovered no tests",
            json!({ "exitCode": out.status.code(), "hint": "check the test directory or pass an explicit path" }),
        ));
    }
    let mut result = parsed;
    if let Value::Object(m) = &mut result {
        m.insert("exitCode".into(), json!(out.status.code()));
    }
    Ok(result)
}

fn tail_str(s: &str, n: usize) -> String {
    s.chars().rev().take(n).collect::<Vec<_>>().into_iter().rev().collect()
}

fn decode_entities(s: &str) -> String {
    s.replace("&#10;", "\n")
        .replace("&#13;", "\r")
        .replace("&#9;", "\t")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// JUnit XML parser — a direct port of test.mjs parseJunitXml (regex-based,
/// one parser for node:test and pytest reports).
fn parse_junit_xml(xml: &str, framework: &str) -> Result<Value, ToolError> {
    let mut passed = 0u64;
    let mut failed = 0u64;
    let errors = 0u64;
    let mut skipped = 0u64;
    let mut duration_ms = 0f64;
    let mut failures: Vec<Value> = Vec::new();

    let case_re = regex::Regex::new(r"<testcase\b[\s\S]*?(?:</testcase>|/>)").unwrap();
    let name_re = regex::Regex::new(r#"\bname="([^"]*)""#).unwrap();
    let time_re = regex::Regex::new(r#"time="([\d.]+)""#).unwrap();
    let failure_child_re = regex::Regex::new(r"<failure\b").unwrap();
    let error_child_re = regex::Regex::new(r"<error\b").unwrap();
    let attr_failure_re = regex::Regex::new(r#"\bfailure="([^"]*)""#).unwrap();
    let child_msg_re = regex::Regex::new(r#"<(?:failure|error)[^>]*\bmessage="([^"]*)""#).unwrap();
    let child_body_re = regex::Regex::new(r"<(?:failure|error)[^>]*>([\s\S]*?)</(?:failure|error)>").unwrap();
    let file_re = regex::Regex::new(r#"\bfile="([^"]*)""#).unwrap();
    let skipped_re = regex::Regex::new(r"<skipped\b").unwrap();

    for c in case_re.find_iter(xml) {
        let c = c.as_str();
        let name = name_re
            .captures(c)
            .and_then(|m| m.get(1))
            .map(|m| decode_entities(m.as_str()))
            .unwrap_or_else(|| "unknown".to_string());
        let time: f64 = time_re
            .captures(c)
            .and_then(|m| m.get(1))
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0.0);
        duration_ms += time * 1000.0;
        let child_failure = failure_child_re.is_match(c) || error_child_re.is_match(c);
        let attr_failure: Option<String> = if attr_failure_re.is_match(c) && !child_failure {
            attr_failure_re.captures(c).and_then(|m| m.get(1)).map(|m| m.as_str().to_string())
        } else {
            None
        };
        if child_failure || attr_failure.is_some() {
            failed += 1;
            let mut message = attr_failure;
            if child_failure {
                message = child_msg_re
                    .captures(c)
                    .and_then(|m| m.get(1))
                    .map(|m| m.as_str().to_string())
                    .or_else(|| {
                        child_body_re
                            .captures(c)
                            .and_then(|m| m.get(1))
                            .map(|m| m.as_str().to_string())
                    });
            }
            let file = file_re
                .captures(c)
                .and_then(|m| m.get(1))
                .map(|m| {
                    let parts: Vec<&str> = m.as_str().split(['\\', '/']).collect();
                    let start = parts.len().saturating_sub(3);
                    parts[start..].join("/")
                });
            let message = decode_entities(message.as_deref().unwrap_or("failure"))
                .trim()
                .split('\n')
                .next()
                .unwrap_or("")
                .chars()
                .take(300)
                .collect::<String>();
            failures.push(json!({ "name": name, "file": file, "message": message }));
        } else if skipped_re.is_match(c) {
            skipped += 1;
        } else {
            passed += 1;
        }
    }
    let total = passed + failed + errors + skipped;
    Ok(json!({
        "framework": framework,
        "passed": passed,
        "failed": failed,
        "errors": errors,
        "skipped": skipped,
        "total": total,
        "durationMs": duration_ms.round() as u64,
        "failures": failures,
    }))
}

// ---- cargo (Rust) driver ----------------------------------------------------

fn run_cargo(k: &Kernel, path: Option<&str>, timeout_ms: Option<u64>) -> Result<Value, ToolError> {
    let dir = match path {
        Some(p) => {
            let abs = resolve_checked(&k.root, p)?;
            if abs.is_file() {
                abs.parent().map(|d| d.to_path_buf()).unwrap_or(abs.clone())
            } else {
                abs
            }
        }
        None => k.root.clone(),
    };
    let timeout = timeout_ms.unwrap_or(300_000);
    let mut c = Command::new("cargo");
    c.arg("test")
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .envs(child_env(&k.session_env.snapshot()));
    let started = std::time::Instant::now();
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ToolError::new("ERR_CMD_NOT_FOUND", "cargo not found on PATH"));
        }
        Err(e) => return Err(ToolError::new("ERR_SPAWN", e.to_string())),
    };
    let out_buf = Arc::new(Mutex::new(String::new()));
    let err_buf = Arc::new(Mutex::new(String::new()));
    let t_out = child.stdout.take().map(|mut s| {
        let b = out_buf.clone();
        std::thread::spawn(move || {
            let mut tmp = String::new();
            let _ = s.read_to_string(&mut tmp);
            *b.lock().unwrap() = tmp;
        })
    });
    let t_err = child.stderr.take().map(|mut s| {
        let b = err_buf.clone();
        std::thread::spawn(move || {
            let mut tmp = String::new();
            let _ = s.read_to_string(&mut tmp);
            *b.lock().unwrap() = tmp;
        })
    });
    let deadline = started + Duration::from_millis(timeout);
    let _timed_out = loop {
        match child.try_wait() {
            Ok(Some(_)) => break false,
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break false,
        }
    };
    if let Some(t) = t_out {
        let _ = t.join();
    }
    if let Some(t) = t_err {
        let _ = t.join();
    }
    let elapsed = started.elapsed().as_millis() as u64;
    let stdout = out_buf.lock().unwrap().clone();
    let stderr = err_buf.lock().unwrap().clone();
    let code = child.try_wait().ok().flatten().and_then(|s| s.code());
    parse_cargo_output(&stdout, &stderr, code, elapsed)
}

fn parse_cargo_output(stdout: &str, stderr: &str, code: Option<i32>, duration_ms: u64) -> Result<Value, ToolError> {
    let mut passed = 0u64;
    let mut failed = 0u64;
    let mut ignored = 0u64;
    let mut failures: Vec<Value> = Vec::new();
    let test_re = regex::Regex::new(r"test (\S+) \.\.\. (ok|FAILED|ignored)").unwrap();
    for cap in test_re.captures_iter(stdout) {
        let name = cap.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
        match cap.get(2).map(|m| m.as_str()).unwrap_or("") {
            "ok" => passed += 1,
            "FAILED" => {
                failed += 1;
                failures.push(json!({ "name": name, "file": Value::Null, "message": "failed" }));
            }
            "ignored" => ignored += 1,
            _ => {}
        }
    }
    let err_re = regex::Regex::new(r"error(?:\[E\d+\])?: ([^\n]*)").unwrap();
    for e in err_re.find_iter(stderr) {
        failures.push(json!({ "name": "compile error", "file": Value::Null, "message": e.as_str().to_string() }));
    }
    let compile_broke = !failures.is_empty() && code.map(|c| c != 0).unwrap_or(false);
    if compile_broke && failed == 0 {
        failed = 1;
    }
    let total = passed + failed + ignored;
    Ok(json!({
        "framework": "cargo",
        "passed": passed,
        "failed": failed,
        "errors": if compile_broke && failed == 1 && passed == 0 { 1 } else { 0 },
        "skipped": ignored,
        "total": total,
        "durationMs": duration_ms,
        "failures": failures,
    }))
}
