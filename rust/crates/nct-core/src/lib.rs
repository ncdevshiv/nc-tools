// nc-tools core: error taxonomy, config, paths, journal, session env,
// registry + kernel. The Rust port of src/kernel/* — behavior-parity with
// the JS implementation is graded by conformance/golden + tools/conformance.mjs.
pub mod childenv;
pub mod config;
pub mod errors;
pub mod helpers;
pub mod journal;
pub mod kernel;
pub mod paths;
pub mod schema;
pub mod session;

pub use errors::ToolError;
pub use helpers::{fwd, now_iso, now_ms, platform_str, rel_slash, sha256_hex};
pub use kernel::{parse_args, CallOutcome, Handler, Hook, Kernel, ToolEntry};
pub use paths::{is_reparse_point, resolve_checked, resolve_path};
pub use schema::{plain_object_schema, schema_for};
pub use session::SessionEnv;

/// Windows creation flag to hide child console windows (node windowsHide:true).
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
