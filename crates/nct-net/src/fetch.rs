// net.fetch — read a URL as clean markdown for an agent: SSRF-guarded, cached
// with ETag revalidation, markdown-content-negotiation aware, journaled.
// net.robots — robots.txt isAllowed + sitemaps + llms.txt discovery.
// net.search — keyless federated search: engine fan-out → RRF fusion → local
// MiniLM rerank (nct-semantic). The reranker is what turns concatenated
// engine results into an ordered answer; it runs offline in pure Rust.
use std::sync::Arc;

use serde_json::{json, Value};

use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};

use crate::cache;
use crate::engines;
use crate::extract;
use crate::httpx;
use crate::robots;
use crate::ssrf;

pub const FETCH_DESC: &str = "Read a web page as CLEAN MARKDOWN for an agent: main-content extraction discards nav/ads/boilerplate, returns title, links, token estimate, and extraction confidence. SSRF-guarded by default (private/loopback targets refused; allowPrivate to reach internal hosts), ETag-revalidated cache (set refresh to force), and Accept: text/markdown negotiation for sites that serve it. Prefer over net.http whenever you want page CONTENT, not raw protocol bytes.";
pub const ROBOTS_DESC: &str = "Check what a site allows an agent to fetch: robots.txt isAllowed for a URL (RFC 9309 longest-match), crawl-delay, sitemap URLs, and llms.txt discovery (the site's own curated agent index). Fetch this before crawling or fetching many pages from one host.";
pub const SEARCH_DESC: &str = "Search the WEB keylessly: fans the query across free engines (DuckDuckGo Lite, Mojeek, Wikipedia, Hacker News), fuses results with reciprocal-rank fusion, then RERANKS them locally with a MiniLM transformer (same offline neural stack as search.semantic) so the best answer ranks first without any API key. Returns title, url, snippet, engines, score. Use for anything outside the workspace; pair with net.fetch to read the top hits.";

pub fn register(k: &mut Kernel) {
    k.register("net.fetch", FETCH_DESC, nct_core::schema::schema_for::<FetchArgs>(), Arc::new(FetchHandler));
    k.register("net.robots", ROBOTS_DESC, nct_core::schema::schema_for::<RobotsArgs>(), Arc::new(RobotsHandler));
    k.register("net.search", SEARCH_DESC, nct_core::schema::schema_for::<SearchArgs>(), Arc::new(SearchHandler));
}

// ---- net.fetch ---------------------------------------------------------------

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FetchArgs {
    pub url: String,
    #[doc = "Allow private/loopback targets (for local dev servers); default blocks them"]
    #[serde(default)]
    pub allowPrivate: Option<bool>,
    #[serde(default)]
    #[schemars(range(min = 100, max = 120000))]
    pub timeoutMs: Option<u64>,
    #[doc = "Bypass the cache and revalidate with the server"]
    #[serde(default)]
    pub refresh: Option<bool>,
    #[doc = "Estimate token cap for the markdown (truncates output)"]
    #[serde(default)]
    #[schemars(range(min = 100, max = 200000))]
    pub maxTokens: Option<u64>,
}

pub struct FetchHandler;

impl Handler for FetchHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: FetchArgs = parse_args(args)?;
        let allow_private = a.allowPrivate.unwrap_or(false);
        let timeout = a.timeoutMs.unwrap_or(30_000);
        let started = std::time::Instant::now();

        let url = ssrf::parse_http_url(&a.url)?;
        if !allow_private {
            ssrf::assert_public(url.as_str())?;
        }

        let ttl = k.cfg.limits.net_fetch_ttl_ms;
        // load cached validators even on refresh — that's what makes the
        // conditional GET (If-None-Match → 304) possible; refresh only skips
        // the freshness short-circuit
        let cached = cache::load(&k.root, &a.url);
        if let Some(entry) = &cached {
            if !a.refresh.unwrap_or(false) && cache_fresh(entry, ttl) {
                return Ok(cached_result(entry, "hit", started, a.maxTokens));
            }
        }

        // markdown negotiation: sites on Cloudflare (and llms.txt-aware hosts)
        // serve converted markdown when the agent asks for it
        let mut opts = httpx::FetchOpts::get(url.clone())
            .guard(!allow_private)
            .timeout(timeout)
            .max_body(k.cfg.limits.net_max_body)
            .header("Accept", "text/markdown, text/html;q=0.9, */*;q=0.8");
        if let Some(entry) = &cached {
            opts = opts.revalidate(entry.etag.clone(), entry.last_modified.clone());
        }
        let outcome = httpx::fetch(opts)?;

        // 304 Not Modified → cached copy is still the truth
        if outcome.status == 304 {
            let entry = cached.expect("304 implies a cached entry with validators");
            let _ = cache::store(&k.root, &cache::CacheEntry {
                url: entry.url.clone(),
                etag: entry.etag.clone(),
                last_modified: entry.last_modified.clone(),
                content_type: entry.content_type.clone(),
                body: entry.body.clone(),
                fetched_at: nct_core::now_iso(),
            });
            return Ok(cached_result(&entry, "revalidated", started, a.maxTokens));
        }

        let content_type = httpx::header_value(&outcome, "content-type").unwrap_or_default();
        let etag = httpx::header_value(&outcome, "etag");
        let last_modified = httpx::header_value(&outcome, "last-modified");
        let content_signals = httpx::header_value(&outcome, "content-signal");

        let cache_state = if cached.is_some() { "refreshed" } else { "miss" };
        let (markdown, title, links, confidence, source) = if content_type.starts_with("text/markdown") {
            (outcome.body.clone(), None, Vec::new(), 1.0, "markdown-negotiated".to_string())
        } else if content_type.starts_with("text/html") || content_type.contains("html") {
            let ex = extract::extract(&outcome.body, &outcome.final_url)?;
            (ex.markdown, ex.title, ex.links, ex.confidence, "extracted".to_string())
        } else if content_type.starts_with("application/json") {
            (format!("```json\n{}\n```", outcome.body.trim()), None, Vec::new(), 1.0, "json".to_string())
        } else if content_type.starts_with("text/plain") {
            (outcome.body.clone(), None, Vec::new(), 1.0, "text".to_string())
        } else {
            return Err(ToolError::with_hint(
                "ERR_NET",
                format!("unsupported content-type: {content_type}"),
                json!({ "url": a.url, "status": outcome.status, "hint": "use net.http for binary/unknown content types" }),
            ));
        };

        let truncated = if let Some(cap) = a.maxTokens {
            let max_chars = (cap as usize).saturating_mul(4);
            markdown.len() > max_chars
        } else {
            false
        };
        let body_store = if let Some(cap) = a.maxTokens {
            take_chars(&markdown, (cap as usize).saturating_mul(4))
        } else {
            markdown.clone()
        };

        let entry = cache::CacheEntry {
            url: a.url.clone(),
            etag,
            last_modified,
            content_type: content_type.clone(),
            body: body_store.clone(),
            fetched_at: nct_core::now_iso(),
        };
        cache::store(&k.root, &entry)?;

        let tokens = estimate_tokens(&body_store);
        let result = json!({
            "url": a.url,
            "finalUrl": outcome.final_url.to_string(),
            "status": outcome.status,
            "ok": outcome.ok,
            "contentType": content_type,
            "source": source,
            "title": title,
            "markdown": body_store,
            "markdownTruncated": truncated,
            "links": links.iter().take(200).collect::<Vec<_>>(),
            "tokens": tokens,
            "extractionConfidence": confidence,
            "contentSignals": content_signals,
            "redirects": outcome.redirects,
            "cache": cache_state,
            "fetchedAt": entry.fetched_at,
            "contentHash": nct_core::sha256_hex(body_store.as_bytes())[..16].to_string(),
            "fetchMs": outcome.duration_ms,
        });
        let _ = k.journal.append("net.fetch", json!({
            "url": a.url,
            "finalUrl": outcome.final_url.to_string(),
            "status": outcome.status,
            "cache": cache_state,
            "hash": result["contentHash"],
            "tokens": tokens,
            "sid": k.sid,
        }));
        Ok(result)
    }
}

fn cached_result(entry: &cache::CacheEntry, state: &str, started: std::time::Instant, max_tokens: Option<u64>) -> Value {
    let body = match max_tokens {
        Some(cap) => take_chars(&entry.body, (cap as usize).saturating_mul(4)),
        None => entry.body.clone(),
    };
    json!({
        "url": entry.url,
        "finalUrl": entry.url,
        "status": Value::Null,
        "ok": true,
        "contentType": entry.content_type,
        "source": "cache",
        "markdown": body,
        "links": [],
        "tokens": estimate_tokens(&body),
        "cache": state,
        "fetchedAt": entry.fetched_at,
        "contentHash": nct_core::sha256_hex(entry.body.as_bytes())[..16].to_string(),
        "durationMs": started.elapsed().as_millis() as u64,
    })
}

fn cache_fresh(entry: &cache::CacheEntry, ttl_ms: u64) -> bool {
    if ttl_ms == 0 {
        return false;
    }
    let fetched = match chrono::DateTime::parse_from_rfc3339(&entry.fetched_at) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let age_ms = (chrono::Utc::now() - fetched.with_timezone(&chrono::Utc)).num_milliseconds();
    age_ms >= 0 && (age_ms as u64) < ttl_ms
}

fn take_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        s.chars().take(max_chars).collect()
    }
}

/// ~4 chars/token heuristic (OpenAI-style estimate); good enough for agents
/// to budget context, cheap to compute, deterministic.
fn estimate_tokens(s: &str) -> u64 {
    (s.chars().count() as f64 / 4.0).ceil() as u64
}

// ---- net.robots --------------------------------------------------------------

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RobotsArgs {
    pub url: String,
    #[doc = "Allow private/loopback targets (for local dev servers)"]
    #[serde(default)]
    pub allowPrivate: Option<bool>,
    #[serde(default)]
    #[schemars(range(min = 100, max = 120000))]
    pub timeoutMs: Option<u64>,
}

pub struct RobotsHandler;

impl Handler for RobotsHandler {
    fn call(&self, _k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: RobotsArgs = parse_args(args)?;
        let allow_private = a.allowPrivate.unwrap_or(false);
        let timeout = a.timeoutMs.unwrap_or(15_000);
        let url = ssrf::parse_http_url(&a.url)?;
        let root_url = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
        if let Some(port) = url.port() {
            // host_str excludes the port; rebuild with it
            let host = url.host_str().unwrap_or_default();
            let root_url = format!("{}://{}:{}", url.scheme(), host, port);
            return robots_result(&root_url, url.clone(), allow_private, timeout);
        }
        robots_result(&root_url, url.clone(), allow_private, timeout)
    }
}

fn robots_result(root_url: &str, target: url::Url, allow_private: bool, timeout: u64) -> Result<Value, ToolError> {
    let robots_url = format!("{root_url}/robots.txt");
    let outcome = httpx::fetch(
        httpx::FetchOpts::get(ssrf::parse_http_url(&robots_url)?)
            .guard(!allow_private)
            .timeout(timeout)
            .max_body(512_000),
    )?;
    let robots = if outcome.ok {
        robots::Robots::parse(&outcome.body)
    } else {
        // no robots.txt → all allowed (RFC 9309)
        robots::Robots::default()
    };

    // llms.txt probe: the site's curated agent index (llmstxt.org v2)
    let llms_url = format!("{root_url}/llms.txt");
    let llms = httpx::fetch(
        httpx::FetchOpts::get(ssrf::parse_http_url(&llms_url)?)
            .guard(!allow_private)
            .timeout(timeout)
            .max_body(256_000),
    );
    let llms_txt = match llms {
        Ok(r) if r.ok && !r.body.trim().is_empty() => Some(json!({
            "found": true,
            "url": llms_url,
            "bytes": r.body.len(),
            "content": r.body.trim(),
        })),
        _ => Some(json!({ "found": false, "url": llms_url })),
    };

    Ok(robots::report(&robots, engines::AGENT_TOKEN, &target, llms_txt, None))
}

// ---- net.search --------------------------------------------------------------

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchArgs {
    pub query: String,
    #[doc = "Comma-separated engine names (hn, wikipedia, ddg, mojeek) or auto for all"]
    #[serde(default)]
    pub engines: Option<String>,
    #[serde(default)]
    #[schemars(range(min = 1, max = 25))]
    pub maxResults: Option<u64>,
    #[doc = "Local neural rerank after fusion (default true; falls back to fusion order if the model is unavailable)"]
    #[serde(default)]
    pub rerank: Option<bool>,
    #[serde(default)]
    #[schemars(range(min = 100, max = 30000))]
    pub timeoutMs: Option<u64>,
}

pub struct SearchHandler;

impl Handler for SearchHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: SearchArgs = parse_args(args)?;
        let query = a.query.trim().to_string();
        if query.is_empty() {
            return Err(ToolError::new("ERR_BAD_INPUT", "query must be a non-empty string"));
        }
        let limit = a.maxResults.unwrap_or(10) as usize;
        let fetch_limit = (limit * 2).max(10);
        let timeout = a.timeoutMs.unwrap_or(k.cfg.limits.net_engine_timeout_ms);

        let selected: Vec<&engines::EngineDef> = match a.engines.as_deref() {
            None | Some("auto") | Some("") => engines::ENGINES.iter().collect(),
            Some(list) => {
                let mut out = Vec::new();
                for name in list.split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()) {
                    match engines::engine_by_name(&name) {
                        Some(e) => out.push(e),
                        None => {
                            return Err(ToolError::with_hint(
                                "ERR_BAD_INPUT",
                                format!("unknown engine: {name}"),
                                json!({ "known": engines::ENGINES.iter().map(|e| e.name).collect::<Vec<_>>() }),
                            ))
                        }
                    }
                }
                out
            }
        };

        let started = std::time::Instant::now();
        let politeness = k.cfg.limits.net_search_politeness_ms;
        let mut engine_reports: Vec<Value> = Vec::new();
        let mut lists: Vec<(String, Vec<engines::RawResult>)> = Vec::new();
        for engine in selected {
            match engines::run_engine(engine, &query, fetch_limit, timeout, politeness) {
                Ok(results) => {
                    let n = results.len();
                    lists.push((engine.name.to_string(), results));
                    engine_reports.push(json!({ "name": engine.name, "status": "ok", "results": n }));
                }
                Err(e) => engine_reports.push(json!({
                    "name": engine.name, "status": "error",
                    "error": { "code": e.code, "message": e.message },
                })),
            }
        }
        if lists.is_empty() {
            return Err(ToolError::with_hint(
                "ERR_ENGINE",
                "all search engines failed",
                json!({ "engines": engine_reports, "hint": "check network access; engines are also rate-limit protected" }),
            ));
        }

        let fused = engines::fuse(&lists);
        let rerank_enabled = a.rerank.unwrap_or(true);

        // Local neural rerank: query embedding vs title+snippet embedding.
        // Falls back to fusion order when the model can't load (honest flag).
        let (results, reranked) = if rerank_enabled {
            match nct_semantic::Embedder::get(model_cache_dir(k)) {
                Ok(embedder) => {
                    let q = embedder.embed(&query)?;
                    // rerank text = title + snippet + HOST. The host is a real
                    // ranking signal a human uses (rust-lang.org outranks a
                    // 2011 blog comment on the same words); without it, HN
                    // comment pages tie with official sites.
                    let mut scored: Vec<(f64, usize, engines::FusedResult)> = fused
                        .into_iter()
                        .enumerate()
                        .map(|(i, f)| {
                            let host = url::Url::parse(&f.url)
                                .ok()
                                .and_then(|u| u.host_str().map(|h| h.replace("www.", "")))
                                .unwrap_or_default();
                            let text = format!(
                                "{}. {} {}",
                                f.title,
                                f.snippet,
                                host.replace('-', " ").replace('.', " "),
                            );
                            let v = embedder.embed(&text).unwrap_or_default();
                            let cos = if v.len() == q.len() { dot(&q, &v) } else { 0.0 };
                            (cos, i, f)
                        })
                        .collect();
                    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1)));
                    let out: Vec<Value> = scored
                        .into_iter()
                        .take(limit)
                        .map(|(cos, _, f)| fused_json(&f, Some(round4(cos))))
                        .collect();
                    (out, true)
                }
                Err(_) => (fused.iter().take(limit).map(|f| fused_json(f, None)).collect(), false),
            }
        } else {
            (fused.iter().take(limit).map(|f| fused_json(f, None)).collect(), false)
        };

        Ok(json!({
            "query": query,
            "engines": engine_reports,
            "results": results,
            "fusion": "rrf",
            "reranked": reranked,
            "reranker": reranked.then_some("all-MiniLM-L6-v2 (local)"),
            "durationMs": started.elapsed().as_millis() as u64,
        }))
    }
}

fn fused_json(f: &engines::FusedResult, score: Option<f64>) -> Value {
    let mut v = json!({
        "title": f.title,
        "url": f.url,
        "snippet": f.snippet,
        "engine": f.engine,
        "engines": f.engines,
        "rrf": round4(f.rrf),
    });
    if let Some(s) = score {
        v["score"] = json!(s);
    }
    v
}

fn model_cache_dir(k: &Kernel) -> std::path::PathBuf {
    if let Ok(d) = std::env::var("NCTOOLS_MODEL_CACHE") {
        if !d.is_empty() {
            return std::path::PathBuf::from(d);
        }
    }
    k.root.join(".nc-tools").join("model-cache")
}

fn dot(a: &[f32], b: &[f32]) -> f64 {
    a.iter().zip(b.iter()).map(|(x, y)| (*x as f64) * (*y as f64)).sum()
}

fn round4(v: f64) -> f64 {
    (v * 10000.0).round() / 10000.0
}
