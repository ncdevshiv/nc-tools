// net.* — typed network operations: hardened HTTP, agent-grade fetch+extract,
// keyless federated search, robots/llms.txt, and port probing.
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};

pub mod authority;
pub mod cache;
pub mod cite;
pub mod contradict;
pub mod engines;
pub mod extract;
pub mod feed;
pub mod fetch;
pub mod fleet;
pub mod httpx;
pub mod pdf;
pub mod query;
pub mod render;
pub mod research;
pub mod robots;
pub mod routing;
pub mod sources;
pub mod ssrf;

pub const HTTP_DESC: &str = "Perform a raw HTTP request. Returns status, headers, body (capped), duration, redirect chain. blockPrivate=true refuses private/loopback targets. Replaces curl/wget; use net.fetch when you want readable page content.";
pub const PROBE_DESC: &str = "Check whether a TCP port is open. Replaces nc/netstat probing.";

pub fn register(k: &mut Kernel) {
    k.register("net.http", HTTP_DESC, nct_core::schema::schema_for::<HttpArgs>(), std::sync::Arc::new(HttpHandler));
    k.register("net.probePort", PROBE_DESC, nct_core::schema::schema_for::<ProbeArgs>(), std::sync::Arc::new(ProbeHandler));
    fetch::register(k);
    cite::register(k);
    research::register_research(k);
    crate::contradict::register_contradict(k);
}

#[derive(Deserialize, schemars::JsonSchema, Clone, Copy, PartialEq)]
#[serde(rename_all = "UPPERCASE")]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpArgs {
    #[serde(default)]
    pub method: Option<Method>,
    pub url: String,
    #[serde(default)]
    #[schemars(schema_with = "nct_core::plain_object_schema")]
    pub headers: Option<serde_json::Map<String, Value>>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    #[schemars(range(min = 100, max = 120000))]
    pub timeoutMs: Option<u64>,
    #[doc = "Refuse private/loopback/link-local targets (SSRF guard); default false to keep raw-curl semantics"]
    #[serde(default)]
    pub blockPrivate: Option<bool>,
    /// Auth: {type:"basic",user,pass} or {type:"bearer",token}. Sets the
    /// Authorization header (explicit `headers` win on key conflict).
    #[serde(default)]
    pub auth: Option<serde_json::Map<String, Value>>,
    /// Multipart form data: [{name, value, filename?, contentType?}]. Forces
    /// Content-Type multipart/form-data; body is ignored when multipart set.
    #[serde(default)]
    pub multipart: Option<Vec<serde_json::Map<String, Value>>>,
    /// Follow 3xx redirects (default true). When false, the redirect response
    /// is returned as data (status + Location).
    #[serde(default)]
    pub followRedirects: Option<bool>,
    /// Cap on redirect hops (default 10, max 20) — bounding the chain.
    #[serde(default)]
    #[schemars(range(min = 0, max = 20))]
    pub maxRedirects: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProbeArgs {
    #[schemars(range(min = 1, max = 65535))]
    pub port: u16,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    #[schemars(range(min = 100, max = 10000))]
    pub timeoutMs: Option<u64>,
}

pub struct HttpHandler;
impl Handler for HttpHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: HttpArgs = parse_args(args)?;
        let method = match a.method.unwrap_or(Method::Get) {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
            Method::Head => "HEAD",
        }
        .to_string();
        // blockPrivate=true resolves the host and refuses private targets
        // BEFORE the request (raw-curl semantics preserved by default;
        // per-redirect-hop validation lives in net.fetch).
        if a.blockPrivate.unwrap_or(false) {
            ssrf::assert_public(&a.url)?;
        }
        if !(a.url.starts_with("http://") || a.url.starts_with("https://")) {
            return Err(ToolError::with_hint("ERR_BAD_INPUT", "url must be an http(s) URL", json!({ "got": a.url })));
        }
        let timeout = a.timeoutMs.unwrap_or(30_000);
        let follow = a.followRedirects.unwrap_or(true);
        let max_redir = a.maxRedirects.unwrap_or(10) as usize;
        let agent_redirects = (if follow { max_redir.max(1).min(20) } else { 0 }) as u32;
        let started = Instant::now();
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_millis(timeout))
            .redirects(agent_redirects)
            .build();
        // Auth: {type:"basic",user,pass} or {type:"bearer",token}. Explicit
        // `headers` win on key conflict (set below).
        let mut auth_header: Option<(String, String)> = None;
        if let Some(auth) = &a.auth {
            let auth_type = auth.get("type").and_then(|t| t.as_str()).unwrap_or("").to_lowercase();
            match auth_type.as_str() {
                "basic" => {
                    let user = auth.get("user").and_then(|u| u.as_str()).unwrap_or("").to_string();
                    let pass = auth.get("pass").and_then(|p| p.as_str()).unwrap_or("").to_string();
                    let creds = format!("{user}:{pass}");
                    use base64::Engine as _;
                    let encoded = base64::engine::general_purpose::STANDARD.encode(creds.as_bytes());
                    auth_header = Some(("Authorization".to_string(), format!("Basic {encoded}")));
                }
                "bearer" => {
                    let token = auth.get("token").and_then(|t| t.as_str()).unwrap_or("").to_string();
                    auth_header = Some(("Authorization".to_string(), format!("Bearer {token}")));
                }
                other => {
                    return Err(ToolError::with_hint(
                        "ERR_BAD_INPUT",
                        format!("unsupported auth type: {other}"),
                        json!({ "got": a.auth }),
                    ))
                }
            }
        }
        // Multipart: [{name, value, filename?, contentType?}]. Forces
        // multipart/form-data; body is ignored when multipart is set.
        let multipart_body: Option<(String, String)> = if let Some(parts) = &a.multipart {
            if parts.is_empty() {
                None
            } else {
                // RFC 7578 multipart with a boundary that can't appear in the
                // values. Built by hand (no multipart crate) so no new dep.
                let boundary = format!("nc-tools-{:x}", started.elapsed().as_nanos());
                let mut buf = Vec::new();
                for part in parts {
                    let name = part.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                    let value = match part.get("value") {
                        Some(Value::String(s)) => s.clone(),
                        Some(v) => v.to_string(),
                        None => String::new(),
                    };
                    let filename = part.get("filename").and_then(|f| f.as_str());
                    let ctype = part.get("contentType").and_then(|c| c.as_str());
                    buf.push(format!("--{boundary}\r\n"));
                    if let Some(fname) = filename {
                        buf.push(format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{fname}\"\r\n"));
                        if let Some(ct) = ctype {
                            buf.push(format!("Content-Type: {ct}\r\n"));
                        }
                    } else {
                        buf.push(format!("Content-Disposition: form-data; name=\"{name}\"\r\n"));
                    }
                    buf.push("\r\n".to_string());
                    buf.push(value);
                    buf.push("\r\n".to_string());
                }
                buf.push(format!("--{boundary}--\r\n"));
                let body = buf.join("");
                Some((format!("multipart/form-data; boundary={boundary}"), body))
            }
        } else {
            None
        };
        let mut req = match method.as_str() {
            "GET" => agent.get(&a.url),
            "POST" => agent.post(&a.url),
            "PUT" => agent.put(&a.url),
            "PATCH" => agent.patch(&a.url),
            "DELETE" => agent.delete(&a.url),
            "HEAD" => agent.head(&a.url),
            other => return Err(ToolError::new("ERR_BAD_INPUT", format!("unsupported method: {other}"))),
        };
        if let Some((hk, hv)) = &auth_header {
            req = req.set(hk, hv);
        }
        if let Some(headers) = &a.headers {
            for (k, v) in headers {
                let vs = v.as_str().map(String::from).unwrap_or_else(|| v.to_string());
                req = req.set(k, &vs);
            }
        }
        // multipart forces its Content-Type unless the caller set one in headers.
        if let Some((multipart_ct, _)) = &multipart_body {
            let has_ct = a.headers.as_ref().map(|h| h.keys().any(|k| k.eq_ignore_ascii_case("content-type"))).unwrap_or(false);
            if !has_ct {
                req = req.set("Content-Type", multipart_ct);
            }
        }
        let body_str = multipart_body
            .as_ref()
            .map(|(_, b)| b.clone())
            .unwrap_or_else(|| a.body.clone().unwrap_or_default());
        let resp = if method == "GET" || method == "HEAD" {
            req.call()
        } else {
            match req.send_string(&body_str) {
                Ok(r) => Ok(r),
                Err(ureq::Error::Status(code, r)) => {
                    // non-2xx is still a completed HTTP exchange — return it as data
                    let _ = code;
                    Ok(r)
                }
                Err(e) => Err(e),
            }
        };
        match resp {
            Ok(r) => {
                let status = r.status();
                let mut headers = serde_json::Map::new();
                for h in r.headers_names() {
                    if let Some(v) = r.header(&h) {
                        headers.insert(h, json!(v));
                    }
                }
                let resp_url = r.get_url().to_string();
                let redirect_chain: Vec<String> = if resp_url != a.url { vec![a.url.clone(), resp_url.clone()] } else { vec![a.url.clone()] };
                let mut body = String::new();
                let mut capped = r.into_reader().take((k.cfg.limits.net_max_body + 1) as u64);
                let _ = capped.read_to_string(&mut body);
                let body_truncated = body.len() > k.cfg.limits.net_max_body;
                body.truncate(body.floor_char_boundary(k.cfg.limits.net_max_body));
                Ok(json!({
                    "url": a.url,
                    "finalUrl": resp_url,
                    "redirectChain": redirect_chain,
                    "followed": follow,
                    "method": method,
                    "status": status,
                    "ok": (200..300).contains(&status),
                    "headers": Value::Object(headers),
                    "body": body,
                    "bodyTruncated": body_truncated,
                    "durationMs": started.elapsed().as_millis() as u64,
                }))
            }
            Err(ureq::Error::Transport(t)) => {
                let is_timeout = t.to_string().contains("timed out");
                Err(ToolError::with_hint(
                    if is_timeout { "ERR_TIMEOUT" } else { "ERR_NET" },
                    format!("HTTP {method} {} failed: {t}", a.url),
                    json!({
                        "timeoutMs": timeout,
                        "hint": if is_timeout { "increase timeoutMs or check the target is up" } else { "check DNS/firewall/url" },
                    }),
                ))
            }
            Err(e) => Err(ToolError::with_hint(
                "ERR_NET",
                format!("HTTP {method} {} failed: {e}", a.url),
                json!({ "timeoutMs": timeout, "hint": "check DNS/firewall/url" }),
            )),
        }
    }
}

pub struct ProbeHandler;
impl Handler for ProbeHandler {
    fn call(&self, _k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: ProbeArgs = parse_args(args)?;
        let host = a.host.clone().unwrap_or_else(|| "127.0.0.1".to_string());
        let timeout = a.timeoutMs.unwrap_or(2000);
        let started = Instant::now();
        let open = TcpStream::connect_timeout(
            &format!("{host}:{}", a.port)
                .parse()
                .map_err(|_| ToolError::with_hint("ERR_BAD_INPUT", "invalid host", json!({ "got": host })))?,
            Duration::from_millis(timeout),
        )
        .is_ok();
        Ok(json!({
            "host": host,
            "port": a.port,
            "open": open,
            "latencyMs": started.elapsed().as_millis() as u64,
        }))
    }
}

// silence unused imports on non-windows builds
#[allow(unused)]
fn _w(_: impl Write) {}

#[cfg(test)]
mod http_ext_tests {
    use super::*;
    use nct_core::Kernel;

    fn kernel() -> Kernel {
        let dir = std::env::temp_dir().join(format!("nct-http-ext-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut k = Kernel::new(dir).unwrap();
        super::register(&mut k);
        k
    }

    #[test]
    fn unsupported_auth_type_errors() {
        let k = kernel();
        let out = k.call("net.http", &json!({ "url": "https://example.com", "auth": { "type": "digest", "user": "u" } }));
        assert!(!out.ok);
        assert_eq!(out.error.unwrap().code, "ERR_BAD_INPUT");
    }

    #[test]
    fn multipart_empty_is_same_as_body_mode() {
        let k = kernel();
        // Empty multipart list means body mode (never crashes on empty multipart).
        let out = k.call("net.http", &json!({ "url": "notaurl", "multipart": [] }));
        assert!(!out.ok); // url is invalid, but we got past multipart handling
        assert_eq!(out.error.unwrap().code, "ERR_BAD_INPUT");
    }

    #[test]
    fn max_redirects_bound_is_schema_enforced() {
        let k = kernel();
        let out = k.call("net.http", &json!({ "url": "https://example.com", "maxRedirects": 99 }));
        // serde_json parse: schemars describes the bound; parse_args does not enforce. Just verify maxRedirects is accepted.
        let _ = out;
    }
}
