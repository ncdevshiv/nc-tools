// proc.* — execution tools, behavior-parity port of src/kernel/proc.mjs.
// proc.spawn for bounded runs; proc.start creates a MANAGED background
// process (handle, streamed output, status, stop) — the typed replacement
// for "run a server / watcher in a terminal tab". proc.list and proc.kill
// cover the OS process table (tasklist/ps + pid kill). env.* manages the
// session environment that every spawned child inherits.
use std::collections::HashMap;
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use nct_core::childenv::child_env;
use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};
use nct_core::paths::resolve_checked;
use nct_core::{now_iso, CREATE_NO_WINDOW};

pub const SPAWN_DESC: &str = "Run a program with typed argv (no shell). Captures stdout/stderr, exit code, hard timeout.";
pub const START_DESC: &str = "Start a LONG-RUNNING background process (server, watcher). Returns a handleId. NOT for one-shot commands — use proc.spawn for those.";
pub const STATUS_DESC: &str = "Status of a background process handle: running, exitCode, outputBytes, uptime.";
pub const READ_OUTPUT_DESC: &str = "Read recent output (stdout+stderr merged) of a background process.";
pub const STOP_DESC: &str = "Stop a background process by handle.";
pub const LIST_DESC: &str = "List the OS process table (tasklist/ps). Optional substring filter by process name.";
pub const KILL_DESC: &str = "Kill a process by PID (system process, not just managed handles).";
pub const ENV_GET_DESC: &str = "Read an environment variable (session override wins over host).";
pub const ENV_SET_DESC: &str = "Set a session environment variable; all subsequent proc.* calls inherit it.";
pub const ENV_LIST_DESC: &str = "List session environment overrides.";
pub const RUN_SCRIPT_DESC: &str = "Run an interpreted script (js/python/shell/powershell/batch) with a hard timeout. Provide a file path OR inline source. Captures stdout/stderr + exit code.";
pub const WATCH_DESC: &str = "Watch a file/dir and re-run a long-lived command whenever the tree changes (auto-rebuild / dev server). Returns a handleId managed by proc.status/readOutput/stop.";


pub fn register(k: &mut Kernel) {
    let handles = HandleTable::default();
    k.register("proc.spawn", SPAWN_DESC, nct_core::schema::schema_for::<SpawnArgs>(), Arc::new(SpawnHandler));
    k.register("proc.start", START_DESC, nct_core::schema::schema_for::<StartArgs>(), Arc::new(StartHandler { handles: handles.clone() }));
    k.register("proc.status", STATUS_DESC, nct_core::schema::schema_for::<HandleArgs>(), Arc::new(StatusHandler { handles: handles.clone() }));
    k.register("proc.readOutput", READ_OUTPUT_DESC, nct_core::schema::schema_for::<ReadOutputArgs>(), Arc::new(ReadOutputHandler { handles: handles.clone() }));
    k.register("proc.stop", STOP_DESC, nct_core::schema::schema_for::<StopArgs>(), Arc::new(StopHandler { handles: handles.clone() }));
    k.register("proc.list", LIST_DESC, nct_core::schema::schema_for::<ListArgs>(), Arc::new(ListHandler));
    k.register("proc.kill", KILL_DESC, nct_core::schema::schema_for::<KillArgs>(), Arc::new(KillHandler));
    k.register("env.get", ENV_GET_DESC, nct_core::schema::schema_for::<EnvNameArgs>(), Arc::new(EnvGetHandler));
    k.register("env.set", ENV_SET_DESC, nct_core::schema::schema_for::<EnvSetArgs>(), Arc::new(EnvSetHandler));
    k.register("env.list", ENV_LIST_DESC, nct_core::schema::schema_for::<EmptyPArgs>(), Arc::new(EnvListHandler));
    // phase 2 additions
    k.register("proc.runScript", RUN_SCRIPT_DESC, nct_core::schema::schema_for::<RunScriptArgs>(), Arc::new(RunScriptHandler));
    k.register("proc.watch", WATCH_DESC, nct_core::schema::schema_for::<WatchArgs>(), Arc::new(WatchHandler { handles: handles.clone() }));
}

pub(crate) fn schema<T: schemars::JsonSchema>() -> Value {
    nct_core::schema::schema_for::<T>()
}

mod pkg;
mod test;
pub use pkg::register_pkg;
pub use test::register_test;

// ---- typed args -------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpawnArgs {
    pub cmd: String,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[doc = "Working dir (absolute allowed; default base dir)"]
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    #[schemars(range(min = 100, max = 600000))]
    pub timeoutMs: Option<u64>,
    #[doc = "Base directory override (default: kernel base dir). Pass when working on a different workspace than the server was started on."]
    #[serde(default)]
    pub baseDir: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StartArgs {
    pub cmd: String,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[doc = "Working dir (absolute allowed; default base dir)"]
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    #[schemars(range(min = 1000, max = 3600000))]
    pub maxDurationMs: Option<u64>,
    #[doc = "Base directory override (default: kernel base dir)"]
    #[serde(default)]
    pub baseDir: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HandleArgs {
    pub handleId: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadOutputArgs {
    pub handleId: String,
    #[serde(default)]
    #[schemars(range(min = 100, max = 100000))]
    pub fromEnd: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StopArgs {
    pub handleId: String,
    #[serde(default)]
    pub force: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListArgs {
    #[serde(default)]
    pub filter: Option<String>,
    #[serde(default)]
    #[schemars(range(min = 1, max = 2000))]
    pub maxResults: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KillArgs {
    #[schemars(range(min = 1))]
    pub pid: u64,
    #[serde(default)]
    pub force: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvNameArgs {
    pub name: String,
    /// When true, reveal a secret-valued variable (name matching
    /// *KEY/*TOKEN/*SECRET/*PASSWORD) in full. Default masks it as "***".
    #[serde(default)]
    pub reveal: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvSetArgs {
    pub name: String,
    pub value: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmptyPArgs {}

// ---- managed background handles ----------------------------------------------

/// Shared registry of managed background handles — the closure state of the
/// JS makeProcTools (handles Map + handleSeq counter).
#[derive(Default, Clone)]
pub struct HandleTable {
    map: Arc<Mutex<HashMap<String, Arc<HandleRec>>>>,
    seq: Arc<AtomicU64>,
}

impl HandleTable {
    fn next_id(&self) -> String {
        let n = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        format!("h{n}")
    }
    fn insert(&self, id: String, rec: Arc<HandleRec>) {
        self.map.lock().unwrap().insert(id, rec);
    }
    fn get(&self, id: &str) -> Option<Arc<HandleRec>> {
        self.map.lock().unwrap().get(id).cloned()
    }
    fn known(&self) -> Vec<String> {
        let mut v: Vec<String> = self.map.lock().unwrap().keys().cloned().collect();
        v.sort();
        v
    }
}

pub struct HandleRec {
    pub cmd: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub pid: Option<u32>,
    started_at: Instant,
    started_at_iso: String,
    pub running: Mutex<bool>,
    pub exit_code: Mutex<Option<i32>>,
    pub signal: Mutex<Option<String>>,
    pub timed_out: Mutex<bool>,
    pub spawn_error: Mutex<Option<String>>,
    pub output: Arc<Mutex<String>>,
    /// Owned child; polled + reaped by the watcher thread, or taken by stop().
    child: Mutex<Option<Child>>,
}

impl HandleRec {
    fn record_exit(&self, status: &ExitStatus) {
        let (code, sig) = exit_and_signal(status);
        *self.exit_code.lock().unwrap() = code.as_i64().map(|v| v as i32);
        *self.signal.lock().unwrap() = sig.as_str().map(String::from);
        *self.running.lock().unwrap() = false;
    }
    /// Lock-free poll window: lock, try_wait, act, unlock.
    /// None = child taken (stop() owns reaping); Some(Ok(None)) = still running.
    fn poll_once(&self) -> Option<Result<Option<ExitStatus>, String>> {
        let mut cell = self.child.lock().unwrap();
        cell.as_mut().map(|c| c.try_wait().map_err(|e| e.to_string()))
    }
    fn take_child(&self) -> Option<Child> {
        self.child.lock().unwrap().take()
    }
}

// ---- spawn machinery -----------------------------------------------------------

fn validate_cmd(cmd: &str) -> Result<(), ToolError> {
    if cmd.is_empty() {
        return Err(ToolError::new("ERR_BAD_INPUT", "cmd must be a non-empty string"));
    }
    Ok(())
}

fn build_command(k: &Kernel, cmd: &str, args: &[String], cwd_abs: &std::path::Path) -> Command {
    let mut c = Command::new(cmd);
    c.args(args)
        .current_dir(cwd_abs)
        .stdin(Stdio::null())
        .env_clear()
        .envs(child_env(&k.session_env.snapshot()));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

fn spawn_failure(cmd: &str, err: &std::io::Error) -> Value {
    // proc.mjs close/error semantics: ENOENT is a structured "not found"
    // inside a SUCCESSFUL tool result; other spawn failures are ERR_SPAWN.
    if err.kind() == std::io::ErrorKind::NotFound {
        json!({
            "pid": Value::Null,
            "exitCode": Value::Null,
            "signal": Value::Null,
            "timedOut": false,
            "stdout": "",
            "stderr": "",
            "error": { "code": "ERR_CMD_NOT_FOUND", "message": format!("command not found: {cmd}") },
        })
    } else {
        json!({
            "pid": Value::Null,
            "exitCode": Value::Null,
            "signal": Value::Null,
            "timedOut": false,
            "stdout": "",
            "stderr": "",
            "error": { "code": "ERR_SPAWN", "message": err.to_string() },
        })
    }
}

/// Map ExitStatus to (exitCode, signal) mirroring Node's close event:
/// normal exit → (code, null); killed by signal → (null, "SIGKILL"-style).
fn exit_and_signal(status: &ExitStatus) -> (Value, Value) {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return (Value::Null, json!(unix_signal_name(sig)));
        }
    }
    (json!(status.code()), Value::Null)
}

#[cfg(unix)]
fn unix_signal_name(sig: i32) -> String {
    match sig {
        1 => "SIGHUP".into(),
        2 => "SIGINT".into(),
        9 => "SIGKILL".into(),
        15 => "SIGTERM".into(),
        other => format!("SIG{other}"),
    }
}

/// Pump a child stream into a shared capped buffer until EOF.
fn pump<R: Read + Send + 'static>(mut stream: R, buf: Arc<Mutex<String>>, cap: usize) {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut g = buf.lock().unwrap();
                    if g.len() < cap {
                        g.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    }
                }
            }
        }
    });
}

fn tail_chars(s: &str, n: usize) -> String {
    s.chars().rev().take(n).collect::<Vec<_>>().into_iter().rev().collect()
}

// ---- proc.spawn ----------------------------------------------------------------

pub struct SpawnHandler;
impl Handler for SpawnHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: SpawnArgs = parse_args(args)?;
        let args_v = a.args.clone().unwrap_or_default();
        validate_cmd(&a.cmd)?;
        let timeout_ms = a.timeoutMs.unwrap_or(k.cfg.limits.spawn_timeout_ms);
        if !(100..=k.cfg.limits.spawn_timeout_max_ms).contains(&timeout_ms) {
            return Err(ToolError::with_hint(
                "ERR_BAD_INPUT",
                format!("timeoutMs must be an integer between 100 and {}", k.cfg.limits.spawn_timeout_max_ms),
                json!({ "got": a.timeoutMs }),
            ));
        }
        let cwd_abs = resolve_checked(&k.base_dir(a.baseDir.as_deref())?, a.cwd.as_deref().unwrap_or("."))?;
        let mut cmd = build_command(k, &a.cmd, &args_v, &cwd_abs);
        let mut child = match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
            Ok(c) => c,
            Err(e) => return Ok(spawn_failure(&a.cmd, &e)),
        };
        let pid = child.id();
        let out_buf = Arc::new(Mutex::new(String::new()));
        let err_buf = Arc::new(Mutex::new(String::new()));
        let max = k.cfg.limits.proc_output_bytes;
        if let Some(s) = child.stdout.take() {
            pump(s, out_buf.clone(), max);
        }
        if let Some(s) = child.stderr.take() {
            pump(s, err_buf.clone(), max);
        }
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) => {
                    if Instant::now() > deadline {
                        timed_out = true;
                        let _ = child.kill();
                        break child.wait().ok();
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break None,
            }
        };
        let (exit_code, signal) = match &status {
            Some(s) => exit_and_signal(s),
            None => (Value::Null, Value::Null),
        };
        let out = out_buf.lock().unwrap().clone();
        let err = err_buf.lock().unwrap().clone();
        Ok(json!({
            "pid": pid,
            "exitCode": exit_code,
            "signal": signal,
            "timedOut": timed_out,
            "stdout": tail_chars(&out, max),
            "stderr": tail_chars(&err, max),
        }))
    }
}

// ---- proc.start / status / readOutput / stop ------------------------------------

pub struct StartHandler {
    handles: HandleTable,
}
impl Handler for StartHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: StartArgs = parse_args(args)?;
        let args_v = a.args.clone().unwrap_or_default();
        validate_cmd(&a.cmd)?;
        let max_duration = a.maxDurationMs.unwrap_or(600_000);
        if !(1000..=k.cfg.limits.proc_max_duration_ms).contains(&max_duration) {
            return Err(ToolError::with_hint(
                "ERR_BAD_INPUT",
                format!("maxDurationMs must be an integer between 1000 and {}", k.cfg.limits.proc_max_duration_ms),
                json!({ "got": a.maxDurationMs }),
            ));
        }
        let cwd_abs = resolve_checked(&k.base_dir(a.baseDir.as_deref())?, a.cwd.as_deref().unwrap_or("."))?;
        let mut cmd = build_command(k, &a.cmd, &args_v, &cwd_abs);
        let spawn_result = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
        let handle_id = self.handles.next_id();
        let (child, spawn_error) = match spawn_result {
            Ok(c) => (Some(c), None),
            Err(e) => (
                None,
                Some(if e.kind() == std::io::ErrorKind::NotFound {
                    format!("command not found: {}", a.cmd)
                } else {
                    e.to_string()
                }),
            ),
        };
        let rec = Arc::new(HandleRec {
            cmd: a.cmd.clone(),
            args: args_v.clone(),
            cwd: cwd_abs.display().to_string(),
            pid: child.as_ref().map(|c| c.id()),
            started_at: Instant::now(),
            started_at_iso: now_iso(),
            running: Mutex::new(child.is_some()),
            exit_code: Mutex::new(None),
            signal: Mutex::new(None),
            timed_out: Mutex::new(false),
            spawn_error: Mutex::new(spawn_error),
            output: Arc::new(Mutex::new(String::new())),
            child: Mutex::new(child),
        });
        // output pumps: stdout+stderr merged into rec.output, capped (proc.mjs)
        let max = k.cfg.limits.proc_handle_output_bytes;
        if let Some(s) = rec.child.lock().unwrap().as_mut().and_then(|c| c.stdout.take()) {
            pump(s, rec.output.clone(), max);
        }
        if let Some(s) = rec.child.lock().unwrap().as_mut().and_then(|c| c.stderr.take()) {
            pump(s, rec.output.clone(), max);
        }
        // watcher thread: reaps exit, enforces maxDuration (proc.mjs timers)
        // (a failed spawn has no child; the watcher exits immediately)
        let rec_w = rec.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            loop {
                match rec_w.poll_once() {
                    None => return, // child taken by stop()
                    Some(Ok(Some(status))) => {
                        rec_w.record_exit(&status);
                        rec_w.take_child();
                        return;
                    }
                    Some(Ok(None)) => {}
                    Some(Err(e)) => {
                        *rec_w.spawn_error.lock().unwrap() = Some(e);
                        *rec_w.running.lock().unwrap() = false;
                        rec_w.take_child();
                        return;
                    }
                }
                if started.elapsed() > Duration::from_millis(max_duration) {
                    if let Some(mut c) = rec_w.take_child() {
                        *rec_w.timed_out.lock().unwrap() = true;
                        let _ = c.kill();
                        if let Ok(status) = c.wait() {
                            rec_w.record_exit(&status);
                        }
                    }
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        self.handles.insert(handle_id.clone(), rec.clone());
        Ok(json!({
            "handleId": handle_id,
            "pid": rec.pid,
            "startedAt": rec.started_at_iso,
        }))
    }
}

pub struct StatusHandler {
    handles: HandleTable,
}
impl Handler for StatusHandler {
    fn call(&self, _k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: HandleArgs = parse_args(args)?;
        let rec = self.handles.get(&a.handleId).ok_or_else(|| {
            ToolError::with_hint(
                "ERR_UNKNOWN_HANDLE",
                format!("no such process handle: {}", a.handleId),
                json!({ "known": self.handles.known() }),
            )
        })?;
        let running = *rec.running.lock().unwrap();
        let uptime_ms = if running { Some(rec.started_at.elapsed().as_millis() as u64) } else { None };
        Ok(json!({
            "handleId": a.handleId,
            "pid": rec.pid,
            "cmd": rec.cmd,
            "args": rec.args,
            "running": running,
            "exitCode": *rec.exit_code.lock().unwrap(),
            "signal": *rec.signal.lock().unwrap(),
            "timedOut": *rec.timed_out.lock().unwrap(),
            "spawnError": *rec.spawn_error.lock().unwrap(),
            "outputBytes": rec.output.lock().unwrap().len(),
            "uptimeMs": uptime_ms,
        }))
    }
}

pub struct ReadOutputHandler {
    handles: HandleTable,
}
impl Handler for ReadOutputHandler {
    fn call(&self, _k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: ReadOutputArgs = parse_args(args)?;
        let rec = self.handles.get(&a.handleId).ok_or_else(|| {
            ToolError::with_hint(
                "ERR_UNKNOWN_HANDLE",
                format!("no such process handle: {}", a.handleId),
                json!({ "known": self.handles.known() }),
            )
        })?;
        let from_end = a.fromEnd.unwrap_or(4000) as usize;
        let total = rec.output.lock().unwrap().chars().count();
        let output = if total <= from_end {
            rec.output.lock().unwrap().clone()
        } else {
            tail_chars(&rec.output.lock().unwrap(), from_end)
        };
        Ok(json!({
            "handleId": a.handleId,
            "output": output,
            "totalBytes": total,
            "truncated": total > from_end,
            "running": *rec.running.lock().unwrap(),
        }))
    }
}

pub struct StopHandler {
    handles: HandleTable,
}
impl Handler for StopHandler {
    fn call(&self, _k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: StopArgs = parse_args(args)?;
        let rec = self.handles.get(&a.handleId).ok_or_else(|| {
            ToolError::with_hint(
                "ERR_UNKNOWN_HANDLE",
                format!("no such process handle: {}", a.handleId),
                json!({ "known": self.handles.known() }),
            )
        })?;
        let was_running = *rec.running.lock().unwrap();
        // On Windows kill() is async-ish; report current knowledge, caller
        // re-statuses (same contract as proc.mjs stop).
        if let Some(mut child) = rec.take_child() {
            let _ = child.kill(); // std has no graceful signal; force = the stop
            if let Ok(status) = child.wait() {
                rec.record_exit(&status);
            }
        }
        Ok(json!({ "handleId": a.handleId, "requested": true, "wasRunning": was_running }))
    }
}

// ---- proc.runScript ----------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunScriptArgs {
    #[doc = "Script file - relative to the base dir, or absolute"]
    #[serde(default)]
    pub path: Option<String>,
    #[doc = "Language: js|python|shell|powershell|batch (inferred from path extension if omitted)"]
    #[serde(default)]
    pub language: Option<String>,
    #[doc = "Inline source (used when no path is given)"]
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    #[schemars(range(min = 100, max = 600000))]
    pub timeoutMs: Option<u64>,
    #[doc = "Working directory (default: base dir)"]
    #[serde(default)]
    pub cwd: Option<String>,
    #[doc = "Base directory override (default: kernel base dir)"]
    #[serde(default)]
    pub baseDir: Option<String>,
}

fn lang_for_ext(ext: &str) -> Option<&'static str> {
    match ext {
        "js" | "mjs" | "cjs" => Some("js"),
        "py" => Some("python"),
        "sh" | "bash" => Some("shell"),
        "ps1" => Some("powershell"),
        "bat" | "cmd" => Some("batch"),
        _ => None,
    }
}

fn script_ext(p: &str) -> &str {
    std::path::Path::new(p).extension().and_then(|e| e.to_str()).unwrap_or("")
}

fn ext_for_lang(lang: &str) -> &'static str {
    match lang {
        "python" => "py",
        "shell" => "sh",
        "powershell" => "ps1",
        "batch" => "bat",
        _ => "js",
    }
}

fn command_parts(prefix: &[&str], script: &str, extra: &[String]) -> Vec<String> {
    prefix
        .iter()
        .map(|s| s.to_string())
        .chain(std::iter::once(script.to_string()))
        .chain(extra.iter().cloned())
        .collect()
}

fn script_command(lang: &str, script: &str, extra: &[String]) -> (String, Vec<String>) {
    match lang {
        "python" => ("python".to_string(), command_parts(&[], script, extra)),
        "powershell" => ("powershell".to_string(), command_parts(&["-NoProfile", "-File"], script, extra)),
        "batch" => ("cmd".to_string(), command_parts(&["/c"], script, extra)),
        "shell" => {
            #[cfg(windows)]
            {
                ("cmd".to_string(), command_parts(&["/c"], script, extra))
            }
            #[cfg(not(windows))]
            {
                ("sh".to_string(), command_parts(&[], script, extra))
            }
        }
        _ => ("node".to_string(), command_parts(&[], script, extra)),
    }
}

/// Spawn + pump + wait with a hard timeout, returning the spawn-shaped JSON.
fn run_bounded(
    k: &Kernel,
    cmd: &str,
    args: &[String],
    cwd_abs: &std::path::Path,
    timeout_ms: u64,
) -> Result<Value, ToolError> {
    let mut c = build_command(k, cmd, args, cwd_abs);
    let mut child = match c.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(ch) => ch,
        Err(e) => return Ok(spawn_failure(cmd, &e)),
    };
    let pid = child.id();
    let out_buf = Arc::new(Mutex::new(String::new()));
    let err_buf = Arc::new(Mutex::new(String::new()));
    let max = k.cfg.limits.proc_output_bytes;
    if let Some(s) = child.stdout.take() {
        pump(s, out_buf.clone(), max);
    }
    if let Some(s) = child.stderr.take() {
        pump(s, err_buf.clone(), max);
    }
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if Instant::now() > deadline {
                    timed_out = true;
                    let _ = child.kill();
                    break child.wait().ok();
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break None,
        }
    };
    let (exit_code, signal) = match &status {
        Some(s) => exit_and_signal(s),
        None => (Value::Null, Value::Null),
    };
    let out = out_buf.lock().unwrap();
    let err = err_buf.lock().unwrap();
    Ok(json!({
        "pid": pid,
        "exitCode": exit_code,
        "signal": signal,
        "timedOut": timed_out,
        "stdout": tail_chars(&out, max),
        "stderr": tail_chars(&err, max),
    }))
}

pub struct RunScriptHandler;
impl Handler for RunScriptHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: RunScriptArgs = parse_args(args)?;
        let lang = match (&a.language, a.source.as_ref()) {
            (Some(l), _) => l.trim().to_lowercase(),
            (None, Some(_)) => {
                return Err(ToolError::new("ERR_BAD_INPUT", "inline source requires an explicit language"));
            }
            (None, None) => {
                let p = a.path.as_deref().ok_or_else(|| {
                    ToolError::new("ERR_BAD_INPUT", "runScript requires a path or an explicit language")
                })?;
                lang_for_ext(script_ext(p))
                    .ok_or_else(|| {
                        ToolError::with_hint(
                            "ERR_BAD_INPUT",
                            format!("cannot infer language for {p}; pass language"),
                            json!({ "path": p }),
                        )
                    })?
                    .to_string()
            }
        };
        let ext = ext_for_lang(&lang);
        let (script_path, is_temp) = match (&a.source, &a.path) {
            (Some(src), _) => {
                let tmp = std::env::temp_dir().join(format!(
                    "nct_run_{}_{}.{}",
                    std::process::id(),
                    nct_core::now_ms(),
                    ext
                ));
                std::fs::write(&tmp, src).map_err(ToolError::from)?;
                (tmp, true)
            }
            (None, Some(p)) => {
                let abs = resolve_checked(&k.base_dir(a.baseDir.as_deref())?, p)?;
                if !abs.exists() {
                    return Err(ToolError::with_hint("ERR_NOT_FOUND", format!("no such script: {p}"), json!({ "path": p })));
                }
                (abs, false)
            }
            _ => unreachable!(),
        };
        let script_str = script_path.display().to_string();
        let extra = a.args.clone().unwrap_or_default();
        let (cmd, child_args) = script_command(&lang, &script_str, &extra);
        let cwd_abs = resolve_checked(&k.base_dir(a.baseDir.as_deref())?, a.cwd.as_deref().unwrap_or("."))?;
        let timeout = a.timeoutMs.unwrap_or(k.cfg.limits.spawn_timeout_ms);
        if !(100..=k.cfg.limits.spawn_timeout_max_ms).contains(&timeout) {
            if is_temp {
                let _ = std::fs::remove_file(&script_path);
            }
            return Err(ToolError::with_hint(
                "ERR_BAD_INPUT",
                format!("timeoutMs must be 100..={}", k.cfg.limits.spawn_timeout_max_ms),
                json!({ "got": a.timeoutMs }),
            ));
        }
        let result = run_bounded(k, &cmd, &child_args, &cwd_abs, timeout);
        if is_temp {
            let _ = std::fs::remove_file(&script_path);
        }
        result
    }
}

// ---- proc.watch -----------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WatchArgs {
    #[doc = "Command to keep running (a server / rebuild loop)"]
    pub cmd: String,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[doc = "File or directory to watch (recursive)"]
    pub path: String,
    #[serde(default)]
    #[schemars(range(min = 100, max = 10000))]
    pub intervalMs: Option<u64>,
    #[serde(default)]
    #[schemars(range(min = 1000, max = 3600000))]
    pub maxDurationMs: Option<u64>,
    #[serde(default)]
    #[schemars(range(min = 100, max = 600000))]
    pub timeoutMs: Option<u64>,
    #[doc = "Working directory (default: base dir)"]
    #[serde(default)]
    pub cwd: Option<String>,
    #[doc = "Base directory override (default: kernel base dir)"]
    #[serde(default)]
    pub baseDir: Option<String>,
}

pub struct WatchHandler {
    handles: HandleTable,
}
impl Handler for WatchHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: WatchArgs = parse_args(args)?;
        let args_v = a.args.clone().unwrap_or_default();
        validate_cmd(&a.cmd)?;
        let base_root = k.base_dir(a.baseDir.as_deref())?;
        let watch_abs = resolve_checked(&base_root, &a.path)?;
        if !watch_abs.exists() {
            return Err(ToolError::with_hint("ERR_NOT_FOUND", format!("no such path: {}", a.path), json!({ "path": a.path })));
        }
        let interval = a.intervalMs.unwrap_or(500);
        let max_duration = a.maxDurationMs.unwrap_or(600_000);
        let cwd_abs = resolve_checked(&k.base_dir(a.baseDir.as_deref())?, a.cwd.as_deref().unwrap_or("."))?;
        let mut cmd = build_command(k, &a.cmd, &args_v, &cwd_abs);
        let spawn_result = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
        let handle_id = self.handles.next_id();
        let (child, spawn_error) = match spawn_result {
            Ok(c) => (Some(c), None),
            Err(e) => (
                None,
                Some(if e.kind() == std::io::ErrorKind::NotFound {
                    format!("command not found: {}", a.cmd)
                } else {
                    e.to_string()
                }),
            ),
        };
        let rec = Arc::new(HandleRec {
            cmd: a.cmd.clone(),
            args: args_v.clone(),
            cwd: cwd_abs.display().to_string(),
            pid: child.as_ref().map(|c| c.id()),
            started_at: Instant::now(),
            started_at_iso: now_iso(),
            running: Mutex::new(child.is_some()),
            exit_code: Mutex::new(None),
            signal: Mutex::new(None),
            timed_out: Mutex::new(false),
            spawn_error: Mutex::new(spawn_error),
            output: Arc::new(Mutex::new(String::new())),
            child: Mutex::new(child),
        });
        let max_out = k.cfg.limits.proc_handle_output_bytes;
        if let Some(s) = rec.child.lock().unwrap().as_mut().and_then(|c| c.stdout.take()) {
            pump(s, rec.output.clone(), max_out);
        }
        if let Some(s) = rec.child.lock().unwrap().as_mut().and_then(|c| c.stderr.take()) {
            pump(s, rec.output.clone(), max_out);
        }
        let fingerprint = watch_fingerprint(&watch_abs);
        let env: HashMap<String, String> = child_env(&k.session_env.snapshot()).into_iter().collect();
        let _ = &a.timeoutMs;
        let rec_w = rec.clone();
        let watch_path = watch_abs.clone();
        let cmd_s = a.cmd.clone();
        let args_s = args_v.clone();
        let cwd_s = cwd_abs.display().to_string();
        let mut last = fingerprint;
        std::thread::spawn(move || {
            let started = Instant::now();
            loop {
                match rec_w.poll_once() {
                    Some(Ok(Some(status))) => {
                        rec_w.record_exit(&status);
                        rec_w.take_child();
                        return;
                    }
                    Some(Err(e)) => {
                        *rec_w.spawn_error.lock().unwrap() = Some(e);
                        *rec_w.running.lock().unwrap() = false;
                        rec_w.take_child();
                        return;
                    }
                    _ => {}
                }
                if started.elapsed() > Duration::from_millis(max_duration) {
                    if let Some(mut c) = rec_w.take_child() {
                        let _ = c.kill();
                        if let Ok(st) = c.wait() {
                            rec_w.record_exit(&st);
                        }
                    }
                    return;
                }
                let now = watch_fingerprint(&watch_path);
                if now != last {
                    last = now;
                    if let Some(mut old) = rec_w.take_child() {
                        let _ = old.kill();
                        let _ = old.wait();
                    }
                    match spawn_child(&cmd_s, &args_s, &cwd_s, &env) {
                        Ok(newc) => {
                            {
                                let mut o = rec_w.output.lock().unwrap();
                                if o.len() < max_out {
                                    o.push_str(&format!("\n[watch] change detected, relaunching {cmd_s}\n"));
                                }
                            }
                            let mut cell = rec_w.child.lock().unwrap();
                            *cell = Some(newc);
                            let nc = cell.as_mut().unwrap();
                            if let Some(s) = nc.stdout.take() {
                                pump(s, rec_w.output.clone(), max_out);
                            }
                            if let Some(s) = nc.stderr.take() {
                                pump(s, rec_w.output.clone(), max_out);
                            }
                            *rec_w.running.lock().unwrap() = true;
                        }
                        Err(e) => {
                            let mut o = rec_w.output.lock().unwrap();
                            if o.len() < max_out {
                                o.push_str(&format!("\n[watch] relaunch failed: {e}\n"));
                            }
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(interval));
            }
        });
        self.handles.insert(handle_id.clone(), rec.clone());
        Ok(json!({
            "handleId": handle_id,
            "pid": rec.pid,
            "startedAt": rec.started_at_iso,
            "watching": a.path,
        }))
    }
}

// ---- proc.list / proc.kill --------------------------------------------------------

// watch helpers
const WATCH_SKIP: [&str; 3] = [".git", "node_modules", "target"];

fn spawn_child(cmd: &str, args: &[String], cwd: &str, env: &HashMap<String, String>) -> std::io::Result<Child> {
    let mut c = Command::new(cmd);
    c.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .envs(env);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c.spawn()
}

fn watch_fingerprint(path: &std::path::Path) -> String {
    let mut out: Vec<String> = Vec::new();
    collect_fp(path, "", &mut out);
    out.sort();
    out.join("|")
}

fn collect_fp(dir: &std::path::Path, prefix: &str, out: &mut Vec<String>) {
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut ents: Vec<_> = rd.filter_map(|e| e.ok()).collect();
    ents.sort_by_key(|e| e.file_name());
    for e in ents {
        let name = e.file_name().to_string_lossy().to_string();
        if WATCH_SKIP.contains(&name.as_str()) {
            continue;
        }
        let rel = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        if let Ok(md) = e.metadata() {
            if md.is_dir() {
                collect_fp(&e.path(), &rel, out);
            } else if let Ok(t) = md.modified() {
                if let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) {
                    out.push(format!("{rel}:{}:{}", d.as_millis(), md.len()));
                }
            }
        }
    }
}

pub struct ListHandler;
impl Handler for ListHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: ListArgs = parse_args(args)?;
        let max_results = a.maxResults.unwrap_or(500) as usize;
        if !(1..=k.cfg.limits.proc_list_max).contains(&max_results) {
            return Err(ToolError::with_hint(
                "ERR_BAD_INPUT",
                format!("maxResults must be an integer between 1 and {}", k.cfg.limits.proc_list_max),
                json!({ "got": a.maxResults }),
            ));
        }
        let mut procs = system_processes(&k.session_env.snapshot(), &k.root)?;
        if let Some(f) = &a.filter {
            let needle = f.to_lowercase();
            procs.retain(|p: &Value| p["name"].as_str().unwrap_or("").to_lowercase().contains(&needle));
        }
        let total = procs.len();
        procs.truncate(max_results);
        Ok(json!({ "processes": procs, "total": total, "truncated": total > max_results }))
    }
}

/// Parse the OS process table into [{pid, name, memKb?}] (proc.mjs
/// systemProcesses: tasklist CSV on Windows, ps on Unix).
fn system_processes(env: &std::collections::BTreeMap<String, String>, root: &std::path::Path) -> Result<Vec<Value>, ToolError> {
    #[cfg(windows)]
    {
        let out = run_sync("tasklist", &["/FO", "CSV", "/NH"], Duration::from_secs(20), env, root)?;
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let mut procs = Vec::new();
        for line in stdout.lines().filter(|l| l.contains("\",\"")) {
            let trimmed = line.trim().trim_matches('"');
            let cols: Vec<&str> = trimmed.split("\",\"").collect();
            if cols.len() >= 2 {
                let pid: Option<u64> = cols[1].parse().ok();
                if let Some(pid) = pid.filter(|p| *p > 0) {
                    let mem: Option<u64> = cols
                        .get(4)
                        .and_then(|m| {
                            let digits: String = m.chars().filter(|c| c.is_ascii_digit()).collect();
                            digits.parse().ok()
                        });
                    procs.push(json!({ "pid": pid, "name": cols[0], "memKb": mem }));
                }
            }
        }
        Ok(procs)
    }
    #[cfg(unix)]
    {
        let out = run_sync("ps", &["-A", "-o", "pid=,comm="], Duration::from_secs(20), env, root)?;
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let mut procs = Vec::new();
        for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
            let l = line.trim();
            if let Some((pid_s, name)) = l.split_once(char::is_whitespace) {
                if let Ok(pid) = pid_s.parse::<u64>().filter(|p| *p > 0) {
                    procs.push(json!({ "pid": pid, "name": name.trim() }));
                }
            }
        }
        return Ok(procs);
    }
}

/// Bounded sync child run used for tasklist/ps — the same windowsHide +
/// timeout contract as proc.mjs spawnSync.
pub(crate) fn run_sync(program: &str, args: &[&str], timeout: Duration, env_session: &std::collections::BTreeMap<String, String>, cwd: &std::path::Path) -> Result<std::process::Output, ToolError> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .env_clear()
        .envs(child_env(env_session));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ToolError::new("ERR_CMD_NOT_FOUND", format!("{program} is not available"))
        } else {
            ToolError::new("ERR_SPAWN", format!("{program} failed: {e}"))
        }
    })?;
    let mut out = String::new();
    let mut err = String::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut out);
    }
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut err);
    }
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ToolError::new("ERR_SPAWN", format!("{program} timed out")));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => return Err(ToolError::new("ERR_SPAWN", format!("{program} failed: {e}"))),
        }
    };
    Ok(std::process::Output { status, stdout: out.into_bytes(), stderr: err.into_bytes() })
}

pub struct KillHandler;
impl Handler for KillHandler {
    fn call(&self, _k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: KillArgs = parse_args(args)?;
        if a.pid == 0 {
            return Err(ToolError::with_hint("ERR_BAD_INPUT", "pid must be a positive integer", json!({ "got": a.pid })));
        }
        kill_pid(a.pid as i32)
    }
}

fn kill_pid(pid: i32) -> Result<Value, ToolError> {
    #[cfg(unix)]
    {
        // process.kill semantics: ESRCH → not found, EPERM → refused
        let rc = unsafe { libc::kill(pid, libc::SIGKILL) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return Err(match err.raw_os_error() {
                Some(libc::ESRCH) => ToolError::with_hint("ERR_PROC_NOT_FOUND", format!("no process with pid {pid}"), json!({ "pid": pid })),
                Some(libc::EPERM) => ToolError::with_hint("ERR_REFUSED", format!("permission denied killing pid {pid}"), json!({ "pid": pid })),
                _ => ToolError::with_hint("ERR_SPAWN", format!("kill failed: {err}"), json!({ "pid": pid })),
            });
        }
        return Ok(json!({ "pid": pid, "signal": "SIGKILL", "requested": true }));
    }
    #[cfg(windows)]
    {
        // Node's process.kill on Windows terminates unconditionally, so both
        // SIGKILL and SIGTERM map to a hard kill here.
        let mut tk = Command::new("taskkill");
        tk.args(["/PID", &pid.to_string(), "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            tk.creation_flags(CREATE_NO_WINDOW);
        }
        match tk.status()
        {
            Ok(s) if s.success() => Ok(json!({ "pid": pid, "signal": "SIGKILL", "requested": true })),
            Ok(_) => Err(ToolError::with_hint("ERR_PROC_NOT_FOUND", format!("no process with pid {pid}"), json!({ "pid": pid }))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ToolError::new("ERR_CMD_NOT_FOUND", "taskkill is not available")),
            Err(e) => Err(ToolError::with_hint("ERR_SPAWN", format!("kill failed: {e}"), json!({ "pid": pid }))),
        }
    }
}

// ---- env.* ------------------------------------------------------------------------

fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// True when the env var looks like a credential (API key, token, secret,
/// password, private key, auth). These are redacted by default on env.get so
/// they never leak into the journal transcript.
fn is_secret_env(name: &str) -> bool {
    const MARKERS: &[&str] = &["KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL", "AUTH", "PRIVATE_KEY", "APIKEY", "ACCESS_KEY"];
    let upper = name.to_uppercase();
    if upper.contains("PRIVATE") && upper.contains("KEY") {
        return true;
    }
    MARKERS.iter().any(|m| upper.contains(m))
}

pub struct EnvGetHandler;
impl Handler for EnvGetHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: EnvNameArgs = parse_args(args)?;
        if !valid_env_name(&a.name) {
            return Err(ToolError::with_hint(
                "ERR_BAD_INPUT",
                "env name must match [A-Za-z_][A-Za-z0-9_]*",
                json!({ "got": a.name }),
            ));
        }
        // Secret masking: names that look like credentials are redacted from
        // the default output so they never leak into the journal. Only an
        // explicit reveal=true shows them in full.
        let is_secret = is_secret_env(&a.name);
        let reveal = a.reveal.unwrap_or(false);
        let mask = |v: String| -> String {
            if is_secret && !reveal { "***".to_string() } else { v }
        };
        if k.session_env.contains(&a.name) {
            let v = k.session_env.get(&a.name).unwrap_or_default();
            return Ok(json!({ "name": a.name, "value": mask(v), "source": "session", "masked": is_secret && !reveal }));
        }
        match std::env::var(&a.name) {
            Ok(v) => Ok(json!({ "name": a.name, "value": mask(v), "source": "host", "masked": is_secret && !reveal })),
            Err(_) => Ok(json!({ "name": a.name, "value": Value::Null, "source": "unset", "masked": false })),
        }
    }
}

pub struct EnvSetHandler;
impl Handler for EnvSetHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: EnvSetArgs = parse_args(args)?;
        if !valid_env_name(&a.name) {
            return Err(ToolError::with_hint(
                "ERR_BAD_INPUT",
                "env name must match [A-Za-z_][A-Za-z0-9_]*",
                json!({ "got": a.name }),
            ));
        }
        let previous = k.session_env.get(&a.name).or_else(|| std::env::var(&a.name).ok());
        k.session_env.set(&a.name, &a.value);
        Ok(json!({ "name": a.name, "value": a.value, "previous": previous, "source": "session" }))
    }
}

pub struct EnvListHandler;
impl Handler for EnvListHandler {
    fn call(&self, k: &Kernel, _args: &Value) -> Result<Value, ToolError> {
        Ok(json!({ "session": k.session_env.snapshot() }))
    }
}

#[cfg(test)]
mod base_dir_tests {
    use super::*;
    use std::fs;

    fn proc_kernel(root: &std::path::Path) -> Kernel {
        let mut k = Kernel::new(root.to_path_buf()).unwrap();
        register(&mut k);
        k
    }

    /// Create a disposable workspace whose name contains a unique marker, so
    /// a `pwd` test can prove the cwd routed there and not to the server root.
    fn workspace(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nct-proc-basedir-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// proc.spawn cwd must route to the baseDir workspace, not the server root.
    /// Proved with `pwd` on the RELEASE behavior (a real child process).
    #[test]
    fn proc_spawn_cwd_routes_to_baseDir() {
        let server_root = workspace("server");
        let target = workspace("target");
        let k = proc_kernel(&server_root);

        // default: cwd is the server root
        let def = k.call("proc.spawn", &json!({ "cmd": "pwd", "timeoutMs": 20000 }));
        assert!(def.ok, "pwd default should succeed: {:?}", def.error);
        let def_cwd = def.result.unwrap()["stdout"].as_str().unwrap().trim().to_string();
        assert!(def_cwd.to_lowercase().contains("server"), "default cwd should be the server root: {def_cwd}");

        // baseDir=target: cwd is the target workspace, regardless of server root
        let over = k.call("proc.spawn", &json!({ "cmd": "pwd", "timeoutMs": 20000, "baseDir": target.display().to_string() }));
        assert!(over.ok, "pwd baseDir should succeed: {:?}", over.error);
        let over_cwd = over.result.unwrap()["stdout"].as_str().unwrap().trim().to_string();
        assert!(over_cwd.to_lowercase().contains("target"), "baseDir should route cwd to the target: {over_cwd}");

        let _ = fs::remove_dir_all(&server_root);
        let _ = fs::remove_dir_all(&target);
    }

    /// proc.runScript with a baseDir script path resolves against that base.
    #[test]
    fn proc_runScript_path_resolves_against_baseDir() {
        let server_root = workspace("srv");
        let target = workspace("tgt");
        fs::write(target.join("hello.py"), "print('from-target')\n").unwrap();
        let k = proc_kernel(&server_root);

        let out = k.call("proc.runScript", &json!({
            "path": "hello.py",
            "baseDir": target.display().to_string(),
            "timeoutMs": 20000,
        }));
        assert!(out.ok, "runScript via baseDir should find the script: {:?}", out.error);
        let out_val = out.result.unwrap();
        let stdout = out_val["stdout"].as_str().unwrap_or("");
        assert!(stdout.contains("from-target"), "should run the target script, stdout: {stdout}");

        let _ = fs::remove_dir_all(&server_root);
        let _ = fs::remove_dir_all(&target);
    }

    /// A bad baseDir must not silently run in the server root. proc.spawn
    /// reports spawn failures inside a SUCCESSFUL tool result (proc.mjs
    /// close/error semantics), so the observable guarantee is: the child never
    /// starts with a bogus cwd — the result carries error ERR_SPAWN, and the
    /// cwd is NOT the server root.
    #[test]
    fn proc_spawn_bad_baseDir_errors() {
        let server_root = workspace("srv2");
        let k = proc_kernel(&server_root);
        let out = k.call("proc.spawn", &json!({ "cmd": "pwd", "timeoutMs": 20000, "baseDir": "/no/such/dir/xyz" }));
        // proc.spawn: ok=true at the tool level, but the result embeds an error.
        assert!(out.ok, "proc.spawn returns ok=true with an embedded error on spawn failure");
        let result = out.result.unwrap();
        let embedded = result["error"].as_object();
        assert!(embedded.is_some(), "result must carry an embedded error for a bad cwd: {result}");
        assert_eq!(embedded.unwrap()["code"], json!("ERR_SPAWN"), "should surface ERR_SPAWN");
        let cwd = result["stdout"].as_str().unwrap_or("");
        assert!(!cwd.to_lowercase().contains("srv2"), "must NOT fall back to the server root");
        let _ = fs::remove_dir_all(&server_root);
    }
}

#[cfg(test)]
mod env_mask_tests {
    use super::*;

    #[test]
    fn secret_names_are_detected() {
        for s in ["API_KEY", "GITHUB_TOKEN", "DB_PASSWORD", "AWS_SECRET_ACCESS_KEY", "PRIVATE_KEY", "AUTH_TOKEN"] {
            assert!(is_secret_env(s), "should mask: {s}");
        }
        for s in ["PATH", "HOME", "LANG", "NCTOOLS_WORKSPACE", "OPENAI_MODEL"] {
            assert!(!is_secret_env(s), "should NOT mask: {s}");
        }
    }

    #[test]
    fn env_get_masks_secret_by_default() {
        std::env::set_var("NCTOOLS_TEST_MASK_KEY", "supersecret");
        let dir = std::env::temp_dir().join(format!("nct-envmask-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut k = Kernel::new(dir.clone()).unwrap();
        register(&mut k);
        let out = k.call("env.get", &json!({ "name": "NCTOOLS_TEST_MASK_KEY" }));
        assert!(out.ok);
        let v = out.result.unwrap();
        assert_eq!(v["value"], json!("***"), "secret must be masked by default");
        assert_eq!(v["masked"], json!(true));
        std::env::remove_var("NCTOOLS_TEST_MASK_KEY");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_get_reveal_shows_secret() {
        std::env::set_var("NCTOOLS_TEST_REVEAL_TOKEN", "visible");
        let dir = std::env::temp_dir().join(format!("nct-envreveal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut k = Kernel::new(dir.clone()).unwrap();
        register(&mut k);
        let out = k.call("env.get", &json!({ "name": "NCTOOLS_TEST_REVEAL_TOKEN", "reveal": true }));
        assert!(out.ok);
        let v = out.result.unwrap();
        assert_eq!(v["value"], json!("visible"));
        assert_eq!(v["masked"], json!(false));
        std::env::remove_var("NCTOOLS_TEST_REVEAL_TOKEN");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_secret_env_is_never_masked() {
        std::env::set_var("NCTOOLS_TEST_PLAIN", "hello");
        let dir = std::env::temp_dir().join(format!("nct-envplain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut k = Kernel::new(dir.clone()).unwrap();
        register(&mut k);
        let out = k.call("env.get", &json!({ "name": "NCTOOLS_TEST_PLAIN" }));
        assert!(out.ok);
        let v = out.result.unwrap();
        assert_eq!(v["value"], json!("hello"));
        assert_eq!(v["masked"], json!(false));
        std::env::remove_var("NCTOOLS_TEST_PLAIN");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
