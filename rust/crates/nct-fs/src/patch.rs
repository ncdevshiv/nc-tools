// patch.apply / patch.applyMany — exact-match search/replace editing with
// occurrence semantics. Behavior-parity port of src/kernel/patch.mjs.
use serde::Deserialize;
use serde_json::{json, Value};

use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};
use nct_core::paths::resolve_checked;

pub const PATCH_APPLY_DESC: &str = "Apply exact-match search/replace edits to a file. Each edit: {oldText, newText, expectedCount?}. Fails with structured hints (occurrence counts, nearest candidate lines) if not found or ambiguous.";
pub const PATCH_APPLY_MANY_DESC: &str = "Apply patch edits to up to 20 files in ONE call: [{path, edits:[{oldText,newText,expectedCount?}]}]. Per-file ok/error results.";

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditArgs {
    pub oldText: String,
    pub newText: String,
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub expectedCount: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyArgs {
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    pub path: String,
    #[schemars(length(min = 1))]
    pub edits: Vec<EditArgs>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyManyArgs {
    #[schemars(length(min = 1, max = 20))]
    pub edits: Vec<FileEdits>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileEdits {
    pub path: String,
    #[schemars(length(min = 1))]
    pub edits: Vec<EditArgs>,
}

pub struct ApplyHandler;
impl Handler for ApplyHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: ApplyArgs = parse_args(args)?;
        let abs = resolve_checked(&k.root, &a.path)?;
        let applied = apply_impl(&abs, &a.path, &a.edits)?;
        Ok(applied)
    }
}

pub struct ApplyManyHandler;
impl Handler for ApplyManyHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: ApplyManyArgs = parse_args(args)?;
        if a.edits.is_empty() {
            return Err(ToolError::new("ERR_BAD_INPUT", "edits must be a non-empty array of {path, edits}"));
        }
        if a.edits.len() > k.cfg.limits.patch_many {
            return Err(ToolError::new(
                "ERR_BAD_INPUT",
                format!("max {} files per patch.applyMany call", k.cfg.limits.patch_many),
            ));
        }
        let mut results = Vec::new();
        for fe in &a.edits {
            match resolve_checked(&k.root, &fe.path).and_then(|abs| apply_impl(&abs, &fe.path, &fe.edits)) {
                Ok(v) => results.push(json!({
                    "path": fe.path,
                    "ok": true,
                    "applied": v["applied"],
                })),
                Err(e) => results.push(json!({ "path": fe.path, "ok": false, "error": e })),
            }
        }
        let patched = results.iter().filter(|r| r["ok"] == json!(true)).count();
        let failed = results.len() - patched;
        Ok(json!({ "results": results, "patched": patched, "failed": failed }))
    }
}

/// Core edit loop — every oldText must occur exactly `expectedCount ?? 1`
/// times, else nothing is written and a structured error carries hints.
fn apply_impl(abs: &std::path::Path, path: &str, edits: &[EditArgs]) -> Result<Value, ToolError> {
    use std::fs;
    let meta = fs::metadata(abs).ok();
    let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(true);
    if meta.is_none() || is_dir {
        return Err(crate::fs_tools::err_no_file(path, abs));
    }
    let src = fs::read_to_string(abs).map_err(|e| ToolError::new("ERR_INTERNAL", e.to_string()))?;
    let lines: Vec<&str> = src.split('\n').collect();

    let mut applied = Vec::new();
    let mut out = src.clone();
    for (i, edit) in edits.iter().enumerate() {
        if edit.oldText.is_empty() {
            return Err(ToolError::new("ERR_BAD_EDIT", format!("edits[{i}].oldText must be a non-empty string")));
        }
        let count = out.matches(&edit.oldText).count() as u64;
        let want = edit.expectedCount.unwrap_or(1);
        if count == 0 {
            let candidates = nearest_matches(&lines, &edit.oldText);
            return Err(ToolError::with_hint(
                "PATCH_NO_MATCH",
                format!("edits[{i}]: oldText not found in {path}"),
                json!({ "editIndex": i, "occurrences": 0, "expected": want, "nearestCandidateLines": candidates }),
            ));
        }
        if count != want {
            return Err(ToolError::with_hint(
                "PATCH_AMBIGUOUS",
                format!(
                    "edits[{i}]: oldText occurs {count} time(s) in {path}, expected {want}. Include more surrounding context or pass expectedCount."
                ),
                json!({ "editIndex": i, "occurrences": count, "expected": want }),
            ));
        }
        out = out.replace(&edit.oldText, &edit.newText);
        applied.push(json!({ "index": i, "replacements": count }));
    }
    fs::write(abs, &out).map_err(|e| ToolError::new("ERR_INTERNAL", e.to_string()))?;
    Ok(json!({ "path": path, "applied": applied, "bytes": out.len() }))
}

/// Line numbers of lines sharing the most tokens (>3 chars) with oldText's
/// first line — patch.mjs nearestMatches.
fn nearest_matches(lines: &[&str], old_text: &str) -> Vec<u64> {
    let needle = old_text.split('\n').next().unwrap_or("").trim();
    if needle.is_empty() {
        return Vec::new();
    }
    let tokens: Vec<String> = needle
        .split_whitespace()
        .filter(|t| t.len() > 3)
        .map(|t| t.to_lowercase())
        .collect();
    if tokens.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(u64, usize)> = Vec::new(); // (line_no, hits)
    for (i, line) in lines.iter().enumerate() {
        let lower = line.to_lowercase();
        let hits = tokens.iter().filter(|t| lower.contains(t.as_str())).count();
        if hits > 0 {
            scored.push(((i + 1) as u64, hits));
        }
    }
    scored.sort_by_key(|(_, hits)| std::cmp::Reverse(*hits));
    scored.into_iter().take(5).map(|(line, _)| line).collect()
}
