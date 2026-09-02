// nct-agent binary: runs the reference terminal-free agent loop against an
// OpenAI-compatible endpoint. Env: NCTOOLS_LLM_BASEURL, NCTOOLS_LLM_MODEL,
// NCTOOLS_LLM_API_KEY; task from argv or stdin. Prints the run report as JSON.
use nct_agent::{run_agent, Chat};
use nct_mcp::build_kernel;

fn main() {
    let base_url = std::env::var("NCTOOLS_LLM_BASEURL").unwrap_or_default();
    let model = std::env::var("NCTOOLS_LLM_MODEL").unwrap_or_default();
    if base_url.is_empty() || model.is_empty() {
        eprintln!("NCTOOLS_LLM_BASEURL and NCTOOLS_LLM_MODEL are required");
        std::process::exit(1);
    }
    let api_key = std::env::var("NCTOOLS_LLM_API_KEY").ok().filter(|s| !s.is_empty());
    let mode = std::env::var("NCTOOLS_AGENT_MODE").unwrap_or_else(|_| "kernel".into());

    let args: Vec<String> = std::env::args().skip(1).collect();
    let task = if args.is_empty() {
        let mut t = String::new();
        use std::io::Read;
        let _ = std::io::stdin().read_to_string(&mut t);
        t.trim().to_string()
    } else {
        args.join(" ")
    };
    if task.is_empty() {
        eprintln!("task required: pass as argv or stdin");
        std::process::exit(1);
    }

    let root = std::env::var("NCTOOLS_WORKSPACE")
        .map(std::path::PathBuf::from)
        .or_else(|_| std::env::current_dir())
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    let kernel = match build_kernel(root) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("kernel init failed: {e}");
            std::process::exit(1);
        }
    };
    let chat = Chat::new(&base_url, api_key, &model);
    let max_steps: usize = std::env::var("NCTOOLS_AGENT_MAX_STEPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    match run_agent(&chat, &kernel, &task, max_steps, &mode, |line| eprintln!("{line}")) {
        Ok(report) => {
            println!(
                "{}",
                serde_json::json!({
                    "finalText": report.final_text,
                    "steps": report.steps,
                    "toolCalls": report.tool_calls,
                    "errors": report.errors,
                    "usage": report.usage,
                    "stopped": report.stopped,
                })
            );
        }
        Err(e) => {
            eprintln!("agent failed: {e}");
            std::process::exit(1);
        }
    }
}
