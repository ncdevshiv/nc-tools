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
