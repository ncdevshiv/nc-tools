// Client workspace anchoring over the MCP `roots` protocol (spec 2024-11-05+
// capabilities.roots): after initialize, the SERVER asks the CLIENT for its
// workspace roots via a `roots/list` request. This module parses that response
// — there is intentionally NO support for rootUri/rootPath/workspaceFolders in
// the initialize request: those are LSP fields, not MCP, and spec-compliant
// hosts never send them. Anchoring is applied as a session-scoped DEFAULT
// base (Kernel::set_default_base), never by mutating the server root, so
// journal/coordination side-channels keep their startup home.
use std::path::PathBuf;

/// Parse a workspace candidate string into a path. Accepts plain filesystem
/// paths and `file://` URIs (Windows-hostile forms included: `file:///C:/...`
/// and `file://C:/...`). Returns None for empty/non-path strings.
pub fn parse_workspace_candidate(s: &str) -> Option<PathBuf> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix("file://") {
        // Percent-decode the URI path (spaces as %20 etc.). A URL crate
        // dependency is deliberately avoided: file URIs are simple enough
        // that decode + host-strip covers every observed client form.
        let decoded = percent_decode(rest);
        let mut p = decoded.as_str();
        // file:///C:/x -> /C:/x ; drop the leading slash on Windows drive
        // forms. file:///unix/path -> /unix/path keeps its slash.
        #[cfg(windows)]
        {
            if let Some(stripped) = p.strip_prefix('/') {
                let bytes = stripped.as_bytes();
                if bytes.len() >= 2 && bytes[1] == b':' {
                    p = stripped;
                }
            }
        }
        #[cfg(not(windows))]
        {
            // A non-local host part (file://server/share) is not a path.
            if let Some(after_host) = p.strip_prefix('/') {
                if p.starts_with('/') && !after_host.starts_with('/') && p.contains('/') {
                    // keep as-is (absolute local path)
                }
            }
        }
        let candidate = PathBuf::from(p);
        if candidate.as_os_str().is_empty() {
            return None;
        }
        return Some(candidate);
    }
    Some(PathBuf::from(trimmed))
}

/// Minimal percent-decoding for URI path components.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Extract the first filesystem-directory root from a `roots/list` RESULT
/// object: `{ roots: [{ uri: "file:///path" | plain path, name? }] }`.
/// Plain paths are accepted alongside file URIs (several hosts send raw
/// paths despite the schema saying uri). Non-plain-string uri forms that do
/// not look like paths (http://, socket://) are skipped.
pub fn first_root_from_result(result: &serde_json::Value) -> Option<PathBuf> {
    let roots = result.get("roots")?.as_array()?;
    for r in roots {
        let raw = r.get("uri").or_else(|| r.get("path")).and_then(|v| v.as_str());
        let Some(raw) = raw else { continue };
        // Only file:// URIs or strings that start like a filesystem path.
        let looks_like_path = raw.starts_with("file://")
            || raw.starts_with('/')
            || raw.starts_with("\\\\")
            || (raw.len() >= 2 && raw.as_bytes()[1] == b':');
        if !looks_like_path {
            continue;
        }
        if let Some(p) = parse_workspace_candidate(raw) {
            if p.is_dir() {
                return Some(p);
            }
        }
    }
    None
}

/// True when the client's initialize `capabilities` declare the roots
/// capability — the ONLY spec-correct signal that roots/list will work.
pub fn client_supports_roots(capabilities: &serde_json::Value) -> bool {
    capabilities.get("roots").is_some()
}

#[cfg(test)]
mod roots_tests {
    use super::*;
    use serde_json::json;

    #[cfg(windows)]
    #[test]
    fn parses_file_uris_on_windows() {
        assert_eq!(
            parse_workspace_candidate("file:///C:/Users/me/proj"),
            Some(PathBuf::from("C:/Users/me/proj"))
        );
        assert_eq!(
            parse_workspace_candidate("file://C:/Users/me/proj"),
            Some(PathBuf::from("C:/Users/me/proj"))
        );
        assert_eq!(
            parse_workspace_candidate("file:///C:/Users/my%20proj"),
            Some(PathBuf::from("C:/Users/my proj"))
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn parses_file_uris_on_unix() {
        assert_eq!(
            parse_workspace_candidate("file:///home/me/proj"),
            Some(PathBuf::from("/home/me/proj"))
        );
        assert_eq!(
            parse_workspace_candidate("file:///home/me/my%20proj"),
            Some(PathBuf::from("/home/me/my proj"))
        );
    }

    #[test]
    fn parses_plain_paths_and_rejects_garbage() {
        #[cfg(windows)]
        assert_eq!(
            parse_workspace_candidate("F:\\some\\repo"),
            Some(PathBuf::from("F:\\some\\repo"))
        );
        #[cfg(not(windows))]
        assert_eq!(
            parse_workspace_candidate("/some/repo"),
            Some(PathBuf::from("/some/repo"))
        );
        assert_eq!(parse_workspace_candidate(""), None);
        assert_eq!(parse_workspace_candidate("   "), None);
    }

    #[test]
    fn extracts_first_existing_dir_root() {
        let dir = std::env::temp_dir().join(format!(
            "nct-roots-extract-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // canonicalize up front: temp_dir() can hand back an 8.3 short path,
        // and the comparison below is against the canonical (long) form
        let dir = dunce::canonicalize(&dir).unwrap_or(dir);
        let uri = if cfg!(windows) {
            format!("file:///{}", dir.display().to_string().replace('\\', "/"))
        } else {
            format!("file://{}", dir.display())
        };
        let result = json!({ "roots": [
            { "uri": "http://not-a-path", "name": "skip" },
            { "uri": "/definitely/missing/dir", "name": "missing" },
            { "uri": uri, "name": "workspace" },
        ]});
        let got = first_root_from_result(&result).unwrap();
        assert_eq!(got, dunce::canonicalize(&dir).unwrap_or(dir.clone()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_roots_gives_none() {
        assert_eq!(first_root_from_result(&json!({ "roots": [] })), None);
        assert_eq!(first_root_from_result(&json!({})), None);
    }

    #[test]
    fn roots_capability_detection() {
        assert!(client_supports_roots(&json!({ "roots": { "listChanged": true } })));
        assert!(client_supports_roots(&json!({ "roots": {} })));
        assert!(!client_supports_roots(&json!({ "tools": {} })));
        assert!(!client_supports_roots(&json!({})));
    }
}
