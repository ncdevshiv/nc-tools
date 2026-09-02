// Structured error taxonomy: stable machine-readable code, human message,
// structured remediation hint. Codes are byte-stable protocol identifiers
// (see docs/PROTOCOL.md §5) — every code on the wire is registered in the
// `codes` module below, which is byte-matched against PROTOCOL.md §5 by test.
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub struct ToolError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<Value>,
}

impl ToolError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        ToolError { code: code.to_string(), message: message.into(), hint: None }
    }

    pub fn with_hint(code: &str, message: impl Into<String>, hint: Value) -> Self {
        ToolError { code: code.to_string(), message: message.into(), hint: Some(hint) }
    }

    /// Attach an already-built hint object (e.g. from parse_args field
    /// extraction) without changing code or message.
    pub fn with_value_hint(mut self, hint: serde_json::Map<String, Value>) -> Self {
        self.hint = Some(Value::Object(hint));
        self
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ToolError {}

/// The single source of truth for every error code that may appear on the
/// wire. Tools reference these constants; docs/PROTOCOL.md §5 documents the
/// same set byte-for-byte (enforced by the tests at the bottom of this file),
/// so adding a code without documenting it — or renaming one silently — fails
/// the build.
pub mod codes {
    pub const BAD_INPUT: &str = "ERR_BAD_INPUT";
    pub const BAD_EDIT: &str = "ERR_BAD_EDIT";
    pub const BAD_PATH: &str = "ERR_BAD_PATH";
    pub const BAD_REGEX: &str = "ERR_BAD_REGEX";
    pub const CMD_NOT_FOUND: &str = "ERR_CMD_NOT_FOUND";
    pub const EMBED_UNAVAILABLE: &str = "ERR_EMBED_UNAVAILABLE";
    pub const ENGINE: &str = "ERR_ENGINE";
    pub const GIT: &str = "ERR_GIT";
    pub const GIT_SPAWN: &str = "ERR_GIT_SPAWN";
    pub const INTERNAL: &str = "ERR_INTERNAL";
    pub const IS_DIRECTORY: &str = "ERR_IS_DIRECTORY";
    pub const NET: &str = "ERR_NET";
    pub const NETWORK: &str = "ERR_NETWORK";
    pub const NOT_A_REPO: &str = "ERR_NOT_A_REPO";
    pub const NOT_FOUND: &str = "ERR_NOT_FOUND";
    pub const NO_KEY: &str = "ERR_NO_KEY";
    pub const NO_TESTS: &str = "ERR_NO_TESTS";
    pub const PANIC: &str = "ERR_PANIC";
    pub const PARSE: &str = "ERR_PARSE";
    pub const PERMISSION: &str = "ERR_PERMISSION";
    pub const PKG: &str = "ERR_PKG";
    pub const PROC_NOT_FOUND: &str = "ERR_PROC_NOT_FOUND";
    pub const REFUSED: &str = "ERR_REFUSED";
    pub const RENDER: &str = "ERR_RENDER";
    pub const RENDER_UNAVAILABLE: &str = "ERR_RENDER_UNAVAILABLE";
    pub const SPAWN: &str = "ERR_SPAWN";
    pub const SSRF_BLOCKED: &str = "ERR_SSRF_BLOCKED";
    pub const TEST_PARSE: &str = "ERR_TEST_PARSE";
    pub const TIMEOUT: &str = "ERR_TIMEOUT";
    pub const UNKNOWN_HANDLE: &str = "ERR_UNKNOWN_HANDLE";
    pub const UNKNOWN_SNAPSHOT: &str = "ERR_UNKNOWN_SNAPSHOT";
    pub const UNKNOWN_TOOL: &str = "ERR_UNKNOWN_TOOL";
    pub const PATCH_AMBIGUOUS: &str = "PATCH_AMBIGUOUS";
    pub const PATCH_NO_MATCH: &str = "PATCH_NO_MATCH";

    /// Every registered code. Order matches the PROTOCOL.md §5 table.
    pub const ALL: &[&str] = &[
        BAD_INPUT,
        BAD_EDIT,
        BAD_PATH,
        BAD_REGEX,
        CMD_NOT_FOUND,
        EMBED_UNAVAILABLE,
        ENGINE,
        GIT,
        GIT_SPAWN,
        INTERNAL,
        IS_DIRECTORY,
        NET,
        NETWORK,
        NOT_A_REPO,
        NOT_FOUND,
        NO_KEY,
        NO_TESTS,
        PANIC,
        PARSE,
        PERMISSION,
        PKG,
        PROC_NOT_FOUND,
        REFUSED,
        RENDER,
        RENDER_UNAVAILABLE,
        SPAWN,
        SSRF_BLOCKED,
        TEST_PARSE,
        TIMEOUT,
        UNKNOWN_HANDLE,
        UNKNOWN_SNAPSHOT,
        UNKNOWN_TOOL,
        PATCH_AMBIGUOUS,
        PATCH_NO_MATCH,
    ];
}

/// Map an io::Error to the specific code its kind implies, instead of the
/// blanket ERR_INTERNAL — a missing file is ERR_NOT_FOUND, a denial is
/// ERR_PERMISSION, a timeout is ERR_TIMEOUT. Unmapped kinds stay INTERNAL.
pub fn from_io_error(e: std::io::Error) -> ToolError {
    use std::io::ErrorKind::*;
    let code = match e.kind() {
        NotFound => codes::NOT_FOUND,
        PermissionDenied => codes::PERMISSION,
        TimedOut | WouldBlock => codes::TIMEOUT,
        ConnectionRefused => codes::REFUSED,
        InvalidInput | InvalidData | UnexpectedEof => codes::BAD_INPUT,
        _ => codes::INTERNAL,
    };
    ToolError::new(code, e.to_string())
}

impl From<std::io::Error> for ToolError {
    fn from(e: std::io::Error) -> Self {
        from_io_error(e)
    }
}

impl From<serde_json::Error> for ToolError {
    fn from(e: serde_json::Error) -> Self {
        ToolError::new(codes::INTERNAL, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_no_duplicates() {
        let mut seen = std::collections::BTreeSet::new();
        for c in codes::ALL {
            assert!(seen.insert(*c), "duplicate code in registry: {c}");
        }
    }

    /// docs/PROTOCOL.md §5 is the wire contract: its code table must list
    /// exactly the registry — byte-for-byte, both directions.
    #[test]
    fn registry_matches_protocol_doc() {
        let doc = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/PROTOCOL.md"
        ))
        .expect("PROTOCOL.md readable from crates/nct-core");
        let mut documented: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for line in doc.lines() {
            let t = line.trim_start();
            if let Some(rest) = t.strip_prefix("| `") {
                if let Some(code) = rest.split('`').next() {
                    if code.starts_with("ERR_") || code.starts_with("PATCH_") {
                        documented.insert(code);
                    }
                }
            }
        }
        let registry: std::collections::BTreeSet<&str> = codes::ALL.iter().copied().collect();
        let missing_in_doc: Vec<_> = registry.difference(&documented).collect();
        let stale_in_doc: Vec<_> = documented.difference(&registry).collect();
        assert!(
            missing_in_doc.is_empty(),
            "codes used in code but missing from PROTOCOL.md §5: {missing_in_doc:?}"
        );
        assert!(
            stale_in_doc.is_empty(),
            "codes documented in PROTOCOL.md §5 but not in the registry: {stale_in_doc:?}"
        );
    }

    #[test]
    fn io_errors_map_to_specific_codes() {
        assert_eq!(from_io_error(std::io::Error::from(std::io::ErrorKind::NotFound)).code, "ERR_NOT_FOUND");
        assert_eq!(
            from_io_error(std::io::Error::from(std::io::ErrorKind::PermissionDenied)).code,
            "ERR_PERMISSION"
        );
        assert_eq!(from_io_error(std::io::Error::from(std::io::ErrorKind::TimedOut)).code, "ERR_TIMEOUT");
        assert_eq!(
            from_io_error(std::io::Error::from(std::io::ErrorKind::ConnectionRefused)).code,
            "ERR_REFUSED"
        );
        assert_eq!(
            from_io_error(std::io::Error::from(std::io::ErrorKind::InvalidInput)).code,
            "ERR_BAD_INPUT"
        );
        assert_eq!(
            from_io_error(std::io::Error::other("esoteric")).code,
            "ERR_INTERNAL"
        );
    }
}
