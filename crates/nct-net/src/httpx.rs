// Shared HTTP transport for net.fetch/net.robots/net.search: manual redirect
// loop so every hop re-passes the SSRF guard, charset-aware body decoding,
// conditional-request support for the cache, and a redirect chain report.
// ureq v2 with redirects(0) hands us 3xx responses as data; everything after
// that is ours.
use std::io::Read;
use std::time::Duration;

use serde_json::{json, Value};

use nct_core::errors::ToolError;

pub struct FetchOpts {
    pub method: String,
    pub url: url::Url,
    pub timeout_ms: u64,
    pub max_body: usize,
    pub max_redirects: usize,
    pub guard_private: bool,
    pub headers: Vec<(String, String)>,
    /// JSON body for POST engines (Tavily/Serper)
    pub json_body: Option<Value>,
    /// If-None-Match for cache revalidation
    pub if_none_match: Option<String>,
    pub if_modified_since: Option<String>,
}

#[derive(Debug)]
pub struct FetchOutcome {
    pub status: u16,
    pub ok: bool,
    pub headers: Vec<(String, String)>,
    /// Decoded body (charset from Content-Type, utf-8 fallback), size-capped.
    pub body: String,
    pub body_truncated: bool,
    pub final_url: url::Url,
    pub redirects: Vec<Value>,
    pub duration_ms: u64,
}

impl FetchOpts {
    pub fn get(url: url::Url) -> FetchOpts {
        FetchOpts {
            method: "GET".into(),
            url,
            timeout_ms: 30_000,
            max_body: 2_000_000,
            max_redirects: 10,
            guard_private: false,
            headers: Vec::new(),
            json_body: None,
            if_none_match: None,
            if_modified_since: None,
        }
    }

    pub fn guard(mut self, guard_private: bool) -> FetchOpts {
        self.guard_private = guard_private;
        self
    }
    pub fn timeout(mut self, ms: u64) -> FetchOpts {
        self.timeout_ms = ms;
        self
    }
    pub fn max_body(mut self, bytes: usize) -> FetchOpts {
        self.max_body = bytes;
        self
    }
    pub fn header(mut self, k: &str, v: &str) -> FetchOpts {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }
    /// POST with a JSON body (keyed search engines).
    pub fn post_json(url: url::Url, body: Value) -> FetchOpts {
        let mut o = FetchOpts::get(url);
        o.method = "POST".into();
        o.json_body = Some(body);
        o
    }
    pub fn revalidate(mut self, etag: Option<String>, modified: Option<String>) -> FetchOpts {
        self.if_none_match = etag;
        self.if_modified_since = modified;
        self
    }
}

/// One-shot send + manual redirect following. On `guard_private`, the initial
/// URL and every redirect hop are resolved and checked (fail closed).
pub fn fetch(opts: FetchOpts) -> Result<FetchOutcome, ToolError> {
    if opts.guard_private {
        crate::ssrf::assert_public(opts.url.as_str())?;
    }
    let started = std::time::Instant::now();
    let mut current = opts.url.clone();
    let mut redirects: Vec<Value> = Vec::new();

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(opts.timeout_ms))
        // socket-level read timeout: the agent-level timeout does NOT cover
        // body reads — a response with no content-length/transfer-encoding
        // ("read until close") blocks read_to_string forever on a keep-alive
        // socket (observed live: en.wikipedia.org article pages). The read
        // timeout turns that hang into a structured ERR_TIMEOUT.
        .timeout_read(Duration::from_millis(opts.timeout_ms.min(20_000)))
        .timeout_connect(Duration::from_secs(10))
        .redirects(0)
        .user_agent("nc-tools/1.0 (+agent; net.fetch)")
        .build();

    let resp = loop {
        let mut req = match opts.method.as_str() {
            "POST" => agent.post(current.as_str()),
            "GET" => agent.get(current.as_str()),
            "HEAD" => agent.head(current.as_str()),
            m => {
                return Err(ToolError::new(
                    "ERR_BAD_INPUT",
                    format!("unsupported method: {m}"),
                ))
            }
        };
        for (k, v) in &opts.headers {
            req = req.set(k, v);
        }
        if let Some(etag) = &opts.if_none_match {
            req = req.set("If-None-Match", etag);
        }
        if let Some(modified) = &opts.if_modified_since {
            req = req.set("If-Modified-Since", modified);
        }

        let result = if let Some(body) = &opts.json_body {
            req.send_json(body.clone())
        } else {
            req.call()
        };
        let response = match result {
            Ok(r) => r,
            Err(ureq::Error::Status(_code, r)) => r, // 4xx/5xx are data here
            Err(ureq::Error::Transport(t)) => {
                let msg = t.to_string();
                let is_timeout = msg.contains("timed out") || msg.contains("Died");
                return Err(ToolError::with_hint(
                    if is_timeout { "ERR_TIMEOUT" } else { "ERR_NET" },
                    format!("HTTP {} {} failed: {msg}", opts.method, current),
                    json!({
                        "url": current.as_str(),
                        "timeoutMs": opts.timeout_ms,
                        "hint": if is_timeout { "increase timeoutMs or check the target is up" } else { "check DNS/firewall/url" },
                    }),
                ));
            }
        };

        let status = response.status();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let loc = response.header("location").unwrap_or_default().to_string();
            if loc.is_empty() || redirects.len() >= opts.max_redirects {
                return Err(ToolError::with_hint(
                    "ERR_NET",
                    if loc.is_empty() {
                        "redirect with no Location header"
                    } else {
                        "too many redirects"
                    },
                    json!({ "url": current.as_str(), "redirects": redirects.len() }),
                ));
            }
            let next = current
                .join(&loc)
                .map_err(|e| ToolError::new("ERR_NET", format!("bad redirect Location: {e}")))?;
            if opts.guard_private {
                crate::ssrf::assert_public_redirect(next.as_str(), redirects.len() + 1)?;
            }
            redirects.push(json!({ "status": status, "location": loc, "url": next.to_string() }));
            current = next;
            continue;
        }
        break response;
    };

    let status = resp.status();
    let mut headers: Vec<(String, String)> = Vec::new();
    for name in resp.headers_names() {
        if let Some(v) = resp.header(&name) {
            headers.push((name, v.to_string()));
        }
    }
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };

    let content_type = header("content-type").unwrap_or_default();
    let mut raw: Vec<u8> = Vec::new();
    let mut capped = resp.into_reader().take((opts.max_body + 1) as u64);
    let _ = capped.read_to_end(&mut raw);
    let body_truncated = raw.len() > opts.max_body;
    raw.truncate(opts.max_body);

    // charset: Content-Type param wins, encoding_rs sniff, else UTF-8 lossy
    let charset = content_type
        .split(';')
        .map(|s| s.trim())
        .find_map(|s| s.strip_prefix("charset="))
        .map(|c| c.trim_matches('"').to_string());
    let body = match charset
        .as_deref()
        .and_then(|c| encoding_rs::Encoding::for_label(c.as_bytes()))
    {
        Some(enc) => enc.decode(&raw).0.into_owned(),
        None => {
            let (decoded, _, had_errors) = encoding_rs::UTF_8.decode(&raw);
            if had_errors {
                String::from_utf8_lossy(&raw).into_owned()
            } else {
                decoded.into_owned()
            }
        }
    };

    Ok(FetchOutcome {
        status,
        ok: (200..300).contains(&status),
        headers,
        body,
        body_truncated,
        final_url: current,
        redirects,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

pub fn header_value(outcome: &FetchOutcome, name: &str) -> Option<String> {
    outcome
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}
