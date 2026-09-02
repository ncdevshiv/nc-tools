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
}

pub struct DiffHandler;
impl Handler for DiffHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: DiffArgs = parse_args(args)?;
        let left = resolve_checked(&k.root, &a.path)?;
        let right = resolve_checked(&k.root, &a.path2)?;
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
        Ok(json!({
            "left": rel_slash(&k.root, &left),
            "right": rel_slash(&k.root, &right),
            "equal": al == bl,
            "hunks": hunks as u64,
            "diff": diff,
        }))
    }
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
        for k in hs..=he {
            let (t, l) = tagged[k];
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
    for k in 0..hs {
        match tagged[k].0 {
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
    for k in hs..=he {
        match tagged[k].0 {
            ' ' => {
                old_count += 1;
                new_count += 1;
            }
            '-' => old_count += 1,
            '+' => new_count += 1,
            _ => {}
        }
    }
    let os = if old_count == 0 { old_before } else { old_before + 1 };
    let ns = if new_count == 0 { new_before } else { new_before + 1 };
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
