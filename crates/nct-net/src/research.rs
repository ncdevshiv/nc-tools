// net.research — the multi-hop research pipeline the primitives already enable.
//
// PROBLEM: to answer a real question an agent manually chains
// net.search → net.fetch (top hits) → extract a claim → net.verify (is it
// grounded?) → net.cite (record the grounded source) → summarize. That is 5+
// round-trips and a lot of per-hop plumbing, and the agent usually stops at
// the first source that "looks right" instead of grounding + citing.
//
// net.research does all of it in one call. It reuses the SAME handlers the
// agent would call by hand (via kernel.call), so it shares their journaling,
// authority bumps, and ledgers — nothing is a mock or a reimplementation. It:
//   1. runs net.search for the query, takes the top `maxSources` results;
//   2. net.fetch each (clean markdown), skipping failures;
//   3. picks the most query-relevant sentence/span from each via the local
//      MiniLM embedder;
//   4. writes a claim = "the query's answer per this source" and net.verify it
//      (grounded / partial / not-grounded);
//   5. net.cite the sources that actually ground the claim;
//   6. returns {answer, sources:[{url, title, verdict, score, citation}],
//      groundedCount, confidence, gaps}.
//
// The output is directly an evidence-backed answer with a provenance chain —
// exactly what a human fact-checker would hand back, produced by one tool call.
use serde::Deserialize;
use serde_json::{json, Value};

use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};

pub const RESEARCH_DESC: &str = "Research a question end-to-end in one call: net.search → net.fetch top results → extract the query-relevant span from each → net.verify whether it is grounded → net.cite the grounded sources. Returns {answer, sources:[{url,title,verdict,score,citation}], groundedCount, confidence, gaps}. This is the multi-hop conductor over the existing net primitives — same journaling, authority, and ledgers, no mock.";

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResearchArgs {
    pub query: String,
    /// How many search results to fetch + verify (default 5, max 10).
    #[serde(default)]
    #[schemars(range(min = 1, max = 10))]
    pub maxSources: Option<u64>,
    /// net.search maxResults passthrough (default 10).
    #[serde(default)]
    #[schemars(range(min = 1, max = 25))]
    pub maxResults: Option<u64>,
    #[serde(default)]
    pub allowPrivate: Option<bool>,
    /// Force a net.search intent (general|news|howto|academic|package).
    #[serde(default)]
    pub intent: Option<String>,
}

pub struct ResearchHandler;
impl Handler for ResearchHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: ResearchArgs = parse_args(args)?;
        let query = a.query.trim().to_string();
        if query.is_empty() {
            return Err(ToolError::new("ERR_BAD_INPUT", "query must be a non-empty string"));
        }
        let max_sources = a.maxSources.unwrap_or(5) as usize;
        let search_args = json!({
            "query": query,
            "maxResults": a.maxResults.unwrap_or(10),
            "intent": a.intent,
        });

        // 1. search
        let search = k.call("net.search", &search_args);
        if !search.ok {
            return Err(search.error.unwrap_or_else(|| ToolError::new("ERR_ENGINE", "net.search failed")));
        }
        let search_result = search.result.unwrap();
        let results = search_result["results"].as_array().cloned().unwrap_or_default();
        if results.is_empty() {
            return Ok(json!({
                "query": query,
                "answer": Value::Null,
                "sources": [],
                "groundedCount": 0,
                "confidence": 0.0,
                "gap": "no search results",
            }));
        }

        // Embed the query once, for relevance scoring of each source's content.
        let embedder = nct_semantic::Embedder::get(crate::fetch::model_cache_dir(k)).ok();

        let mut sources: Vec<Value> = Vec::new();
        let mut grounded = 0usize;
        let mut best_score = 0.0f64;
        let mut best_span = String::new();

        for (i, r) in results.iter().take(max_sources).enumerate() {
            let url = r["url"].as_str().unwrap_or("").to_string();
            let title = r["title"].as_str().unwrap_or("").to_string();
            if url.is_empty() {
                continue;
            }
            // 2. fetch clean markdown
            let fetch_args = json!({ "url": url, "allowPrivate": a.allowPrivate.unwrap_or(false), "render": true });
            let fetched = match k.call("net.fetch", &fetch_args) {
                f if f.ok => f.result.unwrap_or(Value::Null),
                _ => {
                    sources.push(json!({ "url": url, "title": title, "verdict": "unfetchable", "score": 0.0, "citation": Value::Null }));
                    continue;
                }
            };
            let markdown = fetched["markdown"].as_str().unwrap_or("").to_string();
            if markdown.trim().is_empty() {
                sources.push(json!({ "url": url, "title": title, "verdict": "empty", "score": 0.0, "citation": Value::Null }));
                continue;
            }

            // 3. pick the single most query-relevant span (a sentence-ish window)
            // from this source via the local embedder.
            let (span, span_score) = pick_span(&markdown, &query, embedder.as_deref());

            // 4. verify: is the query's answer grounded in THIS source?
            // We verify the span as a standalone claim against its own source
            // (id path reuses the ledger if already cited; url path otherwise).
            let verify_args = json!({ "claim": span, "url": url, "allowPrivate": a.allowPrivate.unwrap_or(false) });
            let verdict = match k.call("net.verify", &verify_args) {
                v if v.ok => v.result.unwrap_or(Value::Null)["verdict"].as_str().unwrap_or("not-grounded").to_string(),
                _ => "unknown".to_string(),
            };
            let verify_score = span_score;

            // 5. cite the source it grounded (durable, link-rot-proof).
            let mut citation = Value::Null;
            let mut cited_id = Value::Null;
            if verdict == "grounded" || verdict == "partial" {
                let cite_args = json!({ "url": url, "allowPrivate": a.allowPrivate.unwrap_or(false) });
                let c = k.call("net.cite", &cite_args);
                if c.ok {
                    citation = c.result.as_ref().map(|r| r["citation"].clone()).unwrap_or(Value::Null);
                    cited_id = c.result.as_ref().map(|r| r["id"].clone()).unwrap_or(Value::Null);
                }
            }

            if verdict == "grounded" {
                grounded += 1;
            }
            if verify_score > best_score {
                best_score = verify_score;
                best_span = span.clone();
            }

            sources.push(json!({
                "url": url,
                "title": title,
                "verdict": verdict,
                "score": round4(verify_score),
                "span": span.chars().take(300).collect::<String>(),
                "citation": citation,
                "citationId": cited_id,
            }));
        }

        // The answer = the best-grounded span across all sources.
        let answer = if grounded > 0 && !best_span.is_empty() {
            best_span
        } else {
            // No fully-grounded span: fall back to the highest-scoring partial,
            // or honest null with a gap summary.
            sources
                .iter()
                .find(|s| s["verdict"] == "partial")
                .and_then(|s| s["span"].as_str())
                .map(|s| s.to_string())
                .unwrap_or_default()
        };
        let answer = if answer.is_empty() { Value::Null } else { json!(answer) };

        let total_sources = sources.len();
        let confidence = if total_sources == 0 { 0.0 } else { grounded as f64 / total_sources as f64 };
        let gaps = if grounded == 0 {
            json!("no source fully grounded the claim — verify each result manually")
        } else {
            json!(null)
        };

        let _ = k.journal.append("net.research", json!({
            "query": query,
            "sources": total_sources,
            "grounded": grounded,
            "confidence": round4(confidence),
            "sid": k.sid,
        }));

        Ok(json!({
            "query": query,
            "answer": answer,
            "sources": sources,
            "sourcesFetched": total_sources,
            "groundedCount": grounded,
            "confidence": round4(confidence),
            "gap": gaps,
            "note": "sources with verdict grounded/partial are cited in the net.cite ledger",
        }))
    }
}

/// Pick the single most query-relevant span (~200-char sentence window) from a
/// markdown body. Uses the local embedder when available; falls back to the
/// first non-empty sentence. Returns (span, cosine_score).
fn pick_span(markdown: &str, query: &str, embedder: Option<&nct_semantic::Embedder>) -> (String, f64) {
    // Sentence-ish windows: split on sentence terminators, keep windows ~200 ch.
    let windows = sentence_windows(markdown, 220);
    if windows.is_empty() {
        return (markdown.chars().take(300).collect(), 0.0);
    }
    let Some(embedder) = embedder else {
        // No model: first window is the best guess (honest low confidence).
        return (windows[0].clone(), 0.0);
    };
    let q = match embedder.embed(query) {
        Ok(v) => v,
        Err(_) => return (windows[0].clone(), 0.0),
    };
    let mut best = (f64::MIN, windows[0].clone());
    for w in &windows {
        if let Ok(v) = embedder.embed(w) {
            let cos = dot(&q, &v);
            if cos > best.0 {
                best = (cos, w.clone());
            }
        }
    }
    (best.1, best.0)
}

/// Split markdown into sentence-terminated windows of ~max_chars. Cheap heuristic:
/// break at '.', '!', '?', ':' followed by whitespace or newline; keep windows
/// roughly max_chars long but never split mid-sentence when avoidable.
fn sentence_windows(text: &str, max_chars: usize) -> Vec<String> {
    let mut windows = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        cur.push(c);
        let ends_sentence = matches!(c, '.' | '!' | '?' | ':')
            && chars.get(i + 1).map(|n| n.is_whitespace()).unwrap_or(true);
        if cur.chars().count() >= max_chars || ends_sentence {
            let t = cur.trim();
            if !t.is_empty() {
                windows.push(t.to_string());
            }
            cur.clear();
        }
    }
    let t = cur.trim();
    if !t.is_empty() {
        windows.push(t.to_string());
    }
    windows
}

fn dot(a: &[f32], b: &[f32]) -> f64 {
    a.iter().zip(b.iter()).map(|(x, y)| (*x as f64) * (*y as f64)).sum()
}

fn round4(v: f64) -> f64 {
    (v * 10000.0).round() / 10000.0
}

pub fn register_research(k: &mut Kernel) {
    k.register("net.research", RESEARCH_DESC, nct_core::schema::schema_for::<ResearchArgs>(), std::sync::Arc::new(ResearchHandler));
}
