// Config: one place for every tunable — no magic literals in tool handlers.
// Layering: compiled defaults < nc-tools.toml in the workspace root < env.
// The audit rule "numeric limits come from the config module" is enforced
// here: handlers read these fields, never inline constants.
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct Config {
    pub limits: Limits,
    pub mcp: McpConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Limits {
    /// fs.read default page size (lines)
    pub read_limit: usize,
    /// fs.readMany / fs.writeMany max items
    pub fs_many: usize,
    /// patch.applyMany max files
    pub patch_many: usize,
    /// batch.execute max sub-calls
    pub batch_max: usize,
    /// search.grep default / max results
    pub grep_max_results: usize,
    /// proc.spawn default timeout
    pub spawn_timeout_ms: u64,
    /// proc.spawn max timeout
    pub spawn_timeout_max_ms: u64,
    /// proc.spawn output cap per stream
    pub proc_output_bytes: usize,
    /// proc.start max duration
    pub proc_max_duration_ms: u64,
    /// managed-process output buffer
    pub proc_handle_output_bytes: usize,
    /// net.http body cap
    pub net_max_body: usize,
    /// net.fetch cache TTL (ms) — 0 disables freshness (always revalidate)
    pub net_fetch_ttl_ms: u64,
    /// net.search per-engine timeout
    pub net_engine_timeout_ms: u64,
    /// net.search politeness: minimum interval between hits to one engine
    pub net_search_politeness_ms: u64,
    /// git/pkg child process timeout
    pub child_timeout_ms: u64,
    /// pkg.runScript stdout tail
    pub script_stdout_tail: usize,
    /// fs.list recursive depth
    pub list_depth: usize,
    /// walk depth for search/semantic
    pub walk_depth: usize,
    /// search.grep line snippet cap
    pub grep_line_chars: usize,
    /// test.run timeout
    pub test_timeout_ms: u64,
    /// max process table rows
    pub proc_list_max: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            read_limit: 2000,
            fs_many: 50,
            patch_many: 20,
            batch_max: 25,
            grep_max_results: 200,
            spawn_timeout_ms: 120_000,
            spawn_timeout_max_ms: 600_000,
            proc_output_bytes: 2_000_000,
            proc_max_duration_ms: 3_600_000,
            proc_handle_output_bytes: 2_000_000,
            net_max_body: 2_000_000,
            net_fetch_ttl_ms: 3_600_000,
            net_engine_timeout_ms: 10_000,
            net_search_politeness_ms: 750,
            child_timeout_ms: 300_000,
            script_stdout_tail: 100_000,
            list_depth: 8,
            walk_depth: 12,
            grep_line_chars: 400,
            test_timeout_ms: 300_000,
            proc_list_max: 2000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    /// idle auto-exit after this many ms (0 disables) — NCTOOLS_MCP_IDLE_MS
    pub idle_ms: u64,
}

impl Default for McpConfig {
    fn default() -> Self {
        McpConfig { idle_ms: 30 * 60 * 1000 }
    }
}


impl Config {
    /// defaults < <root>/nc-tools.toml < env overrides
    pub fn load(root: &Path) -> Config {
        let mut cfg = Config::default();
        let toml_path = root.join("nc-tools.toml");
        if let Ok(raw) = std::fs::read_to_string(&toml_path) {
            if let Ok(parsed) = toml::from_str::<Config>(&raw) {
                cfg = parsed;
            }
        }
        if let Ok(v) = std::env::var("NCTOOLS_MCP_IDLE_MS") {
            if let Ok(n) = v.parse::<u64>() {
                cfg.mcp.idle_ms = n;
            }
        }
        cfg
    }
}
