// git.stash / git.cherryPick / git.tag — the daily git workflow surface the
// kernel was missing. Each follows the same contract as the rest of git.*:
// HasBaseDir for the effective base, in_repo for the concrete repo, porcelain
// formats only, structured errors.
use serde::Deserialize;
use serde_json::{json, Value};

use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};

use super::{base_of, in_repo, HasBaseDir};

pub const STASH_DESC: &str = "Stash working-tree changes: push (save + revert, optional message), pop (restore most recent), apply (restore, keep stash), list (all stashes), drop (delete one). The daily save/resume workflow.";
pub const CHERRY_PICK_DESC: &str = "Cherry-pick one or more commits onto the current branch: applies them in order, no-commit mode is testable. Aborts on conflict with structured output.";
pub const TAG_DESC: &str = "Tags: list (all tags), create (annotated with -m message, or lightweight), delete.";

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StashArgs {
    #[doc = "push | pop | apply | list | drop (default: list)"]
    #[serde(default)]
    pub op: Option<String>,
    #[doc = "Message for push; stash ref for drop (default stash at 0)"]
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub stash: Option<String>,
    #[doc = "Repo directory (default: base dir)"]
    #[serde(default)]
    pub repo: Option<String>,
    #[doc = "Base directory override (default: kernel base dir)"]
    #[serde(default)]
    pub baseDir: Option<String>,
}
impl HasBaseDir for StashArgs {
    fn base_dir(&self) -> Option<&str> {
        self.baseDir.as_deref()
    }
}

pub struct StashHandler;
impl Handler for StashHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: StashArgs = parse_args(args)?;
        let base = base_of(k, &a)?;
        let r = in_repo(&base, a.repo.as_deref())?;
        let op = a.op.as_deref().unwrap_or("list").to_lowercase();
        match op.as_str() {
            "push" => {
                let out = if let Some(m) = a.message.as_deref().filter(|m| !m.trim().is_empty()) {
                    super::git(&r, &["stash", "push", "-m", m.trim()], k)?
                } else {
                    super::git(&r, &["stash", "push"], k)?
                };
                Ok(json!({ "repo": r.display().to_string(), "op": "push", "output": out.trim() }))
            }
            "pop" => {
                let out = super::git(&r, &["stash", "pop"], k)?;
                Ok(json!({ "repo": r.display().to_string(), "op": "pop", "output": out.trim() }))
            }
            "apply" => {
                let stash_ref = a.stash.clone().unwrap_or_else(|| "stash@{0}".to_string());
                let out = super::git(&r, &["stash", "apply", &stash_ref], k)?;
                Ok(json!({ "repo": r.display().to_string(), "op": "apply", "output": out.trim() }))
            }
            "list" => {
                let out = super::git(&r, &["stash", "list", "--pretty=format:%gd%x1f%gs"], k)?;
                let stashes: Vec<Value> = out.split('\n').filter(|l| !l.is_empty()).filter_map(|l| {
                    let parts: Vec<&str> = l.split('\x1f').collect();
                    if parts.len() >= 2 { Some(json!({ "ref": parts[0], "message": parts[1] })) } else { None }
                }).collect();
                Ok(json!({ "repo": r.display().to_string(), "op": "list", "stashes": stashes, "count": stashes.len() }))
            }
            "drop" => {
                let stash_ref = if a.stash.is_some() { a.stash.clone().unwrap() } else { "stash@{0}".to_string() };
                let out = super::git(&r, &["stash", "drop", &stash_ref], k)?;
                Ok(json!({ "repo": r.display().to_string(), "op": "drop", "output": out.trim() }))
            }
            other => Err(ToolError::new("ERR_BAD_INPUT", format!("unknown stash op: {other} (push|pop|apply|list|drop)"))),
        }
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CherryPickArgs {
    #[doc = "Commit shas to cherry-pick, applied in order"]
    #[schemars(length(min = 1))]
    pub commits: Vec<String>,
    #[doc = "no-commit mode: apply to index/worktree without committing"]
    #[serde(default)]
    pub noCommit: Option<bool>,
    #[doc = "Abort an in-progress cherry-pick instead of applying"]
    #[serde(default)]
    pub abort: Option<bool>,
    #[doc = "Repo directory (default: base dir)"]
    #[serde(default)]
    pub repo: Option<String>,
    #[doc = "Base directory override (default: kernel base dir)"]
    #[serde(default)]
    pub baseDir: Option<String>,
}
impl HasBaseDir for CherryPickArgs {
    fn base_dir(&self) -> Option<&str> {
        self.baseDir.as_deref()
    }
}

pub struct CherryPickHandler;
impl Handler for CherryPickHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: CherryPickArgs = parse_args(args)?;
        let base = base_of(k, &a)?;
        let r = in_repo(&base, a.repo.as_deref())?;
        if a.abort.unwrap_or(false) {
            let out = super::git(&r, &["cherry-pick", "--abort"], k)?;
            return Ok(json!({ "repo": r.display().to_string(), "aborted": true, "output": out.trim() }));
        }
        if a.commits.is_empty() {
            return Err(ToolError::new("ERR_BAD_INPUT", "commits must be a non-empty array of shas"));
        }
        let mut ga: Vec<String> = vec!["cherry-pick".to_string()];
        if a.noCommit.unwrap_or(false) {
            ga.push("--no-commit".to_string());
        }
        for c in &a.commits {
            if c.trim().is_empty() {
                return Err(ToolError::new("ERR_BAD_INPUT", "cherry-pick shas must be non-empty"));
            }
            ga.push(c.trim().to_string());
        }
        let refs: Vec<&str> = ga.iter().map(|s| s.as_str()).collect();
        let out = super::git(&r, &refs, k)?;
        Ok(json!({ "repo": r.display().to_string(), "applied": a.commits, "noCommit": a.noCommit.unwrap_or(false), "output": out.trim() }))
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TagArgs {
    #[doc = "list | create | delete (default: list)"]
    #[serde(default)]
    pub op: Option<String>,
    #[doc = "Tag name for create/delete"]
    #[serde(default)]
    pub name: Option<String>,
    #[doc = "Message for an annotated tag (omit = lightweight)"]
    #[serde(default)]
    pub message: Option<String>,
    #[doc = "Ref the tag points at (create only; default HEAD)"]
    #[serde(default)]
    pub target: Option<String>,
    #[doc = "Repo directory (default: base dir)"]
    #[serde(default)]
    pub repo: Option<String>,
    #[doc = "Base directory override (default: kernel base dir)"]
    #[serde(default)]
    pub baseDir: Option<String>,
}
impl HasBaseDir for TagArgs {
    fn base_dir(&self) -> Option<&str> {
        self.baseDir.as_deref()
    }
}

pub struct TagHandler;
impl Handler for TagHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: TagArgs = parse_args(args)?;
        let base = base_of(k, &a)?;
        let r = in_repo(&base, a.repo.as_deref())?;
        let op = a.op.as_deref().unwrap_or("list").to_lowercase();
        match op.as_str() {
            "list" => {
                let out = super::git(&r, &["tag", "--list"], k)?;
                let tags: Vec<String> = out.split('\n').filter(|l| !l.is_empty()).map(String::from).collect();
                Ok(json!({ "repo": r.display().to_string(), "op": "list", "tags": tags, "count": tags.len() }))
            }
            "create" => {
                let name = a.name.as_deref().filter(|n| !n.trim().is_empty()).ok_or_else(|| {
                    ToolError::new("ERR_BAD_INPUT", "tag create needs a name")
                })?;
                let out = if let Some(m) = a.target.as_deref().filter(|t| !t.trim().is_empty()) {
                    if let Some(msg) = a.message.as_deref().filter(|mm| !mm.trim().is_empty()) {
                        super::git(&r, &["tag", "-a", name.trim(), m.trim(), "-m", msg.trim()], k)?
                    } else {
                        super::git(&r, &["tag", name.trim(), m.trim()], k)?
                    }
                } else if let Some(msg) = a.message.as_deref().filter(|mm| !mm.trim().is_empty()) {
                    super::git(&r, &["tag", "-a", name.trim(), "-m", msg.trim()], k)?
                } else {
                    super::git(&r, &["tag", name.trim()], k)?
                };
                Ok(json!({ "repo": r.display().to_string(), "op": "create", "tag": name.trim(), "output": out.trim() }))
            }
            "delete" => {
                let name = a.name.as_deref().filter(|n| !n.trim().is_empty()).ok_or_else(|| {
                    ToolError::new("ERR_BAD_INPUT", "tag delete needs a name")
                })?;
                let out = super::git(&r, &["tag", "-d", name.trim()], k)?;
                Ok(json!({ "repo": r.display().to_string(), "op": "delete", "tag": name.trim(), "output": out.trim() }))
            }
            other => Err(ToolError::new("ERR_BAD_INPUT", format!("unknown tag op: {other} (list|create|delete)"))),
        }
    }
}

#[cfg(test)]
mod extra_tests {
    use super::*;
    use std::fs;

    fn git_kernel(root: &std::path::Path) -> Kernel {
        let mut k = Kernel::new(root.to_path_buf()).unwrap();
        super::super::register(&mut k);
        k
    }

    fn repo(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nct-extra-{}-{}-{}",
            tag,
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
        dir
    }

    fn commit(dir: &std::path::Path, file: &str, content: &str, msg: &str) {
        fs::write(dir.join(file), content).unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", msg])
            .current_dir(dir)
            .status()
            .unwrap();
    }

    fn dirty_write(dir: &std::path::Path, file: &str) {
        // Write with a real newline; assertions trim, so CRLF is irrelevant.
        fs::write(dir.join(file), "dirty-content-here\n").unwrap();
    }

    fn read_trim(dir: &std::path::Path, file: &str) -> String {
        fs::read_to_string(dir.join(file)).unwrap().trim().to_string()
    }

    #[test]
    fn stash_push_list_pop_roundtrip() {
        let r = repo("stash");
        let k = git_kernel(&r);
        commit(&r, "a.txt", "v1", "init");
        dirty_write(&r, "a.txt");
        let push = k.call(
            "git.stash",
            &serde_json::json!({ "repo": r.display().to_string(), "op": "push", "message": "wip" }),
        );
        assert!(push.ok, "stash push: {:?}", push.error);
        assert_eq!(read_trim(&r, "a.txt"), "v1");
        let list = k.call(
            "git.stash",
            &serde_json::json!({ "repo": r.display().to_string(), "op": "list" }),
        );
        assert_eq!(list.result.unwrap()["count"], serde_json::json!(1));
        let pop = k.call(
            "git.stash",
            &serde_json::json!({ "repo": r.display().to_string(), "op": "pop" }),
        );
        assert!(pop.ok, "stash pop: {:?}", pop.error);
        assert_eq!(read_trim(&r, "a.txt"), "dirty-content-here");
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn cherry_pick_applies_a_commit() {
        let r = repo("pick");
        let k = git_kernel(&r);
        commit(&r, "a.txt", "v1", "init");
        std::process::Command::new("git")
            .args(["checkout", "-b", "feat"])
            .current_dir(&r)
            .status()
            .unwrap();
        commit(&r, "b.txt", "feature", "feat: work");
        let sha_out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&r)
            .output()
            .unwrap();
        let sha = String::from_utf8_lossy(&sha_out.stdout).trim().to_string();
        std::process::Command::new("git")
            .args(["checkout", "main"])
            .current_dir(&r)
            .status()
            .unwrap();
        let cp = k.call(
            "git.cherryPick",
            &serde_json::json!({ "repo": r.display().to_string(), "commits": [sha] }),
        );
        assert!(cp.ok, "cherry-pick: {:?}", cp.error);
        assert!(r.join("b.txt").exists());
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn tag_create_list_delete_roundtrip() {
        let r = repo("tag");
        let k = git_kernel(&r);
        commit(&r, "a.txt", "v1", "init");
        let c = k.call(
            "git.tag",
            &serde_json::json!({ "repo": r.display().to_string(), "op": "create", "name": "v1.0", "message": "release" }),
        );
        assert!(c.ok, "tag create: {:?}", c.error);
        let l = k.call(
            "git.tag",
            &serde_json::json!({ "repo": r.display().to_string(), "op": "list" }),
        );
        let found = l
            .result
            .unwrap()["tags"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t == "v1.0");
        assert!(found, "v1.0 should be listed");
        let d = k.call(
            "git.tag",
            &serde_json::json!({ "repo": r.display().to_string(), "op": "delete", "name": "v1.0" }),
        );
        assert!(d.ok, "tag delete: {:?}", d.error);
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn bad_ops_error_cleanly() {
        let r = repo("badops");
        let k = git_kernel(&r);
        commit(&r, "a.txt", "v1", "init");
        let s = k.call(
            "git.stash",
            &serde_json::json!({ "repo": r.display().to_string(), "op": "frobnicate" }),
        );
        assert!(!s.ok);
        assert_eq!(s.error.unwrap().code, "ERR_BAD_INPUT");
        let t = k.call(
            "git.tag",
            &serde_json::json!({ "repo": r.display().to_string(), "op": "create" }),
        );
        assert!(!t.ok);
        let _ = fs::remove_dir_all(&r);
    }
}
