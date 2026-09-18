// pkg.* — typed package-ecosystem drivers (npm, pip). Behavior-parity port
// of src/kernel/pkg.mjs: installing, listing, scripts — without shelling
// into package-manager CLI semantics ad hoc.
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{json, Value};

use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};
use nct_core::paths::resolve_checked;

use super::{run_sync, schema};
use std::time::Duration;

pub const ADD_DESC: &str =
    "Install packages (npm or pip). Structured result; ERR_NETWORK hint if registry unreachable.";
pub const LIST_DESC: &str = "List installed packages for npm (from package.json) or pip.";
pub const SCRIPTS_DESC: &str = "List npm scripts defined in package.json.";
pub const RUN_SCRIPT_DESC: &str =
    "Run an npm script with typed args. Returns exit code + captured output.";

pub fn register_pkg(k: &mut Kernel) {
    k.register(
        "pkg.add",
        ADD_DESC,
        schema::<AddArgs>(),
        std::sync::Arc::new(AddHandler),
    );
    k.register(
        "pkg.list",
        LIST_DESC,
        schema::<ListArgs>(),
        std::sync::Arc::new(ListHandler),
    );
    k.register(
        "pkg.scripts",
        SCRIPTS_DESC,
        schema::<ScriptsArgs>(),
        std::sync::Arc::new(ScriptsHandler),
    );
    k.register(
        "pkg.runScript",
        RUN_SCRIPT_DESC,
        schema::<RunScriptArgs>(),
        std::sync::Arc::new(RunScriptHandler),
    );
}

const NETWORK_HINTS: &[&str] = &[
    "ENOTFOUND",
    "ETIMEDOUT",
    "ECONNREFUSED",
    "EAI_AGAIN",
    "network",
    "ECONNRESET",
];

#[derive(Deserialize, schemars::JsonSchema, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Manager {
    Npm,
    Pip,
}

/// Windows: npm is a .CMD shim; nc-tools never uses shells, so npm runs as
/// `node <npm-cli.js>`. npm-cli.js lives next to the node executable (portable
/// Windows layout) or under <prefix>/lib/node_modules (unix global layout);
/// both are resolved from the host PATH. A bundled layout (npm shipped next
/// to the kernel binary itself) is probed as a fallback.
fn resolve_node_dir() -> Option<std::path::PathBuf> {
    let exe = if cfg!(windows) { "node.exe" } else { "node" };
    let paths = std::env::var("PATH")
        .or_else(|_| std::env::var("Path"))
        .ok()?;
    for dir in std::env::split_paths(&paths) {
        let cand = dir.join(exe);
        if cand.is_file() {
            return cand.parent().map(|p| p.to_path_buf());
        }
    }
    None
}

fn resolve_npm_cli() -> Result<PathBuf, ToolError> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(node_dir) = resolve_node_dir() {
        candidates.push(
            node_dir
                .join("node_modules")
                .join("npm")
                .join("bin")
                .join("npm-cli.js"),
        );
        candidates.push(
            node_dir
                .join("..")
                .join("lib")
                .join("node_modules")
                .join("npm")
                .join("bin")
                .join("npm-cli.js"),
        );
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            candidates.push(
                exe_dir
                    .join("node_modules")
                    .join("npm")
                    .join("bin")
                    .join("npm-cli.js"),
            );
            candidates.push(
                exe_dir
                    .join("..")
                    .join("lib")
                    .join("node_modules")
                    .join("npm")
                    .join("bin")
                    .join("npm-cli.js"),
            );
        }
    }
    for c in &candidates {
        if c.exists() {
            return Ok(c.to_path_buf());
        }
    }
    Err(ToolError::new(
        "ERR_CMD_NOT_FOUND",
        "npm-cli.js not found: no node on PATH and none next to the running executable",
    ))
}

/// Run a package-manager child; npm is dispatched through node + npm-cli.js.
fn pm_run(
    k: &Kernel,
    dir: &std::path::Path,
    cmd: &str,
    args: &[String],
    timeout_ms: u64,
) -> Result<std::process::Output, ToolError> {
    if cmd == "npm" {
        let cli = resolve_npm_cli()?;
        let node = if cfg!(windows) { "node.exe" } else { "node" };
        let mut full: Vec<String> = vec![cli.display().to_string()];
        full.extend(args.iter().cloned());
        return run_sync(
            node,
            &full.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            Duration::from_millis(timeout_ms),
            &k.session_env.snapshot(),
            dir,
        );
    }
    run_sync(
        cmd,
        &args.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        Duration::from_millis(timeout_ms),
        &k.session_env.snapshot(),
        dir,
    )
}

fn is_network_failure(stderr_tail: &str) -> bool {
    let lower = stderr_tail.to_lowercase();
    NETWORK_HINTS
        .iter()
        .any(|h| lower.contains(&h.to_lowercase()))
}

fn tail(s: &str, n: usize) -> String {
    s.chars()
        .rev()
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

// ---- typed args ----------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddArgs {
    #[serde(default)]
    pub manager: Option<Manager>,
    #[schemars(length(min = 1))]
    pub names: Vec<String>,
    #[serde(default)]
    pub dev: Option<bool>,
    #[serde(default)]
    #[schemars(range(min = 1000))]
    pub timeoutMs: Option<u64>,
    #[doc = "Directory (default: base dir)"]
    #[serde(default)]
    pub dir: Option<String>,
    #[doc = "Base dir for relative paths (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListArgs {
    #[serde(default)]
    pub manager: Option<Manager>,
    #[doc = "Directory (default: base dir)"]
    #[serde(default)]
    pub dir: Option<String>,
    #[doc = "Base dir for relative paths (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScriptsArgs {
    #[doc = "Directory (default: base dir)"]
    #[serde(default)]
    pub dir: Option<String>,
    #[doc = "Base dir for relative paths (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunScriptArgs {
    pub name: String,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    #[schemars(range(min = 1000))]
    pub timeoutMs: Option<u64>,
    #[doc = "Directory (default: base dir)"]
    #[serde(default)]
    pub dir: Option<String>,
    #[doc = "Base dir for relative paths (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
}

// ---- handlers -------------------------------------------------------------------

pub struct AddHandler;
impl Handler for AddHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: AddArgs = parse_args(args)?;
        let manager = match a.manager.unwrap_or(Manager::Npm) {
            Manager::Npm => "npm",
            Manager::Pip => "pip",
        }
        .to_string();
        if a.names.is_empty() || a.names.iter().any(|n| n.trim().is_empty()) {
            return Err(ToolError::new(
                "ERR_BAD_INPUT",
                "names must be a non-empty array of strings",
            ));
        }
        let d = resolve_checked(
            &k.base_dir(a.baseDir.as_deref())?,
            a.dir.as_deref().unwrap_or("."),
        )?;
        let timeout = a.timeoutMs.unwrap_or(k.cfg.limits.child_timeout_ms);
        let dev = a.dev.unwrap_or(false);
        let install_args: Vec<String> = match manager.as_str() {
            "npm" => {
                let mut v: Vec<String> = vec![
                    "install".into(),
                    "--no-audit".into(),
                    "--no-fund".into(),
                    "--loglevel=error".into(),
                ];
                if dev {
                    v.push("--save-dev".into());
                }
                v.extend(a.names.iter().cloned());
                v
            }
            "pip" => {
                let mut v: Vec<String> = vec!["-m".into(), "pip".into(), "install".into()];
                v.extend(a.names.iter().cloned());
                v
            }
            other => {
                return Err(ToolError::new(
                    "ERR_BAD_INPUT",
                    format!("unsupported manager: {other} (supported: npm, pip)"),
                ))
            }
        };
        let out = pm_run(k, &d, &manager, &install_args, timeout)?;
        if !out.status.success() {
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stderr),
                String::from_utf8_lossy(&out.stdout)
            );
            let err_tail = tail(combined.trim_end(), 400);
            let is_net = is_network_failure(&err_tail);
            return Err(ToolError::with_hint(
                if is_net { "ERR_NETWORK" } else { "ERR_PKG" },
                format!(
                    "{manager} install failed (exit {})",
                    out.status.code().unwrap_or(-1)
                ),
                serde_json::json!({ "names": a.names, "stderrTail": err_tail }),
            ));
        }
        if manager == "pip" {
            return Ok(json!({ "manager": manager, "installed": a.names }));
        }
        Ok(json!({ "manager": manager, "installed": a.names, "dev": dev }))
    }
}

pub struct ListHandler;
impl Handler for ListHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: ListArgs = parse_args(args)?;
        let manager = match a.manager.unwrap_or(Manager::Npm) {
            Manager::Npm => "npm",
            Manager::Pip => "pip",
        }
        .to_string();
        let d = resolve_checked(
            &k.base_dir(a.baseDir.as_deref())?,
            a.dir.as_deref().unwrap_or("."),
        )?;
        match manager.as_str() {
            "npm" => {
                if !d.join("package.json").exists() {
                    return Ok(
                        json!({ "manager": manager, "packages": [], "note": "no package.json in workspace" }),
                    );
                }
                let out = pm_run(
                    k,
                    &d,
                    "npm",
                    &["ls".to_string(), "--json".into(), "--depth=0".into()],
                    k.cfg.limits.child_timeout_ms,
                )?;
                let parsed: Value = serde_json::from_slice(&out.stdout)
                    .map_err(|_| ToolError::new("ERR_PKG", "npm ls produced unparseable output"))?;
                let mut packages = Vec::new();
                if let Some(deps) = parsed.get("dependencies").and_then(|d| d.as_object()) {
                    for (name, info) in deps {
                        packages.push(json!({
                            "name": name,
                            "version": info.get("version").cloned().unwrap_or(Value::Null),
                            "missing": info.get("missing").and_then(|m| m.as_bool()).unwrap_or(false),
                            "problems": info.get("problems").cloned(),
                        }));
                    }
                }
                let total = packages.len();
                Ok(json!({ "manager": manager, "packages": packages, "total": total }))
            }
            "pip" => {
                let out = pm_run(
                    k,
                    &d,
                    "python",
                    &[
                        "-m".into(),
                        "pip".into(),
                        "list".into(),
                        "--format".into(),
                        "json".into(),
                    ],
                    k.cfg.limits.child_timeout_ms,
                )?;
                let parsed: Value = serde_json::from_slice(&out.stdout).map_err(|_| {
                    ToolError::new("ERR_PKG", "pip list produced unparseable output")
                })?;
                let total = parsed.as_array().map(|a| a.len()).unwrap_or(0);
                Ok(json!({ "manager": manager, "packages": parsed, "total": total }))
            }
            other => Err(ToolError::new(
                "ERR_BAD_INPUT",
                format!("unsupported manager: {other}"),
            )),
        }
    }
}

pub struct ScriptsHandler;
impl Handler for ScriptsHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: ScriptsArgs = parse_args(args)?;
        let d = resolve_checked(
            &k.base_dir(a.baseDir.as_deref())?,
            a.dir.as_deref().unwrap_or("."),
        )?;
        let pj = d.join("package.json");
        if !pj.exists() {
            return Err(ToolError::with_hint(
                "ERR_NOT_FOUND",
                "no package.json in workspace",
                json!({ "path": "package.json" }),
            ));
        }
        let raw = std::fs::read_to_string(&pj).map_err(ToolError::from)?;
        let parsed: Value = serde_json::from_str(&raw).map_err(|e| {
            ToolError::new("ERR_PARSE", format!("package.json is not valid JSON: {e}"))
        })?;
        Ok(json!({ "scripts": parsed.get("scripts").cloned().unwrap_or_else(|| json!({})) }))
    }
}

pub struct RunScriptHandler;
impl Handler for RunScriptHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: RunScriptArgs = parse_args(args)?;
        if a.name.is_empty() {
            return Err(ToolError::new("ERR_BAD_INPUT", "script name required"));
        }
        let script_args = a.args.clone().unwrap_or_default();
        if script_args.iter().any(|s| s.trim().is_empty() && false) {
            return Err(ToolError::new(
                "ERR_BAD_INPUT",
                "args must be an array of strings",
            ));
        }
        let d = resolve_checked(
            &k.base_dir(a.baseDir.as_deref())?,
            a.dir.as_deref().unwrap_or("."),
        )?;
        let timeout = a.timeoutMs.unwrap_or(k.cfg.limits.child_timeout_ms);
        let mut npm_args: Vec<String> = vec!["run".into(), a.name.clone(), "--".into()];
        npm_args.extend(script_args);
        let out = pm_run(k, &d, "npm", &npm_args, timeout)?;
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        Ok(json!({
            "script": a.name,
            "exitCode": out.status.code(),
            "stdout": tail(stdout.trim_end(), k.cfg.limits.script_stdout_tail),
            "stderr": tail(stderr.trim_end(), 50_000),
            "ok": out.status.success(),
        }))
    }
}

#[cfg(test)]
mod base_dir_tests {
    use super::*;

    fn pkg_kernel(root: &std::path::Path) -> Kernel {
        let mut k = Kernel::new(root.to_path_buf()).unwrap();
        crate::register_pkg(&mut k);
        k
    }

    fn workspace(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nct-pkg-basedir-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// pkg.scripts must read the TARGET workspace's package.json when baseDir
    /// is passed, and keep reading the server root's when not (default
    /// unchanged). Distinctive script names prove which file was read.
    #[test]
    fn pkg_scripts_routes_to_baseDir() {
        let server_root = workspace("server");
        let target = workspace("target");
        std::fs::write(
            server_root.join("package.json"),
            r#"{ "name": "server-ws", "scripts": { "SERVER_MARKER": "echo server" } }"#,
        )
        .unwrap();
        std::fs::write(
            target.join("package.json"),
            r#"{ "name": "target-ws", "scripts": { "TARGET_MARKER": "echo target" } }"#,
        )
        .unwrap();
        let k = pkg_kernel(&server_root);

        // baseDir=target: the target's scripts, never the server root's
        let over = ScriptsHandler
            .call(&k, &json!({ "baseDir": target.display().to_string() }))
            .unwrap();
        let over_scripts = over["scripts"].as_object().unwrap();
        assert!(
            over_scripts.contains_key("TARGET_MARKER"),
            "must read the target package.json: {over}"
        );
        assert!(
            !over_scripts.contains_key("SERVER_MARKER"),
            "must NOT read the server package.json: {over}"
        );

        // default: unchanged — the server root's scripts
        let def = ScriptsHandler.call(&k, &json!({})).unwrap();
        let def_scripts = def["scripts"].as_object().unwrap();
        assert!(
            def_scripts.contains_key("SERVER_MARKER"),
            "default must read the server package.json: {def}"
        );

        let _ = std::fs::remove_dir_all(&server_root);
        let _ = std::fs::remove_dir_all(&target);
    }

    /// A bad baseDir is an error, never a silent fallback to the server root.
    #[test]
    fn pkg_bad_baseDir_errors() {
        let server_root = workspace("srv2");
        std::fs::write(
            server_root.join("package.json"),
            r#"{ "name": "server-ws", "scripts": { "x": "echo x" } }"#,
        )
        .unwrap();
        let k = pkg_kernel(&server_root);
        let err = ScriptsHandler
            .call(&k, &json!({ "baseDir": "/no/such/ws/xyz" }))
            .unwrap_err();
        assert_eq!(
            err.code, "ERR_BAD_PATH",
            "bad baseDir must surface as ERR_BAD_PATH"
        );
        let _ = std::fs::remove_dir_all(&server_root);
    }
}
