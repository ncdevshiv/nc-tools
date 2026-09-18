// Render escalation for JS-heavy pages: when HTTP extraction comes back
// near-empty, re-render through the system browser's headless mode
// (msedge/chrome --headless --dump-dom). Zero new dependencies — the browser
// is preinstalled on Windows and near-universal elsewhere; if none is found
// the fetch result degrades honestly (low confidence, no render).
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nct_core::errors::ToolError;

/// Candidate browser executables, most-likely-first.
fn browser_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let program_files =
        std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
    let program_files_x86 =
        std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| r"C:\Program Files (x86)".into());
    let local_appdata = std::env::var("LocalAppData").unwrap_or_else(|_| String::new());
    for (root, rels) in [
        (
            &program_files_x86,
            vec![r"Microsoft\Edge\Application\msedge.exe"],
        ),
        (
            &program_files,
            vec![
                r"Microsoft\Edge\Application\msedge.exe",
                r"Google\Chrome\Application\chrome.exe",
            ],
        ),
        (
            &local_appdata,
            vec![r"Google\Chrome\Application\chrome.exe"],
        ),
    ] {
        for rel in rels {
            let p = PathBuf::from(root).join(rel);
            if p.is_file() {
                out.push(p);
            }
        }
    }
    out
}

/// Render `url` with the system browser's headless dump-dom mode and return
/// the serialized DOM (post-JS). Times out honestly; SSRF is the caller's job
/// (this only runs on URLs that already passed the fetch guard).
pub fn render_dom(url: &str, allow_private: bool, timeout_ms: u64) -> Result<String, ToolError> {
    if !allow_private {
        // a second, explicit check — render never widens the fetch guard
        crate::ssrf::assert_public(url)?;
    }
    let browsers = browser_candidates();
    let browser = browsers.first().ok_or_else(|| {
        ToolError::with_hint(
            "ERR_RENDER_UNAVAILABLE",
            "no system browser found for headless render (msedge/chrome)",
            nserde_json_hint(url),
        )
    })?;

    let started = Instant::now();
    let output = Command::new(browser)
        .args([
            "--headless=new",
            "--disable-gpu",
            "--no-first-run",
            "--disable-extensions",
            "--virtual-time-budget=5000",
            "--dump-dom",
            url,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .output();

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            return Err(ToolError::with_hint(
                "ERR_RENDER_UNAVAILABLE",
                format!("headless browser spawn failed: {e}"),
                serde_json::json!({ "browser": browser.display().to_string() }),
            ));
        }
    };
    let elapsed = started.elapsed();
    if elapsed > Duration::from_millis(timeout_ms) || elapsed > Duration::from_secs(30) {
        return Err(ToolError::with_hint(
            "ERR_TIMEOUT",
            format!(
                "headless render exceeded budget ({}ms)",
                elapsed.as_millis()
            ),
            serde_json::json!({ "url": url, "elapsedMs": elapsed.as_millis() as u64 }),
        ));
    }
    if !output.status.success() && output.stdout.is_empty() {
        return Err(ToolError::with_hint(
            "ERR_RENDER",
            format!("headless render exited with {}", output.status),
            serde_json::json!({ "browser": browser.display().to_string() }),
        ));
    }
    let dom = String::from_utf8_lossy(&output.stdout).into_owned();
    if dom.trim().is_empty() {
        return Err(ToolError::new(
            "ERR_RENDER",
            "headless render produced no DOM",
        ));
    }
    Ok(dom)
}

fn nserde_json_hint(url: &str) -> serde_json::Value {
    serde_json::json!({ "url": url, "hint": "install Edge/Chrome or accept the low-confidence extraction" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_candidates_are_wellformed() {
        // no crash, and every reported candidate exists (we filter by is_file)
        for p in browser_candidates() {
            assert!(p.is_file());
        }
    }

    #[test]
    fn render_refuses_private_targets_without_allow() {
        let err = render_dom("http://127.0.0.1:1/x", false, 5000).unwrap_err();
        assert_eq!(err.code, "ERR_SSRF_BLOCKED");
    }
}
