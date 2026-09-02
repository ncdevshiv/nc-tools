// code.symbols: lexical symbol extraction (regex scan, not a parser).

use crate::fs_tools::err_no_path;
use crate::search::{glob_match, walk_files_ext};
use nct_core::errors::ToolError;
use nct_core::helpers::rel_slash;
use nct_core::kernel::{parse_args, Handler, Kernel};
use nct_core::paths::resolve_checked;
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

pub const SYMBOLS_DESC: &str = "Extract code symbols (fns, structs, enums, traits, impls, mods, classes, interfaces, defs) with line numbers - a fast lexical scan, not a full parser. Languages: Rust, JS/TS/TSX/JSX, Python.";

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SymbolsArgs {
    #[doc = "Path - relative to the base dir, or absolute (any location allowed)"]
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub glob: Option<String>,
    #[doc = "Only return these kinds, e.g. fn, struct"]
    #[serde(default)]
    pub kinds: Option<Vec<String>>,
    #[serde(default)]
    #[schemars(range(min = 1, max = 5000))]
    pub maxResults: Option<u64>,
}

pub struct SymbolsHandler;
impl Handler for SymbolsHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: SymbolsArgs = parse_args(args)?;
        let base = resolve_checked(&k.root, a.path.as_deref().unwrap_or("."))?;
        if !base.exists() {
            return Err(err_no_path(a.path.as_deref().unwrap_or(".")));
        }
        let max_results = a.maxResults.unwrap_or(500) as usize;
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        if base.is_file() {
            files.push(base.clone());
        } else {
            walk_files_ext(&base, 0, &mut files)
                .map_err(ToolError::from)?;
        }
        let rust_pats = compile(&[
            ("fn", r#"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+"[^"]*"\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)"#),
            ("struct", r"^\s*(?:pub(?:\([^)]*\))?\s+)?struct\s+([A-Za-z_][A-Za-z0-9_]*)"),
            ("enum", r"^\s*(?:pub(?:\([^)]*\))?\s+)?enum\s+([A-Za-z_][A-Za-z0-9_]*)"),
            ("trait", r"^\s*(?:pub(?:\([^)]*\))?\s+)?trait\s+([A-Za-z_][A-Za-z0-9_]*)"),
            ("mod", r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)"),
            ("impl", r"^\s*impl(?:<[^>]*>)?\s+(.+?)\s*\{"),
        ]);
        let js_pats = compile(&[
            ("fn", r"(?:^|[^\w$])function\s*\*?\s*([A-Za-z_$][\w$]*)"),
            ("class", r"(?:^|[^\w$])class\s+([A-Za-z_$][\w$]*)"),
            ("fn", r"(?:^|[^\w$])(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*(?:async\s*)?\("),
            ("interface", r"(?:^|[^\w$])interface\s+([A-Za-z_$][\w$]*)"),
            ("type", r"(?:^|[^\w$])type\s+([A-Za-z_$][\w$]*)\s*=\s*[^=]"),
        ]);
        let py_pats = compile(&[
            ("fn", r"^\s*(?:async\s+)?def\s+([A-Za-z_]\w*)"),
            ("class", r"^\s*class\s+([A-Za-z_]\w*)"),
        ]);
        let mut symbols: Vec<Value> = Vec::new();
        let mut total: u64 = 0;
        let mut truncated = false;
        for f in files {
            let lang = match lang_of(&f) {
                Some(l) => l,
                None => continue,
            };
            let rel = rel_slash(&k.root, &f);
            if let Some(g) = &a.glob {
                if !glob_match(g, &rel) {
                    continue;
                }
            }
            let content = match fs::read_to_string(&f) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if content[..content.len().min(8192)].contains('\0') {
                continue; // binary
            }
            let pats: &[(&str, fancy_regex::Regex)] = match lang {
                "rust" => &rust_pats,
                "js" => &js_pats,
                _ => &py_pats,
            };
            for (i, line) in content.lines().enumerate() {
                for (kind, re) in pats {
                    let caps = match re.captures(line) {
                        Ok(Some(c)) => c,
                        _ => continue,
                    };
                    let mut name = caps.get(1).map(|m| m.as_str()).unwrap_or("").trim().to_string();
                    if lang == "rust" && *kind == "impl" {
                        name = name
                            .split(" where ")
                            .next()
                            .unwrap_or(&name)
                            .trim()
                            .to_string();
                    }
                    if name.is_empty() {
                        continue;
                    }
                    if let Some(want) = &a.kinds {
                        if !want.iter().any(|w| w == kind) {
                            continue;
                        }
                    }
                    total += 1;
                    if symbols.len() < max_results {
                        symbols.push(json!({
                            "path": rel,
                            "line": i + 1,
                            "kind": kind,
                            "name": name,
                        }));
                    } else {
                        truncated = true;
                    }
                    break; // one symbol per line
                }
            }
        }
        Ok(json!({
            "symbols": symbols,
            "total": total,
            "returned": symbols.len(),
            "truncated": truncated,
        }))
    }
}

fn lang_of(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()? {
        "rs" => Some("rust"),
        "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" => Some("js"),
        "py" => Some("py"),
        _ => None,
    }
}

fn compile<'a>(pats: &[(&'a str, &str)]) -> Vec<(&'a str, fancy_regex::Regex)> {
    pats.iter()
        .filter_map(|(k, p)| fancy_regex::Regex::new(p).ok().map(|re| (*k, re)))
        .collect()
}
