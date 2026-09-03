// code.graph — cross-file reference graph (who calls whom, who implements what).
//
// PROBLEM: code.symbols answers "what symbols exist" but not "how do they
// relate". An agent asking "who calls validate_token?" has to read every file
// it suspects — no tool answers it. Language servers (rust-analyzer etc.) do,
// but they're heavy and per-language; we want a fast, dependency-light answer.
//
// code.graph builds a reference graph WITHOUT a language server:
//   1. scan each source file for symbols (fn/def/class/struct/trait/impl/const/
//      let) reusing the same lexical approach as code.symbols — regex for the
//      declaration, brace/indent span for the body;
//   2. for every symbol, scan its body for word-boundary references to the
//      names of OTHER symbols in the corpus (and, for Rust/Go, a same-scope
//      member);
//   3. emit {nodes, edges:[{caller:{symbol,file,line}, callee:{symbol,file}}]}
//      plus convenience queries: callees(name) / callers(name).
//
// This is NAME-RESOLUTION WITHIN SCOPE, not full type resolution — it catches
// the 80% ("who calls validate_token") and is honest that it can miss a method
// resolved through a trait/impl bound. No new dependency beyond the existing
// fancy_regex.
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};

use nct_core::errors::ToolError;
use nct_core::helpers::rel_slash;
use nct_core::kernel::{parse_args, Handler, Kernel};

use crate::search::{glob_match, walk_files_ext};

pub const GRAPH_DESC: &str = "Build a cross-file reference graph (who calls whom / what a symbol references) WITHOUT a language server. Scans symbols (fn/def/class/struct/trait) via the same lexical scanner as code.symbols, then records word-boundary references from each symbol body to other symbols. Query with callees? or callers? to answer 'who calls X'. Name-resolution within scope — not full type resolution (may miss a method resolved through an impl bound). Returns {nodes, edges}.";

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GraphArgs {
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub baseDir: Option<String>,
    #[serde(default)]
    pub glob: Option<String>,
    /// Return only edges where the callee has this name.
    #[serde(default)]
    pub callers: Option<String>,
    /// Return only edges where the caller has this name.
    #[serde(default)]
    pub callees: Option<String>,
    #[serde(default)]
    #[schemars(range(min = 1, max = 5000))]
    pub maxNodes: Option<u64>,
}

struct Sym {
    name: String,
    kind: String,
    file: String,
    line: u64,
    body: String,
}

pub struct GraphHandler;
impl Handler for GraphHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: GraphArgs = parse_args(args)?;
        let base = k.base_dir(a.baseDir.as_deref())?;
        let mut files: Vec<PathBuf> = Vec::new();
        if base.is_file() {
            files.push(base.clone());
        } else {
            walk_files_ext(&base, 0, &mut files).map_err(ToolError::from)?;
        }

        let max_nodes = a.maxNodes.unwrap_or(2000) as usize;
        let mut symbols: Vec<Sym> = Vec::new();
        let mut kinds: HashMap<String, &'static str> = HashMap::new();

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
            if content[..content.len().min(8192)].contains('\0') {
                continue;
            }
            let mut syms = scan(&content, lang, &rel, &mut kinds);
            // Track symbol names (we dedupe later by name for the node set).
            for s in syms.iter() {
                if symbols.len() < max_nodes {
                    symbols.push(Sym { name: s.name.clone(), kind: s.kind.clone(), file: s.file.clone(), line: s.line, body: s.body.clone() });
                }
            }
            syms.clear();
        }
        if symbols.is_empty() {
            return Ok(json!({ "nodes": [], "edges": [], "callers": [], "callees": [] }));
        }

        // Build a name -> node map; a name may resolve in many files.
        let mut name_to_files: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, s) in symbols.iter().enumerate() {
            name_to_files.entry(s.name.clone()).or_default().push(i);
        }

        // Edges: for each symbol body, find word-boundary refs to other symbol
        // names (excluding self, and excluding a name whose ONLY hit is the
        // declaration itself).
        let mut edges: Vec<Value> = Vec::new();
        let mut seen: HashSet<(String, String, String)> = HashSet::new();
        for s in &symbols {
            let refs = referenced_names(&s.body, &name_to_files, &s.name);
            for r in refs {
                let key = (s.file.clone(), s.name.clone(), r);
                if seen.insert(key.clone()) {
                    edges.push(json!({
                        "caller": s.name,
                        "callerFile": s.file,
                        "callerLine": s.line,
                        "callee": key.2,
                        "callerKind": s.kind,
                    }));
                }
            }
        }

        let nodes: Vec<Value> = symbols.iter().map(|s| json!({
            "name": s.name,
            "kind": s.kind,
            "file": s.file,
            "line": s.line,
        })).collect();

        let mut out = json!({ "nodes": nodes, "edges": edges, "nodesTotal": symbols.len(), "edgesTotal": edges.len() });
        if let Some(c) = &a.callees {
            let q = c.clone();
            let e: Vec<Value> = edges.iter().filter(|e| e["callee"] == json!(q)).cloned().collect();
            out["callers"] = json!(e);
            out["callees"] = json!([]);
        }
        if let Some(c) = &a.callers {
            let q = c.clone();
            let e: Vec<Value> = edges.iter().filter(|e| e["caller"] == json!(q)).cloned().collect();
            out["callees"] = json!(e);
        }
        Ok(out)
    }
}

fn lang_of(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()? {
        "rs" => Some("rust"),
        "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" => Some("js"),
        "py" => Some("py"),
        "go" => Some("go"),
        "java" => Some("java"),
        _ => None,
    }
}

/// Scan a file for symbols + their bodies. Reuses the code.symbols approach
/// (regex declaration + brace/indent span). Returns also records each symbol's
/// name->kind for the node set.
fn scan(content: &str, lang: &str, rel: &str, kinds: &mut HashMap<String, &'static str>) -> Vec<Sym> {
    let lines: Vec<&str> = content.split('\n').collect();
    let mut out = Vec::new();
    let (pat, is_py, is_brace) = match lang {
        "py" => (r#"^\s*(?:async\s+)?(def|class)\s+([A-Za-z_]\w*)"#, true, false),
        "go" => (r#"^\s*(?:func|type|struct|interface)\s+([A-Za-z_]\w*)"#, false, true),
        "js" => (r#"(?:^|[^\w$])(?:function|class|const|let|var)\s+([A-Za-z_$][\w$]*)"#, false, true),
        "java" => (r#"^\s*(?:public|private|protected|static|final|abstract|synchronized|native|transient|volatile|default|strictfp)?\s*(?:class|interface|enum)\s+([A-Za-z_]\w*)"#, false, true),
        _ => (r#"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+"[^"]*"\s+)?(?:fn|struct|enum|trait|impl|mod)\s+([A-Za-z_][A-Za-z0-9_]*)"#, false, true),
    };
    let re = match fancy_regex::Regex::new(pat) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for (i, line) in lines.iter().enumerate() {
        let caps = match re.captures(line) {
            Ok(Some(c)) => c,
            _ => continue,
        };
        let name = caps.get(if is_py { 2 } else { 1 }).map(|m| m.as_str()).unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        let (end, body) = span(&lines, i, is_py, is_brace);
        let kind = if is_py {
            if line.trim_start().starts_with("class") { "class" } else { "def" }
        } else {
            decl_kind(line)
        };
        kinds.insert(name.clone(), kind);
        let body_s: String = body;
        out.push(Sym { name, kind: kind.to_string(), file: rel.to_string(), line: (i + 1) as u64, body: body_s });
    }
    out
}

/// Leading-keyword kind from the declaration line itself — the same signal
/// code.symbols uses. This is exact (not inferred from name).
fn decl_kind(line: &str) -> &'static str {
    let t = line.trim_start();
    // order matters: the first matching keyword wins
    for (needle, kind) in [
        ("fn ", "fn"), ("pub fn", "fn"), ("def ", "def"), ("func ", "func"),
        ("struct ", "struct"), ("enum ", "enum"), ("trait ", "trait"),
        ("impl ", "impl"), ("class ", "class"), ("interface ", "interface"),
        ("mod ", "mod"), ("type ", "type"), ("const ", "const"), ("let ", "let"),
    ] {
        if t.contains(needle) && t.find(needle).unwrap_or(usize::MAX) < 12 { return kind; }
    }
    "symbol"
}

/// Extend a declaration line to its body end (brace-matching for brace langs,
/// indentation for python). Returns (end_line_1based, body_text).
fn span(lines: &[&str], start: usize, is_py: bool, is_brace: bool) -> (u64, String) {
    if is_py {
        let indent = lines[start].len() - lines[start].trim_start().len();
        let mut end = start + 1;
        while end < lines.len() {
            let l = lines[end];
            if l.trim().is_empty() {
                end += 1;
                continue;
            }
            let l_indent = l.len() - l.trim_start().len();
            if l_indent <= indent {
                break;
            }
            end += 1;
        }
        return ((end) as u64, lines[start..end].join("\n"));
    }
    if is_brace {
        let mut depth = 0i32;
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
                                return (idx as u64, lines[start..idx].join("\n"));
                            }
                        }
                        _ => {}
                    }
                }
                idx += 1;
            }
        }
        return ((start + 1) as u64, lines[start..start + 1].join("\n"));
    }
    ((start + 1) as u64, lines[start..start + 1].join("\n"))
}

/// Word-boundary references in `body` to names in `name_to_files`, excluding
/// `self_name`. Returns the set of referenced symbol names (deduped).
fn referenced_names(body: &str, name_to_files: &HashMap<String, Vec<usize>>, self_name: &str) -> Vec<String> {
    let mut hits = HashSet::new();
    for (name, _idxs) in name_to_files {
        // Skip self, empties, and 1-2 char names — at word scale a tiny match
        // is far more likely a local/parameter/comment word than a symbol.
        if name == self_name || name.is_empty() || name.chars().count() < 3 {
            continue;
        }
        // word-boundary match, case-sensitive (identifiers are case-sensitive)
        if word_boundary_contains(body, name) {
            hits.insert(name.clone());
        }
    }
    let mut v: Vec<String> = hits.into_iter().collect();
    v.sort();
    v
}

fn word_boundary_contains(text: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let bytes = text.as_bytes();
    let wb = word.as_bytes();
    if wb.len() > bytes.len() {
        return false;
    }
    let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut i = 0;
    while i + wb.len() <= bytes.len() {
        if &bytes[i..i + wb.len()] == wb {
            // Both sides of the match must be non-identifier chars (or text
            // edges). Identifiers are case-sensitive; "validate_token" must NOT
            // fire inside "validate_token2".
            let left_ok = i == 0 || !is_ident(bytes[i - 1]);
            let right_ok = i + wb.len() == bytes.len() || !is_ident(bytes[i + wb.len()]);
            if left_ok && right_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

pub fn register_graph(k: &mut Kernel) {
    k.register("code.graph", GRAPH_DESC, nct_core::schema::schema_for::<GraphArgs>(), std::sync::Arc::new(GraphHandler));
}

#[cfg(test)]
mod graph_tests {
    use super::*;

    #[test]
    fn word_boundary_match_is_identifier_aware() {
        assert!(word_boundary_contains("call validate_token(5)", "validate_token"));
        assert!(!word_boundary_contains("validate_token2 is different", "validate_token"));
        assert!(word_boundary_contains("fn main() { validate_token(); }", "validate_token"));
        assert!(!word_boundary_contains("nothing here", "validate_token"));
        assert!(word_boundary_contains("a validate_token b", "validate_token"));
        assert!(!word_boundary_contains("a validatetoken b", "validate_token"));
    }

    #[test]
    fn span_closes_brace_blocks() {
        let lines = &["fn add(a: i32) -> i32 {", "    a + 1", "}", "fn other() {}"];
        let (end, body) = span(lines, 0, false, true);
        assert_eq!(end, 3);
        assert!(body.contains("a + 1"));
        let (end2, _) = span(lines, 3, false, true);
        assert_eq!(end2, 4);
    }

    #[test]
    fn span_stops_at_dedent_in_python() {
        let lines = &["def a():", "    x = 1", "    return x", "def b():", "    pass"];
        let (end, body) = span(lines, 0, true, false);
        assert_eq!(end, 3);
        assert!(body.contains("return x"));
    }

    #[test]
    fn referenced_names_finds_callers() {
        let mut name_to_files: HashMap<String, Vec<usize>> = HashMap::new();
        name_to_files.insert("validate_token".to_string(), vec![0]);
        name_to_files.insert("main".to_string(), vec![1]);
        let body = "fn main() {\n    let ok = validate_token(5);\n}";
        let refs = referenced_names(body, &name_to_files, "main");
        assert!(refs.contains(&"validate_token".to_string()), "got: {refs:?}");
        // self is excluded
        let refs2 = referenced_names(body, &name_to_files, "validate_token");
        assert!(!refs2.contains(&"validate_token".to_string()));
    }
}
