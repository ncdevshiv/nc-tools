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
    /// When true, auto-fix the healthy-but-degraded cases the doctor can repair:
    /// superseded/stale coordination rows and orphaned lock releases.
    #[serde(default)]
    pub repair: Option<bool>,
    /// Per-call workspace override: diagnose THIS base dir instead of the
    /// server root. When absent, the server-rooted session workspace is used
    /// (identical behavior to before).
    #[serde(default)]
    pub baseDir: Option<String>,
}

pub struct DoctorHandler;
impl Handler for DoctorHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: DoctorArgs = parse_args(args)?;
        let deep = a.deep.unwrap_or(false);
        let base = k.base_dir(a.baseDir.as_deref())?;
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
            "root": base.display().to_string(),
            "serverRoot": k.root.display().to_string(),
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
                "grepMaxScanFiles": limits.grep_max_scan_files,
                "grepMaxScanMs": limits.grep_max_scan_ms,
                "grepMaxFileBytes": limits.grep_max_file_bytes,
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
            let root_writable = probe_write(&base.join(".nc-tools-doctor-probe"));
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
        if a.repair.unwrap_or(false) {
            let repaired = repair_self(&base);
            out["repair"] = json!({
                "requested": true,
                "applied": repaired,
            });
        }
        Ok(out)
    }
}

/// Self-heal the healthy-but-degraded state the coordination layer can leave
/// behind: a stale full-history roster where the SAME agentId appears many
/// times, and orphaned lock rows. Repair compacts the roster to one row per
/// agentId (last-seen wins) and drops lock rows that are already expired or
/// redundantly released. Non-destructive — never touches live locks.
fn repair_self(root: &std::path::Path) -> Value {
    use std::io::Write;
    let mut repaired = json!({ "rosterCompacted": 0, "staleLocksDropped": 0 });

    let roster_path = root.join(".nc-tools").join("agents.jsonl");
    if let Ok(raw) = std::fs::read_to_string(&roster_path) {
        let mut latest: std::collections::HashMap<String, serde_json::Value> = std::collections::HashMap::new();
        let mut order: Vec<String> = Vec::new();
        for line in raw.lines().filter(|l| !l.is_empty()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                let id = v["agentId"].as_str().unwrap_or("").to_string();
                if id.is_empty() { continue; }
                if !latest.contains_key(&id) { order.push(id.clone()); }
                latest.insert(id.clone(), v);
            }
        }
        if latest.len() < order.len() {
            let mut buf = String::new();
            match &latest {
                _ => {}
            }
            // one row per agent in insertion order, last-seen wins
            let mut rebuilt = String::new();
            for id in &order {
                if let Some(v) = latest.get(id) {
                    rebuilt.push_str(&(serde_json::to_string(v).unwrap_or_default() + "\n"));
                }
            }
            if let Ok(mut f) = std::fs::File::create(&roster_path) {
                let _ = f.write_all(rebuilt.as_bytes());
                repaired["rosterCompacted"] = json!(order.len());
            }
        }
    }

    let locks_path = root.join(".nc-tools").join("locks.jsonl");
    if let Ok(raw) = std::fs::read_to_string(&locks_path) {
        let my_now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut kept: Vec<serde_json::Value> = Vec::new();
        let mut dropped = 0u64;
        for line in raw.lines().filter(|l| !l.is_empty()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                let expiry = v["expiresAtMs"].as_u64().unwrap_or(0);
                let released = v["released"].as_bool().unwrap_or(false);
                if released || (expiry != 0 && my_now_ms > expiry) {
                    dropped += 1;
                } else {
                    kept.push(v);
                }
            }
        }
        if dropped > 0 {
            let mut rebuilt = String::new();
            for v in &kept {
                rebuilt.push_str(&(serde_json::to_string(v).unwrap_or_default() + "\n"));
            }
            if let Ok(mut f) = std::fs::File::create(&locks_path) {
                let _ = f.write_all(rebuilt.as_bytes());
                repaired["staleLocksDropped"] = json!(dropped);
            }
        }
    }
    repaired
}

fn probe_write(path: &std::path::Path) -> bool {
    std::fs::write(path, b"ok").is_ok() && std::fs::remove_file(path).is_ok()
}

#[cfg(test)]
mod doctor_tests {
    use super::*;

    fn doctor_kernel() -> Kernel {
        let dir = std::env::temp_dir().join(format!(
            "nct-doctor-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Kernel::new(dir).unwrap()
    }

    /// sys.doctor reports the EFFECTIVE base: default = server root
    /// (unchanged), baseDir = the other workspace, whose writability the deep
    /// probe then actually checks. A bad baseDir errors — never a silent
    /// fallback to the server root.
    #[test]
    fn doctor_reports_baseDir_not_server_root() {
        let k = doctor_kernel();
        let target = std::env::temp_dir().join(format!(
            "nct-doctor-target-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&target);
        std::fs::create_dir_all(&target).unwrap();

        let def = DoctorHandler.call(&k, &json!({ "deep": true })).unwrap();
        assert_eq!(def["root"], json!(k.root.display().to_string()));
        assert_eq!(def["serverRoot"], json!(k.root.display().to_string()));

        // (compare canonicalized — resolve_checked returns the long path)
        let canon = dunce::canonicalize(&target).unwrap();
        let over = DoctorHandler
            .call(&k, &json!({ "deep": true, "baseDir": target.display().to_string() }))
            .unwrap();
        assert_eq!(over["root"], json!(canon.display().to_string()));
        assert_eq!(over["serverRoot"], json!(k.root.display().to_string()));
        assert_eq!(over["checks"]["baseWritable"], json!(true), "the probe must check the TARGET's writability");

        let bad = DoctorHandler.call(&k, &json!({ "baseDir": "/no/such/ws/xyz" }));
        assert!(bad.is_err(), "bad baseDir must error, not fall back");

        let _ = std::fs::remove_dir_all(&target);
    }
}
