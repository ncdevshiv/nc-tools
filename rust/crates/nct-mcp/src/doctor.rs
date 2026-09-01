// sys.doctor — one-call health/diagnostics for the kernel: tool inventory,
// config limits, session env, journal stats, and (deep) runtime checks.
use nct_core::kernel::{parse_args, Handler, Kernel};
use nct_core::errors::ToolError;
use serde::Deserialize;
use serde_json::{json, Value};
use std::process::Command;

pub const DOCTOR_DESC: &str = "Self-diagnostics: registered tools, config limits, session env, journal stats. deep=true also probes runtime (workspace writable, cargo on PATH).";

pub fn register_sys_doctor(k: &mut Kernel) {
    k.register("sys.doctor", DOCTOR_DESC, nct_core::schema::schema_for::<DoctorArgs>(), std::sync::Arc::new(DoctorHandler));
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DoctorArgs {
    #[doc = "Include runtime probes (writable root, cargo available)"]
    #[serde(default)]
    pub deep: Option<bool>,
}

pub struct DoctorHandler;
impl Handler for DoctorHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: DoctorArgs = parse_args(args)?;
        let deep = a.deep.unwrap_or(false);
        let tools = k.list_tools();
        let events = k.journal.last_n(None);
        let ev_count = events.len();
        let last_seq = events.last().and_then(|e| e.get("seq")).and_then(|s| s.as_u64()).unwrap_or(0);
        let env = k.session_env.snapshot();
        let env_keys: Vec<String> = env.iter().map(|(k, _)| k.clone()).collect();
        let limits = &k.cfg.limits;
        let mut out = json!({
            "version": env!("CARGO_PKG_VERSION"),
            "platform": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "sid": k.sid,
            "root": k.root.display().to_string(),
            "tools": {
                "count": tools.len(),
                "names": tools,
            },
            "limits": {
                "readLimit": limits.read_limit,
                "fsMany": limits.fs_many,
                "patchMany": limits.patch_many,
                "batchMax": limits.batch_max,
                "grepMaxResults": limits.grep_max_results,
                "spawnTimeoutMs": limits.spawn_timeout_ms,
                "spawnTimeoutMaxMs": limits.spawn_timeout_max_ms,
                "procOutputBytes": limits.proc_output_bytes,
                "procMaxDurationMs": limits.proc_max_duration_ms,
                "procHandleOutputBytes": limits.proc_handle_output_bytes,
                "netMaxBody": limits.net_max_body,
                "childTimeoutMs": limits.child_timeout_ms,
                "listDepth": limits.list_depth,
                "walkDepth": limits.walk_depth,
                "testTimeoutMs": limits.test_timeout_ms,
                "procListMax": limits.proc_list_max,
            },
            "env": {
                "count": env.len(),
                "keys": env_keys,
            },
            "journal": {
                "path": k.journal.file_path.display().to_string(),
                "eventCount": ev_count,
                "lastSeq": last_seq,
            },
        });
        if deep {
            let root_writable = probe_write(&k.root.join(".nc-tools-doctor-probe"));
            let temp_writable = probe_write(&std::env::temp_dir().join("nct-doctor-probe.tmp"));
            let cargo = Command::new("cargo")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            out["checks"] = json!({
                "baseWritable": root_writable,
                "tempWritable": temp_writable,
                "cargoAvailable": cargo,
            });
        }
        Ok(out)
    }
}

fn probe_write(path: &std::path::Path) -> bool {
    std::fs::write(path, b"ok").is_ok() && std::fs::remove_file(path).is_ok()
}
