// git.* tools — thin typed wrappers over the git CLI (porcelain formats only).
// Behavior-parity port of src/kernel/git.mjs: git is an external program; the
// kernel's job is to turn its output into data. Each tool accepts `repo`
// (default: the base dir) so remote agents can work on ANY repository.
use std::collections::HashMap;
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
pub const BLAME_DESC: &str = "Per-line blame for a file: commit sha, author, timestamp, content, line number. Uses git blame --line-porcelain.";


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
    k.register("git.blame", BLAME_DESC, nct_core::schema::schema_for::<BlameArgs>(), Arc::new(BlameHandler));
}

use std::sync::Arc;

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepoArgs {
    #[doc = "Repo directory (default: base dir)"]
    #[serde(default)]
    pub repo: Option<String>,
    #[doc = "Base directory override (default: kernel base dir). Pass when working on a different workspace than the server was started on."]
    #[serde(default)]
    pub baseDir: Option<String>,
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
    #[doc = "Base directory override (default: kernel base dir)"]
    #[serde(default)]
    pub baseDir: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddArgs {
    #[schemars(length(min = 1))]
    pub paths: Vec<String>,
    #[doc = "Repo directory (default: base dir)"]
    #[serde(default)]
    pub repo: Option<String>,
    #[doc = "Base directory override (default: kernel base dir)"]
    #[serde(default)]
    pub baseDir: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommitArgs {
    pub message: String,
    #[doc = "Repo directory (default: base dir)"]
    #[serde(default)]
    pub repo: Option<String>,
    #[doc = "Base directory override (default: kernel base dir)"]
    #[serde(default)]
    pub baseDir: Option<String>,
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
    #[doc = "Base directory override (default: kernel base dir)"]
    #[serde(default)]
    pub baseDir: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BranchArgs {
    #[serde(default)]
    pub name: Option<String>,
    #[doc = "Repo directory (default: base dir)"]
    #[serde(default)]
    pub repo: Option<String>,
    #[doc = "Base directory override (default: kernel base dir)"]
    #[serde(default)]
    pub baseDir: Option<String>,
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
    #[doc = "Base directory override (default: kernel base dir)"]
    #[serde(default)]
    pub baseDir: Option<String>,
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
    #[doc = "Base directory override (default: kernel base dir)"]
    #[serde(default)]
    pub baseDir: Option<String>,
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
    #[doc = "Base directory override (default: kernel base dir)"]
    #[serde(default)]
    pub baseDir: Option<String>,
}

/// Resolve the repo dir for a call; must actually be a git repository.
fn in_repo(base: &std::path::Path, repo: Option<&str>) -> Result<PathBuf, ToolError> {
    let r = resolve_checked(base, repo.unwrap_or("."))?;
    if !r.join(".git").exists() {
        return Err(ToolError::with_hint(
            "ERR_NOT_A_REPO",
            format!("not a git repository: {}", r.display()),
            json!({ "repo": r.display().to_string() }),
        ));
    }
    Ok(r)
}

/// The per-call workspace override every git arg carries (default: kernel base).
trait HasBaseDir {
    fn base_dir(&self) -> Option<&str>;
}
macro_rules! impl_has_base_dir {
    ($($t:ty),+ $(,)?) => {
        $(impl HasBaseDir for $t {
            fn base_dir(&self) -> Option<&str> {
                self.baseDir.as_deref()
            }
        })+
    };
}
impl_has_base_dir!(RepoArgs, DiffArgs, AddArgs, CommitArgs, LogArgs, BranchArgs, CheckoutArgs, PushArgs, PullArgs, BlameArgs);

/// Effective base directory for a git call: the per-call baseDir override
/// (workspace override) wins over the server root. Mirrors the fs.* family.
fn base_of<T: HasBaseDir>(k: &Kernel, a: &T) -> Result<PathBuf, ToolError> {
    k.base_dir(a.base_dir())
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
        let base = base_of(k, &a)?;
        let r = in_repo(&base, a.repo.as_deref())?;
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
        let base = base_of(k, &a)?;
        let r = in_repo(&base, a.repo.as_deref())?;
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
        let base = base_of(k, &a)?;
        let r = in_repo(&base, a.repo.as_deref())?;
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
        let base = base_of(k, &a)?;
        let r = in_repo(&base, a.repo.as_deref())?;
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
        let base = base_of(k, &a)?;
        let r = in_repo(&base, a.repo.as_deref())?;
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
        let base = base_of(k, &a)?;
        let r = in_repo(&base, a.repo.as_deref())?;
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
        let base = base_of(k, &a)?;
        let r = in_repo(&base, a.repo.as_deref())?;
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
        let base = base_of(k, &a)?;
        let r = in_repo(&base, a.repo.as_deref())?;
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
        let base = base_of(k, &a)?;
        let r = in_repo(&base, a.repo.as_deref())?;
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

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlameArgs {
    #[doc = "File to blame — relative to the repo base dir, or absolute"]
    pub path: String,
    #[doc = "Repo directory (default: base dir)"]
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub lineStart: Option<u64>,
    #[serde(default)]
    pub lineEnd: Option<u64>,
    #[doc = "Base directory override (default: kernel base dir)"]
    #[serde(default)]
    pub baseDir: Option<String>,
}

pub struct BlameHandler;
impl Handler for BlameHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: BlameArgs = parse_args(args)?;
        let base = base_of(k, &a)?;
        let r = in_repo(&base, a.repo.as_deref())?;
        let abs = resolve_checked(&base, &a.path)?;
        if !abs.exists() {
            return Err(ToolError::with_hint("ERR_NOT_FOUND", format!("no such file: {}", a.path), json!({ "path": a.path })));
        }
        let rel = nct_core::helpers::rel_slash(&r, &abs);
        let mut gargs: Vec<String> = vec!["blame".to_string(), "--line-porcelain".to_string()];
        if let Some(s) = a.lineStart {
            let e = a.lineEnd.unwrap_or(s);
            gargs.push(format!("-L{s},{e}"));
        }
        gargs.push(rel.clone());
        let raw = git(&r, &gargs.iter().map(|s| s.as_str()).collect::<Vec<_>>(), k)?;
        let mut lines: Vec<Value> = Vec::new();
        let mut cur: HashMap<String, String> = HashMap::new();
        for l in raw.lines() {
            if l.is_empty() {
                continue;
            }
            if let Some(content) = l.strip_prefix('\t') {
                lines.push(json!({
                    "line": cur.get("final").and_then(|s| s.parse::<u64>().ok()),
                    "commit": cur.get("commit"),
                    "author": cur.get("author"),
                    "time": cur.get("time").and_then(|s| s.parse::<u64>().ok()),
                    "content": content,
                }));
                cur.clear();
            } else if l.len() >= 40 && l.chars().take(40).all(|c| c.is_ascii_hexdigit()) {
                let mut it = l.split_whitespace();
                cur.insert("commit".to_string(), it.next().unwrap_or("").to_string());
                it.next();
                if let Some(f) = it.next() {
                    cur.insert("final".to_string(), f.to_string());
                }
            } else if let Some(rest) = l.strip_prefix("author ") {
                cur.insert("author".to_string(), rest.trim().to_string());
            } else if let Some(rest) = l.strip_prefix("author-time ") {
                cur.insert("time".to_string(), rest.trim().to_string());
            }
        }
        Ok(json!({
            "repo": r.display().to_string(),
            "path": rel,
            "lines": lines,
            "total": lines.len(),
        }))
    }
}

#[cfg(test)]
mod base_dir_tests {
    use super::*;
    use std::fs;

    /// Build a fresh Kernel with the git tools registered, rooted on `root`.
    fn git_kernel(root: &std::path::Path) -> Kernel {
        let mut k = Kernel::new(root.to_path_buf()).unwrap();
        register(&mut k);
        k
    }

    /// A real, initialized git repo throws away; returns its path.
    fn init_repo(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nct-git-basedir-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&dir)
            .status()
            .unwrap();
        fs::write(dir.join("README.md"), format!("{tag}\n")).unwrap();
        std::process::Command::new("git").args(["add", "."]).current_dir(&dir).status().unwrap();
        std::process::Command::new("git").args(["commit", "-m", "init"]).current_dir(&dir).status().unwrap();
        dir
    }

    /// A server rooted on A must see B's git data when baseDir=B is passed, and
    /// A's data by default. This is the exact wrong-workspace regression the
    /// wave fixed.
    #[test]
    fn git_status_baseDir_routes_to_requested_workspace() {
        let server_root = init_repo("server");
        let target = init_repo("target");
        let k = git_kernel(&server_root);

        // default (no baseDir): server root's repo
        let def = git_kernel(&server_root).call("git.status", &json!({}));
        assert!(def.ok, "status default should succeed: {:?}", def.error);
        let def_repo = def.result.unwrap()["repo"].as_str().unwrap().to_string();
        assert!(def_repo.contains("server"), "default should be the server root repo: {def_repo}");

        // with baseDir=target: the OTHER workspace's repo
        let overridden = k.call("git.status", &json!({ "baseDir": target.display().to_string() }));
        assert!(overridden.ok, "status baseDir should succeed: {:?}", overridden.error);
        let o_repo = overridden.result.unwrap()["repo"].as_str().unwrap().to_string();
        assert!(o_repo.contains("target"), "baseDir should route to the target repo: {o_repo}");

        // both must read the same file; target README says "target", so diff
        // the committed tree to prove we touched the right repo.
        let head = k.call("git.log", &json!({ "baseDir": target.display().to_string(), "maxCount": 1 }));
        assert!(head.ok);
        let head_val = head.result.unwrap();
        let commits = head_val["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 1, "target repo should have exactly 1 commit");

        let _ = fs::remove_dir_all(&server_root);
        let _ = fs::remove_dir_all(&target);
    }

    /// bad baseDir must ERROR, not silently fall back to the server root.
    /// A non-existent dir is path-resolvable (loose resolution) but not a git
    /// repo — so in_repo rejects it as ERR_NOT_A_REPO. The contract is "never
    /// silently reuse the server root's repo", which this proves: it errors.
    #[test]
    fn bad_baseDir_errors_instead_of_silent_fallback() {
        let server_root = init_repo("server2");
        let k = git_kernel(&server_root);
        let out = k.call("git.status", &json!({ "baseDir": "/definitely/not/a/real/dir" }));
        assert!(!out.ok, "bad baseDir must fail, got false-ok");
        let e = out.error.unwrap();
        // Not a git repo at that path — must NOT fall back to the server repo.
        assert_eq!(e.code, "ERR_NOT_A_REPO", "should reject the dir as not-a-repo: {}", e.code);
        let _ = fs::remove_dir_all(&server_root);
    }

    /// commit into a target repo via baseDir must land in the TARGET, not server.
    #[test]
    fn git_commit_via_baseDir_lands_in_target_repo() {
        let server_root = init_repo("srv");
        let target = init_repo("tgt");
        let k = git_kernel(&server_root);
        // mutate a file in the target
        fs::write(target.join("feature.txt"), "feature work\n").unwrap();
        let add = k.call("git.add", &json!({ "paths": ["feature.txt"], "baseDir": target.display().to_string() }));
        assert!(add.ok, "add should succeed: {:?}", add.error);
        let commit = k.call("git.commit", &json!({ "message": "feat: via baseDir", "baseDir": target.display().to_string() }));
        assert!(commit.ok, "commit should succeed: {:?}", commit.error);
        // target repo now has 2 commits; server root still 1 (untouched).
        let tgt_log = k.call("git.log", &json!({ "baseDir": target.display().to_string(), "maxCount": 10 }));
        assert_eq!(tgt_log.result.unwrap()["commits"].as_array().unwrap().len(), 2);
        let srv_log = k.call("git.log", &json!({ "maxCount": 10 }));
        assert_eq!(srv_log.result.unwrap()["commits"].as_array().unwrap().len(), 1, "server root must be untouched");
        let _ = fs::remove_dir_all(&server_root);
        let _ = fs::remove_dir_all(&target);
    }
}
