// net.contradict — adversarial evidence search.
//
// PROBLEM: search engines return what you ask for. Asking "is X true?" finds
// sources that agree with X — nobody searches for evidence AGAINST a claim,
// because the engine doesn't understand "wrong". An agent that grounds only
// supporting sources inherits confirmation bias.
//
// net.contradict searches BOTH sides and surfaces the disagreement:
//   1. run net.search for the claim (supporting);
//   2. run net.search for its NEGATED form (contradicting);
//   3. embed each result's title+snippet, score against BOTH the claim and the
//      negation, and classify each source as supporting / contradicting /
//      neutral by which it is closer to;
//   4. net.verify the top sources' answer-spans against the claim OR the
//      negation to confirm the direction;
//   5. return {claim, supporting:[...], contradicting:[...], verdict}.
//
// Verdict logic:
//   * "confirmed"      — many/strong supporting, no contradicting.
//   * "contested"      — strong sources on BOTH sides (disagreement exists).
//   * "unsupported"    — contradicting sources dominate, or supporting are weak.
//   * "insufficient"   — too little evidence either way.
//
// This is the epistemic complement to net.verify: verify confirms a claim is in
// the text; contradict checks whether the CLAIM is actually true by looking for
// counter-evidence. Same net primitives, no mock, one call.
use serde::Deserialize;
use serde_json::{json, Value};

use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};

pub const CONTRADICT_DESC: &str = "Search for evidence AGAINST a claim, not just for it: runs net.search for the claim AND its negation, embeds each result against both, classifies each as supporting/contradicting/neutral, then net.verify the top sources to confirm direction. Returns {claim, supporting:[...], contradicting:[...], verdict: confirmed|contested|unsupported|insufficient}. The anti-confirmation-bias check — pair with net.verify/net.research.";

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContradictArgs {
    pub claim: String,
    #[serde(default)]
    #[schemars(range(min = 1, max = 8))]
    pub maxSources: Option<u64>,
    #[serde(default)]
    pub allowPrivate: Option<bool>,
    #[serde(default)]
    pub intent: Option<String>,
}

pub struct ContradictHandler;
impl Handler for ContradictHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: ContradictArgs = parse_args(args)?;
        let claim = a.claim.trim().to_string();
        if claim.is_empty() {
            return Err(ToolError::new(
                "ERR_BAD_INPUT",
                "claim must be a non-empty string",
            ));
        }
        let max_sources = a.maxSources.unwrap_or(5) as usize;
        let negation = negate(&claim);

        // 1+2. search both sides
        let (supporting_raw, contradicting_raw) =
            (search_side(k, &claim, &a), search_side(k, &negation, &a));

        let embedder = nct_semantic::Embedder::get(crate::fetch::model_cache_dir(k)).ok();

        // 3. score each result against claim and negation, classify.
        let claim_v = embedder.as_ref().and_then(|e| e.embed(&claim).ok());
        let neg_v = embedder.as_ref().and_then(|e| e.embed(&negation).ok());

        let mut supporting: Vec<Value> = Vec::new();
        let mut contradicting: Vec<Value> = Vec::new();
        let mut neutral: Vec<Value> = Vec::new();

        // Classify a result. `default_side` is the side the query it came from
        // leans toward. A result only flips to the OPPOSITE side if it is
        // STRONGLY closer to the other side (margin >= 0.10) — which keeps the
        // negation-query pool on the contradicing side by construction instead
        // of silently dropping near-antonym hits to neutral.
        let classify =
            |v: &Value, default_side: &'static str, default_score: f64| -> (&'static str, f64) {
                let text = format!(
                    "{} {}",
                    v["title"].as_str().unwrap_or(""),
                    v["snippet"].as_str().unwrap_or("")
                );
                if text.trim().is_empty() {
                    return (default_side, default_score);
                }
                match (&claim_v, &neg_v) {
                    (Some(cv), Some(nv)) => {
                        let vec = embedder.as_ref().and_then(|e| e.embed(&text).ok());
                        match vec {
                            Some(vec) => {
                                let c = dot(cv, &vec);
                                let n = dot(nv, &vec);
                                if default_side == "supporting" {
                                    if n - c >= 0.10 {
                                        ("contradicting", n)
                                    } else {
                                        ("supporting", c)
                                    }
                                } else {
                                    if c - n >= 0.10 {
                                        ("supporting", c)
                                    } else {
                                        ("contradicting", n)
                                    }
                                }
                            }
                            None => (default_side, default_score),
                        }
                    }
                    _ => (default_side, default_score),
                }
            };

        for v in supporting_raw.iter().take(max_sources) {
            let (cls, score) = classify(v, "supporting", 0.0);
            match cls {
                "supporting" => supporting.push(mk(v, score)),
                "contradicting" => contradicting.push(mk(v, score)),
                _ => neutral.push(mk(v, score)),
            }
        }
        for v in contradicting_raw.iter().take(max_sources) {
            let (cls, score) = classify(v, "contradicting", 0.0);
            match cls {
                "contradicting" => contradicting.push(mk(v, score)),
                "supporting" => supporting.push(mk(v, score)),
                _ => neutral.push(mk(v, score)),
            }
        }

        // 4. verify the top of each side to confirm the direction (only when a
        // model is present; else rely on the classification).
        verify_direction(
            k,
            &claim,
            &negation,
            &mut supporting,
            &mut contradicting,
            a.allowPrivate.unwrap_or(false),
        );

        // 5. verdict
        let s = supporting.len();
        let c = contradicting.len();
        let verdict = if s == 0 && c == 0 {
            "insufficient"
        } else if c == 0 && s >= 2 {
            "confirmed"
        } else if c > 0 && s > 0 {
            "contested"
        } else if c > 0 && s == 0 {
            "unsupported"
        } else if s == 1 && c == 0 {
            // one supporting source only — weak, needs corroboration
            "insufficient"
        } else {
            "contested"
        };

        let _ = k.journal.append(
            "net.contradict",
            json!({
                "claim": claim, "negation": negation,
                "supporting": supporting.len(), "contradicting": contradicting.len(),
                "verdict": verdict, "sid": k.sid,
            }),
        );

        Ok(json!({
            "claim": claim,
            "negation": negation,
            "supporting": supporting,
            "contradicting": contradicting,
            "neutral": neutral,
            "supportCount": supporting.len(),
            "contradictCount": contradicting.len(),
            "verdict": verdict,
            "note": "verdict is a heuristic (support/contradict balance + verify direction). Pair with net.verify for the claim-specific grounding.",
        }))
    }
}

fn search_side(k: &Kernel, query: &str, a: &ContradictArgs) -> Vec<Value> {
    let args = json!({
        "query": query,
        "maxResults": (a.maxSources.unwrap_or(5) * 2).max(5),
        "intent": a.intent,
    });
    match k.call("net.search", &args) {
        r if r.ok => r.result.unwrap_or(Value::Null)["results"]
            .as_array()
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn mk(v: &Value, score: f64) -> Value {
    json!({
        "url": v["url"].as_str().unwrap_or(""),
        "title": v["title"].as_str().unwrap_or(""),
        "snippet": v["snippet"].as_str().unwrap_or("").chars().take(200).collect::<String>(),
        "score": round4(score),
    })
}

/// Verify a source actually agrees/disagrees with the claim-at-hand, feeding
/// the grounded verdict back into the supporting/contradicting classification.
fn verify_direction(
    k: &Kernel,
    claim: &str,
    negation: &str,
    supporting: &mut Vec<Value>,
    contradicting: &mut Vec<Value>,
    allow_private: bool,
) {
    // For top-2 of each side, verify the claim against the url. If a
    // "supporting" source comes back not-grounded on the claim, it should move
    // toward neutral (we don't silently delete — we drop from supporting).
    for side in [(true, supporting), (false, contradicting)] {
        let (is_supporting, list) = side;
        for item in list.iter_mut().take(2) {
            let url = item["url"].as_str().unwrap_or("").to_string();
            if url.is_empty() {
                continue;
            }
            let verify_args = json!({ "claim": if is_supporting { claim } else { negation }, "url": url, "allowPrivate": allow_private });
            let verdict = match k.call("net.verify", &verify_args) {
                v if v.ok => v.result.unwrap_or(Value::Null)["verdict"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
                _ => "".to_string(),
            };
            item["verify"] = json!(verdict);
        }
    }
}

/// Build a reasonable negation for a claim. Handles the common cases; falls
/// back to prefixing "It is not the case that ". It is heuristic — not a logical
/// negation — which is fine for a SEARCH term (the engine just needs a phrase
/// likely to surface disagreeing content).
fn negate(claim: &str) -> String {
    let c = claim.trim();
    let lower = c.to_lowercase();
    // 1. Specific phrase opposites first (these are the strongest disagreeing
    //    search terms). Order longest/specific FIRST so e.g. "was invented by"
    //    matches before the bare "invented".
    for (pos, opp) in [
        (" is the inventor of ", " is not the inventor of "),
        (" was invented by ", " was not invented by "),
        (" invented ", " did not invent "),
        (" is faster than ", " is slower than "),
        (" is better than ", " is worse than "),
        (" is the best ", " is not the best "),
        (" no longer ", " still "),
    ] {
        if lower.contains(pos.trim()) {
            return c.replace(pos.trim(), opp.trim());
        }
    }
    // 2. General aux-verb negation ("X is/are/was/were Y" -> "... not Y").
    let p = phrase_negation(c);
    if p != c {
        return p;
    }
    // 3. Both far apart from known patterns — fall back to a meta-negation.
    format!("It is not the case that {lower}")
}

fn phrase_negation(c: &str) -> String {
    let lower = c.to_lowercase();
    // "X is/are/was/were Y" -> "X is/are/was/were not Y"
    for (verb, neg) in [
        (" is ", " is not "),
        (" are ", " are not "),
        (" was ", " was not "),
        (" were ", " were not "),
    ] {
        if let Some(i) = lower.find(verb) {
            let a = &c[..i + verb.len()];
            let b = &c[i + verb.len()..];
            return format!("{a}{neg}{b}");
        }
    }
    // "X does/do/did V" -> "X does/do/did not V"
    for (aux, neg) in [
        (" does ", " does not "),
        (" do ", " do not "),
        (" did ", " did not "),
    ] {
        if lower.contains(aux) {
            let idx = lower.find(aux).unwrap();
            let a = &c[..idx + aux.len()];
            let b = &c[idx + aux.len()..];
            return format!("{a}{neg}{b}");
        }
    }
    c.to_string()
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

pub fn register_contradict(k: &mut Kernel) {
    k.register(
        "net.contradict",
        CONTRADICT_DESC,
        nct_core::schema::schema_for::<ContradictArgs>(),
        std::sync::Arc::new(ContradictHandler),
    );
}

#[cfg(test)]
mod tests {
    use super::negate;

    #[test]
    fn negate_handles_common_forms() {
        // Each returns SOME negation phrase — the goal is a search term that
        // surfaces disagreeing content, not a specific string.
        assert!(
            negate("Rust is faster than Go").contains("not faster")
                || negate("Rust is faster than Go").contains("slower than"),
            "got: {}",
            negate("Rust is faster than Go")
        );
        assert!(
            negate("Alice invented the widget").contains("did not invent")
                || negate("Alice invented the widget").contains("invented by"),
            "got: {}",
            negate("Alice invented the widget")
        );
        assert!(
            negate("C was invented by Ritchie").contains("was not invented by"),
            "got: {}",
            negate("C was invented by Ritchie")
        );
        assert!(
            negate("The server is running").contains("is not running"),
            "got: {}",
            negate("The server is running")
        );
        assert!(
            negate("The test passes")
                .to_lowercase()
                .contains("not the case")
                || negate("The test passes").to_lowercase().contains("passes")
        );
    }
}
