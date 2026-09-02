// Structured error taxonomy: stable machine-readable code, human message,
// structured remediation hint. Codes are byte-stable protocol identifiers
// (see docs/PROTOCOL.md §5).
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
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ToolError {}

impl From<std::io::Error> for ToolError {
    fn from(e: std::io::Error) -> Self {
        ToolError::new("ERR_INTERNAL", e.to_string())
    }
}

impl From<serde_json::Error> for ToolError {
    fn from(e: serde_json::Error) -> Self {
        ToolError::new("ERR_INTERNAL", e.to_string())
    }
}
