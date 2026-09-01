// net.search engine layer: keyless engine adapters defined as data (endpoint
// template + parse strategy), pure parse functions per engine (unit-tested
// offline against committed fixtures), RRF fusion + URL dedupe. A failing
// engine degrades to a per-engine error entry — it never fails the call.
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
}

pub struct EngineDef {
    pub name: &'static str,
    pub kind: EngineKind,
}

pub const ENGINES: &[EngineDef] = &[
    EngineDef { name: "hn", kind: EngineKind::Hn },
    EngineDef { name: "wikipedia", kind: EngineKind::Wikipedia },
    EngineDef { name: "ddg", kind: EngineKind::DdgLite },
    EngineDef { name: "mojeek", kind: EngineKind::Mojeek },
];

pub fn engine_by_name(name: &str) -> Option<&'static EngineDef> {
    ENGINES.iter().find(|e| e.name == name)
}

/// The endpoint for one engine query (GET form-encoded).
pub fn endpoint(engine: &EngineDef, query: &str, limit: usize) -> String {
    let q = urlencode(query);
    match engine.kind {
        EngineKind::Hn => format!("https://hn.algolia.com/api/v1/search?query={q}&hitsPerPage={limit}"),
        EngineKind::Wikipedia => {
            format!("https://en.wikipedia.org/w/api.php?action=query&list=search&srsearch={q}&format=json&srlimit={limit}&origin=*")
        }
        EngineKind::DdgLite => format!("https://lite.duckduckgo.com/lite/?q={q}"),
        EngineKind::Mojeek => format!("https://www.mojeek.com/search?q={q}"),
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
    let results = match engine.kind {
        EngineKind::Hn => parse_hn(body, limit)?,
        EngineKind::Wikipedia => parse_wikipedia(body, limit)?,
        EngineKind::DdgLite => parse_ddg_lite(body, limit),
        EngineKind::Mojeek => parse_mojeek(body, limit),
    };
    Ok(results.into_iter().take(limit).collect())
}

fn parse_hn(body: &str, limit: usize) -> Result<Vec<RawResult>, ToolError> {
    let v: Value = serde_json::from_str(body)
        .map_err(|e| ToolError::new("ERR_ENGINE", format!("hn algolia returned invalid json: {e}")))?;
    let mut out = Vec::new();
    if let Some(hits) = v["hits"].as_array() {
        for hit in hits {
            let title = hit["title"].as_str().or_else(|| hit["story_title"].as_str()).unwrap_or_default().trim().to_string();
            let url = hit["url"]
                .as_str()
                .or_else(|| hit["story_url"].as_str())
                .map(String::from)
                .unwrap_or_else(|| {
                    format!("https://news.ycombinator.com/item?id={}", hit["objectID"].as_str().unwrap_or(""))
                });
            let snippet = hit["story_text"]
                .as_str()
                .or_else(|| hit["comment_text"].as_str())
                .unwrap_or_default();
            if title.is_empty() {
                continue;
            }
            out.push(RawResult { title, url, snippet: strip_html(snippet) });
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

fn parse_wikipedia(body: &str, limit: usize) -> Result<Vec<RawResult>, ToolError> {
    let v: Value = serde_json::from_str(body)
        .map_err(|e| ToolError::new("ERR_ENGINE", format!("wikipedia returned invalid json: {e}")))?;
    let mut out = Vec::new();
    if let Some(hits) = v["query"]["search"].as_array() {
        for hit in hits {
            let title = hit["title"].as_str().unwrap_or_default().trim().to_string();
            if title.is_empty() {
                continue;
            }
            let url = format!("https://en.wikipedia.org/wiki/{}", urlencode(&title.replace(' ', "_")));
            let snippet = strip_html(hit["snippet"].as_str().unwrap_or_default());
            out.push(RawResult { title, url, snippet });
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
        let absolute = if let Some(uddg) = href.split("uddg=").nth(1).and_then(|s| s.split('&').next()) {
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
        out.push(RawResult { title, url: absolute, snippet: String::new() });
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
            url: if url.starts_with("http") { url } else { format!("https://www.mojeek.com{url}") },
            snippet: snippets.get(i).cloned().unwrap_or_default(),
        })
        .collect()
}

// ---- fusion -----------------------------------------------------------------

/// Tracking-query params stripped during URL normalization (dedupe keys).
const TRACKING_PARAMS: &[&str] = &[
    "utm_source", "utm_medium", "utm_campaign", "utm_term", "utm_content", "fbclid", "gclid",
    "msclkid", "ref", "ref_src", "igshid",
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
            let Some(key) = normalize_url(&r.url) else { continue };
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
                    by_url.insert(key, FusedResult {
                        title: r.title.clone(),
                        url: r.url.clone(),
                        snippet: r.snippet.clone(),
                        engine: engine.clone(),
                        engines: vec![engine.clone()],
                        rrf: contribution,
                    });
                }
            }
        }
    }
    let mut fused: Vec<FusedResult> = by_url.into_values().collect();
    fused.sort_by(|a, b| b.rrf.partial_cmp(&a.rrf).unwrap_or(std::cmp::Ordering::Equal));
    fused
}

// ---- politeness -------------------------------------------------------------

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
            Some(last) => min_interval_ms.saturating_sub(now.duration_since(*last).as_millis() as u64),
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
        LAST.lock().unwrap().insert(host.to_string(), Instant::now());
    }
}

/// Fetch + parse one engine. Network failure or bad parse degrades to Err —
/// the caller records it per-engine and keeps going.
pub fn run_engine(engine: &EngineDef, query: &str, limit: usize, timeout_ms: u64, politeness_ms: u64) -> Result<Vec<RawResult>, ToolError> {
    politeness_gate(engine.name, politeness_ms);
    let endpoint = endpoint(engine, query, limit);
    let parsed_url = crate::ssrf::parse_http_url(&endpoint)?;
    let outcome = crate::httpx::fetch(
        crate::httpx::FetchOpts::get(parsed_url)
            .timeout(timeout_ms)
            .guard(false) // engine endpoints are compile-time constants, not user URLs
            .max_body(1_000_000),
    )?;
    if !outcome.ok {
        return Err(ToolError::with_hint(
            "ERR_ENGINE",
            format!("engine {} returned HTTP {}", engine.name, outcome.status),
            json!({ "engine": engine.name, "status": outcome.status }),
        ));
    }
    parse(engine, &outcome.body, limit)
}

// ---- text helpers -----------------------------------------------------------

pub fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*b as char),
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
        assert_eq!(rs[0].url, "https://en.wikipedia.org/wiki/Rust_%28programming_language%29");
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
            ("ddg".to_string(), vec![RawResult { title: "Rust".into(), url: "https://www.rust-lang.org/".into(), snippet: "official".into() }, RawResult { title: "Book".into(), url: "https://doc.rust-lang.org/book/".into(), snippet: String::new() }]),
            ("mojeek".to_string(), vec![RawResult { title: "Rust".into(), url: "https://rust-lang.org/".into(), snippet: "mirror".into() }]),
            ("hn".to_string(), vec![RawResult { title: "Book".into(), url: "https://doc.rust-lang.org/book/?utm_source=x".into(), snippet: "learn".into() }]),
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
    fn url_roundtrip() {
        assert_eq!(urlencode("rust async"), "rust+async");
        assert_eq!(urldecode("https%3A%2F%2Fx.dev%2Fa+b"), Some("https://x.dev/a b".to_string()));
        assert_eq!(normalize_url("https://WWW.Example.com/a/?utm_source=r#frag").unwrap(), "https://example.com/a");
    }
}
