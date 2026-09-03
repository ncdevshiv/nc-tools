// Kernel builder: assembles the full 48-tool kernel from the family crates.
// Families are compile-time plugins (cargo features); every registered tool
// gets MCP exposure, journaling, and batch support automatically.
use nct_core::{Kernel, ToolError};

pub mod sysbatch;
pub use sysbatch::register_sys_batch;

pub mod coordination;
pub use coordination::register_coordination;

pub mod doctor;
pub use doctor::register_sys_doctor;

pub const SERVER_NAME: &str = nct_core::MCP_SERVER_NAME;
pub const SERVER_VERSION: &str = "0.2.0";

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
