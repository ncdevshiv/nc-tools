// SearXNG fleet: runtime discovery + probing + caching + rotation.
// The registry (searx.space instances.json) lists ~90 live instances; most
// rate-limit the JSON API (429) — the fleet PROBES candidates, keeps only
// those that answer format=json, scores them by latency, caches the working
// set with a TTL, and rotates across it. Designed to function with ANY
// number of survivors, including zero (the caller falls back to the static
// keyless sources). This is the anti-fragile core: when an instance dies,
// the fleet heals on the next refresh without a code change.
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::json;

use nct_core::errors::ToolError;

use super::engines::RawResult;
use super::sources;

const REGISTRY_URL: &str = "https://searx.space/data/instances.json";
const FLEET_SIZE: usize = 6; // probe this many registry candidates per refresh
const TTL_MS: u64 = 30 * 60 * 1000; // refresh the working set every 30 min
const PROBE_TIMEOUT_MS: u64 = 8_000;

#[derive(Clone, Debug)]
pub struct FleetMember {
    pub base_url: String,
    pub latency_ms: u64,
}

#[derive(Default)]
struct FleetState {
    members: Vec<FleetMember>,
    /// Round-robin offset across searches so no single instance is hammered
    next: usize,
    refreshed_at: Option<Instant>,
    refreshing: std::sync::atomic::AtomicBool,
    last_error: Option<String>,
}

fn state() -> std::sync::MutexGuard<'static, FleetState> {
    static STATE: std::sync::LazyLock<Mutex<FleetState>> = std::sync::LazyLock::new(|| Mutex::new(FleetState::default()));
    STATE.lock().unwrap()
}

/// Minimum gap between registry refresh ATTEMPTS when the fleet is empty —
/// without this, a fully-429'd fleet re-probes the registry on EVERY search
/// (30s+ latency bomb per call).
const REFRESH_BACKOFF_MS: u64 = 5 * 60 * 1000;

/// Public snapshot for result metadata / diagnostics.
pub fn fleet_status() -> (usize, Vec<String>) {
    let s = state();
    let urls = s.members.iter().map(|m| m.base_url.clone()).collect();
    (s.members.len(), urls)
}

/// Query one instance of the fleet. Returns Err when the fleet is empty or
/// every tried member failed — the caller degrades gracefully.
pub fn search(query: &str, limit: usize, timeout_ms: u64) -> Result<(String, Vec<RawResult>), ToolError> {
    ensure_fresh()?;
    let (members, start_idx) = {
        let s = state();
        if s.members.is_empty() {
            return Err(ToolError::with_hint(
                "ERR_ENGINE",
                "searxng fleet has no live members (instances rate-limit the JSON api or registry unreachable)",
                json!({ "lastError": s.last_error, "hint": "fleet refreshes every 30 min; keyless static sources still serve the query" }),
            ));
        }
        (s.members.clone(), s.next)
    };
    {
        let mut s = state();
        s.next = (s.next + 1) % members.len().max(1);
    }

    // Try up to 3 members starting at the rotation offset (spreads load).
    let mut last_err = None;
    for i in 0..3.min(members.len()) {
        let member = &members[(start_idx + i) % members.len()];
        // JSON attempt (preferred — structured, cheap); HTML fallback on 429/403
        let url = format!(
            "{}{}search?q={}&format=json",
            member.base_url,
            if member.base_url.ends_with('/') { "" } else { "/" },
            super::engines::urlencode(query)
        );
        let Ok(parsed) = super::ssrf::parse_http_url(&url) else { continue };
        let fetched = super::httpx::fetch(super::httpx::FetchOpts::get(parsed).timeout(timeout_ms.min(PROBE_TIMEOUT_MS)).max_body(1_000_000));
        match fetched {
            Ok(outcome) if outcome.ok => match sources::parse_searxng(&outcome.body, limit) {
                Ok(results) if !results.is_empty() => {
                    mark_health(&member.base_url, true, None);
                    return Ok((member.base_url.clone(), results));
                }
                Ok(_) => last_err = Some(format!("{}: empty results", member.base_url)),
                Err(e) => {
                    // some instances return html/error json with 200 — treat as failure
                    mark_health(&member.base_url, false, Some(e.message.clone()));
                    last_err = Some(format!("{}: {}", member.base_url, e.message));
                }
            },
            Ok(outcome) => {
                // HTML fallback: many public instances 429/403 the JSON API
                // but serve HTML to browsers. Parse the stable result markup.
                if outcome.status == 429 || outcome.status == 403 {
                    let html_url = format!(
                        "{}{}search?q={}",
                        member.base_url,
                        if member.base_url.ends_with('/') { "" } else { "/" },
                        super::engines::urlencode(query)
                    );
                    if let Ok(html_parsed) = super::ssrf::parse_http_url(&html_url) {
                        if let Ok(html_outcome) = super::httpx::fetch(
                            super::httpx::FetchOpts::get(html_parsed)
                                .timeout(timeout_ms.min(PROBE_TIMEOUT_MS))
                                .max_body(1_000_000)
                                .header("Accept", "text/html")
                        ) {
                            if html_outcome.ok {
                                let results = sources::parse_searxng_html(&html_outcome.body, &member.base_url, limit);
                                if !results.is_empty() {
                                    mark_health(&member.base_url, true, None);
                                    return Ok((member.base_url.clone(), results));
                                }
                            }
                        }
                    }
                }
                let msg = format!("{}: HTTP {}", member.base_url, outcome.status);
                mark_health(&member.base_url, false, Some(msg.clone()));
                last_err = Some(msg);
            }
            Err(e) => {
                mark_health(&member.base_url, false, Some(e.message.clone()));
                last_err = Some(format!("{}: {}", member.base_url, e.message));
            }
        }
    }
    Err(ToolError::with_hint("ERR_ENGINE", "all tried fleet members failed", json!({ "lastError": last_err })))
}

fn mark_health(base_url: &str, ok: bool, error: Option<String>) {
    let mut s = state();
    if ok {
        if let Some(m) = s.members.iter_mut().find(|m| m.base_url == base_url) {
            m.latency_ms = 0; // healthy; ordering refreshes latency anyway
        }
    } else {
        // a hard 429/403 member is removed immediately — it'll rejoin at the
        // next registry refresh if it recovers
        s.members.retain(|m| m.base_url != base_url);
        s.last_error = error;
    }
}

/// Refresh the working set when stale. Probes registry candidates in parallel
/// (bounded threads), keeping FLEET_SIZE fastest responders.
fn ensure_fresh() -> Result<(), ToolError> {
    {
        let s = state();
        if let Some(t) = s.refreshed_at {
            if t.elapsed() < Duration::from_millis(TTL_MS) && !s.members.is_empty() {
                return Ok(());
            }
            // empty fleet + recent failed attempt → back off, fail fast
            if s.members.is_empty() && t.elapsed() < Duration::from_millis(REFRESH_BACKOFF_MS) {
                return Err(ToolError::with_hint(
                    "ERR_ENGINE",
                    "searxng fleet empty (recent refresh found no JSON-capable members; backing off)",
                    json!({ "lastError": s.last_error, "retryInMin": ((REFRESH_BACKOFF_MS as f64 - t.elapsed().as_millis() as f64) / 60000.0).ceil() as u64 }),
                ));
            }
        }
        if s.refreshing.load(std::sync::atomic::Ordering::Relaxed) {
            // another thread is refreshing; proceed with whatever we have
            return Ok(());
        }
        s.refreshing.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let result = refresh_inner();
    let mut s = state();
    s.refreshing.store(false, std::sync::atomic::Ordering::Relaxed);
    s.refreshed_at = Some(Instant::now());
    if let Err(e) = result {
        s.last_error = Some(e.message.clone());
    }
    Ok(())
}

fn refresh_inner() -> Result<(), ToolError> {
    // 1. registry fetch (cached by the net cache? no — separate tiny fetch)
    let registry = super::ssrf::parse_http_url(REGISTRY_URL)?;
    let outcome = super::httpx::fetch(super::httpx::FetchOpts::get(registry).timeout(20_000).max_body(4_000_000))?;
    if !outcome.ok {
        return Err(ToolError::with_hint("ERR_ENGINE", format!("searx.space registry returned HTTP {}", outcome.status), json!({})));
    }
    // 2. candidates = https, status 200, highest uptime first
    let candidates = sources::instances_from_searxspace(&outcome.body, FLEET_SIZE * 4);
    if candidates.is_empty() {
        return Err(ToolError::new("ERR_ENGINE", "registry listed no usable candidates"));
    }
    // 3. probe in parallel: format=json?q=test, keep 200+parseable, rank by latency
    let probes: Vec<std::sync::Arc<String>> = candidates.into_iter().map(std::sync::Arc::new).collect();
    let (tx, rx) = std::sync::mpsc::channel::<(String, u64)>();
    let mut handles = Vec::new();
    for chunk in probes.chunks((probes.len() / 8).max(1)) {
        let tx = tx.clone();
        let chunk: Vec<_> = chunk.to_vec();
        handles.push(std::thread::spawn(move || {
            for url in chunk {
                let probe = format!("{url}{}search?q=test&format=json", if url.ends_with('/') { "" } else { "/" });
                let started = Instant::now();
                let ok = super::ssrf::parse_http_url(&probe).ok()
                    .and_then(|p| super::httpx::fetch(super::httpx::FetchOpts::get(p).timeout(PROBE_TIMEOUT_MS).max_body(512_000)).ok())
                    .map(|o| {
                        o.status == 200
                            && sources::parse_searxng(&o.body, 1).map(|r| !r.is_empty()).unwrap_or(false)
                    })
                    .unwrap_or(false);
                if ok {
                    let _ = tx.send(((*url).clone(), started.elapsed().as_millis() as u64));
                }
            }
        }));
    }
    drop(tx);
    for h in handles {
        let _ = h.join();
    }
    let mut members: Vec<FleetMember> = rx
        .into_iter()
        .map(|(base_url, latency_ms)| FleetMember { base_url, latency_ms })
        .collect();
    members.sort_by_key(|m| m.latency_ms);
    members.truncate(FLEET_SIZE);
    let mut s = state();
    s.members = members;
    s.next = 0;
    Ok(())
}
