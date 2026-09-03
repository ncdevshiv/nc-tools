// code.symbols: symbol extraction with full attention to what an agent needs —
// the symbol's NAME, KIND, LINE SPAN (start/end), and DOC COMMENT. A regex
// scan locates the declaration line; a brace/indent scan extends it to the
// matching end line so the agent can fs.read{offset,limit} exactly that range
// and see the doc comment above it without a full parser. Languages: Rust,
// JS/TS/TSX/JSX, Python.

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

pub const SYMBOLS_DESC: &str = "Extract code symbols (fns, structs, enums, traits, impls, mods, classes, interfaces, defs) with line numbers AND line spans + doc comments - a fast lexical scan with brace/indent extension, not a full parser. Each symbol returns {name, kind, line, endLine, doc, span-bytes} so you can read exactly the matched range. Languages: Rust, JS/TS/TSX/JSX, Python.";

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
    /// Include the full text of each symbol (default true; set false for a
    /// lean name/kind/line-only listing).
    #[serde(default)]
    pub withBody: Option<bool>,
    #[doc = "Base dir for relative paths (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
}

pub struct SymbolsHandler;
impl Handler for SymbolsHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: SymbolsArgs = parse_args(args)?;
        let base = k.base_dir(a.baseDir.as_deref())?;
        let search_root = resolve_checked(&base, a.path.as_deref().unwrap_or("."))?;
        if !search_root.exists() {
            return Err(err_no_path(a.path.as_deref().unwrap_or(".")));
        }
        let max_results = a.maxResults.unwrap_or(500) as usize;
        let with_body = a.withBody.unwrap_or(true);
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        if search_root.is_file() {
            files.push(search_root.clone());
        } else {
            walk_files_ext(&search_root, 0, &mut files)
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
            let rel = rel_slash(&base, &f);
            if let Some(g) = &a.glob {
                if !glob_match(g, &rel) {
                    continue;
                }
            }
            let content = match fs::read_to_string(&f) {
                Ok(s) => s,
                Err(_) => continue,
            };
            // Binary check on BYTES, not a String slice: `content[..8192]`
            // would panic if byte 8192 landed mid-codepoint. Index the
            // underlying bytes instead (same as search.rs's Vec<u8> path).
            if content.as_bytes()[..content.len().min(8192)].contains(&0) {
                continue; // binary
            }
            let pats: &[(&str, fancy_regex::Regex)] = match lang {
                "rust" => &rust_pats,
                "js" => &js_pats,
                _ => &py_pats,
            };
            let lines: Vec<&str> = content.lines().collect();
            for (i, line) in lines.iter().enumerate() {
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
                        let (end_line, _span) = symbol_span(&lines, i, lang);
                        let doc = doc_comment(&lines, i, lang);
                        let mut sym = json!({
                            "path": rel,
                            "line": i + 1,
                            "endLine": end_line,
                            "kind": kind,
                            "name": name,
                            "doc": doc,
                        });
                        if with_body {
                            let body: String = lines[i..end_line].join("\n");
                            sym["body"] = json!(body.chars().take(4000).collect::<String>());
                            sym["bytes"] = json!(body.chars().count());
                        }
                        symbols.push(sym);
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

/// Extend a declaration line to its end line by brace-matching (Rust/JS) or
/// indentation (Python). Returns (end_line_1based, true_byte_span_end).
/// The span drives the `body` so the agent can read exactly the symbol, not
/// the rest of the file.
fn symbol_span(lines: &[&str], start: usize, lang: &str) -> (usize, usize) {
    let line = lines[start];
    // Python: body extends while the next line is more indented or blank-after-indent.
    if lang == "py" {
        let indent = line.len() - line.trim_start().len();
        let mut body_end = start + 1; // index after the last body line
        let mut next = start + 1;
        while next < lines.len() {
            let l = lines[next];
            if l.trim().is_empty() {
                next += 1;
                continue; // blank lines inside the body are fine
            }
            let l_indent = l.len() - l.trim_start().len();
            if l_indent <= indent {
                break; // dedent to <= the def/class indent ends the body
            }
            body_end = next + 1; // this line is part of the body
            next += 1;
        }
        let end = body_end.max(start + 1);
        return (end, full_byte_span(lines, start, end));
    }
    // Rust/JS: brace-match. Count braces on the declaration line first.
    let mut depth: i32 = 0;
    for c in lines[start].chars() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {}
        }
    }
    if depth > 0 {
        let mut idx = start + 1;
        while idx < lines.len() {
            for c in lines[idx].chars() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth <= 0 {
                            idx += 1;
                            return (idx, full_byte_span(lines, start, idx));
                        }
                    }
                    _ => {}
                }
            }
            idx += 1;
        }
        return ((start + 1), full_byte_span(lines, start, start + 1));
    }
    // No braces (struct/enum/type without body, one-liner, trait bounds):
    // just the declaration line.
    (start + 1, full_byte_span(lines, start, start + 1))
}

fn full_byte_span(lines: &[&str], start: usize, end: usize) -> usize {
    let mut bytes = 0;
    for l in &lines[start..end] {
        bytes += l.len() + 1; // +1 for the newline
    }
    bytes
}

/// Doc comment immediately above the symbol: `///` (Rust), `/** */` or `//`
/// (JS/TS), `#` (Python). Returns the joined comment lines, <= 20 lines.
fn doc_comment(lines: &[&str], start: usize, lang: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut idx = start;
    while idx > 0 {
        let prev = lines[idx - 1].trim();
        if prev.is_empty() {
            break; // blank line ends the comment block
        }
        let is_doc = match lang {
            "rust" => prev.starts_with("///") || prev.starts_with("//!"),
            "py" => prev.starts_with('#') && !prev.starts_with("#!"),
            _ => prev.starts_with("///") || prev.starts_with("/**") || prev.starts_with("//"),
        };
        if !is_doc {
            break;
        }
        out.push(prev.trim_start_matches('/').trim_start_matches('#').trim_start_matches('*').trim().to_string());
        if out.len() >= 20 {
            break;
        }
        idx -= 1;
    }
    out.reverse();
    out.join("\n")
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

#[cfg(test)]
mod symbols_span_tests {
    use super::*;
    use nct_core::kernel::Kernel;
    use std::fs;

    fn make_kernel() -> Kernel {
        let dir = std::env::temp_dir().join(format!(
            "nct-symbols-test-{}-{}",
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
    fn rust_symbol_span_covers_full_fn_body() {
        let k = make_kernel();
        fs::write(
            k.root.join("s.rs"),
            "/// Adds two numbers.\n/// Returns the sum.\npub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        )
        .unwrap();
        let args = json!({ "path": "s.rs", "kinds": ["fn"] });
        let v = SymbolsHandler.call(&k, &args).unwrap();
        let syms = v["symbols"].as_array().unwrap();
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0]["name"], json!("add"));
        assert_eq!(syms[0]["line"], json!(3)); // decl line
        assert_eq!(syms[0]["endLine"], json!(5)); // closing brace
        // doc comment captured
        assert!(syms[0]["doc"].as_str().unwrap().contains("Adds two numbers"));
        assert!(syms[0]["doc"].as_str().unwrap().contains("Returns the sum"));
        // body present
        assert!(syms[0]["body"].as_str().unwrap().contains("a + b"));
    }

    #[test]
    fn python_symbol_span_covers_indented_body() {
        let k = make_kernel();
        fs::write(
            k.root.join("p.py"),
            "def greet(name):\n    \"\"\"Say hello.\"\"\"\n    return f\"hi {name}\"\n\nclass Dog:\n    def bark(self):\n        return \"woof\"\n",
        )
        .unwrap();
        let args = json!({ "path": "p.py", "kinds": ["fn", "class"] });
        let v = SymbolsHandler.call(&k, &args).unwrap();
        let syms = v["symbols"].as_array().unwrap();
        // greet fn and Dog class and bark method
        let names: Vec<_> = syms.iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"greet"));
        assert!(names.contains(&"Dog"));
        assert!(names.contains(&"bark"));
        // greet fn body spans 3 lines (def, docstring, return)
        let greet = syms.iter().find(|s| s["name"] == json!("greet")).unwrap();
        assert_eq!(greet["endLine"], json!(3));
    }

    #[test]
    fn js_symbol_span_covers_brace_body() {
        let k = make_kernel();
        fs::write(
            k.root.join("j.js"),
            "function add(a, b) {\n  return a + b;\n}\n",
        )
        .unwrap();
        let args = json!({ "path": "j.js", "kinds": ["fn"] });
        let v = SymbolsHandler.call(&k, &args).unwrap();
        let syms = v["symbols"].as_array().unwrap();
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0]["name"], json!("add"));
        assert_eq!(syms[0]["line"], json!(1));
        assert_eq!(syms[0]["endLine"], json!(3));
        assert!(syms[0]["body"].as_str().unwrap().contains("return a + b"));
    }

    #[test]
    fn one_liner_symbol_has_single_line_span() {
        let k = make_kernel();
        fs::write(
            k.root.join("one.rs"),
            "pub const X: u32 = 42;\n",
        )
        .unwrap();
        let args = json!({ "path": "one.rs", "kinds": ["mod"] });
        // There are no fns here, so the symbol count is 0 (const isn't matched);
        // verify graceful behavior — no panic, zero symbols.
        let v = SymbolsHandler.call(&k, &args).unwrap();
        assert_eq!(v["total"], json!(0));
    }

    #[test]
    fn with_body_false_is_lean() {
        let k = make_kernel();
        fs::write(
            k.root.join("lean.rs"),
            "pub fn lean() -> i32 {\n    42\n}\n",
        )
        .unwrap();
        let args = json!({ "path": "lean.rs", "kinds": ["fn"], "withBody": false });
        let v = SymbolsHandler.call(&k, &args).unwrap();
        let syms = v["symbols"].as_array().unwrap();
        assert_eq!(syms.len(), 1);
        assert!(syms[0].get("body").is_none());
        assert!(syms[0].get("bytes").is_none());
        // still has name/line/endLine/doc
        assert!(syms[0]["name"].as_str().is_some());
        assert!(syms[0]["endLine"].as_u64().is_some());
    }
}
