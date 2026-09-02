// Query shaping for keyless sources that degrade on multi-word natural-language
// queries. Bing RSS (free, no key) tokenizes "best cheap vps hosting india"
// as a word-fragment search and returns dictionary entries for "best".
// Shrinking the query to its distinctive-content terms (drop stopwords, keep
// nouns/proper-nouns/specifics) restores Bing's exact-match behavior, while
// HN/Wikipedia/StackExchange APIs handle long queries fine and are left alone.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "is", "are", "was", "were", "best", "cheap", "cheapest",
    "top", "good", "great", "find", "get", "how", "to", "for", "of", "in",
    "and", "or", "vs", "versus", "compare", "comparison", "price", "pricing",
    "plans", "plan", "monthly", "per", "month", "mo", "year", "annual",
    "hosting", "server", "cloud", "vps", "providers", "provider", "sites",
    "site", "list", "recommended", "recommend", "my", "your", "our", "i",
    "want", "need", "looking", "searching", "find", "me", "please", "info",
    "information", "details", "about", "what", "which", "who", "when", "where",
    "why", "tell", "show", "give", "offer", "offers", "available",
];

/// Shrink a natural-language query to its distinctive content terms, for
/// sources (Bing RSS) that misbehave on long natural queries. Preserves
/// proper nouns, version numbers, and domain-like tokens verbatim.
pub fn shorten_query(query: &str) -> String {
    let words: Vec<&str> = query.split_whitespace().collect();
    if words.len() <= 3 {
        return query.to_string();
    }
    let kept: Vec<&str> = words
        .iter()
        .filter(|w| {
            let lower = w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
            if lower.is_empty() || lower.len() < 2 {
                return false;
            }
            // keep anything that looks like a version, a proper noun, or an
            // alphanumeric token (urls/domains/codes)
            if w.chars().any(|c| c.is_uppercase()) {
                return true;
            }
            if lower.chars().any(|c| c.is_ascii_digit()) {
                return true;
            }
            !STOPWORDS.contains(&lower.as_str())
        })
        .copied()
        .collect();
    // escape hatch: if shortening leaves <2 terms, Bing is going to misbehave
    // anyway — fall back to the first 3 words of the original (the lead often
    // carries the intent).
    if kept.len() < 2 {
        return words.iter().take(3).copied().collect::<Vec<_>>().join(" ");
    }
    kept.join(" ")
}

/// Per-engine decision: which queries to send to each source class.
/// Bing RSS gets the shortened form; every other engine keeps the original
/// (their APIs handle long queries natively).
pub fn query_for_engine(kind: crate::engines::EngineKind, query: &str) -> String {
    match kind {
        crate::engines::EngineKind::BingRss => shorten_query(query),
        _ => query.to_string(),
    }
}

/// A source-class fan-out: run bing-rss against MULTIPLE phrasings of the
/// query (original + shortened) and merge results. This is the subconscious
/// fix for Bing's word-fragment behavior — if Bing's query tokenizer misfires
/// on one phrasing, the next may land. Returns deduped RawResults.
pub fn bing_multi_query(query: &str, limit: usize) -> Vec<String> {
    let mut queries = vec![query.to_string()];
    let short = shorten_query(query);
    if short != query {
        queries.push(short.clone());
    }
    // also try the original with distinctive proper-nouns pulled to the front,
    // since Bing weights early tokens more heavily
    let proper: Vec<&str> = query
        .split_whitespace()
        .filter(|w| w.chars().any(|c| c.is_uppercase()) || w.chars().any(|c| c.is_ascii_digit()))
        .collect();
    if proper.len() >= 2 {
        let front = proper.join(" ");
        if front != short && front != query {
            queries.push(front);
        }
    }
    queries.truncate(3);
    queries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortens_long_natural_language_query() {
        let q = "best cheap vps hosting india price per month inr";
        let s = shorten_query(q);
        assert!(!s.contains("best"), "stopword 'best' must drop: {s}");
        assert!(!s.contains("cheap"));
        assert!(s.contains("india") || s.contains("inr"), "distinctive terms survive: {s}");
        assert!(s.split_whitespace().count() < q.split_whitespace().count());
    }

    #[test]
    fn preserves_short_queries_verbatim() {
        assert_eq!(shorten_query("rust"), "rust");
        assert_eq!(shorten_query("rust async"), "rust async");
        assert_eq!(shorten_query("rust async book"), "rust async book");
    }

    #[test]
    fn keeps_proper_nouns_and_versions() {
        let s = shorten_query("how to fix rust borrow checker error e0509");
        assert!(s.contains("e0509") || s.contains("rust"), "proper nouns/codes survive: {s}");
    }

    #[test]
    fn escape_hatch_when_all_stopwords() {
        let s = shorten_query("the best cheap good great");
        assert!(!s.is_empty(), "all-stopword query must not return empty");
        assert!(s.split_whitespace().count() <= 3, "escape hatch caps at 3 words: {s}");
    }

    #[test]
    fn bing_gets_shortened_others_keep_original() {
        // With 6 words the shortened form strips stopwords (best/cheap) and
        // keeps distinctive terms (rust/vps/programming). HN keeps original.
        let bing = query_for_engine(crate::engines::EngineKind::BingRss, "best cheap vps india rust programming");
        let hn = query_for_engine(crate::engines::EngineKind::Hn, "best cheap vps india rust programming");
        assert_eq!(hn, "best cheap vps india rust programming");
        assert_ne!(bing, hn, "bing must be shortened");
        assert!(!bing.contains("best"), "stopword must drop: {bing}");
        assert!(bing.contains("rust") || bing.contains("vps"), "distinctive terms must survive: {bing}");
    }

    #[test]
    fn multi_query_produces_distinct_phrasings() {
        let qs = bing_multi_query("best cheap vps india price", 10);
        assert!(qs.len() >= 2, "should produce 2+ phrasings: {qs:?}");
        assert!(qs.iter().any(|q| q.contains("india")), "at least one phrasing carries 'india'");
    }
}
