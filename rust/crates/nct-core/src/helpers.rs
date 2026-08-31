// Small shared helpers: timestamps, digests, platform string, relative paths.
use std::path::Path;

use sha2::{Digest, Sha256};

/// ISO-8601 UTC with milliseconds and trailing Z — matches JS toISOString().
pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Milliseconds since the Unix epoch — matches JS Date.now().
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Full lowercase hex SHA-256.
pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{:02x}", b)).collect()
}

/// process.platform-compatible value: win32 / linux / darwin / freebsd / ...
pub fn platform_str() -> &'static str {
    if cfg!(windows) {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "freebsd") {
        "freebsd"
    } else {
        "unknown"
    }
}

/// Relative path from root to abs with forward slashes (JS relative(root, p)
/// plus backslash-to-slash conversion). Falls back to the absolute path
/// when there is no common ancestor.
pub fn rel_slash(root: &Path, abs: &Path) -> String {
    let rel = pathdiff::diff_paths(abs, root).unwrap_or_else(|| abs.to_path_buf());
    rel.to_string_lossy().replace('\\', "/")
}

/// Absolute path as a string with forward slashes (used for keys and output).
pub fn fwd(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}
