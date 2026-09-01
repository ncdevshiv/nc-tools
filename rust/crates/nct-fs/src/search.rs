// search.* tools — line-based regex grep and glob-ish file search, machine-wide.
// Behavior-parity port of src/kernel/search.mjs.
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};

use nct_core::errors::ToolError;
use nct_core::helpers::rel_slash;
use nct_core::kernel::{parse_args, Handler, Kernel};
use nct_core::paths::{is_reparse_point, resolve_checked};

use crate::fs_tools::{err_no_path, err_no_path_with_siblings};

pub const GREP_DESC: &str = "Regex search across files. Returns file/line/text matches.";
pub const FILES_DESC: &str = "Find files by glob pattern (e.g. \"**/*.test.mjs\").";

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

pub struct GrepHandler;
impl Handler for GrepHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: GrepArgs = parse_args(args)?;
        let re = fancy_regex::Regex::new(&a.pattern)
            .map_err(|e| ToolError::with_hint("ERR_BAD_REGEX", format!("Invalid regex: {e}"), json!({ "pattern": a.pattern })))?;
        let path_str = a.path.clone().unwrap_or_else(|| ".".to_string());
        let base = resolve_checked(&k.root, &path_str)?;
        if !base.exists() {
            return Err(err_no_path_with_siblings(&path_str, &base));
        }
        let max_results = a.maxResults.unwrap_or(k.cfg.limits.grep_max_results as u64) as usize;
        let mut files: Vec<PathBuf> = Vec::new();
        if fs::metadata(&base)?.is_dir() {
            walk(&base, 0, &mut files);
        } else {
            files.push(base.clone());
        }
        let mut matches: Vec<Value> = Vec::new();
        let mut total = 0usize;
        let mut truncated = false;
        for file in &files {
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
        Ok(json!({ "matches": matches, "total": total, "truncated": truncated }))
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
        let mut out: Vec<String> = Vec::new();
        if fs::metadata(&base)?.is_dir() {
            let mut files: Vec<PathBuf> = Vec::new();
            walk(&base, 0, &mut files);
            for file in &files {
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
        Ok(json!({ "files": out, "total": out.len() }))
    }
}

/// Sorted walk with cycle safety: skip .git/node_modules/.nc-tools, never
/// follow symlinks/junctions, depth-capped (search.mjs walkFiles).
pub fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 12 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    entries.sort();
    for full in entries {
        let name = full.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if name == ".git" || name == "node_modules" || name == ".nc-tools" {
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
            walk(&full, depth + 1, out);
        } else {
            out.push(full);
        }
    }
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

const REPLACE_SKIP: [&str; 5] = [".git", "node_modules", ".nc-tools", "target", "dist"];

/// Like walk(), but also skips target/dist/build so bulk edits never touch
/// generated artifacts. Used by search.replace and code.symbols.
pub fn walk_files_ext(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if depth > 32 {
        return Ok(());
    }
    let rd = fs::read_dir(dir)?;
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    entries.sort();
    for full in entries {
        let name = full.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if REPLACE_SKIP.contains(&name.as_str()) {
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
