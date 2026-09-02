// search.* tools — line-based regex grep and glob-ish file search, machine-wide.
// Behavior-parity port of src/kernel/search.mjs, with bounded directory scans:
// the walk skips generated trees, drops oversized files, and stops on a
// configurable file/time budget so one grep can never own the server (the
// journal once recorded a 228s root grep on a tree with a 23 GB sqlite).
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Deserialize;
use serde_json::{json, Value};

use nct_core::config::Limits;
use nct_core::errors::ToolError;
use nct_core::helpers::rel_slash;
use nct_core::kernel::{parse_args, Handler, Kernel};
use nct_core::paths::{is_reparse_point, resolve_checked};

use crate::fs_tools::{err_no_path, err_no_path_with_siblings};

pub const GREP_DESC: &str = "Regex search across files. Returns file/line/text matches. Directory scans are bounded: they skip .git/node_modules/.nc-tools/target/dist/build, skip files over 10MB, and stop after 20k files or 10s (limits.grepMaxScan*) — scanTruncated:true in the result means the budget stopped the scan, so matches may be partial. Directly-targeted file paths are always scanned in full.";
pub const FILES_DESC: &str = "Find files by glob pattern (e.g. \"**/*.test.mjs\"). Directory scans are bounded like search.grep; scanTruncated:true means the scan budget stopped early.";

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrepArgs {
    pub pattern: String,
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub glob: Option<String>,
    #[serde(default)]
    #[schemars(range(min = 1, max = 1000))]
    pub maxResults: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FilesArgs {
    pub pattern: String,
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    #[serde(default)]
    pub path: Option<String>,
}

/// Extension allowlist (lowercased, no dot) — extensionless files included.
/// Mirrors search.mjs TEXT_EXT semantics after extname().
const TEXT_EXT: &[&str] = &[
    "js", "mjs", "cjs", "ts", "tsx", "jsx", "json", "md", "txt", "css", "html", "py", "rs", "go",
    "java", "yml", "yaml", "toml", "sh", "c", "h", "cpp", "hpp", "sql", "env", "gitignore", "log",
];

/// Directories never walked by search.*/: dependency and build-artifact trees
/// are both unboundedly large and never match a code-search intent (the
/// 228s-grep incident walked a Rust target/ plus node-sized data trees).
/// Shared by the grep/files walk and the replace/symbols walk.
pub const SCAN_SKIP: &[&str] = &[".git", "node_modules", ".nc-tools", "target", "dist", "build"];

/// Scan budget for directory walks (grep_max_scan_* limits). Files of
/// `max_file_bytes` or more are dropped from the walk; the walk stops after
/// `max_files` files scanned. The wall-clock cap lives in `ScanGuard` so the
/// deadline spans the whole call — walk AND the read+regex pass.
#[derive(Clone, Copy)]
pub struct WalkBudget {
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_depth: usize,
}

impl WalkBudget {
    pub fn from_limits(l: &Limits) -> Self {
        WalkBudget {
            max_files: l.grep_max_scan_files,
            max_file_bytes: l.grep_max_file_bytes,
            max_depth: l.walk_depth,
        }
    }
}

/// Wall-clock deadline for one whole grep/files scan (walk + reads). Checked
/// in the walk loop and in the read loop; a direct, explicitly-targeted file
/// path is never deadline-truncated by construction (its loop runs once and
/// the deadline could only expire after the walk that never happened).
#[derive(Clone, Copy)]
pub struct ScanGuard {
    started: Instant,
    max_ms: u64,
}

impl ScanGuard {
    pub fn from_limits(l: &Limits) -> Self {
        ScanGuard { started: Instant::now(), max_ms: l.grep_max_scan_ms }
    }

    pub fn expired(&self) -> bool {
        self.max_ms > 0 && self.started.elapsed().as_millis() >= self.max_ms as u128
    }
}

pub struct GrepHandler;
impl Handler for GrepHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: GrepArgs = parse_args(args)?;
        let re = fancy_regex::Regex::new(&a.pattern)
            .map_err(|e| ToolError::with_hint("ERR_BAD_REGEX", format!("Invalid regex: {e}"), json!({ "pattern": a.pattern })))?;
        let path_str = a.path.clone().unwrap_or_else(|| ".".to_string());
        let base = resolve_checked(&k.root, &path_str)?;
        if !base.exists() {
            return Err(err_no_path_with_siblings(&path_str, &base, &k.root));
        }
        let max_results = a.maxResults.unwrap_or(k.cfg.limits.grep_max_results as u64) as usize;
        let budget = WalkBudget::from_limits(&k.cfg.limits);
        let guard = ScanGuard::from_limits(&k.cfg.limits);
        let mut files: Vec<PathBuf> = Vec::new();
        let mut scan_truncated = if fs::metadata(&base)?.is_dir() {
            walk(&base, 0, budget, &guard, &mut files)
        } else {
            files.push(base.clone());
            false
        };
        let mut matches: Vec<Value> = Vec::new();
        let mut total = 0usize;
        let mut truncated = false;
        'files: for file in &files {
            if guard.expired() {
                scan_truncated = true;
                break;
            }
            if let Some(glob) = &a.glob {
                if !glob_match(glob, &rel_slash(&base, file)) {
                    continue;
                }
            }
            if !is_text_file(file) {
                continue;
            }
            let content = match fs::read_to_string(file) {
                Ok(c) => c,
                Err(_) => continue,
            };
            if content.contains('\0') {
                continue;
            }
            for (i, line) in content.split('\n').enumerate() {
                if (i & 0x3FF) == 0 && guard.expired() {
                    scan_truncated = true;
                    break 'files;
                }
                if let Ok(true) = re.is_match(line) {
                    total += 1;
                    if matches.len() < max_results {
                        matches.push(json!({
                            "file": rel_slash(&k.root, file),
                            "line": i + 1,
                            "text": take_chars(line, k.cfg.limits.grep_line_chars),
                        }));
                    } else {
                        truncated = true;
                    }
                }
            }
        }
        Ok(json!({ "matches": matches, "total": total, "truncated": truncated, "scanTruncated": scan_truncated }))
    }
}

pub struct FilesHandler;
impl Handler for FilesHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: FilesArgs = parse_args(args)?;
        let path_str = a.path.clone().unwrap_or_else(|| ".".to_string());
        let base = resolve_checked(&k.root, &path_str)?;
        if !base.exists() {
            return Err(err_no_path(&path_str));
        }
        let budget = WalkBudget::from_limits(&k.cfg.limits);
        let guard = ScanGuard::from_limits(&k.cfg.limits);
        let mut out: Vec<String> = Vec::new();
        let mut scan_truncated = false;
        if fs::metadata(&base)?.is_dir() {
            let mut files: Vec<PathBuf> = Vec::new();
            scan_truncated = walk(&base, 0, budget, &guard, &mut files);
            for file in &files {
                if guard.expired() {
                    scan_truncated = true;
                    break;
                }
                if glob_match(&a.pattern, &rel_slash(&base, file)) {
                    out.push(rel_slash(&k.root, file));
                }
            }
        } else {
            let base_name = base
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if glob_match(&a.pattern, &base_name) || glob_match(&a.pattern, &rel_slash(&k.root, &base)) {
                out.push(rel_slash(&k.root, &base));
            }
        }
        Ok(json!({ "files": out, "total": out.len(), "scanTruncated": scan_truncated }))
    }
}

/// Sorted walk with cycle safety and a scan budget: skips SCAN_SKIP dirs,
/// never follows symlinks/junctions, skips files >= max_file_bytes, depth-
/// capped, and stops early once max_files files were scanned or the guard's
/// deadline passed. Returns true when the budget cut the scan short —
/// callers surface that as scanTruncated so partial results are never silent.
pub fn walk(dir: &Path, depth: usize, budget: WalkBudget, guard: &ScanGuard, out: &mut Vec<PathBuf>) -> bool {
    if depth > budget.max_depth {
        return false;
    }
    let Ok(rd) = fs::read_dir(dir) else { return false };
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    entries.sort();
    let mut scanned = 0usize;
    for full in entries {
        if guard.expired() {
            return true;
        }
        let name = full.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if SCAN_SKIP.contains(&name.as_str()) {
            continue;
        }
        let Ok(lm) = fs::symlink_metadata(&full) else { continue };
        if lm.is_symlink() {
            continue;
        }
        if lm.is_dir() {
            if is_reparse_point(&full) {
                continue;
            }
            if walk(&full, depth + 1, budget, guard, out) {
                return true;
            }
        } else {
            scanned += 1;
            if budget.max_files > 0 && scanned > budget.max_files {
                return true;
            }
            if lm.len() > budget.max_file_bytes {
                continue;
            }
            out.push(full);
        }
    }
    false
}

fn is_text_file(p: &Path) -> bool {
    match p.extension() {
        None => true, // extensionless (covers dotfiles like .gitignore)
        Some(ext) => TEXT_EXT.contains(&ext.to_string_lossy().to_lowercase().as_str()),
    }
}

fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Minimal glob port (search.mjs globEscape): ** crosses directories,
/// * within a segment, ? one char; everything else literal.
pub fn glob_match(glob: &str, s: &str) -> bool {
    let mut rx = String::from("^");
    let bytes: Vec<char> = glob.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == '*' {
            if i + 1 < bytes.len() && bytes[i + 1] == '*' {
                rx.push_str(".*");
                i += 1;
                if i + 1 < bytes.len() && bytes[i + 1] == '/' {
                    i += 1;
                }
            } else {
                rx.push_str("[^/]*");
            }
        } else if c == '?' {
            rx.push_str("[^/]");
        } else {
            if ".+^${}()|[]\\".contains(c) {
                rx.push('\\');
            }
            rx.push(c);
        }
        i += 1;
    }
    rx.push('$');
    match regex::Regex::new(&rx) {
        Ok(re) => re.is_match(s),
        Err(_) => false,
    }
}

// ---- search.replace (phase 2) -------------------------------------------------

pub const REPLACE_DESC: &str = "Regex search/replace across files. dryRun=true (default) reports per-file match counts without writing; dryRun=false rewrites files (run sys.snapshot first for rollback). Supports $0-$9 capture backrefs ($$ = literal $). Skips binaries and .git/node_modules/target/dist/build/.nc-tools.";

/// Like walk(), but also skips SCAN_SKIP dirs (target/dist/build included)
/// so bulk edits never touch generated artifacts. Used by search.replace and
/// code.symbols. Depth-cap is generous: replace/symbols operate on the whole
/// candidate set, not a bounded scan.
pub fn walk_files_ext(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if depth > 32 {
        return Ok(());
    }
    let rd = fs::read_dir(dir)?;
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    entries.sort();
    for full in entries {
        let name = full.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if SCAN_SKIP.contains(&name.as_str()) {
            continue;
        }
        let Ok(lm) = fs::symlink_metadata(&full) else { continue };
        if lm.is_symlink() {
            continue;
        }
        if lm.is_dir() {
            if is_reparse_point(&full) {
                continue;
            }
            walk_files_ext(&full, depth + 1, out)?;
        } else {
            out.push(full);
        }
    }
    Ok(())
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplaceArgs {
    pub pattern: String,
    pub replacement: String,
    #[doc = "Root of the search (default: base dir)"]
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub glob: Option<String>,
    #[doc = "true (default) = report only; false = write the changes"]
    #[serde(default)]
    pub dryRun: Option<bool>,
    #[serde(default)]
    #[schemars(range(min = 1, max = 2000))]
    pub maxFiles: Option<u64>,
}

pub struct ReplaceHandler;
impl Handler for ReplaceHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: ReplaceArgs = parse_args(args)?;
        let re = fancy_regex::Regex::new(&a.pattern).map_err(|e| {
            ToolError::with_hint("ERR_BAD_REGEX", format!("invalid regex: {e}"), json!({ "pattern": a.pattern }))
        })?;
        let base = resolve_checked(&k.root, a.path.as_deref().unwrap_or("."))?;
        if !base.exists() {
            return Err(err_no_path(a.path.as_deref().unwrap_or(".")));
        }
        let dry_run = a.dryRun.unwrap_or(true);
        let max_files = a.maxFiles.unwrap_or(200) as usize;
        let mut candidates: Vec<PathBuf> = Vec::new();
        if fs::metadata(&base)?.is_dir() {
            walk_files_ext(&base, 0, &mut candidates)
                .map_err(|e| ToolError::new("ERR_INTERNAL", e.to_string()))?;
        } else {
            candidates.push(base.clone());
        }
        let mut files: Vec<Value> = Vec::new();
        let mut total_matches: u64 = 0;
        let mut truncated = false;
        for f in candidates {
            let rel = rel_slash(&base, &f);
            if let Some(g) = &a.glob {
                if !glob_match(g, &rel) {
                    continue;
                }
            }
            let bytes = match fs::read(&f) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if bytes.len() > 10_000_000 {
                continue; // skip very large files
            }
            if bytes[..bytes.len().min(8192)].contains(&0) {
                continue; // binary
            }
            let content = match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let count = re.find_iter(&content).count() as u64;
            if count == 0 {
                continue;
            }
            if files.len() >= max_files {
                truncated = true;
                break;
            }
            let replaced = expand_replacement(&re, &content, &a.replacement);
            let changed = replaced != content;
            if !dry_run && changed {
                fs::write(&f, replaced.as_bytes()).map_err(|e| ToolError::new("ERR_INTERNAL", e.to_string()))?;
            }
            total_matches += count;
            files.push(json!({
                "path": rel,
                "matches": count,
                "bytesBefore": content.len(),
                "bytesAfter": replaced.len(),
                "changed": changed,
            }));
        }
        Ok(json!({
            "dryRun": dry_run,
            "pattern": a.pattern,
            "files": files,
            "totalFiles": files.len(),
            "totalMatches": total_matches,
            "truncated": truncated,
        }))
    }
}

/// Replace every non-overlapping match, honoring $0..$9 backrefs ($$ = literal $).
fn expand_replacement(re: &fancy_regex::Regex, content: &str, replacement: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut last = 0usize;
    for m in re.find_iter(content) {
        let m = match m {
            Ok(m) => m,
            Err(_) => break,
        };
        out.push_str(&content[last..m.start()]);
        let caps = match re.captures(&content[m.start()..m.end()]) {
            Ok(Some(c)) => c,
            _ => {
                out.push_str(&content[m.start()..m.end()]);
                last = m.end();
                continue;
            }
        };
        out.push_str(&apply_backrefs(&caps, replacement));
        last = m.end();
    }
    out.push_str(&content[last..]);
    out
}

fn apply_backrefs(caps: &fancy_regex::Captures, replacement: &str) -> String {
    let chars: Vec<char> = replacement.chars().collect();
    let mut out = String::with_capacity(replacement.len());
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '$' && i + 1 < chars.len() {
            let nxt = chars[i + 1];
            if nxt == '$' {
                out.push('$');
                i += 2;
                continue;
            }
            if nxt.is_ascii_digit() {
                let mut j = i + 1;
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
                let g: usize = chars[i + 1..j].iter().collect::<String>().parse().unwrap_or(0);
                if let Some(cm) = caps.get(g) {
                    out.push_str(cm.as_str());
                }
                i = j;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nct-search-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn budget(max_files: usize) -> WalkBudget {
        WalkBudget { max_files, max_file_bytes: u64::MAX, max_depth: 12 }
    }

    fn guard() -> ScanGuard {
        ScanGuard { started: Instant::now(), max_ms: 0 }
    }

    // Regression: the 228s proxyhub grep walked a Rust target/ plus dist/build
    // trees. Generated + dependency dirs must never be walked at all.
    #[test]
    fn walk_skips_generated_and_dependency_dirs() {
        let root = temp_root("skip");
        for (dir, f) in [
            ("target", "t.rs"),
            ("dist", "d.js"),
            ("build", "b.js"),
            ("node_modules", "n.js"),
            (".git", "g.js"),
            (".nc-tools", "j.js"),
        ] {
            fs::create_dir_all(root.join(dir)).unwrap();
            fs::write(root.join(dir).join(f), b"x").unwrap();
        }
        fs::write(root.join("keep.ts"), b"x").unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub").join("keep2.ts"), b"x").unwrap();

        let mut out = Vec::new();
        let truncated = walk(&root, 0, budget(10_000), &guard(), &mut out);
        assert!(!truncated);
        let names: Vec<String> = out
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["keep.ts", "keep2.ts"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn walk_stops_at_max_files() {
        let root = temp_root("budget");
        for i in 0..30 {
            fs::write(root.join(format!("f{i:02}.txt")), b"x").unwrap();
        }
        let mut out = Vec::new();
        let truncated = walk(&root, 0, budget(10), &guard(), &mut out);
        assert!(truncated, "budget stop must report scanTruncated");
        assert_eq!(out.len(), 10);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn walk_skips_oversized_files() {
        let root = temp_root("size");
        fs::write(root.join("big.txt"), vec![b'x'; 4096]).unwrap();
        fs::write(root.join("small.txt"), b"x").unwrap();
        let mut out = Vec::new();
        let b = WalkBudget { max_files: 100, max_file_bytes: 1024, max_depth: 12 };
        let truncated = walk(&root, 0, b, &guard(), &mut out);
        assert!(!truncated);
        let names: Vec<String> = out
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["small.txt"]);
        let _ = fs::remove_dir_all(&root);
    }
}
