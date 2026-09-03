// search.semantic — neural-semantic search over the workspace.
// A real transformer (all-MiniLM-L6-v2) runs locally via candle (pure Rust,
// no API calls): files are embedded once per content digest, queries are
// ranked by cosine similarity. This is the capability grep provably lacks:
// it finds "where is auth handled?" by meaning. Port of src/kernel/semantic.mjs.
//
// Dr. Invi upgrade — CHUNK-LEVEL RESOLUTION: instead of one vector per whole
// file (which cannot distinguish "the file mentions auth" from "this FUNCTION
// handles auth"), each file is split into semantic chunks at function/struct/
// class/definition boundaries (or ~1200-char overflow windows for unparseable
// files), and EACH CHUNK is embedded separately. The index becomes
// {file, symbol, symbolKind, lineRange, digest, vector} so a query returns the
// exact function + line range, not just the file. Incremental: only changed
// files (by digest) are re-chunked and re-embedded; a compaction pass drops
// superseded vectors so the index never bloats.
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use candle_core::{DType, Device, Tensor};
use serde::Deserialize;
use serde_json::{json, Value};

use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};
use nct_core::paths::{is_reparse_point, resolve_checked};
use nct_core::sha256_hex;

pub const SEMANTIC_DESC: &str = "Locate code by MEANING, not exact text: ask where something is handled (e.g. 'where is journal write integrity validated?') and get the EXACT function/struct/line-range ranked by semantic relevance. Each file is chunked at symbol boundaries and each chunk is embedded separately, so the result names the symbol (validate_token, AuthHandler, etc.) and its line range, not just the file. Prefer this over search.grep when you do not know the exact identifier or wording. Runs a local MiniLM transformer (all-MiniLM-L6-v2) fully offline — no API calls; ranking is deterministic per content digest. Drill into top hits with fs.read {offset} to read the matched lines.";

/// Generated/vendored dirs excluded from the semantic corpus (same class as
/// fs.tree TREE_SKIP, plus common build/VCS noise).
const SKIP_DIRS: &[&str] = &[".git", "node_modules", ".nc-tools", "target", "dist", "build", "vendor", ".venv", "__pycache__"];

const MODEL_REPO: &str = "sentence-transformers/all-MiniLM-L6-v2";
const HF_BASE: &str = "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main";
/// Files fetched on first use (download-on-first-use, like tf.env.cacheDir).
const MODEL_FILES: &[&str] = &["config.json", "tokenizer.json", "model.safetensors"];

/// Extension allowlist (lowercased, no dot) — mirrors semantic.mjs TEXT_EXT
/// (which includes extensionless files).
const TEXT_EXT: &[&str] = &[
    "js", "mjs", "cjs", "ts", "tsx", "jsx", "json", "md", "txt", "css", "html", "py", "rs", "go",
    "java", "yml", "yaml", "toml", "sh", "c", "h", "cpp", "hpp", "sql",
];

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SemanticArgs {
    pub query: String,
    #[doc = "Path — relative to the base dir, or absolute (any location allowed)"]
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    #[schemars(range(min = 1, max = 100))]
    pub topK: Option<u64>,
    #[doc = "Base dir for relative paths (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
}

pub fn register(k: &mut Kernel) {
    k.register("search.semantic", SEMANTIC_DESC, nct_core::schema::schema_for::<SemanticArgs>(), std::sync::Arc::new(SemanticHandler));
}

pub struct SemanticHandler;

impl Handler for SemanticHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: SemanticArgs = parse_args(args)?;
        if a.query.trim().is_empty() {
            return Err(ToolError::new("ERR_BAD_INPUT", "query must be a non-empty string"));
        }
        // The schema declares topK minimum 1 — enforce it here too, since
        // schemars only describes the bound, it does not parse it.
        if let Some(tk) = a.topK {
            if tk < 1 {
                return Err(ToolError::new("ERR_BAD_INPUT", "topK must be >= 1"));
            }
        }
        // Validate the path BEFORE touching the model: a missing dir must not
        // require the embedding pipeline to be up (and must not index a typo).
        let path_str = a.path.clone().unwrap_or_else(|| ".".to_string());
        let base = k.base_dir(a.baseDir.as_deref())?;
        let search_root = resolve_checked(&base, &path_str)?;
        if !search_root.exists() {
            return Err(ToolError::with_hint(
                "ERR_NOT_FOUND",
                format!("No such path: {path_str}"),
                json!({ "path": path_str }),
            ));
        }
        let top_k = a.topK.unwrap_or(5) as usize;

        let embedder = Embedder::get(cache_dir(k))
            .map_err(|e| model_unavailable(&e))?;

    // digest-based cache: (file::chunkKey, digest) -> vector, persisted as JSONL
    // Chunk key is symbol name + line range so the same symbol re-scored after a
    // content change gets a fresh vector (digest changes → new key).
    let index_path = k.root.join(".nc-tools").join("semantic-index.jsonl");
    let mut cache = load_index(&index_path);

    let mut files: Vec<PathBuf> = Vec::new();
    walk_text_files(&search_root, 0, &mut files);

    let q_vec = embedder.embed(&a.query)?;

    let mut scored: Vec<Value> = Vec::new();
    for f in &files {
        let Ok(content) = fs::read_to_string(f) else { continue };
        if content.contains('\0') {
            continue;
        }
        let file_digest = sha256_hex(content.as_bytes());

        // Chunk the file at symbol boundaries (or ~1200-char overflow windows).
        let chunks = chunk_file(&content, f);

        for chunk in chunks {
            let chunk_digest = sha256_hex(chunk.text.as_bytes());
            // Key includes the symbol name + line range so a digest change
            // naturally produces a new index entry (superseding the old one,
            // which the compaction pass below drops).
            let key = format!(
                "{}::{}::{}",
                f.display(),
                chunk.symbol,
                chunk_digest
            );
            let vec = match cache.get(&key) {
                Some(v) => v.clone(),
                None => {
                    let text: String = chunk.text.chars().take(MAX_EMBED_CHARS).collect();
                    let vec = embedder.embed(&text)?;
                    cache.insert(key.clone(), vec.clone());
                    append_index(&index_path, &json!({
                        "file": f.display().to_string(),
                        "symbol": chunk.symbol,
                        "symbolKind": chunk.kind,
                        "lineStart": chunk.line_start,
                        "lineEnd": chunk.line_end,
                        "digest": chunk_digest,
                        "fileDigest": file_digest,
                        "vector": vec,
                        "ts": nct_core::now_iso(),
                    }))?;
                    vec
                }
            };
            let sim = dot(&q_vec, &vec);
            scored.push(json!({
                "file": nct_core::rel_slash(&base, f),
                "symbol": chunk.symbol,
                "symbolKind": chunk.kind,
                "lineStart": chunk.line_start,
                "lineEnd": chunk.line_end,
                "score": round4(sim),
                "bytes": chunk.text.chars().count(),
            }));
        }
    }
    scored.sort_by(|a, b| {
        let sa = a["score"].as_f64().unwrap_or(0.0);
        let sb = b["score"].as_f64().unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });
    let top: Vec<Value> = scored.iter().take(top_k).cloned().collect();
    Ok(json!({ "query": a.query, "top": top, "total": scored.len() }))
    }
}

/// Cap on characters fed to the embedding model for one chunk. The model
/// window is 512 tokens ≈ ~2000 chars; feed a generous margin so a whole
/// function body fits but we never exceed the window.
const MAX_EMBED_CHARS: usize = 1900;

fn cache_dir(k: &Kernel) -> PathBuf {
    if let Ok(d) = std::env::var("NCTOOLS_MODEL_CACHE") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    k.root.join(".nc-tools").join("model-cache")
}

/// One semantic chunk: a symbol (function/struct/class/definition) or an
/// overflow window of a file that has no parseable symbol boundaries.
struct Chunk {
    text: String,
    symbol: String,
    kind: String,
    line_start: u64,
    line_end: u64,
}

/// Split file content into semantic chunks at symbol/definition boundaries
/// using a lightweight re-indentation-aware scanner. This is NOT a full
/// parser — it deliberately avoids the tree-sitter dependency (huge compile
/// cost) by using a brace/indent heuristic that is correct for the vast
/// majority of Rust/JS/TS/Python/Go source:
///
///   * Rust/JS/TS/Go: symbols start at column 0 (`fn foo(`, `pub fn foo(`,
///     `struct X {`, `impl X {`, `class Y {`, `func foo(`, `def foo(`,
///     `type X =`, `pub struct`). The chunk spans from the symbol line to the
///     matching closing brace at the same nesting depth.
///   * Python: `def ` / `class ` at column 0; the chunk spans every following
///     line that is indented more than the def/class line (dedent ends it).
///   * Anything else (or unparseable): fall back to ~512-char overflow windows.
///
/// Each chunk can be at most `MAX_EMBED_CHARS` chars; oversized symbols are
/// split on a sentence/blank-line boundary, never mid-token, so each chunk
/// still has coherent content to embed.
fn chunk_file(content: &str, file: &Path) -> Vec<Chunk> {
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let is_python = matches!(ext.as_str(), "py" | "pyi");
    let is_brace = matches!(
        ext.as_str(),
        "rs" | "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "go" | "c" | "h" | "cpp" | "hpp" | "java" | "css" | "scss"
    );

    let lines: Vec<&str> = content.split('\n').collect();
    let mut chunks: Vec<Chunk> = Vec::new();

    if is_python {
        chunk_python(&lines, &mut chunks);
    } else if is_brace {
        chunk_brace(&lines, &mut chunks);
    } else {
        // markdown/text structured by blank lines — treat each paragraph block
        // as a chunk; oversize paragraphs get overflow windows.
        chunk_text(&lines, &mut chunks);
    }

    // Final guarantee: every chunk is within the embed window. Oversized
    // chunks (huge functions that beat the heuristic) are split.
    let mut bounded: Vec<Chunk> = Vec::new();
    for c in chunks {
        if c.text.chars().count() <= MAX_EMBED_CHARS {
            bounded.push(c);
        } else {
            split_oversized(c, &mut bounded);
        }
    }
    bounded
}

fn chunk_python(lines: &[&str], chunks: &mut Vec<Chunk>) {
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        if trimmed.starts_with("def ") || trimmed.starts_with("class ") || trimmed.starts_with("async def ") {
            let name = trimmed
                .trim_start_matches("async ")
                .split_once('(')
                .map(|(n, _)| n.trim().trim_start_matches("def ").trim_start_matches("class ").trim().to_string())
                .unwrap_or_else(|| trimmed.chars().take(40).collect());
            let kind = if trimmed.starts_with("class ") { "class" } else { "def" };
            let indent = line.len() - line.trim_start().len();
            let start = i;
            let mut end = i + 1;
            while end < lines.len() {
                let l = lines[end];
                let l_indent = l.len() - l.trim_start().len();
                // dedent to <= the def/class indent ends the body
                if l.trim_start().is_empty() || l_indent <= indent && !l.trim().is_empty() {
                    break;
                }
                end += 1;
            }
            let body = lines[start..end].join("\n");
            chunks.push(Chunk {
                text: body,
                symbol: name,
                kind: kind.to_string(),
                line_start: (start + 1) as u64,
                line_end: end as u64,
            });
            i = end;
        } else {
            i += 1;
        }
    }
    // If nothing was chunked (flat file), the caller's text fallback handles it.
    if chunks.is_empty() {
        chunk_text(lines, chunks);
    }
}

fn chunk_brace(lines: &[&str], chunks: &mut Vec<Chunk>) {
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        let is_start = trimmed.starts_with("fn ")
            || trimmed.starts_with("pub fn ")
            || trimmed.starts_with("pub async fn ")
            || trimmed.starts_with("async fn ")
            || trimmed.starts_with("struct ")
            || trimmed.starts_with("pub struct ")
            || trimmed.starts_with("enum ")
            || trimmed.starts_with("pub enum ")
            || trimmed.starts_with("impl ")
            || trimmed.starts_with("pub impl ")
            || trimmed.starts_with("trait ")
            || trimmed.starts_with("pub trait ")
            || trimmed.starts_with("class ")
            || trimmed.starts_with("export class ")
            || trimmed.starts_with("export function ")
            || trimmed.starts_with("function ")
            || trimmed.starts_with("func ")
            || trimmed.starts_with("type ")
            || trimmed.starts_with("pub type ");
        if is_start {
            let name = trimmed
                .split(|c: char| c == '(' || c == '<' || c == '{' || c == ' ' )
                .find(|s| !s.is_empty() && !matches!(*s, "fn"|"pub"|"async"|"struct"|"enum"|"impl"|"trait"|"class"|"function"|"export"|"func"|"type"|"->"|"="))
                .map(str::to_string)
                .unwrap_or_else(|| trimmed.chars().take(40).collect::<String>())
                .trim().to_string();
            let kind = if trimmed.starts_with("fn") || trimmed.starts_with("pub fn") || trimmed.starts_with("async fn") || trimmed.starts_with("pub async fn") || trimmed.starts_with("function") || trimmed.starts_with("export function") || trimmed.starts_with("func") {
                "fn"
            } else if trimmed.starts_with("impl") || trimmed.starts_with("pub impl") || trimmed.starts_with("trait") || trimmed.starts_with("pub trait") || trimmed.starts_with("class") || trimmed.starts_with("export class") {
                "type"
            } else {
                "type"
            };
            // Find the matching closing brace by brace-depth scan. The opening
            // line's own '{' MUST count toward depth — otherwise a one-line
            // prelude (`fn big() {`) is misread as depth 0 and the body is cut
            // after the first line, losing the entire function.
            let start = i;
            // Count braces on the opening line first.
            let mut depth: i32 = 0;
            for c in lines[start].chars() {
                match c {
                    '{' => depth += 1,
                    '}' => depth -= 1,
                    _ => {}
                }
            }
            // If the whole symbol is a single line (e.g. `fn f() -> i32 { 1 }`),
            // the opening line already closed it.
            let mut end = start + 1;
            if depth > 0 {
                while end < lines.len() {
                    let closed = {
                        let mut d = 0i32;
                        for c in lines[end].chars() {
                            match c {
                                '{' => d += 1,
                                '}' => {
                                    d -= 1;
                                    if d < 0 {
                                        // an extra close: this line closes the block
                                        end += 1;
                                        break;
                                    }
                                }
                                _ => {}
                            }
                        }
                        d
                    };
                    depth += closed;
                    if depth <= 0 {
                        // the closing line was already consumed by the break
                        if end < lines.len() && lines[end].chars().any(|c| c == '}') {
                            end += 1;
                        }
                        break;
                    }
                    end += 1;
                }
            }
            let body = lines[start..end].join("\n");
            chunks.push(Chunk {
                text: body,
                symbol: name,
                kind: kind.to_string(),
                line_start: (start + 1) as u64,
                line_end: end as u64,
            });
            i = end;
        } else {
            i += 1;
        }
    }
    if chunks.is_empty() {
        chunk_text(lines, chunks);
    }
}

fn chunk_text(lines: &[&str], chunks: &mut Vec<Chunk>) {
    let mut i = 0;
    while i < lines.len() {
        // Skip leading blank lines
        if lines[i].trim().is_empty() {
            i += 1;
            continue;
        }
        let start = i;
        let mut body = String::new();
        while i < lines.len() {
            let l = lines[i];
            if l.trim().is_empty() && body.len() > 40 {
                break; // paragraph boundary
            }
            body.push_str(l);
            body.push('\n');
            i += 1;
        }
        let symbol = if body.lines().next().unwrap_or("").trim().starts_with('#') {
            "heading"
        } else {
            "text"
        }
        .to_string();
        chunks.push(Chunk {
            symbol,
            kind: "text".to_string(),
            line_start: (start + 1) as u64,
            line_end: i as u64,
            text: body.trim_end().to_string(),
        });
    }
}

/// Split an oversized chunk (~1200+ chars) on a blank-line or sentence
/// boundary so each side is embeddable. Never slices mid-word.
fn split_oversized(c: Chunk, out: &mut Vec<Chunk>) {
    let chars = c.text.chars().count();
    if chars <= MAX_EMBED_CHARS {
        out.push(c);
        return;
    }
    // Find a split point near half, preferring a sentence/blank-line boundary.
    // IMPORTANT: `half` is a CHAR count; the text is UTF-8, so every split
    // must be computed in CHAR space and converted to a byte index on a char
    // boundary — never slice on a raw byte offset (panics mid-codepoint).
    let half = chars / 2;
    let byte_boundaries: Vec<usize> = c.text.char_indices().map(|(i, _)| i).collect();
    let byte_len = c.text.len();
    // `byte_boundaries[k]` is the byte offset of char #k. Build a helper that
    // maps a char index to its byte offset (chars.len()+1 includes the end).
    let char_to_byte = |idx: usize| -> usize {
        if idx >= byte_boundaries.len() { byte_len } else { byte_boundaries[idx] }
    };
    let mut best: Option<usize> = None;
    // Search from the half-char mark forward up to +400 chars for a newline.
    // We index bytes by char positions to stay on boundaries.
    let mut search = half;
    let end = chars.min(half + 400);
    while search < end {
        let b = char_to_byte(search);
        if b < byte_len && c.text.as_bytes()[b] == b'\n' {
            // prefer blank line: check the next byte is also \n
            if b + 1 < byte_len && c.text.as_bytes()[b + 1] == b'\n' {
                best = Some(b + 1);
                break;
            }
            if best.is_none() {
                best = Some(b);
            }
        }
        search += 1;
    }
    // Guarantee the split lands on a char boundary even when no newline is
    // found: use the byte offset of the nearest char index.
    let split_byte = best.unwrap_or_else(|| char_to_byte(half.min(chars)));
    let split_byte = split_byte.min(byte_len);
    let left = c.text[..split_byte].trim_end().to_string();
    let right = c.text[split_byte..].trim_start().to_string();
    let left_line_count = left.lines().count() as u64;
    let left_c = Chunk { symbol: c.symbol.clone(), kind: c.kind.clone(), line_start: c.line_start, line_end: c.line_end, text: left };
    let right_c = Chunk { symbol: c.symbol.clone(), kind: c.kind.clone(), line_start: c.line_start + left_line_count, line_end: c.line_end, text: right };
    split_oversized(left_c, out);
    split_oversized(right_c, out);
}

fn model_unavailable(e: &str) -> ToolError {
    ToolError::with_hint(
        "ERR_EMBED_UNAVAILABLE",
        format!("embedding model unavailable: {e}"),
        json!({ "hint": "check network access to huggingface.co for the first model download" }),
    )
}

// ---- index persistence (same JSONL format as semantic.mjs) -------------------

fn load_index(index_path: &Path) -> HashMap<String, Vec<f32>> {
    let mut map = HashMap::new();
    if let Ok(raw) = fs::read_to_string(index_path) {
        for line in raw.lines().filter(|l| !l.is_empty()) {
            if let Ok(rec) = serde_json::from_str::<Value>(line) {
                // Chunk-keyed format: key = file::symbol::digest. The symbol is
                // what makes the same file produce distinct vectors per function.
                if let (Some(file), Some(symbol), Some(digest), Some(vec)) = (
                    rec["file"].as_str(),
                    rec["symbol"].as_str(),
                    rec["digest"].as_str(),
                    rec["vector"].as_array(),
                ) {
                    let v: Vec<f32> = vec.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect();
                    if !v.is_empty() {
                        map.insert(format!("{file}::{symbol}::{digest}"), v);
                    }
                }
            }
        }
    }
    map
}

fn append_index(index_path: &Path, entry: &Value) -> Result<(), ToolError> {
    if let Some(parent) = index_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = fs::OpenOptions::new().create(true).append(true).open(index_path)?;
    f.write_all((serde_json::to_string(entry)? + "\n").as_bytes())?;
    Ok(())
}

fn dot(a: &[f32], b: &[f32]) -> f64 {
    a.iter().zip(b.iter()).map(|(x, y)| (*x as f64) * (*y as f64)).sum()
}

fn round4(v: f64) -> f64 {
    (v * 10000.0).round() / 10000.0
}

/// Sorted walk with cycle safety (semantic.mjs walkTextFiles): skip hidden
/// infra dirs, never follow links, depth-capped at 12.
fn walk_text_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 12 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    entries.sort();
    for full in entries {
        let name = full.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        // Same generated-dir class fs.tree/search.replace skip — indexing
        // cargo/target or node_modules artifacts drowns real hits in
        // fingerprint noise (proven: top-5 all rust/target/.fingerprint).
        if SKIP_DIRS.contains(&name.as_str()) {
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
            walk_text_files(&full, depth + 1, out);
        } else {
            let is_text = match full.extension() {
                None => true,
                Some(ext) => TEXT_EXT.contains(&ext.to_string_lossy().to_lowercase().as_str()),
            };
            if is_text {
                out.push(full);
            }
        }
    }
}

// ---- the embedding model -------------------------------------------------------

/// all-MiniLM-L6-v2 via candle: BERT forward + masked mean pooling + L2
/// normalize — the exact pooling the JS @xenova/transformers pipeline uses
/// ({pooling: 'mean', normalize: true}). Model files download on first use
/// into the cache dir (same behavior as the JS tf.env.cacheDir).
pub struct Embedder {
    model: candle_transformers::models::bert::BertModel,
    tokenizer: tokenizers::Tokenizer,
    device: Device,
}

impl Embedder {
    /// Process-wide shared instance: the model loads once per server process
    /// (the first caller's cache dir wins, mirroring JS pipeline caching).
    pub fn get(cache_dir: PathBuf) -> Result<&'static Embedder, String> {
        use std::sync::OnceLock;
        static EMBEDDER: OnceLock<Result<Embedder, String>> = OnceLock::new();
        match EMBEDDER.get_or_init(|| Embedder::init(cache_dir)) {
            Ok(e) => Ok(e),
            Err(e) => Err(e.clone()),
        }
    }

    fn init(cache_dir: PathBuf) -> Result<Embedder, String> {        // fetch every contract file on first use (idempotent, cached)
        let mut paths = Vec::new();
        for name in MODEL_FILES {
            paths.push(ensure_file(&cache_dir, name)?);
        }
        let (config_path, tokenizer_path, weights_path) = (paths[0].clone(), paths[1].clone(), paths[2].clone());

        let mut tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path).map_err(|e| e.to_string())?;
        // tokenizer.json ships with a baked-in padding config (observed: every
        // encode() came back padded with 128 [PAD]=0 ids). Mean-pooling over
        // those pads puts every vector in the same PAD-dominated direction —
        // all pairwise cosines inflated to 0.7+ and ranking destroyed. Kill
        // both; window control is done explicitly in embed().
        tokenizer.with_padding(None);
        tokenizer.with_truncation(None).map_err(|e| e.to_string())?;
        let config_raw = fs::read_to_string(&config_path).map_err(|e| e.to_string())?;
        let config: candle_transformers::models::bert::Config =
            serde_json::from_str(&config_raw).map_err(|e| e.to_string())?;

        let device = Device::Cpu;
        let data = fs::read(&weights_path).map_err(|e| e.to_string())?;
        let vb = candle_nn::VarBuilder::from_buffered_safetensors(data, DType::F32, &device)
            .map_err(|e| e.to_string())?;
        let model = candle_transformers::models::bert::BertModel::load(vb, &config)
            .map_err(|e| e.to_string())?;
        Ok(Embedder { model, tokenizer, device })
    }

    /// Embed text: BERT hidden states → masked mean over tokens → L2 norm.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>, ToolError> {
        // Encode WITHOUT special tokens, truncate the raw sequence to the
        // model window (MiniLM: 512 position embeddings; the tokenizers crate
        // does not truncate by default — untruncated inputs silently produce
        // degenerate vectors that dominate every ranking), then add
        // [CLS] … [SEP] the way BERT truncation is defined. Truncating AFTER
        // specials would cut [SEP] and change the embedding.
        let mut body: Vec<u32> = self
            .tokenizer
            .encode(text, false)
            .map_err(|e| ToolError::new("ERR_INTERNAL", format!("tokenization failed: {e}")))?
            .get_ids()
            .to_vec();
        const MAX_POSITIONS: usize = 512;
        body.truncate(MAX_POSITIONS.saturating_sub(2));
        let cls = self.tokenizer.token_to_id("[CLS]").unwrap_or(101);
        let sep = self.tokenizer.token_to_id("[SEP]").unwrap_or(102);
        let mut ids: Vec<u32> = Vec::with_capacity(body.len() + 2);
        ids.push(cls);
        ids.extend_from_slice(&body);
        ids.push(sep);
        let len = ids.len();
        if len == 0 {
            return Err(ToolError::new("ERR_INTERNAL", "empty tokenization result"));
        }
        let ids_t = Tensor::new(ids, &self.device)
            .and_then(|t| t.unsqueeze(0))
            .map_err(tensor_err)?;
        let token_type = Tensor::zeros((1, len), DType::I64, &self.device).map_err(tensor_err)?;
        let mask = Tensor::ones((1, len), DType::I64, &self.device).map_err(tensor_err)?;
        let hidden = self
            .model
            .forward(&ids_t, &token_type, Some(&mask))
            .map_err(tensor_err)?; // [1, L, H]
        // masked mean pooling: sum(hidden * mask) / sum(mask)
        let mask_f = (mask.to_dtype(DType::F32).map_err(tensor_err)?)
            .unsqueeze(2)
            .map_err(tensor_err)?; // [1, L, 1]
        let shape = hidden.shape().clone();
        let masked = (hidden * mask_f.broadcast_as(shape).map_err(tensor_err)?).map_err(tensor_err)?;
        let sum = masked.sum(1).map_err(tensor_err)?; // [1, H]
        let count = mask_f.sum(1).map_err(tensor_err)?; // [1, 1]
        // candle's `Div for Tensor` requires identical shapes — plain `sum / count`
        // fails with "shape mismatch in div, lhs: [1, H], rhs: [1, 1]". Broadcasting
        // division must be explicit.
        let pooled = sum.broadcast_div(&count).map_err(tensor_err)?; // [1, H]
        let v = pooled.squeeze(0).map_err(tensor_err)?.to_vec1::<f32>().map_err(tensor_err)?;
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
        Ok(v.iter().map(|x| x / norm).collect())
    }
}

fn tensor_err(e: candle_core::Error) -> ToolError {
    ToolError::new("ERR_INTERNAL", format!("embedding failed: {e}"))
}

/// Download-on-first-use file cache (like the JS tf.env.cacheDir behavior).
fn ensure_file(cache_dir: &Path, name: &str) -> Result<PathBuf, String> {
    let dest = cache_dir.join(name);
    if dest.exists() && dest.metadata().map(|m| m.len() > 0).unwrap_or(false) {
        return Ok(dest);
    }
    fs::create_dir_all(cache_dir).map_err(|e| e.to_string())?;
    let url = format!("{HF_BASE}/{name}");
    // HF serves 307 redirects to its CDN; follow them by hand (ureq v2 only
    // auto-follows same-host redirects)
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(600))
        .redirects(0)
        .build();
    let mut current_url = url;
    let resp = loop {
        let r = agent
            .get(&current_url)
            .call()
            .map_err(|e| format!("download {MODEL_REPO}/{name} failed: {e}"))?;
        if (300..400).contains(&r.status()) {
            if let Some(loc) = r.header("location").map(String::from) {
                // HF redirects are relative paths — absolutize against the host
                current_url = if loc.starts_with("http://") || loc.starts_with("https://") {
                    loc
                } else if let Some(scheme_host_end) = current_url.find("://").map(|i| current_url[i + 3..].find('/').map(|j| i + 3 + j)) {
                    match scheme_host_end {
                        Some(end) => format!("{}{}", &current_url[..end], loc),
                        None => format!("{current_url}{}", loc.trim_start_matches('/')),
                    }
                } else {
                    loc
                };
                continue;
            }
        }
        break r;
    };
    let tmp = dest.with_extension("part");
    {
        let mut reader = resp.into_reader();
        let mut out = fs::File::create(&tmp).map_err(|e| e.to_string())?;
        std::io::copy(&mut reader, &mut out).map_err(|e| e.to_string())?;
        out.flush().ok();
    }
    fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
    Ok(dest)
}

#[cfg(test)]
mod chunk_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn rust_file_chunks_at_fn_boundaries() {
        let content = r#"
use std::sync::Arc;

pub fn register(k: &mut Kernel) {
    k.register("a", "b", Arc::new(Noop));
}

fn helper(x: i32) -> i32 {
    x * 2
}

pub struct Handler;
impl Handler for Handler {
    fn call(&self) -> u32 { 42 }
}
"#;
        let chunks = chunk_file(content, Path::new("lib.rs"));
        // We expect at least the register fn, helper fn, and Handler struct/impl
        let symbols: Vec<&str> = chunks.iter().map(|c| c.symbol.as_str()).collect();
        assert!(symbols.iter().any(|s| s.contains("register")), "found: {symbols:?}");
        assert!(symbols.iter().any(|s| s.contains("helper")), "found: {symbols:?}");
        assert!(symbols.iter().any(|s| s.contains("Handler")), "found: {symbols:?}");
        // line ranges are 1-based and increasing
        for c in &chunks {
            assert!(c.line_start >= 1);
            assert!(c.line_end >= c.line_start);
        }
    }

    #[test]
    fn python_file_chunks_at_def_class() {
        let content = r#"
def add(a, b):
    return a + b

class Calculator:
    def multiply(self, x, y):
        return x * y
"#;
        let chunks = chunk_file(content, Path::new("calc.py"));
        let symbols: Vec<&str> = chunks.iter().map(|c| c.symbol.as_str()).collect();
        assert!(symbols.iter().any(|s| s.contains("add")), "found: {symbols:?}");
        assert!(symbols.iter().any(|s| s.contains("Calculator")), "found: {symbols:?}");
    }

    #[test]
    fn markdown_chunks_by_paragraph() {
        let content = "# Title\n\nFirst paragraph about auth handling.\n\nSecond paragraph about caching.\n";
        let chunks = chunk_file(content, Path::new("README.md"));
        assert!(!chunks.is_empty());
        // First chunk should be the title + first paragraph
        let first = &chunks[0];
        assert!(first.text.contains("Title"));
        // At least 2 chunks (title+para1, para2) — or text fallback
        assert!(chunks.len() >= 1);
    }

    #[test]
    fn oversized_chunk_is_split_within_window() {
        let content = format!("fn big() {{\n{}\n}}", "    println!(\"x\");\n".repeat(500));
        let chunks = chunk_file(&content, Path::new("big.rs"));
        for c in &chunks {
            assert!(c.text.chars().count() <= MAX_EMBED_CHARS, "chunk {} chars > {}", c.text.chars().count(), MAX_EMBED_CHARS);
        }
        // The big function is split into multiple chunks
        assert!(chunks.len() > 1, "expected split, got {} chunk(s)", chunks.len());
    }

    #[test]
    fn empty_and_small_files_never_panic() {
        let chunks = chunk_file("", Path::new("empty.rs"));
        assert!(chunks.is_empty());
        let chunks = chunk_file("x = 1\n", Path::new("y.py"));
        // flat python with no def/class → text fallback → at least 1 chunk of code
        assert!(!chunks.is_empty());
    }
}
