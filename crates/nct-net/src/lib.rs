// net.* — typed network operations: hardened HTTP, agent-grade fetch+extract,
// keyless federated search, robots/llms.txt, and port probing.
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};

pub mod cache;
pub mod cite;
pub mod engines;
pub mod extract;
pub mod fetch;
pub mod httpx;
pub mod render;
pub mod robots;
pub mod ssrf;

pub const HTTP_DESC: &str = "Perform a raw HTTP request. Returns status, headers, body (capped), duration, redirect chain. blockPrivate=true refuses private/loopback targets. Replaces curl/wget; use net.fetch when you want readable page content.";
pub const PROBE_DESC: &str = "Check whether a TCP port is open. Replaces nc/netstat probing.";

pub fn register(k: &mut Kernel) {
    k.register("net.http", HTTP_DESC, nct_core::schema::schema_for::<HttpArgs>(), std::sync::Arc::new(HttpHandler));
    k.register("net.probePort", PROBE_DESC, nct_core::schema::schema_for::<ProbeArgs>(), std::sync::Arc::new(ProbeHandler));
    fetch::register(k);
    cite::register(k);
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
        let started = Instant::now();
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_millis(timeout))
            .build();
        let mut req = match method.as_str() {
            "GET" => agent.get(&a.url),
            "POST" => agent.post(&a.url),
            "PUT" => agent.put(&a.url),
            "PATCH" => agent.patch(&a.url),
            "DELETE" => agent.delete(&a.url),
            "HEAD" => agent.head(&a.url),
            other => return Err(ToolError::new("ERR_BAD_INPUT", format!("unsupported method: {other}"))),
        };
        if let Some(headers) = &a.headers {
            for (k, v) in headers {
                let vs = v.as_str().map(String::from).unwrap_or_else(|| v.to_string());
                req = req.set(k, &vs);
            }
        }
        let resp = if method == "GET" || method == "HEAD" {
            req.call()
        } else {
            match req.send_string(&a.body.unwrap_or_default()) {
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
                let mut body = String::new();
                let mut capped = r.into_reader().take((k.cfg.limits.net_max_body + 1) as u64);
                let _ = capped.read_to_string(&mut body);
                let body_truncated = body.len() > k.cfg.limits.net_max_body;
                body.truncate(body.floor_char_boundary(k.cfg.limits.net_max_body));
                Ok(json!({
                    "url": a.url,
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
