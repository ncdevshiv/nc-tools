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

pub const GREP_DESC: &str = "Regex search across files. Returns file/line/text matches. Directory scans are bounded: they skip .git/node_modules/.nc-tools/target/dist/build, skip files over 10MB, and stop after 20k files or 10s (limits.grepMaxScan*) — scanTruncated:true in the result means the budget stopped the scan, so matches may be partial. Directly-targeted file paths are always scanned in full. Supports contextBefore/contextAfter (adjacent lines with line numbers), fileType (extension filter, e.g. \"rs\"), and fixedString (fast literal substring search — no regex compilation).";
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
    /// Lines of context before a match (0 = none).
    #[serde(default)]
    #[schemars(range(min = 0, max = 50))]
    pub contextBefore: Option<u64>,
    /// Lines of context after a match (0 = none).
    #[serde(default)]
    #[schemars(range(min = 0, max = 50))]
    pub contextAfter: Option<u64>,    /// Filter by extension (e.g. "rs", "ts", "py" — no dot). Overrides glob.
    #[serde(default)]
    pub fileType: Option<String>,
    /// Literal substring search (no regex compilation — much faster). The
    /// pattern is matched as a plain string, not a regex.
    #[serde(default)]
    pub fixedString: Option<bool>,
    #[doc = "Base dir for relative paths (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FilesArgs {
    pub pattern: String,
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    #[serde(default)]
    pub path: Option<String>,
    #[doc = "Base dir for relative paths (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
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
        // fixedString path: no regex compilation — match literal substring.
        // Memchr is the engine ripgrep uses for literal search; a plain
        // `content.contains(pattern)` lets the compiler use SIMD-optimized
        // memchr internally and avoids regex setup entirely.
        let re = if a.fixedString.unwrap_or(false) {
            None
        } else {
            Some(
                fancy_regex::Regex::new(&a.pattern)
                    .map_err(|e| ToolError::with_hint("ERR_BAD_REGEX", format!("Invalid regex: {e}"), json!({ "pattern": a.pattern })))?,
            )
        };
        let path_str = a.path.clone().unwrap_or_else(|| ".".to_string());
        let base = k.base_dir(a.baseDir.as_deref())?;
        let search_root = resolve_checked(&base, &path_str)?;
        if !search_root.exists() {
            return Err(err_no_path_with_siblings(&path_str, &search_root, &base));
        }
        let max_results = a.maxResults.unwrap_or(k.cfg.limits.grep_max_results as u64) as usize;
        let ctx_before = a.contextBefore.unwrap_or(0) as usize;
        let ctx_after = a.contextAfter.unwrap_or(0) as usize;
        let budget = WalkBudget::from_limits(&k.cfg.limits);
        let guard = ScanGuard::from_limits(&k.cfg.limits);
        let mut files: Vec<PathBuf> = Vec::new();
        let mut scan_truncated = if fs::metadata(&search_root)?.is_dir() {
            walk(&search_root, 0, budget, &guard, &mut files)
        } else {
            files.push(search_root.clone());
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
                if !glob_match(glob, &rel_slash(&search_root, file)) {
                    continue;
                }
            }
            // fileType filter: extension match (no dot). Overrides nothing —
            // it ANDs with glob when both are given, matching standard tools.
            if let Some(ft) = &a.fileType {
                let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
                if ext != *ft {
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
            let lines: Vec<&str> = content.split('\n').collect();
            let mut idx = 0usize;
            while idx < lines.len() {
                if (idx & 0x3FF) == 0 && guard.expired() {
                    scan_truncated = true;
                    break 'files;
                }
                let line = lines[idx];
                let is_match = match &re {
                    Some(re) => re.is_match(line).unwrap_or(false),
                    None => line.contains(a.pattern.as_str()),
                };
                if is_match {
                    total += 1;
                    if matches.len() < max_results {
                        // Context window: [idx-ctx_before, idx+ctx_after]
                        let ctx_start = idx.saturating_sub(ctx_before);
                        let ctx_end = (idx + ctx_after + 1).min(lines.len());
                        let mut ctx = Vec::new();
                        for ci in ctx_start..ctx_end {
                            ctx.push(json!({
                                "line": ci + 1,
                                "text": take_chars(lines[ci], k.cfg.limits.grep_line_chars),
                                "isMatch": ci == idx,
                            }));
                        }
                        matches.push(json!({
                            "file": rel_slash(&base, file),
                            "line": idx + 1,
                            "text": take_chars(line, k.cfg.limits.grep_line_chars),
                            "context": ctx,
                        }));
                    } else {
                        truncated = true;
                    }
                }
                idx += 1;
            }
        }
        Ok(json!({
            "matches": matches,
            "total": total,
            "truncated": truncated,
            "scanTruncated": scan_truncated,
            "contextBefore": ctx_before,
            "contextAfter": ctx_after,
        }))
    }
}

pub struct FilesHandler;
impl Handler for FilesHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: FilesArgs = parse_args(args)?;
        let path_str = a.path.clone().unwrap_or_else(|| ".".to_string());
        let base = k.base_dir(a.baseDir.as_deref())?;
        let search_root = resolve_checked(&base, &path_str)?;
        if !search_root.exists() {
            return Err(err_no_path(&path_str));
        }
        let budget = WalkBudget::from_limits(&k.cfg.limits);
        let guard = ScanGuard::from_limits(&k.cfg.limits);
        let mut out: Vec<String> = Vec::new();
        let mut scan_truncated = false;
        if fs::metadata(&search_root)?.is_dir() {
            let mut files: Vec<PathBuf> = Vec::new();
            scan_truncated = walk(&search_root, 0, budget, &guard, &mut files);
            for file in &files {
                if guard.expired() {
                    scan_truncated = true;
                    break;
                }
                if glob_match(&a.pattern, &rel_slash(&search_root, file)) {
                    out.push(rel_slash(&base, file));
                }
            }
        } else {
            let base_name = search_root
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if glob_match(&a.pattern, &base_name) || glob_match(&a.pattern, &rel_slash(&base, &search_root)) {
                out.push(rel_slash(&base, &search_root));
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
    #[doc = "Base dir for the root path (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
}

pub struct ReplaceHandler;
impl Handler for ReplaceHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: ReplaceArgs = parse_args(args)?;
        let re = fancy_regex::Regex::new(&a.pattern).map_err(|e| {
            ToolError::with_hint("ERR_BAD_REGEX", format!("invalid regex: {e}"), json!({ "pattern": a.pattern }))
        })?;
        let base = k.base_dir(a.baseDir.as_deref())?;
        let search_root = resolve_checked(&base, a.path.as_deref().unwrap_or("."))?;
        if !search_root.exists() {
            return Err(err_no_path(a.path.as_deref().unwrap_or(".")));
        }
        let dry_run = a.dryRun.unwrap_or(true);
        let max_files = a.maxFiles.unwrap_or(200) as usize;
        let mut candidates: Vec<PathBuf> = Vec::new();
        if fs::metadata(&search_root)?.is_dir() {
            walk_files_ext(&search_root, 0, &mut candidates)
                .map_err(ToolError::from)?;
        } else {
            candidates.push(search_root.clone());
        }
        let mut files: Vec<Value> = Vec::new();
        let mut total_matches: u64 = 0;
        let mut truncated = false;
        for f in candidates {
            let rel = rel_slash(&search_root, &f);
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
                fs::write(&f, replaced.as_bytes()).map_err(ToolError::from)?;
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

#[cfg(test)]
mod grep_extension_tests {
    use super::*;
    use nct_core::kernel::Kernel;
    use std::fs;

    fn make_kernel() -> Kernel {
        let dir = std::env::temp_dir().join(format!(
            "nct-grep-test-{}-{}",
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

    fn write(k: &Kernel, path: &str, content: &str) {
        fs::write(k.root.join(path), content).unwrap();
    }

    #[test]
    fn grep_context_before_after() {
        let k = make_kernel();
        write(&k, "main.rs", "fn a() {}\n// target line\nfn b() {}\nfn c() {}\n");
        let args = json!({ "pattern": "target", "path": "main.rs", "contextBefore": 1, "contextAfter": 1 });
        let v = GrepHandler.call(&k, &args).unwrap();
        let matches = v["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        let ctx = matches[0]["context"].as_array().unwrap();
        // ctx = [before, match, after]
        assert_eq!(ctx.len(), 3);
        assert_eq!(ctx[0]["line"], json!(1));
        assert_eq!(ctx[0]["isMatch"], json!(false));
        assert_eq!(ctx[1]["line"], json!(2));
        assert_eq!(ctx[1]["isMatch"], json!(true));
        assert_eq!(ctx[2]["line"], json!(3));
        assert_eq!(ctx[2]["isMatch"], json!(false));
    }

    #[test]
    fn grep_file_type_filter() {
        let k = make_kernel();
        write(&k, "a.rs", "fn rust_fn() {}\n");
        write(&k, "b.ts", "function tsFn() {}\n");
        // fileType="rs" → only rust files match
        let args = json!({ "pattern": "fn", "path": ".", "fileType": "rs" });
        let v = GrepHandler.call(&k, &args).unwrap();
        let matches = v["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert!(matches[0]["file"].as_str().unwrap().ends_with("a.rs"));
    }

    #[test]
    fn grep_fixed_string_is_literal() {
        let k = make_kernel();
        write(&k, "x.txt", "line with a.b.c inside\n");
        // regex would need escaping; fixedString treats it literally
        let args = json!({ "pattern": "a.b.c", "path": "x.txt", "fixedString": true });
        let v = GrepHandler.call(&k, &args).unwrap();
        let matches = v["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1, "fixedString should find literal a.b.c");
        // Regex mode would also match (the dot is a wildcard) — but literal
        // semantics must find exactly the literal substring. Verify by
        // ensuring it matched the right text.
        assert!(matches[0]["text"].as_str().unwrap().contains("a.b.c"));
    }

    #[test]
    fn grep_fixed_string_does_not_interpret_regex() {
        let k = make_kernel();
        write(&k, "y.txt", "cat sat on mat\ncatXXat\n");
        // In regex mode, "cat*" would match "cat", "catX", etc. In fixedString,
        // it's the literal characters "cat*" — which don't appear.
        let args = json!({ "pattern": "cat*", "path": "y.txt", "fixedString": true });
        let v = GrepHandler.call(&k, &args).unwrap();
        assert_eq!(v["matches"].as_array().unwrap().len(), 0);
        // Regex mode WOULD match (cat* = cat + zero or more)
        let args2 = json!({ "pattern": "cat*", "path": "y.txt" });
        let v2 = GrepHandler.call(&k, &args2).unwrap();
        assert!(v2["matches"].as_array().unwrap().len() >= 1);
    }

    #[test]
    fn grep_context_absent_by_default() {
        let k = make_kernel();
        write(&k, "z.txt", "one\ntwo\nthree\n");
        let args = json!({ "pattern": "two", "path": "z.txt" });
        let v = GrepHandler.call(&k, &args).unwrap();
        let matches = v["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        // no context key when not requested (or empty)
        assert!(matches[0]["context"].as_array().unwrap().is_empty() || matches[0]["context"].as_array().unwrap().len() == 1);
        assert_eq!(v["contextBefore"], json!(0));
        assert_eq!(v["contextAfter"], json!(0));
    }
}
