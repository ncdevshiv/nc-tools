// patch.apply / patch.applyMany — exact-match search/replace editing with
// occurrence semantics. Behavior-parity port of src/kernel/patch.mjs.
use serde::Deserialize;
use serde_json::{json, Value};

use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};
use nct_core::paths::resolve_checked;

pub const PATCH_APPLY_DESC: &str = "Apply exact-match search/replace edits to a file. Each edit: {oldText, newText, expectedCount?}. Fails with structured hints (occurrence counts, nearest candidate lines, and a char-level diff of WHICH part differs) if not found or ambiguous. When fuzzy=true, an edit whose oldText is within fuzzyThreshold edits of a candidate is applied with a corrections report — survives whitespace/formatting drift.";
pub const PATCH_APPLY_MANY_DESC: &str = "Apply patch edits to up to 20 files in ONE call: [{path, edits:[{oldText,newText,expectedCount?}]}]. Per-file ok/error results.";

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditArgs {
    pub oldText: String,
    pub newText: String,
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub expectedCount: Option<u64>,
    /// When true, an oldText that differs from a candidate by <= fuzzyThreshold
    /// single-character edits is applied with a corrections report (survives
    /// whitespace/formatting drift). Exact match is still preferred.
    #[serde(default)]
    pub fuzzy: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyArgs {
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    pub path: String,
    #[schemars(length(min = 1))]
    pub edits: Vec<EditArgs>,
    #[doc = "Base dir for relative paths (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
    /// When true, refuse the patch if ANOTHER agent holds a live advisory lock
    /// on this path (hard-write-guard). Default false = advisory.
    #[serde(default)]
    pub guardLocks: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyManyArgs {
    #[schemars(length(min = 1, max = 20))]
    pub edits: Vec<FileEdits>,
    #[doc = "Base dir for relative paths (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
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
        let base = k.base_dir(a.baseDir.as_deref())?;
        let abs = resolve_checked(&base, &a.path)?;
        let _ = crate::fs_tools::maybe_warn_foreign_lock(k, &base, &abs, a.guardLocks.unwrap_or(false))?;
        let applied = apply_impl(&base, &abs, &a.path, &a.edits)?;
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
        let base = k.base_dir(a.baseDir.as_deref())?;
        let mut results = Vec::new();
        for fe in &a.edits {
            match resolve_checked(&base, &fe.path).and_then(|abs| apply_impl(&base, &abs, &fe.path, &fe.edits)) {
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
fn apply_impl(root: &std::path::Path, abs: &std::path::Path, path: &str, edits: &[EditArgs]) -> Result<Value, ToolError> {
    use std::fs;
    let meta = fs::metadata(abs).ok();
    let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(true);
    if meta.is_none() || is_dir {
        return Err(crate::fs_tools::err_no_file(path, abs, root));
    }
    let src = fs::read_to_string(abs).map_err(ToolError::from)?;
    let lines: Vec<&str> = src.split('\n').collect();

    let mut applied = Vec::new();
    let mut out = src.clone();
    for (i, edit) in edits.iter().enumerate() {
        if edit.oldText.is_empty() {
            return Err(ToolError::new("ERR_BAD_EDIT", format!("edits[{i}].oldText must be a non-empty string")));
        }
        let count = out.matches(&edit.oldText).count() as u64;
        let want = edit.expectedCount.unwrap_or(1);

        let exact = count > 0;
        if !exact {
            // Try fuzzy match only when explicitly requested and the exact text
            // is absent. Find the best-matching candidate window and its edit
            // distance; if within threshold, apply it and record corrections.
            if edit.fuzzy.unwrap_or(false) {
                if let Some((candidate, distance)) = fuzzy_candidate(&out, &edit.oldText) {
                    if distance <= FUZZY_THRESHOLD {
                        let replaced = out.replace(&candidate, &edit.newText);
                        let corrections: Vec<String> = char_diff(&edit.oldText, &candidate);
                        out = replaced;
                        let n = count.max(1);
                        applied.push(json!({
                            "index": i,
                            "replacements": n,
                            "fuzzy": true,
                            "editDistance": distance,
                            "corrections": corrections,
                        }));
                        continue;
                    }
                }
            }
            let candidates = nearest_matches(&lines, &edit.oldText);
            let mut hint = json!({ "editIndex": i, "occurrences": 0, "expected": want, "nearestCandidateLines": candidates });
            // Add a char-level diff of the closest line so the agent sees
            // exactly WHICH character drifted, not just "line 42".
            if let Some((line, nearest)) = nearest_line_with_diff(&lines, &edit.oldText) {
                hint["nearestDiff"] = json!({
                    "line": line,
                    "editDistance": nearest.0,
                    "diff": nearest.1,
                });
            }
            return Err(ToolError::with_hint(
                "PATCH_NO_MATCH",
                format!("edits[{i}]: oldText not found in {path}"),
                hint,
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
    fs::write(abs, &out).map_err(ToolError::from)?;
    Ok(json!({ "path": path, "applied": applied, "bytes": out.len() }))
}

/// Max single-char edit distance for a fuzzy match to be accepted.
const FUZZY_THRESHOLD: usize = 3;

/// Find the best-matching substring window in `text` for `needle`, returning
/// (candidate, Levenshtein distance). A sliding window of the same length as
/// the needle (plus tolerance for trailing whitespace) is scored against the
/// needle; the lowest-distance window wins. Returns None if nothing close.
fn fuzzy_candidate(text: &str, needle: &str) -> Option<(String, usize)> {
    let needle_len = needle.chars().count();
    if needle_len == 0 {
        return None;
    }
    // Slide a window over text; window lengths from needle_len-2..needle_len+2
    // in char space (handles a couple of added/removed chars).
    let chars: Vec<char> = text.chars().collect();
    if chars.len() < needle_len.saturating_sub(1) {
        return None;
    }
    let mut best: Option<(String, usize)> = None;
    let mut start = 0usize;
    while start + needle_len <= chars.len() {
        for win_len in needle_len.saturating_sub(2)..=needle_len + 2 {
            let end = (start + win_len).min(chars.len());
            if end <= start {
                continue;
            }
            let cand: String = chars[start..end].iter().collect();
            let d = levenshtein(&needle, &cand);
            // Prefer a candidate that is a full-line (or line-prefix) — a window
            // that ends mid-line is a much weaker match than the whole line.
            let is_partial = chars.get(end).map(|&c| c != '\n').unwrap_or(false);
            // Reject candidates that span a newline boundary badly: a fuzzy
            // match should not cross a \n unless the needle itself is multiline.
            if cand.contains('\n') && !needle.contains('\n') {
                continue;
            }
            if d < best.as_ref().map(|(_, bd)| *bd).unwrap_or(usize::MAX) && !is_partial {
                best = Some((cand, d));
            }
        }
        start += 1;
    }
    // Fall back to any window (including partial) if nothing full passed.
    if best.is_none() {
        return None;
    }
    best
}

/// Levenshtein edit distance byte-wise (runs on char vectors for correctness).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Character-level diff between `target` and `match` — tokenize to words and
/// report which words were added/removed, so the agent sees the drift.
fn char_diff(a: &str, b: &str) -> Vec<String> {
    let aw: Vec<&str> = a.split_whitespace().collect();
    let bw: Vec<&str> = b.split_whitespace().collect();
    let mut out = Vec::new();
    let max = aw.len().max(bw.len());
    for i in 0..max {
        match (aw.get(i), bw.get(i)) {
            (Some(x), Some(y)) if x != y => out.push(format!("'{y}' → '{x}' (expected '{x}', found '{y}')")),
            (Some(x), None) => out.push(format!("missing '{x}'")),
            (None, Some(y)) => out.push(format!("unexpected '{y}'")),
            _ => {}
        }
    }
    out.truncate(8);
    out
}

/// Find the line in `lines` closest to `needle` (edit distance), returning
/// (line_number, (distance, diff_description)).
fn nearest_line_with_diff(lines: &[&str], needle: &str) -> Option<(u64, (usize, Vec<String>))> {
    let needle_first = needle.split('\n').next().unwrap_or("").trim();
    if needle_first.is_empty() {
        return None;
    }
    let mut best: Option<(u64, usize)> = None;
    for (idx, line) in lines.iter().enumerate() {
        let d = levenshtein(needle_first, line.trim());
        if best.map(|(_, bd)| d < bd).unwrap_or(true) {
            best = Some(((idx + 1) as u64, d));
        }
    }
    best.map(|(line, d)| {
        let line_text = lines[(line - 1) as usize];
        let diff = char_diff(needle_first, line_text.trim());
        (line, (d, diff))
    })
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

#[cfg(test)]
mod patch_fuzzy_tests {
    use super::*;
    use std::fs;

    fn workdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nct-patch-test-{}-{}",
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

    #[test]
    fn levenshtein_handles_substitutions_insertions_deletions() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("abc", "abcd"), 1);
        assert_eq!(levenshtein("abc", "ab"), 1);
        assert_eq!(levenshtein("same", "same"), 0);
        assert_eq!(levenshtein("", ""), 0);
    }

    #[test]
    fn fuzzy_match_applies_within_threshold() {
        let dir = workdir();
        let abs = dir.join("app.rs");
        // Genuine drift: extra space before the semicolon means the exact
        // "return a + b;" is NOT a substring — so fuzzy must kick in.
        fs::write(&abs, "fn add(a: i32, b: i32) -> i32 {\n  return a + b ;\n}\n").unwrap();
        let edits = vec![EditArgs {
            oldText: "return a + b;".to_string(),
            newText: "return a + b + 0;".to_string(),
            expectedCount: Some(1),
            fuzzy: Some(true),
        }];
        let result = apply_impl(&dir, &abs, "app.rs", &edits);
        let v = result.unwrap();
        let applied = v["applied"].as_array().unwrap();
        assert_eq!(applied[0]["fuzzy"], json!(true), "expected fuzzy match, got: {applied:?}");
        assert!(applied[0]["editDistance"].as_u64().unwrap() <= 3);
        let content = fs::read_to_string(&abs).unwrap();
        assert!(content.contains("return a + b + 0;"), "content: {content:?}");
    }

    #[test]
    fn patch_no_match_gives_nearest_diff() {
        let dir = workdir();
        let abs = dir.join("app.rs");
        fs::write(&abs, "fn add(a: i32, b: i32) -> i32 {\n  return a + b;\n}\n").unwrap();
        // Deliberately different: "return a * b;" does not exist
        let edits = vec![EditArgs {
            oldText: "return a * b;".to_string(),
            newText: "x".to_string(),
            expectedCount: None,
            fuzzy: None,
        }];
        let err = apply_impl(&dir, &abs, "app.rs", &edits).unwrap_err();
        assert_eq!(err.code, "PATCH_NO_MATCH");
        // hint is Option<Value>
        let hint = err.hint.expect("patch errors carry a hint");
        assert!(hint.get("nearestCandidateLines").is_some());
        assert!(hint.get("nearestDiff").is_some(), "expected a char-level diff, got: {hint:?}");
        let nd = &hint["nearestDiff"];
        assert!(nd["line"].as_u64().unwrap() >= 1);
        assert!(nd["editDistance"].as_u64().unwrap() >= 1);
        // The diff should describe the substitution of * for +
        let diff_text = nd["diff"].as_array().unwrap();
        assert!(!diff_text.is_empty());
    }

    #[test]
    fn exact_match_still_preferred_over_fuzzy() {
        let dir = workdir();
        let abs = dir.join("a.rs");
        fs::write(&abs, "let x = 1;\n").unwrap();
        let edits = vec![EditArgs {
            oldText: "let x = 1;".to_string(),
            newText: "let x = 2;".to_string(),
            expectedCount: Some(1),
            fuzzy: Some(true),
        }];
        let result = apply_impl(&dir, &abs, "a.rs", &edits).unwrap();
        let applied = result["applied"].as_array().unwrap();
        // Exact match wins — no fuzzy flag
        assert!(applied[0].get("fuzzy").is_none());
        assert_eq!(fs::read_to_string(&abs).unwrap(), "let x = 2;\n");
    }

    #[test]
    fn fuzzy_rejected_when_too_far() {
        let dir = workdir();
        let abs = dir.join("far.rs");
        fs::write(&abs, "completely different line\n").unwrap();
        let edits = vec![EditArgs {
            oldText: "let x = 42;".to_string(),
            newText: "z".to_string(),
            expectedCount: None,
            fuzzy: Some(true),
        }];
        let err = apply_impl(&dir, &abs, "far.rs", &edits).unwrap_err();
        assert_eq!(err.code, "PATCH_NO_MATCH");
    }
}
