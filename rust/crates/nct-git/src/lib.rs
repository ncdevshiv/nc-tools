// git.* tools — thin typed wrappers over the git CLI (porcelain formats only).
// Behavior-parity port of src/kernel/git.mjs: git is an external program; the
// kernel's job is to turn its output into data. Each tool accepts `repo`
// (default: the base dir) so remote agents can work on ANY repository.
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use nct_core::childenv::child_env;
use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};
use nct_core::paths::resolve_checked;

pub const STATUS_DESC: &str = "Git status: repo path, branch, head, changed files.";
pub const DIFF_DESC: &str = "Unified diff of unstaged changes.";
pub const ADD_DESC: &str = "Stage files.";
pub const COMMIT_DESC: &str = "Commit staged changes.";
pub const LOG_DESC: &str = "Recent commits.";
pub const BRANCH_DESC: &str = "List branches (no args) or create one ({name}).";
pub const CHECKOUT_DESC: &str = "Switch branches (or create with create=true).";
pub const PUSH_DESC: &str = "Push to a remote (optionally sets upstream).";
pub const PULL_DESC: &str = "Pull from a remote (ff-only by default — no surprise merge commits).";


pub fn register(k: &mut Kernel) {
    k.register("git.status", STATUS_DESC, nct_core::schema::schema_for::<RepoArgs>(), Arc::new(StatusHandler));
    k.register("git.diff", DIFF_DESC, nct_core::schema::schema_for::<DiffArgs>(), Arc::new(DiffHandler));
    k.register("git.add", ADD_DESC, nct_core::schema::schema_for::<AddArgs>(), Arc::new(AddHandler));
    k.register("git.commit", COMMIT_DESC, nct_core::schema::schema_for::<CommitArgs>(), Arc::new(CommitHandler));
    k.register("git.log", LOG_DESC, nct_core::schema::schema_for::<LogArgs>(), Arc::new(LogHandler));
    k.register("git.branch", BRANCH_DESC, nct_core::schema::schema_for::<BranchArgs>(), Arc::new(BranchHandler));
    k.register("git.checkout", CHECKOUT_DESC, nct_core::schema::schema_for::<CheckoutArgs>(), Arc::new(CheckoutHandler));
    k.register("git.push", PUSH_DESC, nct_core::schema::schema_for::<PushArgs>(), Arc::new(PushHandler));
    k.register("git.pull", PULL_DESC, nct_core::schema::schema_for::<PullArgs>(), Arc::new(PullHandler));
}

use std::sync::Arc;

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepoArgs {
    #[doc = "Repo directory (default: base dir)"]
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiffArgs {
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    #[serde(default)]
    pub path: Option<String>,
    #[doc = "Repo directory (default: base dir)"]
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddArgs {
    #[schemars(length(min = 1))]
    pub paths: Vec<String>,
    #[doc = "Repo directory (default: base dir)"]
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommitArgs {
    pub message: String,
    #[doc = "Repo directory (default: base dir)"]
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LogArgs {
    #[serde(default)]
    #[schemars(range(min = 1, max = 200))]
    pub maxCount: Option<u64>,
    #[doc = "Repo directory (default: base dir)"]
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BranchArgs {
    #[serde(default)]
    pub name: Option<String>,
    #[doc = "Repo directory (default: base dir)"]
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckoutArgs {
    pub branch: String,
    #[serde(default)]
    pub create: Option<bool>,
    #[doc = "Repo directory (default: base dir)"]
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PushArgs {
    #[serde(default)]
    pub remote: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub setUpstream: Option<bool>,
    #[doc = "Repo directory (default: base dir)"]
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PullArgs {
    #[serde(default)]
    pub remote: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub ffOnly: Option<bool>,
    #[doc = "Repo directory (default: base dir)"]
    #[serde(default)]
    pub repo: Option<String>,
}

/// Resolve the repo dir for a call; must actually be a git repository.
fn in_repo(k: &Kernel, repo: Option<&str>) -> Result<PathBuf, ToolError> {
    let r = resolve_checked(&k.root, repo.unwrap_or("."))?;
    if !r.join(".git").exists() {
        return Err(ToolError::with_hint(
            "ERR_NOT_A_REPO",
            format!("not a git repository: {}", r.display()),
            json!({ "repo": r.display().to_string() }),
        ));
    }
    Ok(r)
}

/// Run git in dir with porcelain args; stdout on success, ERR_GIT with
/// structured hint on failure (git.mjs `git()`).
fn git(dir: &std::path::Path, args: &[&str], k: &Kernel) -> Result<String, ToolError> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .env_clear()
        .envs(child_env(&k.session_env.snapshot()));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(nct_core::CREATE_NO_WINDOW);
    }
    let started = Instant::now();
    let timeout = Duration::from_millis(60_000);
    let mut child = match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ToolError::new("ERR_GIT_SPAWN", "git failed to start: spawn git ENOENT"));
        }
        Err(e) => return Err(ToolError::new("ERR_GIT_SPAWN", format!("git failed to start: {e}"))),
    };
    // bounded wait: git is expected to be fast; on timeout the child is killed
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                use std::io::Read;
                let mut out = String::new();
                if let Some(mut s) = child.stdout.take() {
                    let _ = s.read_to_string(&mut out);
                }
                let mut err = String::new();
                if let Some(mut s) = child.stderr.take() {
                    let _ = s.read_to_string(&mut err);
                }
                if !status.success() {
                    let tail: String = if !err.trim().is_empty() { &err } else { &out }
                        .trim()
                        .chars()
                        .take(500)
                        .collect();
                    return Err(ToolError::with_hint(
                        "ERR_GIT",
                        format!("git {} failed (exit {}): {}", args[0], status.code().unwrap_or(-1), tail),
                        json!({ "args": args, "stderr": err.chars().take(2000).collect::<String>() }),
                    ));
                }
                return Ok(out);
            }
            Ok(None) => {
                if started.elapsed() > timeout {
                    let _ = child.kill();
                    return Err(ToolError::new("ERR_GIT", "git timed out after 60s"));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => return Err(ToolError::new("ERR_GIT_SPAWN", format!("git failed to start: {e}"))),
        }
    }
}

pub struct StatusHandler;
impl Handler for StatusHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: RepoArgs = parse_args(args)?;
        let r = in_repo(k, a.repo.as_deref())?;
        let out = git(&r, &["status", "--porcelain=v1", "-b"], k)?;
        let lines: Vec<&str> = out.split('\n').filter(|l| !l.is_empty()).collect();
        let branch = lines
            .first()
            .filter(|l| l.starts_with("## "))
            .map(|l| l[3..].split("...").next().unwrap_or("").trim().to_string());
        let files: Vec<Value> = lines
            .iter()
            .skip(1)
            .map(|l| {
                let status_code = l.chars().take(2).collect::<String>().trim().to_string();
                let status_code = if status_code.is_empty() { "?".to_string() } else { status_code };
                let path = l.get(3..).map(|s| s.trim().to_string()).unwrap_or_default();
                json!({ "status": status_code, "path": path })
            })
            .filter(|f| f["path"] != json!(".nc-tools") && !f["path"].as_str().unwrap_or("").starts_with(".nc-tools/"))
            .collect();
        let head = git(&r, &["rev-parse", "--short", "HEAD"], k)
            .ok()
            .map(|s| s.trim().to_string());
        Ok(json!({
            "repo": r.display().to_string(),
            "branch": branch,
            "head": head,
            "files": files,
        }))
    }
}

pub struct DiffHandler;
impl Handler for DiffHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: DiffArgs = parse_args(args)?;
        let r = in_repo(k, a.repo.as_deref())?;
        let out = match &a.path {
            Some(p) => git(&r, &["diff", "--no-color", "--", p], k)?,
            None => git(&r, &["diff", "--no-color"], k)?,
        };
        Ok(json!({ "repo": r.display().to_string(), "diff": out }))
    }
}

pub struct AddHandler;
impl Handler for AddHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: AddArgs = parse_args(args)?;
        let r = in_repo(k, a.repo.as_deref())?;
        if a.paths.is_empty() {
            return Err(ToolError::new("ERR_BAD_INPUT", "paths must be a non-empty array"));
        }
        let mut git_args: Vec<&str> = vec!["add", "--"];
        for p in &a.paths {
            git_args.push(p);
        }
        git(&r, &git_args, k)?;
        Ok(json!({ "repo": r.display().to_string(), "added": a.paths }))
    }
}

pub struct CommitHandler;
impl Handler for CommitHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: CommitArgs = parse_args(args)?;
        let r = in_repo(k, a.repo.as_deref())?;
        if a.message.trim().is_empty() {
            return Err(ToolError::new("ERR_BAD_INPUT", "message required"));
        }
        git(&r, &["commit", "-m", &a.message], k)?;
        let sha = git(&r, &["rev-parse", "--short", "HEAD"], k)?.trim().to_string();
        Ok(json!({ "repo": r.display().to_string(), "sha": sha, "message": a.message }))
    }
}

pub struct LogHandler;
impl Handler for LogHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: LogArgs = parse_args(args)?;
        let r = in_repo(k, a.repo.as_deref())?;
        let max_count = a.maxCount.unwrap_or(20);
        let out = git(
            &r,
            &[
                "log",
                &format!("--max-count={max_count}"),
                "--pretty=format:%H%x1f%an%x1f%aI%x1f%s",
            ],
            k,
        )?;
        let commits: Vec<Value> = out
            .split('\n')
            .filter(|l| !l.is_empty())
            .filter_map(|l| {
                let parts: Vec<&str> = l.split('\x1f').collect();
                if parts.len() >= 4 {
                    Some(json!({
                        "sha": parts[0],
                        "author": parts[1],
                        "date": parts[2],
                        "message": parts[3],
                    }))
                } else {
                    None
                }
            })
            .collect();
        Ok(json!({ "repo": r.display().to_string(), "commits": commits }))
    }
}

pub struct BranchHandler;
impl Handler for BranchHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: BranchArgs = parse_args(args)?;
        let r = in_repo(k, a.repo.as_deref())?;
        if let Some(name) = &a.name {
            if name.trim().is_empty() {
                return Err(ToolError::new("ERR_BAD_INPUT", "branch name required"));
            }
            git(&r, &["branch", name.trim()], k)?;
            return Ok(json!({ "repo": r.display().to_string(), "branch": name.trim(), "created": true }));
        }
        let out = git(&r, &["branch", "--list"], k)?;
        let branches: Vec<Value> = out
            .split('\n')
            .filter(|l| !l.is_empty())
            .map(|l| {
                let current = l.trim_start().starts_with('*');
                let name = l.trim_start().trim_start_matches('*').trim().to_string();
                json!({ "name": name, "current": current })
            })
            .collect();
        let current = branches
            .iter()
            .find(|b| b["current"] == json!(true))
            .and_then(|b| b["name"].as_str())
            .map(String::from);
        Ok(json!({
            "repo": r.display().to_string(),
            "branches": branches,
            "current": current,
        }))
    }
}

pub struct CheckoutHandler;
impl Handler for CheckoutHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: CheckoutArgs = parse_args(args)?;
        let r = in_repo(k, a.repo.as_deref())?;
        if a.branch.trim().is_empty() {
            return Err(ToolError::new("ERR_BAD_INPUT", "branch name required"));
        }
        let create = a.create.unwrap_or(false);
        let out = if create {
            git(&r, &["checkout", "-b", a.branch.trim()], k)
        } else {
            git(&r, &["checkout", a.branch.trim()], k)
        }?;
        let _ = out;
        Ok(json!({ "repo": r.display().to_string(), "branch": a.branch.trim(), "created": create }))
    }
}

pub struct PushHandler;
impl Handler for PushHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: PushArgs = parse_args(args)?;
        let r = in_repo(k, a.repo.as_deref())?;
        let remote = a.remote.as_deref().unwrap_or("origin");
        let name = a.branch.as_deref().filter(|s| !s.is_empty());
        let set_upstream = a.setUpstream.unwrap_or(true);
        let mut git_args: Vec<&str> = vec!["push"];
        let upstream_arg = "-u";
        if name.is_some() && set_upstream {
            git_args.push(upstream_arg);
        }
        git_args.push(remote);
        if let Some(n) = name {
            git_args.push(n);
        }
        let out = git(&r, &git_args, k)?;
        let output: Vec<String> = out
            .trim()
            .split('\n')
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        Ok(json!({
            "repo": r.display().to_string(),
            "remote": remote,
            "branch": a.branch,
            "upstream": name.is_some() && set_upstream,
            "output": output,
        }))
    }
}

pub struct PullHandler;
impl Handler for PullHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: PullArgs = parse_args(args)?;
        let r = in_repo(k, a.repo.as_deref())?;
        let remote = a.remote.as_deref().unwrap_or("origin");
        let name = a.branch.as_deref().filter(|s| !s.is_empty());
        let ff_only = a.ffOnly.unwrap_or(true);
        let mut git_args: Vec<&str> = vec!["pull", "--no-edit"];
        let ff_arg = "--ff-only";
        if ff_only {
            git_args.push(ff_arg);
        }
        git_args.push(remote);
        if let Some(n) = name {
            git_args.push(n);
        }
        let out = git(&r, &git_args, k)?;
        let output: Vec<String> = out
            .trim()
            .split('\n')
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        Ok(json!({
            "repo": r.display().to_string(),
            "remote": remote,
            "branch": a.branch,
            "output": output,
        }))
    }
}
