// Path resolution for the global tool system — mirrors src/kernel/paths.mjs.
// Absolute paths are accepted anywhere (no jail); relative paths resolve
// against the base dir. `lex_normalize` removes `.`/`..` segments textually
// the way Node's path.resolve does (no filesystem round-trip), and
// `is_reparse_point` detects symlinks/junctions so recursive walks cannot
// loop on cycles.
use std::path::{Path, PathBuf};

use dunce::canonicalize;

use crate::errors::ToolError;

/// resolve_path + the ERR_BAD_PATH guard for empty input — the form every
/// path-taking handler uses.
pub fn resolve_checked(base: &Path, p: &str) -> Result<PathBuf, ToolError> {
    if p.is_empty() {
        return Err(ToolError::new("ERR_BAD_PATH", "path must be a non-empty string"));
    }
    resolve_path(base, p).map_err(|e| ToolError::new("ERR_BAD_PATH", e.to_string()))
}

/// Resolve a user-supplied path against the base (paths.mjs resolvePath).
/// Absolute paths are used as-is; relative resolve against `base`.
/// Non-string/empty input is rejected by the caller's typed args (serde),
/// matching ERR_BAD_PATH for empty strings.
pub fn resolve_path(base: &Path, p: &str) -> std::io::Result<PathBuf> {
    if p.is_empty() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "path must be a non-empty string"));
    }
    let candidate = Path::new(p);
    if candidate.is_absolute() {
        Ok(lex_normalize(&canonicalize_loose(candidate)))
    } else {
        Ok(lex_normalize(&canonicalize_loose(&base.join(candidate))))
    }
}

/// Node path.resolve semantics: collapse `.` and `..` textually without
/// touching the filesystem (so `../escape.txt` resolves even when the target
/// does not exist yet — the no-jail conformance case depends on this).
pub fn lex_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Canonicalize if the path exists; otherwise canonicalize the deepest
/// existing ancestor and append the remainder (paths.mjs resolve() analog:
/// resolve does not require existence).
fn canonicalize_loose(p: &Path) -> PathBuf {
    match canonicalize(p) {
        Ok(c) => c,
        Err(_) => {
            let mut anc = p.to_path_buf();
            let mut tail: Vec<std::ffi::OsString> = Vec::new();
            while !anc.as_os_str().is_empty() {
                match canonicalize(&anc) {
                    Ok(c) => {
                        let mut out = c;
                        for t in tail.iter().rev() {
                            out.push(t);
                        }
                        return out;
                    }
                    Err(_) => {
                        let name = anc.file_name().map(|s| s.to_os_string());
                        anc.pop();
                        if let Some(n) = name {
                            tail.push(n);
                        }
                    }
                }
            }
            p.to_path_buf()
        }
    }
}

/// True if abs is a reparse point (symlink or junction): see the platform
/// impls. The Windows attribute check is authoritative and cheap; a
/// canonicalize-vs-textual comparison misfires on 8.3 short names
/// (NCDEVS~1 → Ncdevshiv) and would wrongly prune every subdirectory under
/// an aliased path component.
#[cfg(windows)]
pub fn is_reparse_point(abs: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    match std::fs::symlink_metadata(abs) {
        // FILE_ATTRIBUTE_REPARSE_POINT (junctions and symlinks)
        Ok(m) => m.file_attributes() & 0x400 != 0,
        Err(_) => false,
    }
}

/// Non-Windows port of the canonicalize-vs-textual comparison (symlinks are
/// already skipped by callers, so only mount/bind aliases land here).
#[cfg(not(windows))]
pub fn is_reparse_point(abs: &Path) -> bool {
    match canonicalize(abs) {
        Ok(real) => fold(&real) != fold(&lex_normalize(abs)),
        Err(_) => false,
    }
}

#[cfg(not(windows))]
fn fold(p: &Path) -> String {
    p.to_string_lossy().to_string()
}
