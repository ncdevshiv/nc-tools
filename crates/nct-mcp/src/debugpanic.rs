// debug.panic — opt-in test instrument (NCTOOLS_DEBUG_PANIC=1) that lets the
// error-verifier prove the kernel's panic boundary end to end: the call must
// come back as ERR_PANIC with the injected message, and the server must keep
// serving afterwards. Never registered in the default surface.
use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};
use serde::Deserialize;
use serde_json::Value;

pub const DEBUG_PANIC_DESC: &str = "TEST INSTRUMENT (NCTOOLS_DEBUG_PANIC=1 only): deliberately panics to prove ERR_PANIC recovery — the server must return a structured error and survive.";

pub fn register_debug_panic(k: &mut Kernel) {
    k.register(
        "debug.panic",
        DEBUG_PANIC_DESC,
        nct_core::schema::schema_for::<DebugPanicArgs>(),
        std::sync::Arc::new(DebugPanicHandler),
    );
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DebugPanicArgs {
    #[doc = "Message to embed in the panic payload (root-cause proof)"]
    #[serde(default)]
    pub message: Option<String>,
}

pub struct DebugPanicHandler;
impl Handler for DebugPanicHandler {
    fn call(&self, _k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: DebugPanicArgs = parse_args(args)?;
        let msg = a
            .message
            .unwrap_or_else(|| "debug.panic: intentional test panic".to_string());
        panic!("{msg}");
    }
}
