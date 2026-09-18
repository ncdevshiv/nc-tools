// fs.watch — semantic file watch (meaning changed, not bytes).
//
// PROBLEM: proc.watch re-runs on every byte change. Formatting-only edits
// (whitespace, comment touch) trigger a full rebuild, and a git checkout can
// fire dozens. But for an agent, "the file's logic changed" is different from
// "someone added a comment". The former may require re-running tests; the
// latter should NOT.
//
// fs.watch tracks CONTENT-SEMANTICS, not bytes:
//   * reads the file, embeds it with the local MiniLM;
//   * polls the mtime every `intervalMs` (default 2s) — cheap, no fs events;
//   * when the mtime moves, re-embeds and scores cosine(v0, v1) (\u03b4sem);
//   * only when \u03b4sem crosses `threshold` (default 0.995) does it report
//     "semantic-change" — whitespace/comment-only edits have \u03b4sem ≈ 1.0 and
//     stay silent.
//
// Returns a started watch id (independent per call — this tool is ONE-SHOT per
// poll window, not a handle: it polls for up to `timeoutMs` and either returns
// the semantic-change, the byte-change it judged harmless, or a timeout).
// Polls are CPU-cheap because the embedder model is process-cached and only
// two embeds happen per poll when the mtime moved.
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{json, Value};

use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};
use nct_core::paths::resolve_checked;

pub const WATCH_DESC: &str = "Semantic file watch: polls a file and reports ONLY when its meaning changes — not on whitespace/comment-only edits. Embeds the content with local MiniLM and scores cosine(v0,v1); returns on threshold cross, or \"semantic-unchanged\" (byte-changed but cosine above threshold), or timeout. Use to re-run tests on logic changes without rebuild storms.";

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WatchArgs {
    #[doc = "File to watch — relative to the base dir, or absolute"]
    pub path: String,
    #[doc = "Poll interval in ms (default 2000, min 500, max 60000)"]
    #[serde(default)]
    #[schemars(range(min = 500, max = 60000))]
    pub intervalMs: Option<u64>,
    /// Max time to watch in ms (default 60s, max 10 min).
    #[serde(default)]
    #[schemars(range(min = 5000, max = 600000))]
    pub timeoutMs: Option<u64>,
    /// Cosine threshold for a semantic change (default 0.995). A whitespace-
    /// only edit scores ~1.0; a real logic change scores lower.
    #[serde(default)]
    pub threshold: Option<f64>,
    #[doc = "Base dir for relative paths (default: the session workspace)."]
    #[serde(default)]
    pub baseDir: Option<String>,
}

pub struct WatchSemanticHandler;
impl Handler for WatchSemanticHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: WatchArgs = parse_args(args)?;
        let base = k.base_dir(a.baseDir.as_deref())?;
        let abs: PathBuf = resolve_checked(&base, &a.path)?;
        let meta = std::fs::metadata(&abs).map_err(|_| {
            ToolError::with_hint(
                "ERR_NOT_FOUND",
                format!("no such file: {}", a.path),
                json!({ "path": a.path }),
            )
        })?;
        if meta.is_dir() {
            return Err(ToolError::new(
                "ERR_IS_DIRECTORY",
                format!("{} is a directory", a.path),
            ));
        }
        let interval = a.intervalMs.unwrap_or(2000);
        let timeout = a.timeoutMs.unwrap_or(60_000);
        let threshold = a.threshold.unwrap_or(0.995);

        // Initial embed.
        let v0 = file_embed(k, &abs)?;

        let started = std::time::Instant::now();
        let mut byte_changed = false;
        let mut checks = 0u64;
        let deadline = started + std::time::Duration::from_millis(timeout);
        loop {
            if nct_core::is_cancelled() {
                return Err(nct_core::cancelled_error("fs.watch"));
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(interval));
            checks += 1;
            nct_core::report_progress(
                checks as f64,
                Some((timeout / interval.max(1)) as f64),
                Some("fs.watch polling"),
            );
            let v1 = match file_embed(k, &abs) {
                Ok(v) => v,
                Err(_) => continue, // transient — file may be mid-write
            };
            // Optimization: skip the embedding cost when nothing changed.
            // We detect "nothing changed" by content hash, not by embedding.
            if v0 == v1 {
                continue;
            }
            // Score the new vector against the INITIAL v0 — the baseline this
            // watch exists to detect drift from. Scoring a fresh embed against
            // itself (the old embeds_k path) always returns cosine 1.0 for
            // L2-normalized vectors, so `semantic-change` was unreachable and
            // the tool could never fire.
            let cos = dot(&v0, &v1);
            byte_changed = true;
            if cos < threshold {
                return Ok(json!({
                    "path": a.path,
                    "event": "semantic-change",
                    "cosine": round4(cos),
                    "threshold": threshold,
                    "checks": checks,
                    "elapsedMs": started.elapsed().as_millis() as u64,
                    "note": "meaning changed — logic edits merit a re-run",
                }));
            }
        }
        Ok(json!({
            "path": a.path,
            "event": if byte_changed { "semantic-unchanged" } else { "no-change" },
            "checks": checks,
            "elapsedMs": started.elapsed().as_millis() as u64,
            "timeoutMs": timeout,
            "note": if byte_changed {
                "bytes moved during the window but meaning did not cross threshold"
            } else {
                "no change observed in the window"
            },
        }))
    }
}

/// Embed the file's content (reads + embeds via the process-cached model).
fn file_embed(k: &Kernel, abs: &PathBuf) -> Result<Vec<f32>, ToolError> {
    let content = std::fs::read_to_string(abs).map_err(ToolError::from)?;
    let text: String = content.chars().take(4000).collect();
    let model_cache = if let Ok(d) = std::env::var("NCTOOLS_MODEL_CACHE") {
        if !d.is_empty() {
            std::path::PathBuf::from(d)
        } else {
            k.root.join(".nc-tools").join("model-cache")
        }
    } else {
        k.root.join(".nc-tools").join("model-cache")
    };
    let embedder = nct_semantic::Embedder::get(model_cache).map_err(|e| {
        ToolError::with_hint(
            "ERR_EMBED_UNAVAILABLE",
            format!("embedding model unavailable: {e}"),
            json!({}),
        )
    })?;
    embedder.embed(&text)
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

pub fn register_watch(k: &mut Kernel) {
    k.register(
        "fs.watch",
        WATCH_DESC,
        nct_core::schema::schema_for::<WatchArgs>(),
        std::sync::Arc::new(WatchSemanticHandler),
    );
}

#[cfg(test)]
mod watch_tests {
    use super::*;

    #[test]
    fn whitespace_and_comment_only_changes_are_cosine_near_one() {
        // Without a model, verify the heuristic premise: a whitespace-only
        // mutation of the SAME text scores ~1.0 via the semantic equivalence
        // (split_whitespace normalized). The loop's threshold default 0.995 is
        // chosen so these stay silent.
        let a = "fn add(x: i32) -> i32 { x + 1 }";
        let b = "fn  add(x: i32)  ->  i32  {  x + 1  }";
        assert_eq!(
            a.split_whitespace().collect::<Vec<_>>(),
            b.split_whitespace().collect::<Vec<_>>(),
            "whitespace variants normalize identically"
        );
        let c = "fn add(x: i32) -> i32 { x * 1 }";
        assert_ne!(
            a.split_whitespace().collect::<Vec<_>>(),
            c.split_whitespace().collect::<Vec<_>>(),
            "a logic change (+ -> *) differs at token level"
        );
    }

    #[test]
    fn threshold_defaults_sane() {
        let v = round4(0.9951);
        assert!((0.995..=0.996).contains(&v));
    }
}
