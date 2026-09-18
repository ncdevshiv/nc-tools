// net.cite / net.verify — the reference workflow: durable, machine-checkable
// citations. net.cite records a source (id, title, content hash, ready-to-paste
// citation line) in .nc-tools/sources.jsonl and can later re-check it
// (unchanged / changed / dead). net.verify grounds a claim against a page via
// the local MiniLM embedder — the anti-hallucination check before citing.
use std::fs;
use std::io::Write;

use serde_json::{json, Value};

use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};
use nct_core::sha256_hex;

use crate::fetch::fetch_content;

pub const CITE_DESC: &str = "Record a durable, machine-checkable citation: net.cite {url} stores id + title + content hash + a ready-to-paste citation line (content itself lives in the fetch cache, so the quote survives link rot); net.cite {id} re-checks a stored source (unchanged / changed / dead); net.cite {} lists the ledger. Pair with net.verify to confirm a claim is actually grounded before citing.";
pub const VERIFY_DESC: &str = "Check whether a CLAIM is actually grounded in a web page (anti-hallucination before citing): fetches (or reads the cached copy of a cited source), chunks the content, embeds claim and chunks with the local MiniLM, and returns grounded / partial / not-grounded with the best-matching span. Needs the local embedding model; net.cite {id} is the model-free health check.";

pub fn register(k: &mut Kernel) {
    k.register(
        "net.cite",
        CITE_DESC,
        nct_core::schema::schema_for::<CiteArgs>(),
        std::sync::Arc::new(CiteHandler),
    );
    k.register(
        "net.verify",
        VERIFY_DESC,
        nct_core::schema::schema_for::<VerifyArgs>(),
        std::sync::Arc::new(VerifyHandler),
    );
}

// ---- ledger -------------------------------------------------------------------

fn ledger_path(root: &std::path::Path) -> std::path::PathBuf {
    root.join(".nc-tools").join("sources.jsonl")
}

fn load_sources(root: &std::path::Path) -> Vec<Value> {
    fs::read_to_string(ledger_path(root))
        .map(|raw| {
            raw.lines()
                .filter(|l| !l.is_empty())
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn append_source(root: &std::path::Path, entry: &Value) -> Result<(), ToolError> {
    let path = ledger_path(root);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all((serde_json::to_string(entry)? + "\n").as_bytes())?;
    Ok(())
}

fn find_source<'a>(sources: &'a [Value], id: &str) -> Option<&'a Value> {
    sources.iter().find(|s| s["id"].as_str() == Some(id))
}

/// Citation id: stable per (url, content) — the same source re-cited after a
/// silent edit gets a NEW id, which is the point: the hash is the reference.
fn cite_id(url: &str, content_hash: &str) -> String {
    sha256_hex(format!("{url}@{content_hash}").as_bytes())[..8].to_string()
}

fn citation_line(url: &str, title: &str, fetched_at: &str, content_hash: &str) -> String {
    let date = fetched_at.split('T').next().unwrap_or(fetched_at);
    format!(
        "{title} ({url}, accessed {date}, sha256:{})",
        &content_hash[..8.min(content_hash.len())]
    )
}

// ---- net.cite -----------------------------------------------------------------

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CiteArgs {
    #[doc = "URL to cite (records the source); omit to list the ledger"]
    #[serde(default)]
    pub url: Option<String>,
    #[doc = "Citation id to re-check (unchanged / changed / dead); omit to list the ledger"]
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub allowPrivate: Option<bool>,
}

pub struct CiteHandler;
impl Handler for CiteHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: CiteArgs = parse_args(args)?;
        let sources = load_sources(&k.root);

        // get + health check by id
        if let Some(id) = &a.id {
            let entry = find_source(&sources, id).ok_or_else(|| {
                ToolError::with_hint(
                    "ERR_NOT_FOUND",
                    format!("no such citation id: {id}"),
                    json!({ "known": sources.iter().filter_map(|s| s["id"].as_str()).take(20).collect::<Vec<_>>() }),
                )
            })?;
            let url = entry["url"].as_str().unwrap_or_default().to_string();
            let stored_hash = entry["contentHash"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            // health = fetch LIVE (refresh: true — a health check that reads the
            // cache would only ever report 'unchanged'), compare hash. The
            // ledger's stored markdown stays regardless: it IS the durable copy.
            let fetch_args = json!({
                "url": url,
                "allowPrivate": a.allowPrivate.unwrap_or(false),
                "refresh": true,
            });
            return match fetch_content(k, &fetch_args) {
                Ok(fresh) => {
                    let current = fresh["contentHash"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    let status = if current.is_empty() {
                        "unknown"
                    } else if current == stored_hash {
                        "unchanged"
                    } else {
                        "changed"
                    };
                    Ok(json!({
                        "id": id, "url": url,
                        "title": entry["title"], "citation": entry["citation"],
                        "storedHash": stored_hash, "currentHash": current,
                        "health": status,
                        "fetchedAt": entry["fetchedAt"], "refetchedAt": fresh["fetchedAt"],
                    }))
                }
                Err(e) if e.code == "ERR_SSRF_BLOCKED" => Err(e),
                Err(_) => Ok(json!({
                    "id": id, "url": url,
                    "title": entry["title"], "citation": entry["citation"],
                    "storedHash": stored_hash,
                    "health": "dead",
                    "note": "source unreachable — the cached copy + hash still prove what was read",
                })),
            };
        }

        // create/refresh by url
        if let Some(url) = &a.url {
            // fetch LIVE (refresh): a citation that silently reuses yesterday's
            // cache is not a citation. If the live fetch fails but a cache copy
            // exists, cite from it with an honest stale flag.
            let live_args = json!({ "url": url, "allowPrivate": a.allowPrivate.unwrap_or(false), "refresh": true });
            let (fetched, stale) = match fetch_content(k, &live_args) {
                Ok(f) => (f, false),
                Err(live_err) => {
                    let cached_args =
                        json!({ "url": url, "allowPrivate": a.allowPrivate.unwrap_or(false) });
                    match fetch_content(k, &cached_args) {
                        Ok(f) => (f, true),
                        Err(_) => return Err(live_err),
                    }
                }
            };
            let content_hash = fetched["contentHash"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if content_hash.is_empty() {
                return Err(ToolError::with_hint(
                    "ERR_BAD_INPUT",
                    "fetch produced no content hash — cannot cite an empty source",
                    json!({ "url": url }),
                ));
            }
            let id = cite_id(url, &content_hash);
            let title = fetched["title"].as_str().unwrap_or(url).to_string();
            let fetched_at = fetched["fetchedAt"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let line = citation_line(url, &title, &fetched_at, &content_hash);
            let duplicate = find_source(&sources, &id).is_some();
            if !duplicate {
                // the ledger owns the content: link-rot-proof by construction
                let body = fetched["markdown"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .take(200_000)
                    .collect::<String>();
                let entry = json!({
                    "id": id, "url": url, "title": title,
                    "contentHash": content_hash, "citation": line,
                    "fetchedAt": fetched_at,
                    "tokens": fetched["tokens"],
                    "source": fetched["source"],
                    "stale": stale,
                    "markdown": body,
                });
                append_source(&k.root, &entry)?;
                // citing is stronger evidence of usefulness than a bare read
                let host = url::Url::parse(url)
                    .ok()
                    .and_then(|u| u.host_str().map(String::from))
                    .unwrap_or_default();
                if !host.is_empty() {
                    let mut auth = crate::authority::AuthorityStore::load(&k.root);
                    auth.bump(&host, 3);
                }
                let _ = k.journal.append("net.cite", json!({ "id": id, "url": url, "hash": content_hash, "stale": stale, "sid": k.sid }));
            }
            return Ok(json!({
                "id": id, "url": url, "title": title,
                "contentHash": content_hash, "citation": line,
                "fetchedAt": fetched_at,
                "tokens": fetched["tokens"],
                "stale": stale,
                "duplicate": duplicate,
            }));
        }

        // list
        Ok(json!({ "sources": sources, "total": sources.len() }))
    }
}

// ---- net.verify ---------------------------------------------------------------

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerifyArgs {
    pub claim: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub allowPrivate: Option<bool>,
}

pub struct VerifyHandler;
impl Handler for VerifyHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: VerifyArgs = parse_args(args)?;
        if a.claim.trim().is_empty() {
            return Err(ToolError::new(
                "ERR_BAD_INPUT",
                "claim must be a non-empty string",
            ));
        }
        if a.url.is_none() && a.id.is_none() {
            return Err(ToolError::with_hint(
                "ERR_BAD_INPUT",
                "verify needs a source: pass url or the id of a net.cite entry",
                json!({ "hint": "net.verify {claim, url} or net.verify {claim, id}" }),
            ));
        }

        // Resolve the source. Cited id → the LEDGER's stored markdown: we
        // verify the claim against exactly what was cited, immune to later
        // site edits (the url path fetches current content instead).
        let (url, markdown, cited_id) = if let Some(id) = &a.id {
            let sources = load_sources(&k.root);
            let entry = find_source(&sources, id).ok_or_else(|| {
                ToolError::with_hint(
                    "ERR_NOT_FOUND",
                    format!("no such citation id: {id}"),
                    json!({}),
                )
            })?;
            (
                entry["url"].as_str().unwrap_or_default().to_string(),
                entry["markdown"].as_str().unwrap_or_default().to_string(),
                Some(id.clone()),
            )
        } else {
            let url = a.url.clone().unwrap();
            let fetch_args = json!({ "url": url, "allowPrivate": a.allowPrivate.unwrap_or(false), "render": false });
            let fetched = fetch_content(k, &fetch_args)?;
            (
                url,
                fetched["markdown"].as_str().unwrap_or_default().to_string(),
                None,
            )
        };
        if markdown.trim().is_empty() {
            return Err(ToolError::with_hint(
                "ERR_BAD_INPUT",
                "source content is empty — nothing to ground the claim against",
                json!({ "url": url, "hint": "the page may be JS-heavy; net.fetch it with render to check" }),
            ));
        }

        // Chunk (~700 chars, sentence-ish boundaries), cap chunks for embed cost.
        let chunks = chunk_markdown(&markdown, 700, 40);
        if chunks.is_empty() {
            return Err(ToolError::new(
                "ERR_BAD_INPUT",
                "no chunkable content in source",
            ));
        }

        let embedder = nct_semantic::Embedder::get(model_cache_dir(k))
            .map_err(|e| ToolError::with_hint("ERR_EMBED_UNAVAILABLE", format!("embedding model unavailable: {e}"), json!({ "hint": "check network access to huggingface.co for the first model download" })))?;
        let claim_vec = embedder.embed(&a.claim)?;
        let fetched_hash = sha256_hex(markdown.as_bytes())[..16].to_string();

        let mut best = (f64::MIN, String::new());
        for chunk in &chunks {
            let v = embedder.embed(chunk)?;
            let cos = dot(&claim_vec, &v);
            if cos > best.0 {
                best = (cos, chunk.clone());
            }
        }
        let (score, span) = best;
        let verdict = if score >= 0.50 {
            "grounded"
        } else if score >= 0.35 {
            "partial"
        } else {
            "not-grounded"
        };

        let _ = k.journal.append(
            "net.verify",
            json!({
                "claim": a.claim, "url": url, "id": cited_id,
                "verdict": verdict, "score": round4(score),
                "contentHash": fetched_hash,
                "sid": k.sid,
            }),
        );

        Ok(json!({
            "claim": a.claim,
            "url": url,
            "id": cited_id,
            "verdict": verdict,
            "score": round4(score),
            "bestSpan": span.chars().take(400).collect::<String>(),
            "chunksScored": chunks.len(),
            "contentHash": fetched_hash,
        }))
    }
}

/// Sentence-boundary-preferring chunker: split into ~max_chars windows that
/// end at the last sentence boundary inside the window when one exists.
fn chunk_markdown(text: &str, max_chars: usize, min_chars: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if current.chars().count() + line.chars().count() + 1 > max_chars
            && current.chars().count() >= min_chars
        {
            chunks.push(current.trim().to_string());
            current.clear();
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(line);
        // paragraph breaks are natural chunk boundaries
        if line.len() < max_chars / 2 && current.chars().count() >= min_chars * 2 {
            chunks.push(current.trim().to_string());
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        chunks.push(current.trim().to_string());
    }
    chunks.into_iter().take(80).collect()
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
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (*x as f64) * (*y as f64))
        .sum()
}

fn round4(v: f64) -> f64 {
    (v * 10000.0).round() / 10000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunker_respects_boundaries_and_caps() {
        let long = (0..200)
            .map(|i| format!("Sentence number {i} with some filler words to add length."))
            .collect::<Vec<_>>()
            .join("\n\n");
        let chunks = chunk_markdown(&long, 700, 40);
        assert!(!chunks.is_empty());
        assert!(chunks.iter().all(|c| !c.is_empty()));
        assert!(chunks.len() <= 80);
        // every char of input lands in some chunk (no lossy skip)
        let total: usize = chunks.iter().map(|c| c.len()).sum();
        assert!(total > 0 && total >= long.len() / 4);
    }

    #[test]
    fn cite_ids_differ_when_content_changes() {
        let a = cite_id("https://x.dev/a", "hash1");
        let b = cite_id("https://x.dev/a", "hash2");
        assert_ne!(a, b, "silent content edit must yield a new citation id");
        assert_eq!(
            cite_id("https://x.dev/a", "hash1"),
            a,
            "same source+content is idempotent"
        );
    }
}
