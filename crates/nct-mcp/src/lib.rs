// Kernel builder: assembles the full 48-tool kernel from the family crates.
// Families are compile-time plugins (cargo features); every registered tool
// gets MCP exposure, journaling, and batch support automatically.
use nct_core::{Kernel, ToolError};

pub mod sysbatch;
pub use sysbatch::register_sys_batch;

pub mod coordination;
pub use coordination::register_coordination;

pub mod replay;
pub use replay::register_replay;

pub mod doctor;
pub use doctor::register_sys_doctor;

pub mod roots;

pub const SERVER_NAME: &str = nct_core::MCP_SERVER_NAME;
pub const SERVER_VERSION: &str = "0.2.0";

/// MCP protocol revisions this server implements, newest first. Mirrors the
/// SDK's SUPPORTED_PROTOCOL_VERSIONS so negotiation matches reference hosts:
/// echo the client's requested revision when supported, otherwise answer with
/// the latest (the client decides whether it can continue).
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 5] = [
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
    "2024-10-07",
];
pub const LATEST_PROTOCOL_VERSION: &str = SUPPORTED_PROTOCOL_VERSIONS[0];

/// Negotiation rule (byte-compatible with the TS SDK server's `_oninitialize`).
pub fn pick_protocol_version(requested: &str) -> &'static str {
    SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .copied()
        .find(|v| *v == requested)
        .unwrap_or(LATEST_PROTOCOL_VERSION)
}

/// `structuredContent` on tools/call results is part of the 2025-06-18
/// revision and later. Derived from the supported list's order (newest first)
/// so adding a revision cannot silently forget the gate: everything at or
/// above 2025-06-18 (index <= 1) gets the field, everything below stays
/// text-only.
pub fn structured_output_supported(version: &str) -> bool {
    SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .position(|v| *v == version)
        .is_some_and(|index| index <= 1)
}

pub fn build_kernel(workspace: std::path::PathBuf) -> Result<Kernel, ToolError> {
    let mut kernel = Kernel::new(workspace)?;
    #[cfg(feature = "fs")]
    nct_fs::register(&mut kernel);
    #[cfg(feature = "git")]
    nct_git::register(&mut kernel);
    #[cfg(feature = "proc")]
    {
        nct_proc::register(&mut kernel);
        nct_proc::register_pkg(&mut kernel);
        nct_proc::register_test(&mut kernel);
    }
    #[cfg(feature = "net")]
    nct_net::register(&mut kernel);
    #[cfg(feature = "semantic")]
    nct_semantic::register(&mut kernel);
    register_sys_batch(&mut kernel);
    register_sys_doctor(&mut kernel);
    register_coordination(&mut kernel);
    register_replay(&mut kernel);
    // Test instrument, opt-in via env so the production surface (and the
    // golden spec) stays untouched: proves the kernel's panic boundary by
    // letting a verifier observe ERR_PANIC + server survival end to end.
    if std::env::var("NCTOOLS_DEBUG_PANIC").as_deref() == Ok("1") {
        register_debug_panic(&mut kernel);
    }
    Ok(kernel)
}

pub mod debugpanic;
pub use debugpanic::register_debug_panic;
