// Reference terminal-free agent loop. Model gives tool calls; kernel executes.
// No shell, no terminal. Works with any OpenAI-compatible chat endpoint.
// Port of src/agent/agent.mjs + src/agent/llm.mjs.
use serde_json::{json, Value};

use nct_core::Kernel;

pub fn system_prompt(workspace_root: &str) -> String {
    format!(
        r#"You are a coding agent operating a machine through a typed tool API (nc-tools).
There is NO shell and NO terminal. Do not attempt to run bash/sh commands except via proc.spawn
for real programs (test runners, compilers, git is already provided as tools).

Workspace root: {workspace_root}

Tool discipline:
- BATCH independent work: fs.readMany / fs.writeMany / patch.applyMany / batch.execute exist
  so you do not pay one round-trip per tiny operation. Use them.
- LONG-RUNNING programs (servers, watchers) use proc.start and give you a handleId —
  then proc.status / proc.readOutput / proc.stop. One-shot programs use proc.spawn.
- test.run returns structured pass/fail counts and failing test identities (node, pytest).
- pkg.* installs/lists packages and runs npm scripts; net.http makes HTTP requests;
  net.probePort checks TCP ports; env.set/get manage the session environment.
- Prefer patch.apply for edits: exact-match search/replace. Include enough context to be unique.
- fs.read returns a digest (hash+mtime). If the digest matches what you already saw, the file
  is unchanged — do not re-read it.
- Use search.grep / search.files to locate code. search.semantic ranks files by MEANING
  (local neural embeddings) — use it when you only know what the code does, not what strings
  it contains. Structured errors carry actionable hints
  (e.g. PATCH_NO_MATCH returns nearest candidate lines, ERR_NOT_FOUND returns nearest existing
  files) — use them instead of re-orienting with more calls.
- proc.spawn expects cmd + args array, never a shell string.
- When the task is done, verify it (read the file back, run tests via proc.spawn), then reply with
  a final summary message with NO tool calls.

Reply format: think briefly, call tools as needed, then finish with a short plaintext summary."#
    )
}

/// Some providers require ^[a-zA-Z0-9_-]+$ for function names; kernel tools
/// use "fs.read" style. Map dotted names to underscored for the wire, and back.
pub fn to_wire(name: &str) -> String {
    if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        name.to_string()
    } else {
        name.replace('.', "__")
    }
}

pub fn from_wire(name: &str) -> String {
    if name.contains("__") {
        name.replace("__", ".")
    } else {
        name.to_string()
    }
}

/// Kernel tools in OpenAI tool schema format.
pub fn open_ai_tools(k: &Kernel) -> Value {
    let tools: Vec<Value> = k
        .descriptors()
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": to_wire(t["name"].as_str().unwrap_or_default()),
                    "description": t["description"],
                    "parameters": t["inputSchema"],
                },
            })
        })
        .collect();
    Value::Array(tools)
}

// ---- bash comparison arm ----------------------------------------------------
// The same agent loop, but with a single `bash` tool instead of the kernel.
// Used by the benchmark compare arm to measure typed-tools-vs-shell on
// identical tasks with the same models and the same verifier.

pub fn bash_system_prompt(workspace_root: &str) -> String {
    format!(
        r#"You are a coding agent operating a machine through a single bash tool.
Every machine interaction — reading, writing, searching, editing files, running programs, git —
must go through the bash tool with a shell script string. It returns stdout, stderr, and the
exit code.

Workspace root: {workspace_root}

When the task is done, verify it (read the file back, run the tests), then reply with a final
summary message with NO tool calls."#
    )
}

pub fn bash_tool_descriptor() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "bash",
            "description": "Run a bash shell script in the workspace root. Returns stdout, stderr, exit code.",
            "parameters": {
                "type": "object",
                "properties": {
                    "script": { "type": "string", "description": "The bash script to execute" },
                    "timeoutMs": { "type": "integer", "minimum": 100, "maximum": 600000 },
                },
                "required": ["script"],
                "additionalProperties": false,
            },
        },
    })
}

/// OpenAI-compatible chat client (llm.mjs makeChat): any provider exposing
/// /v1/chat/completions. chat() performs one POST and returns the parsed JSON.
pub struct Chat {
    url: String,
    api_key: Option<String>,
    pub model: String,
    agent: ureq::Agent,
}

impl Chat {
    pub fn new(base_url: &str, api_key: Option<String>, model: &str) -> Chat {
        Chat {
            url: format!("{}/chat/completions", base_url.trim_end_matches('/')),
            api_key,
            model: model.to_string(),
            agent: ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(300)).build(),
        }
    }

    pub fn chat(&self, body: &Value) -> Result<Value, String> {
        let mut payload = body.clone();
        if let Value::Object(m) = &mut payload {
            m.entry("model".to_string()).or_insert_with(|| json!(self.model));
        }
        let mut req = self
            .agent
            .post(&self.url)
            .set("Content-Type", "application/json");
        if let Some(key) = &self.api_key {
            req = req.set("Authorization", &format!("Bearer {key}"));
        }
        let resp = req
            .send_string(&payload.to_string())
            .map_err(|e| format!("LLM request failed: {e}"))?;
        let text = resp.into_string().map_err(|e| format!("LLM response read failed: {e}"))?;
        serde_json::from_str(&text).map_err(|e| format!("LLM response unparseable: {e}"))
    }
}

/// Run the agent loop until final answer, maxSteps cap, or API failure —
/// a direct port of agent.mjs runAgent.
pub struct AgentReport {
    pub final_text: String,
    pub steps: usize,
    pub tool_calls: u32,
    pub errors: u32,
    pub usage: Value,
    pub stopped: String,
}

pub fn run_agent(
    chat: &Chat,
    kernel: &Kernel,
    task: &str,
    max_steps: usize,
    mode: &str,
    mut log: impl FnMut(&str),
) -> Result<AgentReport, nct_core::ToolError> {
    let is_bash = mode == "bash";
    let mut messages = vec![
        json!({ "role": "system", "content": if is_bash { bash_system_prompt(&kernel.root.display().to_string()) } else { system_prompt(&kernel.root.display().to_string()) } }),
        json!({ "role": "user", "content": task }),
    ];
    let tools = if is_bash { vec![bash_tool_descriptor()] } else { open_ai_tools(kernel).as_array().cloned().unwrap_or_default() };
    let mut tool_calls = 0u32;
    let mut errors = 0u32;
    let mut usage = json!({ "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 });
    let mut stopped = "max_steps";
    let mut final_text: Option<String> = None;

    for step in 0..max_steps {
        let body = json!({ "messages": messages, "tools": tools, "tool_choice": "auto" });
        let resp = match chat.chat(&body) {
            Ok(r) => r,
            Err(e) => {
                stopped = "api_error";
                final_text = Some(format!("API error: {e}"));
                break;
            }
        };
        if let Some(u) = resp.get("usage") {
            for key in ["prompt_tokens", "completion_tokens", "total_tokens"] {
                let cur = usage[key].as_u64().unwrap_or(0);
                let add = u[key].as_u64().unwrap_or(0);
                usage[key] = json!(cur + add);
            }
        }
        let msg = resp
            .pointer("/choices/0/message")
            .cloned()
            .unwrap_or(Value::Null);
        if msg.is_null() {
            stopped = "api_error";
            final_text = Some("Malformed API response".to_string());
            break;
        }
        let mut assistant = json!({ "role": "assistant", "content": msg["content"].as_str().unwrap_or("") });
        let tool_calls_in_msg = msg.get("tool_calls").and_then(|t| t.as_array()).cloned().unwrap_or_default();
        if !tool_calls_in_msg.is_empty() {
            assistant["tool_calls"] = json!(tool_calls_in_msg);
        }
        messages.push(assistant);

        if tool_calls_in_msg.is_empty() {
            let finish = resp.pointer("/choices/0/finish_reason").and_then(|f| f.as_str()).unwrap_or("length");
            stopped = if finish == "stop" { "done" } else { "max_steps" };
            final_text = Some(msg["content"].as_str().unwrap_or("").to_string());
            break;
        }

        for tc in &tool_calls_in_msg {
            let fname = tc.pointer("/function/name").and_then(|n| n.as_str()).unwrap_or_default().to_string();
            let fargs_raw = tc.pointer("/function/arguments").and_then(|a| a.as_str()).unwrap_or("{}");
            let args: Value = serde_json::from_str(fargs_raw).unwrap_or(json!({}));
            let tool_name = from_wire(&fname);
            log(&format!("  step {}: {} {}", step + 1, tool_name, serde_json::to_string(&args).unwrap_or_default().chars().take(120).collect::<String>()));
            let out = if is_bash {
                let script = args["script"].as_str().unwrap_or("");
                let timeout = args["timeoutMs"].as_u64().unwrap_or(120_000);
                kernel.call("proc.spawn", &json!({ "cmd": "bash", "args": ["-c", script], "timeoutMs": timeout }))
            } else {
                kernel.call(&tool_name, &args)
            };
            match &out {
                r if !r.ok => errors += 1,
                r if is_bash && r.result.as_ref().and_then(|x| x["exitCode"].as_i64()).unwrap_or(0) != 0 => errors += 1,
                r if is_bash && r.result.as_ref().map(|x| x.get("error").is_some()).unwrap_or(false) => errors += 1,
                _ => {}
            }
            tool_calls += 1;
            let content = if out.ok {
                out.result.clone().unwrap_or(Value::Null).to_string()
            } else if let Some(e) = &out.error {
                json!({ "error": e }).to_string()
            } else {
                "{}".to_string()
            };
            let content_trunc: String = content.chars().take(20_000).collect();
            messages.push(json!({
                "role": "tool",
                "tool_call_id": tc.get("id").cloned().unwrap_or(Value::Null),
                "content": content_trunc,
            }));
        }
    }

    Ok(AgentReport {
        final_text: final_text.unwrap_or_else(|| "Stopped at max steps without a final answer.".to_string()),
        steps: messages.len(),
        tool_calls,
        errors,
        usage,
        stopped: stopped.to_string(),
    })
}
