// net.search engine layer: keyless engine adapters defined as data (endpoint
// template + parse strategy), pure parse functions per engine (unit-tested
// offline against committed fixtures), RRF fusion + URL dedupe. A failing
// engine degrades to a per-engine error entry — it never fails the call.
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use nct_core::errors::ToolError;

pub const AGENT_TOKEN: &str = "nctools";

// ---- engine definitions -----------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EngineKind {
    /// HN Algolia JSON API
    Hn,
    /// Wikipedia search API (JSON)
    Wikipedia,
    /// DuckDuckGo Lite (HTML)
    DdgLite,
    /// Mojeek (HTML)
    Mojeek,
    /// Brave Search API (GET, X-Subscription-Token)
    Brave,
    /// Tavily search API (POST JSON)
    Tavily,
    /// Serper Google search API (POST JSON)
    Serper,
    /// Bing web search RSS (keyless)
    BingRss,
    /// Google News RSS (keyless)
    GnewsRss,
    /// StackExchange API (keyless)
    StackExchange,
    /// OpenAlex academic works API (keyless)
    OpenAlex,
    /// arXiv Atom API (keyless)
    Arxiv,
    /// npm registry search (keyless)
    Npm,
    /// crates.io search API (keyless)
    Crates,
    /// SearXNG public instance (runtime-discovered fleet; run via the fleet
    /// path in SearchHandler, not through run_engine)
    Searxng,
}

pub struct EngineDef {
    pub name: &'static str,
    pub kind: EngineKind,
    /// Env var carrying the API key (keyed engines only run when set)
    pub key_env: Option<&'static str>,
    /// Primary intent class this source serves (routing hint, not a hard gate)
    pub intent: Intent,
}

/// Query intent classes for local routing (W-Net-2b).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Intent {
    General,
    News,
    HowTo,
    Academic,
    Package,
}

pub const ENGINES: &[EngineDef] = &[
    EngineDef {
        name: "hn",
        kind: EngineKind::Hn,
        key_env: None,
        intent: Intent::General,
    },
    EngineDef {
        name: "wikipedia",
        kind: EngineKind::Wikipedia,
        key_env: None,
        intent: Intent::General,
    },
    EngineDef {
        name: "ddg",
        kind: EngineKind::DdgLite,
        key_env: None,
        intent: Intent::General,
    },
    EngineDef {
        name: "mojeek",
        kind: EngineKind::Mojeek,
        key_env: None,
        intent: Intent::General,
    },
    // keyed engines — the recall fix for navigational queries (the keyless
    // HTML scrapers get challenge-walled); they join "auto" only when their
    // env key is present
    EngineDef {
        name: "brave",
        kind: EngineKind::Brave,
        key_env: Some("NCTOOLS_BRAVE_KEY"),
        intent: Intent::General,
    },
    EngineDef {
        name: "tavily",
        kind: EngineKind::Tavily,
        key_env: Some("NCTOOLS_TAVILY_KEY"),
        intent: Intent::General,
    },
    EngineDef {
        name: "serper",
        kind: EngineKind::Serper,
        key_env: Some("NCTOOLS_SERPER_KEY"),
        intent: Intent::General,
    },
    // W-Net-2b keyless source wave — all live-validated 2026-09-02
    EngineDef {
        name: "bing-rss",
        kind: EngineKind::BingRss,
        key_env: None,
        intent: Intent::General,
    },
    EngineDef {
        name: "gnews",
        kind: EngineKind::GnewsRss,
        key_env: None,
        intent: Intent::News,
    },
    EngineDef {
        name: "stackexchange",
        kind: EngineKind::StackExchange,
        key_env: None,
        intent: Intent::HowTo,
    },
    EngineDef {
        name: "openalex",
        kind: EngineKind::OpenAlex,
        key_env: None,
        intent: Intent::Academic,
    },
    EngineDef {
        name: "arxiv",
        kind: EngineKind::Arxiv,
        key_env: None,
        intent: Intent::Academic,
    },
    EngineDef {
        name: "npm",
        kind: EngineKind::Npm,
        key_env: None,
        intent: Intent::Package,
    },
    EngineDef {
        name: "crates",
        kind: EngineKind::Crates,
        key_env: None,
        intent: Intent::Package,
    },
];

pub fn engine_by_name(name: &str) -> Option<&'static EngineDef> {
    ENGINES.iter().find(|e| e.name == name)
}

/// The endpoint for one engine query (GET form-encoded).
pub fn endpoint(engine: &EngineDef, query: &str, limit: usize) -> String {
    endpoint_str(engine.kind, query, limit)
}

fn endpoint_str(kind: EngineKind, query: &str, limit: usize) -> String {
    // Bing RSS degrades on long natural queries (word-fragment matching →
    // dictionaries); shorten to distinctive terms before hitting Bing.
    // Other engines handle long queries natively.
    let q = match kind {
        EngineKind::BingRss => urlencode(crate::query::shorten_query(query).as_str()),
        _ => urlencode(query),
    };
    match kind {
        EngineKind::Hn => {
            format!("https://hn.algolia.com/api/v1/search?query={q}&hitsPerPage={limit}")
        }
        EngineKind::Wikipedia => {
            format!("https://en.wikipedia.org/w/api.php?action=query&list=search&srsearch={q}&format=json&srlimit={limit}&origin=*")
        }
        EngineKind::DdgLite => format!("https://lite.duckduckgo.com/lite/?q={q}"),
        EngineKind::Mojeek => format!("https://www.mojeek.com/search?q={q}"),
        EngineKind::Brave => {
            format!("https://api.search.brave.com/res/v1/web/search?q={q}&count={limit}")
        }
        // POST endpoints: the path is the endpoint; query rides in the body
        EngineKind::Tavily => "https://api.tavily.com/search".to_string(),
        EngineKind::Serper => "https://google.serper.dev/search".to_string(),
        EngineKind::BingRss => {
            format!("https://www.bing.com/search?q={q}&format=rss&count={limit}")
        }
        EngineKind::GnewsRss => {
            format!("https://news.google.com/rss/search?q={q}&hl=en-US&gl=US&ceid=US:en")
        }
        EngineKind::StackExchange => {
            format!("https://api.stackexchange.com/2.3/search/advanced?order=desc&sort=relevance&q={q}&site=stackoverflow&pagesize={limit}&filter=withbody")
        }
        EngineKind::OpenAlex => {
            format!("https://api.openalex.org/works?search={q}&per-page={limit}&mailto=nc-tools@localhost.dev")
        }
        EngineKind::Arxiv => {
            format!("https://export.arxiv.org/api/query?search_query=all:{q}&max_results={limit}&sortBy=relevance")
        }
        EngineKind::Searxng => String::new(), // fleet path builds its own URLs
        EngineKind::Npm => format!("https://registry.npmjs.org/-/v1/search?text={q}&size={limit}"),
        EngineKind::Crates => format!("https://crates.io/api/v1/crates?q={q}&per_page={limit}"),
    }
}

/// Result as the engine produced it (pre-fusion).
#[derive(Debug, Clone)]
pub struct RawResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Parse one engine's response body. Pure: same body → same results (the
/// offline unit tests live at the bottom of this file, fed by fixtures).
pub fn parse(engine: &EngineDef, body: &str, limit: usize) -> Result<Vec<RawResult>, ToolError> {
    parse_by_kind(engine.kind, body, limit)
}

fn parse_by_kind(kind: EngineKind, body: &str, limit: usize) -> Result<Vec<RawResult>, ToolError> {
    let results = match kind {
        EngineKind::Hn => parse_hn(body, limit)?,
        EngineKind::Wikipedia => parse_wikipedia(body, limit)?,
        EngineKind::DdgLite => parse_ddg_lite(body, limit),
        EngineKind::Mojeek => parse_mojeek(body, limit),
        EngineKind::Brave => parse_brave(body, limit)?,
        EngineKind::Tavily => parse_tavily(body, limit)?,
        EngineKind::Serper => parse_serper(body, limit)?,
        EngineKind::BingRss | EngineKind::GnewsRss => super::sources::parse_rss(body, limit),
        EngineKind::StackExchange => super::sources::parse_stackexchange(body, limit)?,
        EngineKind::OpenAlex => super::sources::parse_openalex(body, limit)?,
        EngineKind::Arxiv => super::sources::parse_arxiv(body, limit),
        EngineKind::Searxng => super::sources::parse_searxng(body, limit)?,
        EngineKind::Npm => super::sources::parse_npm(body, limit)?,
        EngineKind::Crates => super::sources::parse_crates(body, limit)?,
    };
    Ok(results.into_iter().take(limit).collect())
}

fn parse_hn(body: &str, limit: usize) -> Result<Vec<RawResult>, ToolError> {
    let v: Value = serde_json::from_str(body).map_err(|e| {
        ToolError::new(
            "ERR_ENGINE",
            format!("hn algolia returned invalid json: {e}"),
        )
    })?;
    let mut out = Vec::new();
    if let Some(hits) = v["hits"].as_array() {
        for hit in hits {
            let title = hit["title"]
                .as_str()
                .or_else(|| hit["story_title"].as_str())
                .unwrap_or_default()
                .trim()
                .to_string();
            let url = hit["url"]
                .as_str()
                .or_else(|| hit["story_url"].as_str())
                .map(String::from)
                .unwrap_or_else(|| {
                    format!(
                        "https://news.ycombinator.com/item?id={}",
                        hit["objectID"].as_str().unwrap_or("")
                    )
                });
            let snippet = hit["story_text"]
                .as_str()
                .or_else(|| hit["comment_text"].as_str())
                .unwrap_or_default();
            if title.is_empty() {
                continue;
            }
            out.push(RawResult {
                title,
                url,
                snippet: strip_html(snippet),
            });
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

fn parse_wikipedia(body: &str, limit: usize) -> Result<Vec<RawResult>, ToolError> {
    let v: Value = serde_json::from_str(body).map_err(|e| {
        ToolError::new(
            "ERR_ENGINE",
            format!("wikipedia returned invalid json: {e}"),
        )
    })?;
    let mut out = Vec::new();
    if let Some(hits) = v["query"]["search"].as_array() {
        for hit in hits {
            let title = hit["title"].as_str().unwrap_or_default().trim().to_string();
            if title.is_empty() {
                continue;
            }
            let url = format!(
                "https://en.wikipedia.org/wiki/{}",
                urlencode(&title.replace(' ', "_"))
            );
            let snippet = strip_html(hit["snippet"].as_str().unwrap_or_default());
            out.push(RawResult {
                title,
                url,
                snippet,
            });
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

/// DDG Lite: table-based results; links may be direct or wrapped in
/// /l/?uddg=<encoded>. Class names have churned before — parse tolerantly by
/// structure (a with href inside the results table), unwrap redirects.
fn parse_ddg_lite(body: &str, limit: usize) -> Vec<RawResult> {
    let doc = scraper::Html::parse_fragment(body);
    let mut out: Vec<RawResult> = Vec::new();
    for a in doc.select(&scraper::Selector::parse("a").unwrap()) {
        let href = a.value().attr("href").unwrap_or_default().to_string();
        let title = a.text().collect::<String>().trim().to_string();
        if title.is_empty() {
            continue;
        }
        let absolute =
            if let Some(uddg) = href.split("uddg=").nth(1).and_then(|s| s.split('&').next()) {
                // /l/?uddg=<encoded> redirect wrapper — unwrap BEFORE treating the
                // href as any other shape (the wrapper is also protocol-relative)
                match urldecode(uddg) {
                    Some(u) => u,
                    None => continue,
                }
            } else if let Some(rest) = href.strip_prefix("//") {
                format!("https:{rest}")
            } else if href.starts_with("http://") || href.starts_with("https://") {
                href.clone()
            } else {
                continue;
            };
        if !absolute.starts_with("http://") && !absolute.starts_with("https://") {
            continue;
        }
        out.push(RawResult {
            title,
            url: absolute,
            snippet: String::new(),
        });
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Mojeek: result links carry class "ob" (title anchors), snippets in <p class="s">.
fn parse_mojeek(body: &str, limit: usize) -> Vec<RawResult> {
    let doc = scraper::Html::parse_fragment(body);
    let title_sel = scraper::Selector::parse("a.ob").unwrap();
    let snippet_sel = scraper::Selector::parse("p.s").unwrap();
    let titles: Vec<(String, String)> = doc
        .select(&title_sel)
        .filter_map(|a| {
            let href = a.value().attr("href").map(String::from)?;
            let title = a.text().collect::<String>().trim().to_string();
            if title.is_empty() {
                None
            } else {
                Some((title, href))
            }
        })
        .collect();
    let snippets: Vec<String> = doc
        .select(&snippet_sel)
        .map(|p| p.text().collect::<String>().trim().to_string())
        .collect();
    titles
        .into_iter()
        .take(limit)
        .enumerate()
        .map(|(i, (title, url))| RawResult {
            title,
            url: if url.starts_with("http") {
                url
            } else {
                format!("https://www.mojeek.com{url}")
            },
            snippet: snippets.get(i).cloned().unwrap_or_default(),
        })
        .collect()
}

fn parse_brave(body: &str, limit: usize) -> Result<Vec<RawResult>, ToolError> {
    let v: Value = serde_json::from_str(body)
        .map_err(|e| ToolError::new("ERR_ENGINE", format!("brave returned invalid json: {e}")))?;
    let mut out = Vec::new();
    if let Some(results) = v["web"]["results"].as_array() {
        for r in results {
            let title = r["title"].as_str().unwrap_or_default().trim().to_string();
            let url = r["url"].as_str().unwrap_or_default().to_string();
            if title.is_empty() || url.is_empty() {
                continue;
            }
            out.push(RawResult {
                title,
                url,
                snippet: r["description"].as_str().unwrap_or_default().to_string(),
            });
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

fn parse_tavily(body: &str, limit: usize) -> Result<Vec<RawResult>, ToolError> {
    let v: Value = serde_json::from_str(body)
        .map_err(|e| ToolError::new("ERR_ENGINE", format!("tavily returned invalid json: {e}")))?;
    let mut out = Vec::new();
    if let Some(results) = v["results"].as_array() {
        for r in results {
            let title = r["title"].as_str().unwrap_or_default().trim().to_string();
            let url = r["url"].as_str().unwrap_or_default().to_string();
            if title.is_empty() || url.is_empty() {
                continue;
            }
            out.push(RawResult {
                title,
                url,
                snippet: r["content"].as_str().unwrap_or_default().to_string(),
            });
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

fn parse_serper(body: &str, limit: usize) -> Result<Vec<RawResult>, ToolError> {
    let v: Value = serde_json::from_str(body)
        .map_err(|e| ToolError::new("ERR_ENGINE", format!("serper returned invalid json: {e}")))?;
    let mut out = Vec::new();
    if let Some(results) = v["organic"].as_array() {
        for r in results {
            let title = r["title"].as_str().unwrap_or_default().trim().to_string();
            let url = r["link"].as_str().unwrap_or_default().to_string();
            if title.is_empty() || url.is_empty() {
                continue;
            }
            out.push(RawResult {
                title,
                url,
                snippet: r["snippet"].as_str().unwrap_or_default().to_string(),
            });
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

// ---- fusion -----------------------------------------------------------------

/// Tracking-query params stripped during URL normalization (dedupe keys).
const TRACKING_PARAMS: &[&str] = &[
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
    "fbclid",
    "gclid",
    "msclkid",
    "ref",
    "ref_src",
    "igshid",
];

/// Normalized dedupe key: lowercase host, drop fragment + tracking params,
/// drop trailing slash on non-root paths.
pub fn normalize_url(raw: &str) -> Option<String> {
    let mut u = url::Url::parse(raw).ok()?;
    u.set_fragment(None);
    if u.scheme() != "http" && u.scheme() != "https" {
        return None;
    }
    if let Some(host) = u.host_str() {
        let lower = host.to_lowercase();
        if let Some(bare) = lower.strip_prefix("www.").filter(|b| !b.is_empty()) {
            let _ = u.set_host(Some(bare));
        } else if lower != host {
            let _ = u.set_host(Some(&lower));
        }
    }
    let pairs: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| !TRACKING_PARAMS.contains(&k.to_lowercase().as_str()))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    u.set_query(None);
    if !pairs.is_empty() {
        u.query_pairs_mut().extend_pairs(pairs);
    }
    let path = u.path().to_string();
    if path.len() > 1 && path.ends_with('/') {
        u.set_path(path.trim_end_matches('/'));
    }
    Some(u.to_string())
}

/// Fused result: RRF across engines (score = Σ 1/(60+rank)), deduped by
/// normalized URL, engine provenance preserved.
pub struct FusedResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub engine: String,
    pub engines: Vec<String>,
    pub rrf: f64,
}

pub fn fuse(lists: &[(String, Vec<RawResult>)]) -> Vec<FusedResult> {
    let mut by_url: indexmap::IndexMap<String, FusedResult> = indexmap::IndexMap::new();
    for (engine, results) in lists {
        for (rank, r) in results.iter().enumerate() {
            let Some(key) = normalize_url(&r.url) else {
                continue;
            };
            let contribution = 1.0 / (60.0 + rank as f64);
            match by_url.get_mut(&key) {
                Some(f) => {
                    f.rrf += contribution;
                    if !f.engines.iter().any(|e| e == engine) {
                        f.engines.push(engine.clone());
                    }
                    if f.title.is_empty() {
                        f.title = r.title.clone();
                    }
                    if f.snippet.is_empty() {
                        f.snippet = r.snippet.clone();
                    }
                }
                None => {
                    by_url.insert(
                        key,
                        FusedResult {
                            title: r.title.clone(),
                            url: r.url.clone(),
                            snippet: r.snippet.clone(),
                            engine: engine.clone(),
                            engines: vec![engine.clone()],
                            rrf: contribution,
                        },
                    );
                }
            }
        }
    }
    let mut fused: Vec<FusedResult> = by_url.into_values().collect();
    fused.sort_by(|a, b| {
        b.rrf
            .partial_cmp(&a.rrf)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    fused
}

// ---- engine health (circuit breaker) ----------------------------------------

/// Consecutive failures before an engine is skipped for the rest of the
/// process. Keyed engines rarely break silently; scrapers break constantly.
const UNHEALTHY_THRESHOLD: u32 = 3;

struct Health {
    consecutive_errors: u32,
    last_error: Option<String>,
}

fn health_map() -> std::sync::MutexGuard<'static, HashMap<String, Health>> {
    static MAP: std::sync::LazyLock<Mutex<HashMap<String, Health>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
    MAP.lock().unwrap()
}

pub fn engine_key_present(engine: &EngineDef) -> bool {
    engine
        .key_env
        .map(|name| {
            std::env::var(name)
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
        })
        .unwrap_or(true)
}

pub fn engine_healthy(name: &str) -> bool {
    let map = health_map();
    map.get(name)
        .map(|h| h.consecutive_errors < UNHEALTHY_THRESHOLD)
        .unwrap_or(true)
}

pub fn engine_mark(name: &str, ok: bool, error: Option<String>) {
    let mut map = health_map();
    let h = map.entry(name.to_string()).or_insert(Health {
        consecutive_errors: 0,
        last_error: None,
    });
    if ok {
        h.consecutive_errors = 0;
        h.last_error = None;
    } else {
        h.consecutive_errors += 1;
        h.last_error = error;
    }
}

/// Per-host minimum interval — one token of politeness, persisted per process.
/// Search engines are the hosts we hammer; static Mutex keeps it trivial and
/// the kernel serializes calls per connection anyway.
fn politeness_gate(host: &str, min_interval_ms: u64) {
    static LAST: std::sync::LazyLock<Mutex<std::collections::HashMap<String, Instant>>> =
        std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));
    if min_interval_ms == 0 {
        return;
    }
    let wait = {
        let mut map = LAST.lock().unwrap();
        let now = Instant::now();
        let w = match map.get(host) {
            Some(last) => {
                min_interval_ms.saturating_sub(now.duration_since(*last).as_millis() as u64)
            }
            None => 0,
        };
        // reserve the slot now so a concurrent caller waits its own turn
        map.insert(host.to_string(), now + Duration::from_millis(w));
        w
    };
    if wait > 0 {
        std::thread::sleep(Duration::from_millis(wait));
    } else {
        // refresh the marker so the NEXT call measures from completion
        LAST.lock()
            .unwrap()
            .insert(host.to_string(), Instant::now());
    }
}

/// Named-dispatch wrapper so parallel workers can run an engine by name+kind.
/// Engine names are 'static literals in ENGINES — resolve to the static def.
pub fn run_engine_named(
    name: &str,
    kind: EngineKind,
    query: &str,
    limit: usize,
    timeout_ms: u64,
    politeness_ms: u64,
) -> Result<Vec<RawResult>, ToolError> {
    let key_env = engine_by_name(name).and_then(|e| e.key_env);
    run_engine_fields(name, kind, key_env, query, limit, timeout_ms, politeness_ms)
}

/// Fetch + parse one engine. Network failure or bad parse degrades to Err —
/// the caller records it per-engine and keeps going.
pub fn run_engine(
    engine: &EngineDef,
    query: &str,
    limit: usize,
    timeout_ms: u64,
    politeness_ms: u64,
) -> Result<Vec<RawResult>, ToolError> {
    run_engine_fields(
        engine.name,
        engine.kind,
        engine.key_env,
        query,
        limit,
        timeout_ms,
        politeness_ms,
    )
}

/// Field-level entry point — parallel workers pass name/kind directly without
/// needing a 'static EngineDef. `name` must be one of the static engine names.
pub fn run_engine_fields(
    name: &str,
    kind: EngineKind,
    key_env: Option<&'static str>,
    query: &str,
    limit: usize,
    timeout_ms: u64,
    politeness_ms: u64,
) -> Result<Vec<RawResult>, ToolError> {
    let api_key = key_env.and_then(|env| std::env::var(env).ok().filter(|v| !v.trim().is_empty()));
    if key_env.is_some() && api_key.is_none() {
        return Err(ToolError::with_hint(
            "ERR_NO_KEY",
            format!("engine {name} requires an API key"),
            json!({ "engine": name, "env": key_env }),
        ));
    }
    politeness_gate(name, politeness_ms);
    let mut opts = match kind {
        EngineKind::Tavily => crate::httpx::FetchOpts::post_json(
            crate::ssrf::parse_http_url(&endpoint_str(kind, query, limit))?,
            json!({ "api_key": api_key, "query": query, "max_results": limit, "search_depth": "basic" }),
        ),
        EngineKind::Serper => crate::httpx::FetchOpts::post_json(
            crate::ssrf::parse_http_url(&endpoint_str(kind, query, limit))?,
            json!({ "q": query, "num": limit }),
        )
        .header("X-API-KEY", api_key.as_deref().unwrap_or_default()),
        _ => {
            let mut o = crate::httpx::FetchOpts::get(crate::ssrf::parse_http_url(&endpoint_str(
                kind, query, limit,
            ))?);
            if kind == EngineKind::Brave {
                o = o
                    .header(
                        "X-Subscription-Token",
                        api_key.as_deref().unwrap_or_default(),
                    )
                    .header("Accept", "application/json");
            }
            o
        }
    };
    opts = opts.timeout(timeout_ms).guard(false).max_body(1_000_000);
    let outcome = crate::httpx::fetch(opts)?;
    if !outcome.ok {
        return Err(ToolError::with_hint(
            "ERR_ENGINE",
            format!("engine {name} returned HTTP {}", outcome.status),
            json!({ "engine": name, "status": outcome.status }),
        ));
    }
    parse_by_kind(kind, &outcome.body, limit)
}

// ---- text helpers -----------------------------------------------------------

pub fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

pub fn urldecode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

pub fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hn_fixture_parses() {
        let body = r#"{"hits":[
            {"objectID":"1","title":"Announcing Rust 1.0","url":"https://blog.rust-lang.org/1.0","story_text":"The wait is <b>over</b>."},
            {"objectID":"2","url":null,"comment_text":"titleless hit is skipped"}
        ]}"#;
        let engine = engine_by_name("hn").unwrap();
        let rs = parse(engine, body, 10).unwrap();
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].title, "Announcing Rust 1.0");
        assert_eq!(rs[0].url, "https://blog.rust-lang.org/1.0");
        assert_eq!(rs[0].snippet, "The wait is over.");
    }

    #[test]
    fn wikipedia_fixture_parses() {
        let body = r#"{"query":{"search":[
            {"title":"Rust (programming language)","snippet":"<span>Rust is a <b>multi-paradigm</b> language</span>","pageid":123}
        ]}}"#;
        let engine = engine_by_name("wikipedia").unwrap();
        let rs = parse(engine, body, 10).unwrap();
        assert_eq!(
            rs[0].url,
            "https://en.wikipedia.org/wiki/Rust_%28programming_language%29"
        );
        assert!(rs[0].snippet.contains("multi-paradigm"));
        assert!(!rs[0].snippet.contains('<'));
    }

    #[test]
    fn ddg_lite_fixture_parses_and_unwraps_uddg() {
        let body = r#"<html><body><table>
            <tr><td><a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F&amp;rut=abc">Rust Programming Language</a></td></tr>
            <tr><td class="result-snippet">Official site</td></tr>
            <tr><td><a rel="nofollow" href="https://doc.rust-lang.org/book/">The Rust Book</a></td></tr>
        </table></body></html>"#;
        let engine = engine_by_name("ddg").unwrap();
        let rs = parse(engine, body, 10).unwrap();
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].url, "https://www.rust-lang.org/");
        assert_eq!(rs[1].url, "https://doc.rust-lang.org/book/");
    }

    #[test]
    fn mojeek_fixture_parses() {
        let body = r#"<ul><li><h2><a class="ob" href="https://www.rust-lang.org/">Rust</a></h2><p class="s">A language empowering everyone.</p></li></ul>"#;
        let engine = engine_by_name("mojeek").unwrap();
        let rs = parse(engine, body, 10).unwrap();
        assert_eq!(rs[0].title, "Rust");
        assert_eq!(rs[0].snippet, "A language empowering everyone.");
    }

    #[test]
    fn fusion_dedupes_and_favors_multi_engine_hits() {
        let lists = vec![
            (
                "ddg".to_string(),
                vec![
                    RawResult {
                        title: "Rust".into(),
                        url: "https://www.rust-lang.org/".into(),
                        snippet: "official".into(),
                    },
                    RawResult {
                        title: "Book".into(),
                        url: "https://doc.rust-lang.org/book/".into(),
                        snippet: String::new(),
                    },
                ],
            ),
            (
                "mojeek".to_string(),
                vec![RawResult {
                    title: "Rust".into(),
                    url: "https://rust-lang.org/".into(),
                    snippet: "mirror".into(),
                }],
            ),
            (
                "hn".to_string(),
                vec![RawResult {
                    title: "Book".into(),
                    url: "https://doc.rust-lang.org/book/?utm_source=x".into(),
                    snippet: "learn".into(),
                }],
            ),
        ];
        let fused = fuse(&lists);
        assert_eq!(fused.len(), 2);
        // rust-lang.org hit by two engines (www-stripped) must outrank the book
        assert!(fused[0].url.contains("rust-lang.org/") && fused[0].engines.len() == 2);
        // book hit twice, one with tracking param — normalized to one entry
        assert!(fused[1].url.starts_with("https://doc.rust-lang.org/book"));
        assert!(!fused[1].url.contains("utm_"));
    }

    #[test]
    fn keyed_engine_fixtures_parse() {
        let brave = r#"{"web":{"results":[{"title":"Rust","url":"https://www.rust-lang.org/","description":"Official site"}]}}"#;
        let rs = parse(engine_by_name("brave").unwrap(), brave, 10).unwrap();
        assert_eq!(rs[0].url, "https://www.rust-lang.org/");
        assert_eq!(rs[0].snippet, "Official site");

        let tavily = r#"{"results":[{"title":"Rust","url":"https://www.rust-lang.org/","content":"A language"}]}"#;
        let rs = parse(engine_by_name("tavily").unwrap(), tavily, 10).unwrap();
        assert_eq!(rs[0].snippet, "A language");

        let serper = r#"{"organic":[{"title":"Rust","link":"https://www.rust-lang.org/","snippet":"Empowering everyone"}]}"#;
        let rs = parse(engine_by_name("serper").unwrap(), serper, 10).unwrap();
        assert_eq!(rs[0].url, "https://www.rust-lang.org/");
    }

    #[test]
    fn circuit_breaker_opens_and_resets() {
        engine_mark("unit-test-engine", true, None);
        for i in 0..3 {
            engine_mark("unit-test-engine", false, Some(format!("err{i}")));
        }
        assert!(
            !engine_healthy("unit-test-engine"),
            "3 consecutive errors must open the breaker"
        );
        engine_mark("unit-test-engine", true, None);
        assert!(
            engine_healthy("unit-test-engine"),
            "success must reset the breaker"
        );
    }

    #[test]
    fn key_presence_gate() {
        let brave = engine_by_name("brave").unwrap();
        // this test process has no NCTOOLS_BRAVE_KEY — presence must be false
        // unless the environment actually carries it
        let expected = std::env::var("NCTOOLS_BRAVE_KEY")
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        assert_eq!(engine_key_present(brave), expected);
        let hn = engine_by_name("hn").unwrap();
        assert!(engine_key_present(hn), "keyless engines are always usable");
    }

    #[test]
    fn url_roundtrip() {
        assert_eq!(urlencode("rust async"), "rust+async");
        assert_eq!(
            urldecode("https%3A%2F%2Fx.dev%2Fa+b"),
            Some("https://x.dev/a b".to_string())
        );
        assert_eq!(
            normalize_url("https://WWW.Example.com/a/?utm_source=r#frag").unwrap(),
            "https://example.com/a"
        );
    }
}
