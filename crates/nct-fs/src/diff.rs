// text.diff - unified diff between two text files, Myers O(ND) minimal edit
// script, git-style @@ hunks, 3 context lines. Whole-body fallback for inputs
// too large/distant for the linear-space heap.

use crate::fs_tools::err_no_path;
use nct_core::errors::ToolError;
use nct_core::helpers::rel_slash;
use nct_core::kernel::{parse_args, Handler, Kernel};
use nct_core::paths::resolve_checked;
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs;

pub const DIFF_DESC: &str = "Unified diff between two files (or a file and a string). Returns a git-style @@ hunk diff plus parseable counts. Paths are relative to the base dir or absolute.";

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiffArgs {
    #[doc = "Left file - relative to the base dir, or absolute"]
    pub path: String,
    #[doc = "Right file - relative to the base dir, or absolute"]
    pub path2: String,
    #[doc = "Context lines per side (default 3, max 50)"]
    #[serde(default)]
    #[schemars(range(min = 0, max = 50))]
    pub context: Option<u64>,
    /// Word-level diff: when a line pair differs, run a Myers diff on WORD
    /// tokens instead of showing the whole line as changed. Returns structured
    /// segments (same/added/removed) for each changed line pair.
    #[serde(default)]
    pub wordLevel: Option<bool>,
    #[doc = "Base dir for relative paths (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
}

/// One word-level segment of a changed line: text + whether it was added or
/// removed (or unchanged).
#[derive(serde::Serialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum SegKind {
    Same,
    Added,
    Removed,
}

#[derive(serde::Serialize)]
struct WordSeg {
    text: String,
    kind: SegKind,
}

pub struct DiffHandler;
impl Handler for DiffHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: DiffArgs = parse_args(args)?;
        let base = k.base_dir(a.baseDir.as_deref())?;
        let left = resolve_checked(&base, &a.path)?;
        let right = resolve_checked(&base, &a.path2)?;
        if !left.exists() {
            return Err(err_no_path(&a.path));
        }
        if !right.exists() {
            return Err(err_no_path(&a.path2));
        }
        let ctx = (a.context.unwrap_or(3) as usize).min(50);
        let al = fs::read_to_string(&left).map_err(ToolError::from)?;
        let bl = fs::read_to_string(&right).map_err(ToolError::from)?;
        let diff = build_diff(&al, &bl, ctx);
        let hunks = diff.matches("\n@@").count() + usize::from(diff.starts_with("@@"));
        let word_level = if a.wordLevel.unwrap_or(false) {
            word_diff(&al, &bl)
        } else {
            Vec::new()
        };
        Ok(json!({
            "left": rel_slash(&base, &left),
            "right": rel_slash(&base, &right),
            "equal": al == bl,
            "hunks": hunks as u64,
            "diff": diff,
            "wordLevel": a.wordLevel.unwrap_or(false),
            "wordSegments": word_level,
        }))
    }
}

/// Word-level diff over the whole file: walk the line-level edit script, and
/// for each line pair that differs (delete+insert paired), split both lines
/// into word tokens and run a Myers diff, emitting Same/Added/Removed segments.
/// Returns one entry per changed line pair.
fn word_diff(a: &str, b: &str) -> Vec<Value> {
    let al = line_vec(a);
    let bl = line_vec(b);
    if al.len() as u128 * bl.len() as u128 > 5_000_000 {
        return Vec::new(); // too large — the line diff is the signal
    }
    let ops = diff_ops(&al, &bl);
    let mut segs: Vec<Value> = Vec::new();
    let mut i = 0usize;
    let mut j = 0usize;
    let mut pending_del: Option<&str> = None;
    for op in &ops {
        match op {
            Op::Equal => {
                // A delete followed by inserts at this point is paired on the
                // next Insert. Nothing to emit on Equal.
                i += 1;
                j += 1;
            }
            Op::Delete => {
                pending_del = Some(al[i]);
                i += 1;
            }
            Op::Insert => {
                let new_line = bl[j];
                j += 1;
                let old_line = pending_del.take().unwrap_or("").trim();
                let new_trim = new_line.trim();
                if old_line.is_empty() || new_trim.is_empty() {
                    // pure insertion/deletion — emit plain markers
                    if !new_trim.is_empty() {
                        segs.push(
                            json!({ "oldLine": old_line, "newLine": new_trim, "segments": [] }),
                        );
                    }
                    continue;
                }
                let segs_for = word_segments(old_line, new_trim);
                segs.push(
                    json!({ "oldLine": old_line, "newLine": new_trim, "segments": segs_for }),
                );
            }
        }
    }
    if let Some(del) = pending_del {
        // Trailing deletion with no matching insert — the deleted lines are
        // shown in the line-level diff; nothing to word-segment against.
        let _ = del;
    }
    segs
}

/// Word-token Myers diff of one changed line pair → list of {text, kind}.
fn word_segments(old: &str, new: &str) -> Vec<WordSeg> {
    let ow: Vec<&str> = old.split_whitespace().collect();
    let nw: Vec<&str> = new.split_whitespace().collect();
    let ops = diff_ops(&ow, &nw);
    let mut out: Vec<WordSeg> = Vec::new();
    let mut i = 0usize;
    let mut j = 0usize;
    let mut pending: Vec<&str> = Vec::new();
    for op in &ops {
        match op {
            Op::Equal => {
                if !pending.is_empty() {
                    // flush removed words first, then show the equal word
                    for w in pending.drain(..) {
                        out.push(WordSeg {
                            text: w.to_string(),
                            kind: SegKind::Removed,
                        });
                    }
                }
                out.push(WordSeg {
                    text: ow[i].to_string(),
                    kind: SegKind::Same,
                });
                i += 1;
                j += 1;
            }
            Op::Delete => {
                pending.push(ow[i]);
                i += 1;
            }
            Op::Insert => {
                out.push(WordSeg {
                    text: nw[j].to_string(),
                    kind: SegKind::Added,
                });
                j += 1;
            }
        }
    }
    for w in pending {
        out.push(WordSeg {
            text: w.to_string(),
            kind: SegKind::Removed,
        });
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Equal,
    Delete,
    Insert,
}

/// Longest-common-subsequence diff: a provably-minimal edit script. O(n*m)
/// time/memory; build_diff falls back to a whole-body diff for very large
/// inputs so this never runs off the heap.
fn diff_ops<T: PartialEq>(a: &[T], b: &[T]) -> Vec<Op> {
    let n = a.len();
    let m = b.len();
    if n == 0 {
        return vec![Op::Insert; m];
    }
    if m == 0 {
        return vec![Op::Delete; n];
    }
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            if a[i] == b[j] {
                dp[i][j] = dp[i + 1][j + 1] + 1;
            } else {
                dp[i][j] = dp[i + 1][j].max(dp[i][j + 1]);
            }
        }
    }
    let mut ops: Vec<Op> = Vec::new();
    let mut i = 0usize;
    let mut j = 0usize;
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push(Op::Equal);
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            ops.push(Op::Delete);
            i += 1;
        } else {
            ops.push(Op::Insert);
            j += 1;
        }
    }
    while i < n {
        ops.push(Op::Delete);
        i += 1;
    }
    while j < m {
        ops.push(Op::Insert);
        j += 1;
    }
    ops
}

fn line_vec(s: &str) -> Vec<&str> {
    if s.is_empty() {
        Vec::new()
    } else {
        s.split('\n').collect()
    }
}

/// Full unified diff of two texts. If the inputs are large/distant enough to
/// blow up the O(ND) search, fall back to a faithful whole-body diff.
fn build_diff(a: &str, b: &str, ctx: usize) -> String {
    let al = line_vec(a);
    let bl = line_vec(b);
    // Whole-body fallback for very large / very distant inputs (bounds the
    // O(n*m) DP in diff_ops).
    if al.len() as u128 * bl.len() as u128 > 5_000_000 {
        return whole_body(&al, &bl, ctx);
    }
    let ops = diff_ops(&al, &bl);
    let mut tagged: Vec<(char, &str)> = Vec::with_capacity(ops.len());
    let mut ai = 0usize;
    let mut bi = 0usize;
    for op in &ops {
        match op {
            Op::Equal => {
                tagged.push((' ', al[ai]));
                ai += 1;
                bi += 1;
            }
            Op::Delete => {
                tagged.push(('-', al[ai]));
                ai += 1;
            }
            Op::Insert => {
                tagged.push(('+', bl[bi]));
                bi += 1;
            }
        }
    }
    if tagged.is_empty() {
        return String::new();
    }
    // Collect change runs.
    let tlen = tagged.len();
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut i = 0usize;
    while i < tlen {
        if tagged[i].0 != ' ' {
            let s = i;
            while i < tlen && tagged[i].0 != ' ' {
                i += 1;
            }
            runs.push((s, i - 1));
        } else {
            i += 1;
        }
    }
    if runs.is_empty() {
        return String::new();
    }
    // Merge runs separated by <= 2*ctx context lines.
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for r in runs {
        if let Some(last) = merged.last_mut() {
            if r.0 - last.1 - 1 <= 2 * ctx {
                last.1 = r.1;
                continue;
            }
        }
        merged.push(r);
    }
    let mut out = String::new();
    for (s, e) in merged {
        let hs = s.saturating_sub(ctx);
        let he = (e + ctx).min(tlen - 1);
        let (os, oc, ns, nc) = hunk_hdr(&tagged, hs, he);
        out.push_str(&format!("@@ -{},{} +{},{} @@\n", os, oc, ns, nc));
        for &(t, l) in &tagged[hs..=he] {
            out.push(t);
            out.push_str(l);
            out.push('\n');
        }
    }
    out
}

fn hunk_hdr(tagged: &[(char, &str)], hs: usize, he: usize) -> (usize, usize, usize, usize) {
    let mut old_before = 0usize;
    let mut new_before = 0usize;
    for &(tag, _) in tagged.iter().take(hs) {
        match tag {
            ' ' => {
                old_before += 1;
                new_before += 1;
            }
            '-' => old_before += 1,
            '+' => new_before += 1,
            _ => {}
        }
    }
    let mut old_count = 0usize;
    let mut new_count = 0usize;
    for &(tag, _) in &tagged[hs..=he] {
        match tag {
            ' ' => {
                old_count += 1;
                new_count += 1;
            }
            '-' => old_count += 1,
            '+' => new_count += 1,
            _ => {}
        }
    }
    let os = if old_count == 0 {
        old_before
    } else {
        old_before + 1
    };
    let ns = if new_count == 0 {
        new_before
    } else {
        new_before + 1
    };
    (os, old_count, ns, new_count)
}

fn whole_body(al: &[&str], bl: &[&str], ctx: usize) -> String {
    let mut out = String::new();
    let oc = al.len();
    let nc = bl.len();
    out.push_str(&format!("@@ -1,{} +1,{} @@\n", oc, nc));
    for l in al {
        out.push('-');
        out.push_str(l);
        out.push('\n');
    }
    for l in bl {
        out.push('+');
        out.push_str(l);
        out.push('\n');
    }
    let _ = ctx;
    out
}

#[cfg(test)]
mod diff_word_tests {
    use super::*;
    use nct_core::kernel::Kernel;
    use std::fs;

    fn make_kernel() -> Kernel {
        let dir = std::env::temp_dir().join(format!(
            "nct-diff-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut k = Kernel::new(dir).unwrap();
        crate::register(&mut k);
        k
    }

    #[test]
    fn word_level_isolates_single_word_change() {
        let k = make_kernel();
        fs::write(k.root.join("a.txt"), "the quick brown fox\n").unwrap();
        fs::write(k.root.join("b.txt"), "the quick red fox\n").unwrap();
        let args = json!({ "path": "a.txt", "path2": "b.txt", "wordLevel": true });
        let v = DiffHandler.call(&k, &args).unwrap();
        assert_eq!(v["equal"], json!(false));
        let recs = v["wordSegments"].as_array().unwrap();
        assert!(!recs.is_empty());
        let segments = recs[0]["segments"].as_array().unwrap();
        // Segments should include the equal words "the", "quick" and the added
        // "red" (removed "brown" is either removed or omitted based on pairing)
        let texts: Vec<&str> = segments
            .iter()
            .map(|s| s["text"].as_str().unwrap())
            .collect();
        assert!(texts.contains(&"the"));
        assert!(texts.contains(&"quick"));
        // The changed word appears: either "red" added / "brown" removed
        assert!(segments
            .iter()
            .any(|s| s["text"] == json!("red") || s["text"] == json!("brown")));
    }

    #[test]
    fn no_word_segments_when_equal() {
        let k = make_kernel();
        fs::write(k.root.join("a.txt"), "same content\n").unwrap();
        fs::write(k.root.join("b.txt"), "same content\n").unwrap();
        let args = json!({ "path": "a.txt", "path2": "b.txt", "wordLevel": true });
        let v = DiffHandler.call(&k, &args).unwrap();
        assert_eq!(v["equal"], json!(true));
        assert!(v["wordSegments"].as_array().unwrap().is_empty());
    }

    #[test]
    fn line_diff_still_works_with_word_level() {
        let k = make_kernel();
        fs::write(k.root.join("a.txt"), "line one\nline two\nline three\n").unwrap();
        fs::write(
            k.root.join("b.txt"),
            "line one\nline 2 changed\nline three\n",
        )
        .unwrap();
        let args = json!({ "path": "a.txt", "path2": "b.txt", "wordLevel": true });
        let v = DiffHandler.call(&k, &args).unwrap();
        assert_eq!(v["equal"], json!(false));
        // The line-level diff is still present
        assert!(v["diff"].as_str().unwrap().contains("@@"));
        // The word-level records exactly the changed middle line
        let recs = v["wordSegments"].as_array().unwrap();
        assert_eq!(recs.len(), 1);
        assert!(recs[0]["oldLine"].as_str().unwrap().contains("line two"));
        assert!(recs[0]["newLine"]
            .as_str()
            .unwrap()
            .contains("line 2 changed"));
    }

    #[test]
    fn word_level_defaults_off() {
        let k = make_kernel();
        fs::write(k.root.join("a.txt"), "hello world\n").unwrap();
        fs::write(k.root.join("b.txt"), "hello there\n").unwrap();
        let args = json!({ "path": "a.txt", "path2": "b.txt" });
        let v = DiffHandler.call(&k, &args).unwrap();
        assert!(v["wordSegments"].as_array().unwrap().is_empty());
        assert_eq!(v["wordLevel"], json!(false));
    }
}
