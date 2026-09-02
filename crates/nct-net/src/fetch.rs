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

use crate::authority::AuthorityStore;
use crate::cache;
use crate::routing;
use crate::engines;
use crate::extract;
use crate::httpx;
use crate::robots;
use crate::ssrf;

pub const FETCH_DESC: &str = "Read a web page as CLEAN MARKDOWN for an agent: main-content extraction discards nav/ads/boilerplate, returns title, links, token estimate, and extraction confidence. SSRF-guarded by default (private/loopback targets refused; allowPrivate to reach internal hosts), ETag-revalidated cache (set refresh to force), Accept: text/markdown negotiation, and automatic headless re-render through the system browser when a JS-heavy page extracts as empty. Prefer over net.http whenever you want page CONTENT, not raw protocol bytes.";
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
    #[doc = "Re-render JS-heavy pages through the system browser's headless mode when extraction looks empty (default true; no-op when no browser is installed)"]
    #[serde(default)]
    pub render: Option<bool>,
}

pub struct FetchHandler;

impl Handler for FetchHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        fetch_content(k, args)
    }
}

/// The full fetch pipeline, shared by net.fetch, net.cite, and net.verify:
/// SSRF guard → cache (hit / revalidate) → markdown negotiation → extract →
/// (render escalation) → cache store → journal provenance.
pub fn fetch_content(k: &Kernel, args: &Value) -> Result<Value, ToolError> {
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
            // Render escalation: JS-heavy pages come back as near-empty shells
            // with near-zero confidence. Re-render through the system browser's
            // headless mode before giving the agent a useless extraction.
            let text_len = ex.markdown.chars().count();
            let weak = text_len < 400 && ex.confidence < 0.5;
            let rendered = if weak && a.render.unwrap_or(true) {
                match crate::render::render_dom(outcome.final_url.as_str(), allow_private, timeout) {
                    Ok(dom) => {
                        let ex2 = extract::extract(&dom, &outcome.final_url)?;
                        if ex2.markdown.chars().count() > text_len * 3 {
                            Some((ex2.markdown, ex2.title, ex2.links, ex2.confidence, "rendered".to_string()))
                        } else {
                            None // render didn't help — keep the honest low-confidence result
                        }
                    }
                    Err(_) => None, // no browser / render failed — degrade, don't fail
                }
            } else {
                None
            };
            match rendered {
                Some((md, t, l, c, s)) => (md, t, l, c, s),
                None => (ex.markdown, ex.title, ex.links, ex.confidence, "extracted".to_string()),
            }
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
        // authority learning: a successful read proves this domain served the
        // agent — future searches rank it a little higher
        let host = outcome.final_url.host_str().unwrap_or_default().to_string();
        if !host.is_empty() {
            let mut auth = AuthorityStore::load(&k.root);
            auth.bump(&host, 1);
        }
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

// ---- net.search (W-Net-2b): intent routing → parallel fan-out → RRF fusion →
// MiniLM rerank with authority boost → optional progressive disclosure.

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchArgs {
    pub query: String,
    #[doc = "Comma-separated engine names, \"all\" for every healthy source, \"auto\" (default) for intent-routed selection"]
    #[serde(default)]
    pub engines: Option<String>,
    #[doc = "Force an intent: general | news | howto | academic | package (default auto-classified locally)"]
    #[serde(default)]
    pub intent: Option<String>,
    #[serde(default)]
    #[schemars(range(min = 1, max = 25))]
    pub maxResults: Option<u64>,
    #[doc = "Local neural rerank after fusion, with authority boost (default true; falls back to fusion order if the model is unavailable)"]
    #[serde(default)]
    pub rerank: Option<bool>,
    #[serde(default)]
    #[schemars(range(min = 100, max = 30000))]
    pub timeoutMs: Option<u64>,
    #[doc = "Auto-read the top N results (0-3) and attach their markdown (progressive disclosure; costs one fetch each)"]
    #[serde(default)]
    #[schemars(range(min = 0, max = 3))]
    pub fetchTop: Option<u64>,
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
        let started = std::time::Instant::now();

        // ---- intent routing --------------------------------------------------
        let embedder = nct_semantic::Embedder::get(model_cache_dir(k)).ok();
        let (intent, intent_method) = match a.intent.as_deref().map(|s| s.to_lowercase()) {
            Some(s) if !s.is_empty() && s != "auto" => {
                let parsed = match s.as_str() {
                    "news" => engines::Intent::News,
                    "howto" | "how-to" | "qa" => engines::Intent::HowTo,
                    "academic" => engines::Intent::Academic,
                    "package" => engines::Intent::Package,
                    _ => engines::Intent::General,
                };
                (parsed, "forced")
            }
            _ => {
                if a.engines.as_deref().map(|s| s != "auto" && !s.is_empty()).unwrap_or(false) {
                    // explicit engine list skips routing
                    (engines::Intent::General, "bypassed")
                } else {
                    routing::classify(&query, embedder)
                }
            }
        };

        // ---- source selection --------------------------------------------------
        // "all" = every healthy source (no routing); "auto"/none = intent-routed;
        // anything else = explicit comma list.
        let explicit = a.engines.as_deref().map(|s| !s.is_empty() && s != "auto" && s != "all").unwrap_or(false);
        let want_all = a.engines.as_deref() == Some("all");
        let selected: Vec<&engines::EngineDef> = if explicit {
            let mut out = Vec::new();
            for name in a.engines.as_deref().unwrap().split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()) {
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
        } else if want_all {
            // everything healthy: full fan-out, no routing
            engines::ENGINES
                .iter()
                .filter(|e| !matches!(e.kind, engines::EngineKind::Searxng) && engines::engine_key_present(e) && engines::engine_healthy(e.name))
                .collect()
        } else {
            // auto: intent-routed priority list, filtered to usable engines
            routing::sources_for(intent)
                .iter()
                .filter_map(|name| engines::engine_by_name(name))
                .filter(|e| engines::engine_healthy(e.name))
                .filter(|e| e.key_env.is_none() || engines::engine_key_present(e))
                .collect()
        };
        if selected.is_empty() {
            return Err(ToolError::with_hint(
                "ERR_ENGINE",
                "no usable search sources for this query",
                json!({ "intent": format!("{intent:?}"), "hint": "all routed sources are unhealthy or unkeyed; pass engines:\"all\" or set NCTOOLS_BRAVE_KEY" }),
            ));
        }
        let planned: Vec<String> = selected.iter().map(|e| e.name.to_string()).collect();

        // ---- parallel fan-out --------------------------------------------------
        let politeness = k.cfg.limits.net_search_politeness_ms;
        let parallel = k.cfg.limits.net_parallel_fanout;
        let query_shared = query.clone();
        let mut handles = Vec::new();
        for engine in selected {
            let q = query_shared.clone();
            let e_name = engine.name.to_string();
            let kind = engine.kind;
            let fetch_limit = fetch_limit;
            let timeout = timeout;
            let politeness = politeness;
            // EngineKind is Copy; the worker reconstructs a static-name EngineDef
            // (all engine names are 'static literals in ENGINES) — no leak, no
            // clone of the registry.
            if parallel {
                handles.push(std::thread::spawn(move || -> (String, Result<Vec<engines::RawResult>, ToolError>) {
                    let r = engines::run_engine_named(&e_name, kind, &q, fetch_limit, timeout, politeness);
                    (e_name, r)
                }));
            } else {
                let r = engines::run_engine_named(&e_name, kind, &query_shared, fetch_limit, timeout, politeness);
                handles.push(std::thread::spawn(move || (e_name, r)));
            }
        }
        let mut engine_reports: Vec<Value> = Vec::new();
        let mut lists: Vec<(String, Vec<engines::RawResult>)> = Vec::new();
        let mut source_dropped: Vec<Value> = Vec::new();
        for h in handles {
            let (name, result) = match h.join() {
                Ok(x) => x,
                Err(_) => {
                    engine_reports.push(json!({ "name": "?", "status": "error", "error": { "code": "ERR_ENGINE", "message": "worker thread panicked" } }));
                    continue;
                }
            };
            match result {
                Ok(results) => {
                    let n = results.len();
                    engines::engine_mark(&name, true, None);
                    lists.push((name.clone(), results));
                    engine_reports.push(json!({ "name": name, "status": "ok", "results": n }));
                }
                Err(e) => {
                    engines::engine_mark(&name, false, Some(e.message.clone()));
                    engine_reports.push(json!({
                        "name": name, "status": "error",
                        "error": { "code": e.code, "message": e.message },
                    }));
                }
            }
        }
        // searxng fleet: one additional parallel-style attempt (fleet has its
        // own rotation and health), included as a regular fused source
        if !explicit || want_all {
            if routing::sources_for(intent).contains(&"searxng") || want_all {
                match crate::fleet::search(&query, fetch_limit, timeout) {
                    Ok((member, results)) => {
                        lists.push(("searxng".to_string(), results.clone()));
                        engine_reports.push(json!({ "name": "searxng", "status": "ok", "results": results.len(), "member": member }));
                    }
                    Err(e) => engine_reports.push(json!({
                        "name": "searxng", "status": "error",
                        "error": { "code": e.code, "message": e.message },
                    })),
                }
            }
        }
        if lists.is_empty() {
            return Err(ToolError::with_hint(
                "ERR_ENGINE",
                "all search sources failed",
                json!({ "engines": engine_reports, "hint": "check network access; keyed engines need their env key; the searxng fleet refreshes every 30 min" }),
            ));
        }

        // ---- fusion + rerank + authority --------------------------------------
        // ---- per-source relevance validation -------------------------------
        // Keyless sources degrade SILENTLY (Bing RSS served Roblox/India-Post
        // results for a rust query — live-observed 2026-09-02). The local
        // embedder scores every source's top result against the query and
        // drops sources whose output is semantically unrelated. Without a
        // model, skip validation (fusion still dedupes).
        let validated_lists = if let Some(embedder) = embedder {
            let q_vec = embedder.embed(&query)?;
            let mut kept: Vec<(String, Vec<engines::RawResult>)> = Vec::new();
            let mut dropped: Vec<Value> = Vec::new();
            for (name, results) in lists.iter() {
                let probe = results.first().map(|r| format!("{} {}", r.title, r.snippet));
                let score = match probe {
                    Some(text) if !text.trim().is_empty() => {
                        let v = embedder.embed(&text).unwrap_or_default();
                        if v.len() == q_vec.len() { dot(&q_vec, &v) } else { 0.0 }
                    }
                    _ => 0.0,
                };
                if results.is_empty() || score >= 0.25 {
                    kept.push((name.clone(), results.clone()));
                } else {
                    dropped.push(json!({ "engine": name, "topScore": round4(score), "reason": "results unrelated to query (source degraded)" }));
                }
            }
            source_dropped = dropped;
            kept
        } else {
            Vec::new()
        };
        let lists = if validated_lists.is_empty() { lists } else { validated_lists };
        let fused = engines::fuse(&lists);
        let rerank_enabled = a.rerank.unwrap_or(true);
        let authority = AuthorityStore::load(&k.root);
        let authority_influence = k.cfg.limits.net_authority_influence;

        let (results, reranked) = if rerank_enabled {
            match embedder {
                Some(embedder) => {
                    let q = embedder.embed(&query)?;
                    let query_words: std::collections::HashSet<String> = query
                        .to_lowercase()
                        .split(|c: char| !c.is_alphanumeric())
                        .filter(|w| w.len() > 2)
                        .map(String::from)
                        .collect();
                    let mut scored: Vec<(f64, usize, engines::FusedResult)> = fused
                        .into_iter()
                        .enumerate()
                        .map(|(i, f)| {
                            let host = url::Url::parse(&f.url)
                                .ok()
                                .and_then(|u| u.host_str().map(|h| h.replace("www.", "")))
                                .unwrap_or_default();
                            let auth = authority.score(&host);
                            let text = format!(
                                "{}. {} {}",
                                f.title,
                                f.snippet,
                                host.replace('-', " ").replace('.', " "),
                            );
                            let v = embedder.embed(&text).unwrap_or_default();
                            let cos = if v.len() == q.len() { dot(&q, &v) } else { 0.0 };
                            // URL↔query token overlap: official pages usually carry
                            // the query's distinctive words in host/path ("async-book",
                            // "pep-0634"). Bounded small — a tie-breaker, not a ruler.
                            let url_words: std::collections::HashSet<String> = url::Url::parse(&f.url)
                                .map(|u| {
                                    format!("{}{}", u.host_str().unwrap_or_default(), u.path())
                                        .to_lowercase()
                                        .split(|c: char| !c.is_alphanumeric())
                                        .filter(|w| w.len() > 2)
                                        .map(String::from)
                                        .collect()
                                })
                                .unwrap_or_default();
                            let overlap = if query_words.is_empty() {
                                0.0
                            } else {
                                query_words.iter().filter(|w| url_words.contains(*w)).count() as f64
                                    / query_words.len() as f64
                            };
                            let score = (cos + authority_influence * auth + 0.15 * overlap).min(1.0);
                            (score, i, f, cos)
                        })
                        .map(|(score, i, f, _)| (score, i, f))
                        .collect();
                    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1)));
                    let out: Vec<Value> = scored
                        .into_iter()
                        .take(limit)
                        .map(|(score, _, f)| {
                            let host = url::Url::parse(&f.url).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default();
                            let mut v = fused_json(&f, Some(round4(score)));
                            v["authority"] = json!(round4(authority.score(&host)));
                            v
                        })
                        .collect();
                    (out, true)
                }
                None => {
                    // no model: fusion order + authority bump (still better than raw concat)
                    let mut with_auth: Vec<(f64, usize, engines::FusedResult)> = fused
                        .into_iter()
                        .enumerate()
                        .map(|(i, f)| {
                            let host = url::Url::parse(&f.url).ok().and_then(|u| u.host_str().map(|h| h.replace("www.", ""))).unwrap_or_default();
                            (f.rrf + authority_influence * authority.score(&host), i, f)
                        })
                        .collect();
                    with_auth.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1)));
                    let out: Vec<Value> = with_auth
                        .into_iter()
                        .take(limit)
                        .map(|(score, _, f)| fused_json(&f, Some(round4(score))))
                        .collect();
                    (out, false)
                }
            }
        } else {
            (fused.iter().take(limit).map(|f| fused_json(f, None)).collect(), false)
        };

        // ---- progressive disclosure --------------------------------------------
        let fetch_top = a.fetchTop.unwrap_or(0).min(3) as usize;
        let mut final_results = results;
        if fetch_top > 0 {
            for i in 0..fetch_top.min(final_results.len()) {
                let url = final_results[i]["url"].as_str().unwrap_or_default().to_string();
                if url.is_empty() {
                    continue;
                }
                let fetch_args = json!({ "url": url });
                match fetch_content(k, &fetch_args) {
                    Ok(f) => {
                        let md = f["markdown"].as_str().unwrap_or_default().chars().take(4000).collect::<String>();
                        final_results[i]["content"] = json!(md);
                    }
                    Err(e) => {
                        final_results[i]["contentError"] = json!(e.message);
                    }
                }
            }
        }

        let duration = started.elapsed().as_millis() as u64;
        let _ = k.journal.append("net.search", json!({
            "query": query,
            "intent": format!("{intent:?}"),
            "intentMethod": intent_method,
            "planned": planned,
            "engaged": engine_reports.iter().filter(|e| e["status"] == "ok").map(|e| e["name"].clone()).collect::<Vec<_>>(),
            "results": final_results.len(),
            "reranked": reranked,
            "authorityDomains": authority.total_tracked(),
            "durationMs": duration,
            "sid": k.sid,
        }));

        Ok(json!({
            "query": query,
            "intent": format!("{intent:?}").to_lowercase(),
            "intentMethod": intent_method,
            "planned": planned,
            "engines": engine_reports,
            "sourcesDropped": source_dropped,
            "results": final_results,
            "fusion": "rrf",
            "reranked": reranked,
            "reranker": reranked.then_some("all-MiniLM-L6-v2 (local, +authority)"),
            "authorityDomains": authority.total_tracked(),
            "durationMs": duration,
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
