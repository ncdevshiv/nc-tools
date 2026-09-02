// Query intent routing — the local brain of keyless search. Two-stage
// classifier: deterministic keyword fast-path first (no model needed), then
// zero-shot classification with the local MiniLM (query vs intent label
// embeddings) when the fast path is silent and the model is available.
// Routing picks the source priority per intent; "auto" search becomes
// intent-shaped instead of an indiscriminate fan-out.
use nct_semantic::Embedder;

use super::engines::Intent;

pub const LABELS: &[(Intent, &str)] = &[
    (Intent::General, "general web search for information about a topic"),
    (Intent::News, "recent news, current events, something that happened today or this week"),
    (Intent::HowTo, "how to solve a technical problem, fix an error, debug code, tutorial"),
    (Intent::Academic, "scientific research paper, academic publication, study, arxiv, doi"),
    (Intent::Package, "software package, library, npm module, crate, install a dependency"),
];

/// Keyword fast-path: cheap, deterministic, catches the strongest signals.
/// Returns None when the query doesn't scream any intent.
pub fn keyword_intent(query: &str) -> Option<Intent> {
    let q = query.to_lowercase();
    if ["npm", "crate", "pypi", "gem", "pip install", "package", "library for", "dependency"]
        .iter()
        .any(|k| q.contains(k))
    {
        return Some(Intent::Package);
    }
    if ["paper", "arxiv", "doi", "study", "research paper", "publication", "preprint", "et al"]
        .iter()
        .any(|k| q.contains(k))
    {
        return Some(Intent::Academic);
    }
    if ["news", "breaking", "today", "yesterday", "this week", "latest on", "just announced"]
        .iter()
        .any(|k| q.contains(k))
    {
        return Some(Intent::News);
    }
    if q.starts_with("how to")
        || q.starts_with("how do")
        || q.starts_with("why does")
        || q.starts_with("why do")
        || q.contains("error")
        || q.contains("fix ")
        || q.contains("debug")
        || q.contains(" not working")
        || q.contains("exception")
    {
        return Some(Intent::HowTo);
    }
    if (q.starts_with("official") && (q.contains("site") || q.contains("website") || q.contains("page")))
        || q.contains("official website")
        || q.contains("homepage")
        || q.contains("download page")
    {
        return Some(Intent::General);
    }
    None
}

/// Neural zero-shot classification when the model is available. Returns
/// (intent, confidence) — low confidence falls back to General.
pub fn neural_intent(query: &str, embedder: &Embedder) -> Option<(Intent, f64)> {
    let q = embedder.embed(query).ok()?;
    let mut best = (Intent::General, f64::MIN);
    for (intent, label) in LABELS {
        let l = embedder.embed(label).ok()?;
        let cos = dot(&q, &l);
        if cos > best.1 {
            best = (*intent, cos);
        }
    }
    if best.1 < 0.18 {
        return None; // too far from every label — don't trust the routing
    }
    Some(best)
}

/// Classify a query: (intent, method used).
pub fn classify(query: &str, embedder: Option<&Embedder>) -> (Intent, &'static str) {
    if let Some(intent) = keyword_intent(query) {
        return (intent, "keyword");
    }
    if let Some(e) = embedder {
        if let Some((intent, _conf)) = neural_intent(query, e) {
            return (intent, "neural");
        }
    }
    (Intent::General, "default")
}

/// Source priority for an intent — the head of the list is queried first and
/// gets the largest budget; the tail is backup coverage.
pub fn sources_for(intent: Intent) -> &'static [&'static str] {
    match intent {
        Intent::General => &["bing-rss", "searxng", "hn", "wikipedia", "stackexchange"],
        Intent::News => &["gnews", "bing-rss", "searxng"],
        Intent::HowTo => &["stackexchange", "hn", "bing-rss"],
        Intent::Academic => &["openalex", "arxiv", "bing-rss"],
        Intent::Package => &["npm", "crates", "bing-rss"],
    }
}

fn dot(a: &[f32], b: &[f32]) -> f64 {
    a.iter().zip(b.iter()).map(|(x, y)| (*x as f64) * (*y as f64)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_paths_cover_strong_signals() {
        assert_eq!(keyword_intent("how to fix E0509 borrow checker error"), Some(Intent::HowTo));
        assert_eq!(keyword_intent("react library for state management"), Some(Intent::Package));
        assert_eq!(keyword_intent("attention is all you need arxiv paper"), Some(Intent::Academic));
        assert_eq!(keyword_intent("latest news on rust 2026"), Some(Intent::News));
        assert_eq!(keyword_intent("what is the capital of France"), None);
        assert_eq!(keyword_intent("rust programming language official website"), Some(Intent::General));
    }

    #[test]
    fn routing_tables_exist_for_every_intent() {
        for intent in [Intent::General, Intent::News, Intent::HowTo, Intent::Academic, Intent::Package] {
            assert!(!sources_for(intent).is_empty());
        }
        assert_eq!(sources_for(Intent::Academic)[0], "openalex");
        assert_eq!(sources_for(Intent::Package)[0], "npm");
        assert_eq!(sources_for(Intent::HowTo)[0], "stackexchange");
        assert_eq!(sources_for(Intent::News)[0], "gnews");
    }
}
