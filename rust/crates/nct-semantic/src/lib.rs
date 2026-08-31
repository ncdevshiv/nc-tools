// search.semantic — neural-semantic search over the workspace.
// A real transformer (all-MiniLM-L6-v2) runs locally via candle (pure Rust,
// no API calls): files are embedded once per content digest, queries are
// ranked by cosine similarity. This is the capability grep provably lacks:
// it finds "where is auth handled?" by meaning. Port of src/kernel/semantic.mjs.
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

pub const SEMANTIC_DESC: &str = "Neural-semantic search: ranks files by meaning-match to a natural-language query. Uses a local MiniLM transformer (all-MiniLM-L6-v2) — no API calls. Returns file, score, bytes.";

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
        // Validate the path BEFORE touching the model: a missing dir must not
        // require the embedding pipeline to be up (and must not index a typo).
        let path_str = a.path.clone().unwrap_or_else(|| ".".to_string());
        let base = resolve_checked(&k.root, &path_str)?;
        if !base.exists() {
            return Err(ToolError::with_hint(
                "ERR_NOT_FOUND",
                format!("No such path: {path_str}"),
                json!({ "path": path_str }),
            ));
        }
        let top_k = a.topK.unwrap_or(5) as usize;

        let embedder = Embedder::get(cache_dir(k))
            .map_err(|e| model_unavailable(&e))?;

        // digest-based cache: (file, sha256) -> vector, persisted as JSONL
        let index_path = k.root.join(".nc-tools").join("semantic-index.jsonl");
        let mut cache = load_index(&index_path);

        let mut files: Vec<PathBuf> = Vec::new();
        walk_text_files(&base, 0, &mut files);

        let q_vec = embedder.embed(&a.query)?;

        let mut scored: Vec<Value> = Vec::with_capacity(files.len());
        for f in &files {
            let Ok(content) = fs::read_to_string(f) else { continue };
            if content.contains('\0') {
                continue;
            }
            let digest = sha256_hex(content.as_bytes());
            let key = format!("{}::{}", f.display(), digest);
            let vec = match cache.get(&key) {
                Some(v) => v.clone(),
                None => {
                    let text: String = content.chars().take(32_000).collect();
                    let vec = embedder.embed(&text)?;
                    cache.insert(key.clone(), vec.clone());
                    append_index(&index_path, &json!({
                        "file": f.display().to_string(),
                        "digest": digest,
                        "vector": vec,
                        "ts": nct_core::now_iso(),
                    }))?;
                    vec
                }
            };
            let sim = dot(&q_vec, &vec);
            scored.push(json!({
                "file": nct_core::rel_slash(&k.root, f),
                "score": round4(sim),
                "bytes": content.chars().count(),
            }));
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

fn cache_dir(k: &Kernel) -> PathBuf {
    if let Ok(d) = std::env::var("NCTOOLS_MODEL_CACHE") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    k.root.join(".nc-tools").join("model-cache")
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
                if let (Some(file), Some(digest), Some(vec)) = (
                    rec["file"].as_str(),
                    rec["digest"].as_str(),
                    rec["vector"].as_array(),
                ) {
                    let v: Vec<f32> = vec.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect();
                    if !v.is_empty() {
                        map.insert(format!("{file}::{digest}"), v);
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

    fn init(cache_dir: PathBuf) -> Result<Embedder, String> {
        // fetch every contract file on first use (idempotent, cached)
        let mut paths = Vec::new();
        for name in MODEL_FILES {
            paths.push(ensure_file(&cache_dir, name)?);
        }
        let (config_path, tokenizer_path, weights_path) = (paths[0].clone(), paths[1].clone(), paths[2].clone());

        let tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path).map_err(|e| e.to_string())?;
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
        let enc = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| ToolError::new("ERR_INTERNAL", format!("tokenization failed: {e}")))?;
        let ids: Vec<u32> = enc.get_ids().to_vec();
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
        let pooled = (sum / count).map_err(tensor_err)?;
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
