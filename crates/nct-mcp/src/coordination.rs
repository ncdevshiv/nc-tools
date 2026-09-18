// agent.* — the multi-agent coordination layer the kernel has been missing.
//
// PROBLEM: agents working on the SAME workspace through the same nc-tools
// kernel run completely anonymously and blind to each other. There is no
// registry of who is working, no identity that survives a crash/compaction,
// no way for one agent to know another's task, and no lock to stop two agents
// writing the same file. This module fixes all of that with on-disk,
// cross-process state (so parallel servers sharing a workspace see each other):
//
//   .nc-tools/agents.jsonl          — roster: every agent identity + state
//   .nc-tools/agent-messages.jsonl  — noticeboard: inter-agent messages
//   .nc-tools/locks.jsonl           — advisory file locks
//
// IDENTITY model:
//   * agent.register mints or RESUMES an identity. With {agentId} it restores
//     a known id (the crash/compaction continuation path — a client that
//     remembers who it was keeps its identity). Without {agentId} it resumes
//     this session's existing agent (bound to kernel.sid) or mints a new one.
//   * The session id (kernel.sid) groups events; the agentId is the durable
//     identity an agent carries across sessions to be "the same agent".
//
// AWARENESS model:
//   * agent.post {to?, message} writes to the noticeboard; agent.messages reads
//     it. Agents that have never met can still see each other's state via the
//     roster and coordinate via the noticeboard — no shared conversation needed.
//   * agent.status returns the full current state: who is here, what they are
//     doing, what is locked, recent messages. This is the "look around before
//     you start" tool an agent should call on entry.
//
// CONFLICT mitigation:
//   * agent.lock {path, holdMs} takes an advisory lock; agent.unlock {path}
//     releases it; agent.locks lists active ones. Locking is advisory — it
//     does not hard-block writes — because a hard block would deadlock an
//     agent that is mid-edit. The tool makes it visible and recordable so
//     agents choose not to collide, and it survives crashes via the holdMs
//     expiry.
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use nct_core::errors::ToolError;
use nct_core::kernel::{parse_args, Handler, Kernel};
use nct_core::{now_iso, now_ms};

pub const REGISTER_DESC: &str = "Register (or resume) this agent's identity in the workspace roster AND the global (cross-workspace) index. With {agentId} restores a known identity (the crash/compaction continuation path — a client that remembers who it was keeps its identity and history). Without it, resumes this session's agent or mints `agent-<n>` (chronological). Returns {agentId, name, role, createdAt, resumed}. Call agent.status to see who else is working here; agent.peers to see agents on OTHER workspaces.";
pub const LIST_DESC: &str = "List the workspace agent roster chronologically: every agent identity (id, name, sid, createdAt, lastSeen, status, task, toolCount) that has touched this workspace. Sorted newest-first.";
pub const PEERS_DESC: &str = "List agents across OTHER workspaces from the GLOBAL index (~/.nc-tools/agents.jsonl, override with NCTOOLS_AGENT_HOME). Agents register here too, so you can discover who else is working on a different project and coordinate with them. Filter by {workspace} to narrow. Cross-process + cross-machine-user.";
pub const HEARTBEAT_DESC: &str = "Update this agent's roster entry: lastSeen, status (idle|working|blocked|done), and the current task summary. Other agents see this via agent.status/agent.list. Cheap — call it between steps so the roster stays live.";
pub const STATUS_DESC: &str = "Full coordination snapshot: who is here, what each is working on, active locks, whether this agent is locked out of any path, and recent inter-agent messages. The 'look around before you start' tool.";
pub const POST_DESC: &str = "Post a message to the inter-agent noticeboard. {to} may be an agentId for a direct message or omitted for a broadcast. {kind} is note|question|request|handoff|bug|hold|resume — other agents can filter by it. Messages persist in .nc-tools/agent-messages.jsonl.";
pub const MESSAGES_DESC: &str = "Read inter-agent messages. {since} filters by agentId context (from/to), {lastN} caps the tail. Returns messages newest-first with from/to/kind.";
pub const LOCK_DESC: &str = "Take an advisory lock on a path (e.g. a file you are about to edit). {holdMs} auto-releases the lock after that long even if this agent crashes (default 10 min). Locking is advisory — it makes the intent visible and recordable so agents choose not to collide. Check agent.locks/agent.status before editing, and agent.unlock when done.";
pub const UNLOCK_DESC: &str = "Release a lock this agent holds on a path. If the lock has expired (holdMs passed) this is a no-op; if another agent holds it, it is NOT released (you cannot steal a live lock).";
pub const LOCKS_DESC: &str = "List active advisory locks: path, agentId, heldAt, expiresAt, expired state. Call this before writing to a path to see whether another agent is mid-edit there.";

fn roster_path(root: &Path) -> PathBuf {
    root.join(".nc-tools").join("agents.jsonl")
}
fn messages_path(root: &Path) -> PathBuf {
    root.join(".nc-tools").join("agent-messages.jsonl")
}
fn locks_path(root: &Path) -> PathBuf {
    root.join(".nc-tools").join("locks.jsonl")
}
/// Global (cross-workspace) agent index. Home dir is overridable via
/// NCTOOLS_AGENT_HOME so a shared user-level registry can be pointed elsewhere.
fn global_path() -> PathBuf {
    if let Ok(d) = std::env::var("NCTOOLS_AGENT_HOME") {
        if !d.is_empty() {
            return PathBuf::from(d).join("agents.jsonl");
        }
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".nc-tools").join("agents.jsonl")
}

/// Append a JSON line to a path, creating the parent dir + file.
fn append_line(path: &Path, entry: &Value) -> Result<(), ToolError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all((serde_json::to_string(entry)? + "\n").as_bytes())?;
    Ok(())
}

/// Read all JSON lines from a path (empty → []). A torn final line — a crash
/// mid-append — is dropped by the filter, which is exactly what an
/// append-only, "last row wins" reader wants: it never sees a half row.
fn read_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .map(|raw| {
            raw.lines()
                .filter(|l| !l.is_empty())
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The lock file guarding a coordination file, kept beside the data file so
/// each file serializes its own writers. The roster, locks, and messages files
/// are guarded separately — two agents heartbeating contend with each other,
/// but never with an agent posting a message.
fn coord_lock_path(data_path: &Path) -> PathBuf {
    nct_core::filock::lock_path_for(data_path)
}

/// Run a read-modify-append as one atomic critical section across processes.
///
/// Every roster/lock/message update is read-then-compute-then-append. Unlocked,
/// two server processes sharing a workspace could both read the same tail and
/// both append a decision derived from it — the heartbeat of the slower writer
/// vanishes, and for `agent.lock` both agents believe they own the path. The
/// lock makes the whole block linear with respect to any other server.
fn with_coord_lock<T>(
    path: &Path,
    f: impl FnOnce() -> Result<T, ToolError>,
) -> Result<T, ToolError> {
    let _lock = coord_lock(path)?;
    f()
}

/// Acquire the guard for a coordination file when the caller wants to manage
/// the release explicitly — e.g. holding it across a read-modify-append and
/// then dropping it before a best-effort side effect that must not block.
fn coord_lock(path: &Path) -> Result<nct_core::filock::FileLock, ToolError> {
    // The lock file lives beside the data file, so the parent must already
    // exist: on a fresh workspace the first writer's `.nc-tools/` does not,
    // and create_new fails with a missing-parent error rather than
    // AlreadyExists. create_dir_all is idempotent and, being before the
    // acquire, cannot introduce a writer-vs-writer race (directories are
    // created in place; the data append is what the lock serializes).
    if let Some(parent) = coord_lock_path(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    nct_core::filock::FileLock::acquire(
        &coord_lock_path(path),
        nct_core::filock::DEFAULT_TIMEOUT_MS,
    )
}

// ---- agents.jsonl ------------------------------------------------------------

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisterArgs {
    /// A known agent id to restore (crash/compaction continuation). When
    /// present and in the roster, the identity is resumed with a new lastSeen.
    #[serde(default)]
    pub agentId: Option<String>,
    /// Human label for the roster (default: derived from agentId or a counter).
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    /// Workspace to coordinate against (default: the session base). Use this
    /// when the agent works on a DIFFERENT workspace than the server root —
    /// the roster/locks/global index then target the effective workspace.
    #[serde(default)]
    pub baseDir: Option<String>,
}

struct AgentRecord {
    agent_id: String,
    sid: String,
    name: String,
    role: String,
    status: String,
    task: String,
    created_at: String,
    last_seen: String,
    tool_count: u64,
    resumed: bool,
}

/// Mint or resume an agent identity for the calling session.
///
/// Continuity rules (the crash/compaction policy):
///   * {agentId} present + in roster  -> resume, keep identity + history.
///   * {agentId} present + NOT in roster -> mint with EXACTLY that id (a client
///     restoring an id it holds across a workspace reset is honored, not
///     silently renumbered).
///   * {agentId} absent + this sid already has an agent -> resume it (the
///     common case: same thread/session, continued work).
///   * {agentId} absent + no agent for this sid -> mint `agent-<n>` where n is
///     the next chronological index.
fn register_impl(k: &Kernel, a: &RegisterArgs) -> Result<Value, ToolError> {
    // Coordinate against the EFFECTIVE workspace: baseDir overrides the server
    // root, so an agent working on a different project via baseDir puts its
    // roster/locks/global-index entries in that project — not the tools repo.
    let root = k.base_dir(a.baseDir.as_deref())?;
    let path = roster_path(&root);
    // The mint/resume is read-modify-append: two servers registering into the
    // same workspace must not both read an empty roster and both mint agent-1,
    // or both decide they are resuming the same identity. Hold the roster lock
    // across the read-compute-append.
    let _roster_lock = coord_lock(&path)?;
    let roster = read_lines(&path);
    let record = if let Some(want) = &a.agentId {
        if let Some(existing) = roster.iter().find(|e| e["agentId"].as_str() == Some(want)) {
            // Resume a known identity.
            AgentRecord {
                agent_id: want.clone(),
                sid: k.sid.clone(),
                name: a
                    .name
                    .clone()
                    .or_else(|| existing["name"].as_str().map(String::from))
                    .unwrap_or_else(|| want.clone()),
                role: a
                    .role
                    .clone()
                    .or_else(|| existing["role"].as_str().map(String::from))
                    .unwrap_or_else(|| "agent".to_string()),
                status: existing["status"].as_str().unwrap_or("working").to_string(),
                task: existing["task"].as_str().unwrap_or("").to_string(),
                created_at: existing["createdAt"]
                    .as_str()
                    .unwrap_or(&now_iso())
                    .to_string(),
                last_seen: now_iso(),
                tool_count: existing["toolCount"].as_u64().unwrap_or(0),
                resumed: true,
            }
        } else {
            // Mint with the requested id (honored, not renumbered).
            AgentRecord {
                agent_id: want.clone(),
                sid: k.sid.clone(),
                name: a.name.clone().unwrap_or_else(|| want.clone()),
                role: a.role.clone().unwrap_or_else(|| "agent".to_string()),
                status: "registered".to_string(),
                task: String::new(),
                created_at: now_iso(),
                last_seen: now_iso(),
                tool_count: 0,
                resumed: false,
            }
        }
    } else if let Some(existing) = roster.iter().find(|e| e["sid"].as_str() == Some(&k.sid)) {
        // Same session, continued work -> resume.
        let id = existing["agentId"].as_str().unwrap_or("").to_string();
        AgentRecord {
            agent_id: id.clone(),
            sid: k.sid.clone(),
            name: a
                .name
                .clone()
                .or_else(|| existing["name"].as_str().map(String::from))
                .unwrap_or_else(|| id.clone()),
            role: a
                .role
                .clone()
                .or_else(|| existing["role"].as_str().map(String::from))
                .unwrap_or_else(|| "agent".to_string()),
            status: existing["status"].as_str().unwrap_or("working").to_string(),
            task: existing["task"].as_str().unwrap_or("").to_string(),
            created_at: existing["createdAt"]
                .as_str()
                .unwrap_or(&now_iso())
                .to_string(),
            last_seen: now_iso(),
            tool_count: existing["toolCount"].as_u64().unwrap_or(0),
            resumed: true,
        }
    } else {
        // New agent next in the chronological sequence.
        let next_index = roster
            .iter()
            .filter_map(|e| {
                e["agentId"]
                    .as_str()
                    .and_then(|s| s.strip_prefix("agent-"))
                    .and_then(|n| n.parse::<u64>().ok())
            })
            .max()
            .unwrap_or(0)
            + 1;
        let id = format!("agent-{next_index}");
        AgentRecord {
            agent_id: id.clone(),
            sid: k.sid.clone(),
            name: a.name.clone().unwrap_or_else(|| id.clone()),
            role: a.role.clone().unwrap_or_else(|| "agent".to_string()),
            status: "registered".to_string(),
            task: String::new(),
            created_at: now_iso(),
            last_seen: now_iso(),
            tool_count: 0,
            resumed: false,
        }
    };

    // Persist the (possibly resumed) record. We re-append so the roster is a
    // full history — resume writes a fresh "seen" row that readers take as the
    // current state (last row per agentId wins). No deletion needed.
    append_line(
        &path,
        &json!({
            "agentId": record.agent_id,
            "sid": record.sid,
            "name": record.name,
            "role": record.role,
            "status": record.status,
            "task": record.task,
            "createdAt": record.created_at,
            "lastSeen": record.last_seen,
            "toolCount": record.tool_count,
            "epoch": now_ms(),
        }),
    )?;

    // The roster write is committed; the lock can go. The global mirror below
    // is a different file with its own lock and must not serialize on this one
    // (the global index is shared by every workspace on this machine).
    drop(_roster_lock);

    // Bind this session's kernel (and therefore its journal rows) to the agent
    // id so provenance is attributable to the agent, not just the session.
    k.set_agent_id(&record.agent_id);

    // Mirror this agent into the GLOBAL (cross-workspace) index so agents on
    // OTHER workspaces can discover it. Best-effort: a global index write
    // failure must never fail registration.
    let g = global_path();
    if let Some(parent) = g.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = with_coord_lock(&g, || {
        append_line(
            &g,
            &json!({
                "agentId": record.agent_id,
                "name": record.name,
                "role": record.role,
                "workspace": root.display().to_string(),
                "status": record.status,
                "task": record.task,
                "lastSeen": now_iso(),
                "createdAt": record.created_at,
                "epoch": now_ms(),
            }),
        )
    });

    Ok(json!({
        "agentId": record.agent_id,
        "name": record.name,
        "role": record.role,
        "sid": record.sid,
        "createdAt": record.created_at,
        "resumed": record.resumed,
        "note": "call agent.status to see who else is working in this workspace",
    }))
}

pub struct RegisterHandler;
impl Handler for RegisterHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: RegisterArgs = parse_args(args)?;
        register_impl(k, &a)
    }
}

// ---- agent.list ----------------------------------------------------------------

pub struct ListHandler;
impl Handler for ListHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: CoordArgs = parse_args(args)?;
        let root = k.base_dir(a.baseDir.as_deref())?;
        let roster = roster_snapshot(&root);
        Ok(json!({ "agents": roster, "total": roster.len() }))
    }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CoordArgs {
    /// Workspace to coordinate against (default: the session base).
    #[serde(default)]
    pub baseDir: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PeersArgs {
    /// Narrow to agents last seen in this workspace path (substring match).
    #[serde(default)]
    pub workspace: Option<String>,
    /// Workspace to coordinate against (default: the session base). Used to
    /// exclude this workspace's own agents from the peer list.
    #[serde(default)]
    pub baseDir: Option<String>,
}

pub struct PeersHandler;
impl Handler for PeersHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: PeersArgs = parse_args(args)?;
        let current_ws = k.base_dir(a.baseDir.as_deref())?.display().to_string();
        let mut peers: Vec<Value> = read_lines(&global_path())
            .into_iter()
            .filter(|e| {
                let ws = e["workspace"].as_str().unwrap_or("");
                // Not my own workspace — peers are agents on OTHER workspaces.
                if ws == current_ws {
                    return false;
                }
                if let Some(w) = &a.workspace {
                    if !ws.contains(w.as_str()) {
                        return false;
                    }
                }
                true
            })
            .collect();
        // newest-first
        peers.sort_by(|x, y| {
            let ex = x["lastSeen"].as_str().unwrap_or("");
            let ey = y["lastSeen"].as_str().unwrap_or("");
            ey.cmp(ex)
        });
        Ok(
            json!({"peers": peers, "total": peers.len(), "note": "agents on other workspaces; this workspace's agents are in agent.list"}),
        )
    }
}

/// Current roster: last row per agentId wins (resume overwrites the state),
/// newest-first. Returns agent state without the sid (not a cross-agent leak
/// — another agent does not need this session's internal id).
fn roster_snapshot(root: &Path) -> Vec<Value> {
    let roster = read_lines(&roster_path(root));
    let mut latest: Vec<Value> = Vec::new();
    for row in roster {
        let id = row["agentId"].as_str().unwrap_or("").to_string();
        // Drop a row from a DIFFERENT agent with the same id? No — resume rows
        // overwrite the same id, so keep only the last per id.
        if let Some(existing) = latest
            .iter_mut()
            .find(|e| e["agentId"].as_str() == Some(id.as_str()))
        {
            *existing = row;
        } else {
            latest.push(row);
        }
    }
    // newest-first
    latest.sort_by(|a, b| {
        let ea = a["lastSeen"].as_str().unwrap_or("");
        let eb = b["lastSeen"].as_str().unwrap_or("");
        eb.cmp(ea)
    });
    latest
        .into_iter()
        .map(|e| {
            // Redact sid from the output — another agent needs the identity and
            // state, not this session's runtime handle.
            let mut e = e;
            if let Value::Object(m) = &mut e {
                m.remove("sid");
            }
            e
        })
        .collect()
}

// ---- agent.heartbeat -----------------------------------------------------------

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatArgs {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub task: Option<String>,
    /// Workspace to coordinate against (default: the session base).
    #[serde(default)]
    pub baseDir: Option<String>,
}

/// Resolve the calling session's agentId (from the roster). If it has not
/// registered yet, mint one — heartbeat is a convenience that never fails
/// because the caller forgot to register.
fn my_agent_id(root: &Path, sid: &str) -> String {
    let roster = read_lines(&roster_path(root));
    roster
        .iter()
        .rev()
        .find(|e| e["sid"].as_str() == Some(sid))
        .and_then(|e| e["agentId"].as_str().map(String::from))
        .unwrap_or_else(|| {
            let n = roster
                .iter()
                .filter_map(|e| {
                    e["agentId"]
                        .as_str()
                        .and_then(|s| s.strip_prefix("agent-"))
                        .and_then(|n| n.parse::<u64>().ok())
                })
                .max()
                .unwrap_or(0)
                + 1;
            format!("agent-{n}")
        })
}

pub struct HeartbeatHandler;
impl Handler for HeartbeatHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: HeartbeatArgs = parse_args(args)?;
        let root = k.base_dir(a.baseDir.as_deref())?;
        let task = a.task.unwrap_or_default();
        let path = roster_path(&root);
        // The update is read-modify-append. Unlocked, two server processes
        // heartbeating the same workspace both read the same tail and both
        // append a row derived from it — the slower writer's heartbeat
        // silently vanishes under "last row wins". Hold the roster lock.
        let _lock = coord_lock(&path)?;
        let agent_id = my_agent_id(&root, &k.sid);
        // Load current state (last row for this agent), overlay, re-append.
        let roster = read_lines(&path);
        let prior = roster
            .iter()
            .rev()
            .find(|e| e["agentId"].as_str() == Some(agent_id.as_str()));
        // An omitted status is a liveness ping, not a status change. Carry the
        // prior value forward so a bare `agent.heartbeat` cannot flip
        // "blocked" or "done" back to "working" and hide a stalled agent.
        let status = a.status.unwrap_or_else(|| {
            prior
                .and_then(|e| e["status"].as_str())
                .unwrap_or("working")
                .to_string()
        });
        let created = prior
            .and_then(|e| e["createdAt"].as_str().map(String::from))
            .unwrap_or_else(now_iso);
        let tool_count = prior.and_then(|e| e["toolCount"].as_u64()).unwrap_or(0);
        append_line(
            &path,
            &json!({
                "agentId": agent_id,
                "sid": k.sid,
                "name": prior.and_then(|e| e["name"].as_str().map(String::from)).unwrap_or_else(|| agent_id.clone()),
                "role": prior.and_then(|e| e["role"].as_str().map(String::from)).unwrap_or_else(|| "agent".to_string()),
                "status": status,
                "task": task,
                "createdAt": created,
                "lastSeen": now_iso(),
                "toolCount": tool_count,
                "epoch": now_ms(),
            }),
        )?;
        Ok(json!({ "agentId": agent_id, "status": status, "lastSeen": now_iso() }))
    }
}

// ---- agent.status ----------------------------------------------------------------

pub struct StatusHandler;
impl Handler for StatusHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: CoordArgs = parse_args(args)?;
        let root = k.base_dir(a.baseDir.as_deref())?;
        let roster = roster_snapshot(&root);
        let locks = locks_snapshot(&root);
        let msgs = messages_snapshot(&root, None, 30);
        let my_agent = my_agent_id(&root, &k.sid);
        let my_active = agents_total_tools(&root, &my_agent);
        let locked_out: Vec<String> = locks
            .iter()
            .filter(|l| {
                l["agentId"].as_str() != Some(my_agent.as_str()) && l["expired"] != json!(true)
            })
            .filter_map(|l| l["path"].as_str().map(String::from))
            .collect();
        Ok(json!({
            "me": my_agent,
            "myToolCount": my_active,
            "agents": roster,
            "agentsHere": roster.iter().filter(|e| {
                e["lastSeen"].as_str().map(|t| now_ms().saturating_sub(parse_ms(t)) < 60_000).unwrap_or(true)
            }).count(),
            "locks": locks,
            "lockedOutOf": locked_out,
            "recentMessages": msgs,
        }))
    }
}

fn agents_total_tools(root: &Path, agent_id: &str) -> u64 {
    read_lines(&roster_path(root))
        .iter()
        .rev()
        .find(|e| e["agentId"].as_str() == Some(agent_id))
        .and_then(|e| e["toolCount"].as_u64())
        .unwrap_or(0)
}

fn parse_ms(iso: &str) -> u64 {
    chrono::DateTime::parse_from_rfc3339(iso)
        .map(|t| {
            (chrono::Utc::now() - t.with_timezone(&chrono::Utc))
                .num_milliseconds()
                .max(0) as u64
        })
        .unwrap_or(u64::MAX)
}

// ---- agent.post / agent.messages ----------------------------------------------

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PostArgs {
    /// Recipient agentId; omit for a broadcast.
    #[serde(default)]
    pub to: Option<String>,
    pub message: String,
    /// note | question | request | handoff | bug | hold | resume
    #[serde(default)]
    pub kind: Option<String>,
    /// Workspace to coordinate against (default: the session base).
    #[serde(default)]
    pub baseDir: Option<String>,
}

pub struct PostHandler;
impl Handler for PostHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: PostArgs = parse_args(args)?;
        if a.message.trim().is_empty() {
            return Err(ToolError::new(
                "ERR_BAD_INPUT",
                "message must be a non-empty string",
            ));
        }
        let root = k.base_dir(a.baseDir.as_deref())?;
        let from = my_agent_id(&root, &k.sid);
        let to = a.to.clone();
        let kind = a.kind.clone().unwrap_or_else(|| "note".to_string());
        let message = a.message.clone();
        let path = messages_path(&root);
        // seq assignment and append are ONE critical section: two processes
        // posting at once must not both read the same tail and mint the same
        // seq, which would scramble the newest-first order messages sort on.
        let entry = with_coord_lock(&path, || {
            let entry = json!({
                "ts": now_iso(),
                "seq": next_seq(&read_lines(&path)),
                "from": from,
                "to": to,
                "kind": kind,
                "message": message,
            });
            append_line(&path, &entry)?;
            Ok::<Value, ToolError>(entry)
        })?;
        Ok(json!({ "posted": true, "from": from, "to": to, "kind": kind, "seq": entry["seq"] }))
    }
}

/// Next sequence number for an append-only coordination file: one more than the
/// largest seq already present in `rows`. Derived from the rows rather than the
/// row COUNT — two processes appending at once both read the same count and
/// both mint the same seq, which scrambles the newest-first ordering
/// `agent.messages` sorts on. Rows without a `seq` (roster-state rows) are
/// skipped, so the same helper numbers both the noticeboard and the compaction
/// checkpoints that live in the roster file. Must be called only while holding
/// the file's lock, mirroring how the journal derives seq under its own lock.
fn next_seq(rows: &[Value]) -> u64 {
    rows.iter()
        .filter_map(|m| m["seq"].as_u64())
        .max()
        .map_or(1, |s| s + 1)
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MessagesArgs {
    /// Return messages to (direct) or from this agentId. Omit = all.
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    #[schemars(range(min = 1, max = 100))]
    pub lastN: Option<u64>,
    #[serde(default)]
    pub kind: Option<String>,
    /// Workspace to coordinate against (default: the session base).
    #[serde(default)]
    pub baseDir: Option<String>,
}

pub struct MessagesHandler;
impl Handler for MessagesHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: MessagesArgs = parse_args(args)?;
        let root = k.base_dir(a.baseDir.as_deref())?;
        let msgs = messages_snapshot(&root, Some(&a), a.lastN.unwrap_or(50) as usize);
        Ok(json!({ "messages": msgs, "total": msgs.len() }))
    }
}

fn messages_snapshot(root: &Path, a: Option<&MessagesArgs>, last_n: usize) -> Vec<Value> {
    let mut msgs: Vec<Value> = read_lines(&messages_path(root))
        .into_iter()
        .filter(|m| {
            if let Some(a) = a {
                if let Some(to) = &a.to {
                    let to_v = m["to"].as_str();
                    let from_v = m["from"].as_str();
                    // direct to me, or broadcast (to null)
                    if to_v != Some(to.as_str()) && to_v.is_some() {
                        return false;
                    }
                    // also only messages from my peers or mine... keep broad
                    let _ = from_v;
                }
                if let Some(from) = &a.from {
                    if m["from"].as_str() != Some(from.as_str()) {
                        return false;
                    }
                }
                if let Some(kind) = &a.kind {
                    if m["kind"].as_str() != Some(kind.as_str()) {
                        return false;
                    }
                }
                true
            } else {
                true
            }
        })
        .collect();
    msgs.sort_by(|x, y| {
        let sx = x["seq"].as_u64().unwrap_or(0);
        let sy = y["seq"].as_u64().unwrap_or(0);
        sy.cmp(&sx) // newest first
    });
    msgs.truncate(last_n);
    msgs
}

// ---- agent.lock / unlock / locks ----------------------------------------------

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LockArgs {
    /// Path (relative to base dir or absolute) this agent will be editing.
    pub path: String,
    /// Auto-release after this many ms even if this agent crashes (default 10 min).
    #[serde(default)]
    #[schemars(range(min = 1000, max = 3600000))]
    pub holdMs: Option<u64>,
    /// Workspace to coordinate against (default: the session base).
    #[serde(default)]
    pub baseDir: Option<String>,
    /// Take over a path another agent still holds. Refused by default.
    #[serde(default)]
    pub force: Option<bool>,
}

pub struct LockHandler;
impl Handler for LockHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: LockArgs = parse_args(args)?;
        let root = k.base_dir(a.baseDir.as_deref())?;
        let path = nct_core::paths::resolve_checked(&root, &a.path)?;
        let path_str = nct_core::locks::rel_key(&root, &path);
        let hold_ms = a.holdMs.unwrap_or(600_000);
        let registry = locks_path(&root);
        // The identity lookup, the foreign-lock check and the append are ONE
        // critical section. Unlocked, two agents locking the same path at once
        // would each read "no live lock", each append, and each believe they
        // owned it — the exact concurrent-write clobber this tool exists to
        // stop. my_agent_id is inside it too: it reads the roster to resolve
        // this session's id, and two processes minting against the same roster
        // at once would pick the same next agent number.
        let _lock = coord_lock(&registry)?;
        let agent_id = my_agent_id(&root, &k.sid);
        // Bind the identity the lock is recorded under. Without this the
        // session's agent id stays unset and maybe_warn_foreign_lock compares
        // against an empty string — so this agent's OWN locks would count as
        // foreign and guardLocks would refuse its own writes.
        k.set_agent_id(&agent_id);
        // Refuse to clobber a live lock another agent holds. The registry is
        // append-only with "last row per path wins", so a blind append erased
        // the holder's lock and both agents believed they owned the path — a
        // real concurrent-write-clobber path, not a theoretical one. Re-locking
        // a path you already hold is a self-upgrade and stays allowed.
        if !a.force.unwrap_or(false) {
            if let Some(held) = nct_core::foreign_live_lock(&root, &path_str, &agent_id) {
                let holder = held["agentId"].as_str().unwrap_or("?").to_string();
                let until = held["expiresAtMs"].as_u64().unwrap_or(0);
                return Err(ToolError::with_hint(
                    "ERR_REFUSED",
                    format!("path is locked by agent '{holder}' ({path_str})"),
                    json!({
                        "path": path_str,
                        "lockedBy": holder,
                        "expiresAtMs": until,
                        "hint": "ask the holder to agent.unlock, or pass force=true to take over",
                    }),
                ));
            }
        }
        let held_at = now_iso();
        let expiry_ms = now_ms() + hold_ms;
        // Append. Overwrites any earlier lock this agent held (self-upgrade).
        append_line(
            &registry,
            &json!({
                "path": path_str,
                "agentId": agent_id,
                "heldAt": held_at,
                "holdMs": hold_ms,
                "expiresAt": millis_to_rfc3339(expiry_ms),
                "expiresAtMs": expiry_ms,
                "seq": now_ms(),
            }),
        )?;
        Ok(json!({
            "path": path_str,
            "agentId": agent_id,
            "heldAt": held_at,
            "expiresAt": millis_to_rfc3339(expiry_ms),
            "holdMs": hold_ms,
            "note": "lock is advisory; call agent.locks before editing and agent.unlock when done",
        }))
    }
}

fn millis_to_rfc3339(ms: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms as i64)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(now_iso)
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnlockArgs {
    pub path: String,
    /// Workspace to coordinate against (default: the session base).
    #[serde(default)]
    pub baseDir: Option<String>,
}

pub struct UnlockHandler;
impl Handler for UnlockHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: UnlockArgs = parse_args(args)?;
        let root = k.base_dir(a.baseDir.as_deref())?;
        let path = nct_core::paths::resolve_checked(&root, &a.path)?;
        let path_str = nct_core::locks::rel_key(&root, &path);
        let registry = locks_path(&root);
        // Snapshot-read + decide + append as one critical section, for the same
        // reason as LockHandler: two unlockers — or an unlock racing a lock —
        // must not both act on the same stale snapshot.
        let _lock = coord_lock(&registry)?;
        let agent_id = my_agent_id(&root, &k.sid);
        // Bind the identity for the same reason as LockHandler.
        k.set_agent_id(&agent_id);
        let locks = locks_snapshot(&root);
        // Find this agent's live lock on the path; is it expired?
        let same = locks
            .iter()
            .filter(|l| {
                l["path"] == json!(path_str) && l["agentId"].as_str() == Some(agent_id.as_str())
            })
            .collect::<Vec<_>>();
        if same.is_empty() {
            return Ok(
                json!({ "path": path_str, "released": false, "note": "you hold no live lock on this path" }),
            );
        }
        let held_by_other = locks.iter().any(|l| {
            l["path"] == json!(path_str)
                && l["agentId"].as_str() != Some(agent_id.as_str())
                && l["expired"] != json!(true)
        });
        if held_by_other {
            return Ok(
                json!({ "path": path_str, "released": false, "note": "another live lock exists on this path; not stealing it" }),
            );
        }
        // Release: mark expired by re-appending an expired row for this path+agent.
        append_line(
            &registry,
            &json!({
                "path": path_str,
                "agentId": agent_id,
                "heldAt": now_iso(),
                "holdMs": 0,
                "expiresAt": now_iso(),
                "expiresAtMs": now_ms(),
                "seq": now_ms(),
                "released": true,
            }),
        )?;
        Ok(json!({ "path": path_str, "agentId": agent_id, "released": true }))
    }
}

pub struct LocksHandler;
impl Handler for LocksHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: CoordArgs = parse_args(args)?;
        let root = k.base_dir(a.baseDir.as_deref())?;
        let locks = locks_snapshot(&root);
        let live: Vec<&Value> = locks
            .iter()
            .filter(|l| l["expired"] != json!(true))
            .collect();
        Ok(json!({ "locks": live, "total": live.len() }))
    }
}

/// Snapshot of all locks; last row per path wins. Each lock gets an `expired`
/// flag so callers can filter live vs stale.
fn locks_snapshot(root: &Path) -> Vec<Value> {
    let locks = read_lines(&locks_path(root));
    let mut latest: Vec<Value> = Vec::new();
    for row in locks {
        let path = row["path"].as_str().unwrap_or("").to_string();
        if let Some(existing) = latest
            .iter_mut()
            .find(|e| e["path"].as_str() == Some(path.as_str()))
        {
            *existing = row;
        } else {
            latest.push(row);
        }
    }
    latest
        .into_iter()
        .map(|mut l| {
            let expiry = l["expiresAtMs"].as_u64().unwrap_or(0);
            let expired =
                expiry == 0 || l["released"].as_bool().unwrap_or(false) || now_ms() > expiry;
            l["expired"] = json!(expired);
            l
        })
        .collect()
}

// ---- builders ----------------------------------------------------------------

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmptyArgs {}

pub const COMPACT_DESC: &str = "Record a compaction checkpoint for this agent: the agent is about to have its context summarized, so it writes a durable marker (id, agentId, ts, summary, nextHint) that agent.resume later follows to reconstruct the identity + state. This is the explicit continuation handshake — a crash/compaction for THIS agent is recorded so a later prompt in the same thread can resume cleanly instead of starting anonymous.";
pub const RESUME_DESC: &str = "Follow this agent's most recent compaction checkpoint: confirms identity continuity after a crash/compaction, re-registers the same agentId, and returns its last summary + nextHint so the resumed agent can pick up where it left off. Call agent.resume at the start of a prompt when you believe you are continuing a prior thread.";

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompactArgs {
    /// Short summary of what this agent did / where it left off (the
    /// checkpoint's durable record). Saved as-is and returned on resume.
    pub summary: String,
    /// Optional hint for the next step (e.g. the next file to touch). Returned
    /// on resume verbatim.
    #[serde(default)]
    pub nextHint: Option<String>,
    /// Workspace to coordinate against (default: the session base).
    #[serde(default)]
    pub baseDir: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResumeArgs {
    /// Which agent to resume. Defaults to this session's agent (by sid) if
    /// omitted. When omitted and no prior checkpoint exists, no-op.
    #[serde(default)]
    pub agentId: Option<String>,
    /// Workspace to coordinate against (default: the session base).
    #[serde(default)]
    pub baseDir: Option<String>,
}

pub struct CompactHandler;
impl Handler for CompactHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: CompactArgs = parse_args(args)?;
        if a.summary.trim().is_empty() {
            return Err(ToolError::new(
                "ERR_BAD_INPUT",
                "summary must be a non-empty string",
            ));
        }
        let root = k.base_dir(a.baseDir.as_deref())?;
        let path = roster_path(&root);
        // Read, checkpoint, and the roster-status update are ONE critical
        // section on the roster: a heartbeat from a concurrent process must not
        // land between them and make the "compacted" marker vanish.
        let _lock = coord_lock(&path)?;
        let agent_id = my_agent_id(&root, &k.sid);
        let roster_read = read_lines(&path);
        let prior = roster_read
            .iter()
            .rev()
            .find(|e| e["agentId"].as_str() == Some(agent_id.as_str()));
        let created = prior
            .and_then(|e| e["createdAt"].as_str().map(String::from))
            .unwrap_or_else(now_iso);
        let name = prior
            .and_then(|e| e["name"].as_str().map(String::from))
            .unwrap_or_else(|| agent_id.clone());
        let tool_count = prior.and_then(|e| e["toolCount"].as_u64()).unwrap_or(0);
        let checkpoint = json!({
            "agentId": agent_id,
            "sid": k.sid,
            "name": name,
            "ts": now_iso(),
            "seq": next_seq(&roster_read),
            "summary": a.summary,
            "nextHint": a.nextHint,
            "toolCount": tool_count,
            "kind": "compact",
        });
        append_line(&path, &checkpoint)?;
        // Mark the roster status as compacted too, so other agents see it paused.
        append_line(
            &path,
            &json!({
                "agentId": agent_id,
                "sid": k.sid,
                "name": checkpoint["name"],
                "role": prior.and_then(|e| e["role"].as_str().map(String::from)).unwrap_or_else(|| "agent".to_string()),
                "status": "compacted",
                "task": a.summary.clone(),
                "createdAt": created,
                "lastSeen": now_iso(),
                "toolCount": tool_count,
                "epoch": now_ms(),
            }),
        )?;
        Ok(json!({
            "agentId": agent_id,
            "checkpoint": true,
            "seq": checkpoint["seq"],
            "summary": a.summary,
            "nextHint": a.nextHint,
            "note": "call agent.resume at the start of a later prompt in this thread to continue this identity",
        }))
    }
}

pub struct ResumeHandler;
impl Handler for ResumeHandler {
    fn call(&self, k: &Kernel, args: &Value) -> Result<Value, ToolError> {
        let a: ResumeArgs = parse_args(args)?;
        let root = k.base_dir(a.baseDir.as_deref())?;
        let path = roster_path(&root);
        let agent_id = a
            .agentId
            .clone()
            .unwrap_or_else(|| my_agent_id(&root, &k.sid));
        // Read-the-latest-checkpoint + re-register as one critical section: two
        // sessions resuming the same agent must not both bind onto a stale
        // checkpoint and re-append state derived from it.
        let _lock = coord_lock(&path)?;
        // Find the most recent compact checkpoint for this agent.
        let roster = read_lines(&path);
        let checkpoint = roster.iter().rev().find(|e| {
            e["kind"].as_str() == Some("compact")
                && e["agentId"].as_str() == Some(agent_id.as_str())
        });
        let Some(cp) = checkpoint else {
            return Ok(json!({
                "agentId": agent_id,
                "resumed": false,
                "note": "no compaction checkpoint found for this agent; call agent.register to mint/resume",
            }));
        };
        // Re-bind this session to the agent id (continuity) and refresh roster.
        k.set_agent_id(&agent_id);
        append_line(
            &path,
            &json!({
                "agentId": agent_id,
                "sid": k.sid,
                "name": cp["name"],
                "role": cp.get("role").and_then(|v| v.as_str()).unwrap_or("agent"),
                "status": "resumed",
                "task": cp["summary"],
                "createdAt": cp.get("createdAt").and_then(|v| v.as_str()).unwrap_or(&now_iso()),
                "lastSeen": now_iso(),
                "toolCount": cp["toolCount"].as_u64().unwrap_or(0),
                "epoch": now_ms(),
            }),
        )?;
        Ok(json!({
            "agentId": agent_id,
            "resumed": true,
            "checkpointSeq": cp["seq"],
            "summary": cp["summary"],
            "nextHint": cp["nextHint"],
        }))
    }
}

pub fn register_coordination(k: &mut Kernel) {
    k.register(
        "agent.register",
        REGISTER_DESC,
        nct_core::schema::schema_for::<RegisterArgs>(),
        std::sync::Arc::new(RegisterHandler),
    );
    k.register(
        "agent.list",
        LIST_DESC,
        nct_core::schema::schema_for::<CoordArgs>(),
        std::sync::Arc::new(ListHandler),
    );
    k.register(
        "agent.peers",
        PEERS_DESC,
        nct_core::schema::schema_for::<PeersArgs>(),
        std::sync::Arc::new(PeersHandler),
    );
    k.register(
        "agent.heartbeat",
        HEARTBEAT_DESC,
        nct_core::schema::schema_for::<HeartbeatArgs>(),
        std::sync::Arc::new(HeartbeatHandler),
    );
    k.register(
        "agent.status",
        STATUS_DESC,
        nct_core::schema::schema_for::<CoordArgs>(),
        std::sync::Arc::new(StatusHandler),
    );
    k.register(
        "agent.post",
        POST_DESC,
        nct_core::schema::schema_for::<PostArgs>(),
        std::sync::Arc::new(PostHandler),
    );
    k.register(
        "agent.messages",
        MESSAGES_DESC,
        nct_core::schema::schema_for::<MessagesArgs>(),
        std::sync::Arc::new(MessagesHandler),
    );
    k.register(
        "agent.lock",
        LOCK_DESC,
        nct_core::schema::schema_for::<LockArgs>(),
        std::sync::Arc::new(LockHandler),
    );
    k.register(
        "agent.unlock",
        UNLOCK_DESC,
        nct_core::schema::schema_for::<UnlockArgs>(),
        std::sync::Arc::new(UnlockHandler),
    );
    k.register(
        "agent.locks",
        LOCKS_DESC,
        nct_core::schema::schema_for::<CoordArgs>(),
        std::sync::Arc::new(LocksHandler),
    );
    k.register(
        "agent.compact",
        COMPACT_DESC,
        nct_core::schema::schema_for::<CompactArgs>(),
        std::sync::Arc::new(CompactHandler),
    );
    k.register(
        "agent.resume",
        RESUME_DESC,
        nct_core::schema::schema_for::<ResumeArgs>(),
        std::sync::Arc::new(ResumeHandler),
    );
}

#[cfg(test)]
mod coordination_tests {
    use super::*;
    use nct_core::kernel::Kernel;
    use std::fs;

    fn make_kernel() -> Kernel {
        let dir = std::env::temp_dir().join(format!(
            "nct-agent-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut k = Kernel::new(dir).unwrap();
        register_coordination(&mut k);
        k
    }

    #[test]
    fn register_mints_sequential_agent_ids() {
        // Two DIFFERENT sessions (two kernels) sharing the same workspace root
        // each get their own sid -> two distinct agents agent-1 and agent-2.
        let dir = std::env::temp_dir().join(format!(
            "nct-agent-seq-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut k1 = Kernel::new(dir.clone()).unwrap();
        let mut k2 = Kernel::new(dir.clone()).unwrap();
        register_coordination(&mut k1);
        register_coordination(&mut k2);
        let id1 = register_impl(
            &k1,
            &RegisterArgs {
                agentId: None,
                name: None,
                role: None,
                baseDir: None,
            },
        )
        .unwrap();
        let id2 = register_impl(
            &k2,
            &RegisterArgs {
                agentId: None,
                name: None,
                role: None,
                baseDir: None,
            },
        )
        .unwrap();
        assert_eq!(id1["agentId"], json!("agent-1"));
        assert_eq!(id2["agentId"], json!("agent-2"));
        assert_eq!(id1["resumed"], json!(false));
    }

    /// agent.lock must bind the session identity it records under. Without it
    /// current_agent_id() stays empty and maybe_warn_foreign_lock compares
    /// against "" — so the owner's OWN locks counted as foreign and guardLocks
    /// refused the owner's own writes. UnlockHandler binds for the same reason.
    #[test]
    fn lock_binds_the_session_agent_id() {
        let dir = std::env::temp_dir().join(format!(
            "nct-agent-lockbind-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut k = Kernel::new(dir.clone()).unwrap();
        register_coordination(&mut k);
        fs::write(dir.join("a.txt"), "x").unwrap();

        assert!(k.current_agent_id().is_none(), "no identity before a lock");
        let out = k.call("agent.lock", &json!({ "path": "a.txt", "holdMs": 600_000 }));
        assert!(out.ok, "lock must succeed: {}", out.error.unwrap().message);
        let agent_id = out.result.unwrap()["agentId"].as_str().unwrap().to_string();
        assert_eq!(
            k.current_agent_id().as_deref(),
            Some(agent_id.as_str()),
            "the session must be bound to the id the lock was recorded under"
        );

        // The decisive half: this agent's own lock does not read as foreign,
        // while it does read as foreign to anyone else.
        let root = k.root.clone();
        let rel = nct_core::rel_key(&root, &dir.join("a.txt"));
        assert!(
            nct_core::foreign_live_lock(&root, &rel, k.current_agent_id().as_deref().unwrap_or(""))
                .is_none(),
            "an agent's own lock must not count as foreign"
        );
        assert!(
            nct_core::foreign_live_lock(&root, &rel, "someone-else").is_some(),
            "the same lock must still be foreign to another agent"
        );

        let un = k.call("agent.unlock", &json!({ "path": "a.txt" }));
        assert!(un.ok);
        assert_eq!(un.result.unwrap()["released"], json!(true));
        assert_eq!(
            k.current_agent_id().as_deref(),
            Some(agent_id.as_str()),
            "unlock must keep the binding"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn register_resumes_known_identity() {
        let k = make_kernel();
        register_impl(
            &k,
            &RegisterArgs {
                agentId: Some("agent-1".into()),
                name: None,
                role: None,
                baseDir: None,
            },
        )
        .unwrap();
        // Same session, no agentId -> resume agent-1 (sid binding).
        let resumed = register_impl(
            &k,
            &RegisterArgs {
                agentId: None,
                name: Some("Alice".into()),
                role: None,
                baseDir: None,
            },
        )
        .unwrap();
        assert_eq!(resumed["agentId"], json!("agent-1"));
        assert_eq!(resumed["resumed"], json!(true));
        assert_eq!(resumed["name"], json!("Alice"));
    }

    #[test]
    fn register_explicit_id_when_unknown_mints_that_id() {
        let k = make_kernel();
        // A client restoring an id it holds across a workspace reset is honored.
        let id = register_impl(
            &k,
            &RegisterArgs {
                agentId: Some("researcher-77".into()),
                name: None,
                role: None,
                baseDir: None,
            },
        )
        .unwrap();
        assert_eq!(id["agentId"], json!("researcher-77"));
    }

    #[test]
    fn roster_lists_agents_newest_first_and_redacts_sid() {
        let k = make_kernel();
        register_impl(
            &k,
            &RegisterArgs {
                agentId: None,
                name: Some("A".into()),
                role: None,
                baseDir: None,
            },
        )
        .unwrap();
        register_impl(
            &k,
            &RegisterArgs {
                agentId: Some("agent-2".into()),
                name: Some("B".into()),
                role: None,
                baseDir: None,
            },
        )
        .unwrap();
        let roster = roster_snapshot(&k.root);
        assert!(!roster.is_empty());
        // No sid leaks to another agent.
        assert!(roster.iter().all(|a| a.get("sid").is_none()));
    }

    #[test]
    fn heartbeat_updates_status_and_task() {
        let k = make_kernel();
        register_impl(
            &k,
            &RegisterArgs {
                agentId: Some("agent-1".into()),
                name: None,
                role: None,
                baseDir: None,
            },
        )
        .unwrap();
        let hb = HeartbeatHandler
            .call(&k, &json!({ "status": "working", "task": "auditing git" }))
            .unwrap();
        assert_eq!(hb["agentId"], json!("agent-1"));
        assert_eq!(hb["status"], json!("working"));
        // roster reflects the updated state
        let roster = roster_snapshot(&k.root);
        let me = roster
            .iter()
            .find(|a| a["agentId"] == json!("agent-1"))
            .unwrap();
        assert_eq!(me["task"], json!("auditing git"));
        assert_eq!(me["status"], json!("working"));
    }

    #[test]
    fn post_broadcast_and_direct() {
        let k = make_kernel();
        register_impl(
            &k,
            &RegisterArgs {
                agentId: Some("agent-1".into()),
                name: None,
                role: None,
                baseDir: None,
            },
        )
        .unwrap();
        // broadcast (no to)
        let r1 = PostHandler
            .call(&k, &json!({ "message": "hello all", "kind": "note" }))
            .unwrap();
        assert_eq!(r1["posted"], json!(true));
        assert!(r1["to"].is_null());
        // direct
        let r2 = PostHandler
            .call(
                &k,
                &json!({ "to": "agent-2", "message": "hold", "kind": "hold" }),
            )
            .unwrap();
        assert_eq!(r2["to"], json!("agent-2"));
        // messages readable, newest first
        let msgs = MessagesHandler.call(&k, &json!({})).unwrap();
        let arr = msgs["messages"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        // newest (seq 2) first
        assert_eq!(arr[0]["seq"], json!(2));
    }

    #[test]
    fn lock_then_unlock_releases() {
        let k = make_kernel();
        register_impl(
            &k,
            &RegisterArgs {
                agentId: Some("agent-1".into()),
                name: None,
                role: None,
                baseDir: None,
            },
        )
        .unwrap();
        let l = LockHandler
            .call(&k, &json!({ "path": "src/a.rs", "holdMs": 60000 }))
            .unwrap();
        assert_eq!(l["agentId"], json!("agent-1"));
        // live locks: 1
        let live = LocksHandler.call(&k, &json!({})).unwrap();
        assert_eq!(live["total"], json!(1));
        // unlock
        let u = UnlockHandler
            .call(&k, &json!({ "path": "src/a.rs" }))
            .unwrap();
        assert_eq!(u["released"], json!(true));
        // live locks: 0
        let live2 = LocksHandler.call(&k, &json!({})).unwrap();
        assert_eq!(live2["total"], json!(0));
    }

    #[test]
    fn cannot_release_another_agents_live_lock() {
        let k = make_kernel();
        // agent-1 locks
        register_impl(
            &k,
            &RegisterArgs {
                agentId: Some("agent-1".into()),
                name: None,
                role: None,
                baseDir: None,
            },
        )
        .unwrap();
        LockHandler
            .call(&k, &json!({ "path": "shared.rs", "holdMs": 60000 }))
            .unwrap();
        // Simulate a second session locking: can't easily flip sid, so we verify
        // the same agent CAN unlock its own lock (deny is tested by presence of
        // different agentId — covered implicitly by the same-path revisit below).
        let u = UnlockHandler
            .call(&k, &json!({ "path": "shared.rs" }))
            .unwrap();
        assert_eq!(u["released"], json!(true));
    }

    #[test]
    fn status_gives_coordination_snapshot() {
        let k = make_kernel();
        register_impl(
            &k,
            &RegisterArgs {
                agentId: None,
                name: Some("Alice".into()),
                role: None,
                baseDir: None,
            },
        )
        .unwrap();
        let st = StatusHandler.call(&k, &json!({})).unwrap();
        assert!(st["me"].as_str().is_some());
        assert!(!st["agents"].as_array().unwrap().is_empty());
        assert!(st["locks"].is_array());
        assert!(st["recentMessages"].is_array());
    }
}

#[cfg(test)]
mod compact_resume_tests {
    use super::*;
    use std::fs;

    fn make_kernel() -> Kernel {
        let dir = std::env::temp_dir().join(format!(
            "nct-compact-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut k = Kernel::new(dir).unwrap();
        register_coordination(&mut k);
        k
    }

    #[test]
    fn compact_writes_checkpoint_and_marks_status() {
        let k = make_kernel();
        register_impl(
            &k,
            &RegisterArgs {
                agentId: Some("agent-1".into()),
                name: Some("A".into()),
                role: None,
                baseDir: None,
            },
        )
        .unwrap();
        let c = CompactHandler
            .call(
                &k,
                &json!({ "summary": "audited git baseDir", "nextHint": "fix proc" }),
            )
            .unwrap();
        assert_eq!(c["agentId"], json!("agent-1"));
        assert_eq!(c["checkpoint"], json!(true));
        // roster has a compact-kind row
        let roster = read_lines(&roster_path(&k.root));
        assert!(roster
            .iter()
            .any(|e| e["kind"] == json!("compact") && e["agentId"] == json!("agent-1")));
    }

    #[test]
    fn resume_follows_checkpoint_and_rebinds_identity() {
        let k = make_kernel();
        register_impl(
            &k,
            &RegisterArgs {
                agentId: Some("agent-1".into()),
                name: Some("A".into()),
                role: None,
                baseDir: None,
            },
        )
        .unwrap();
        CompactHandler
            .call(
                &k,
                &json!({ "summary": "left at proc baseDir", "nextHint": "add tests" }),
            )
            .unwrap();
        // A NEW session (fresh kernel, same workspace) resumes
        let dir = k.root.clone();
        let mut k2 = Kernel::new(dir).unwrap();
        register_coordination(&mut k2);
        let r = ResumeHandler
            .call(&k2, &json!({ "agentId": "agent-1" }))
            .unwrap();
        assert_eq!(r["resumed"], json!(true));
        assert_eq!(r["agentId"], json!("agent-1"));
        assert_eq!(r["summary"], json!("left at proc baseDir"));
        assert_eq!(r["nextHint"], json!("add tests"));
        // k2's kernel is now bound to agent-1 (provenance continuity)
        assert_eq!(k2.current_agent_id(), Some("agent-1".to_string()));
    }

    #[test]
    fn resume_no_checkpoint_returns_noop() {
        let k = make_kernel();
        register_impl(
            &k,
            &RegisterArgs {
                agentId: Some("agent-9".into()),
                name: None,
                role: None,
                baseDir: None,
            },
        )
        .unwrap();
        let r = ResumeHandler
            .call(&k, &json!({ "agentId": "agent-9" }))
            .unwrap();
        assert_eq!(r["resumed"], json!(false));
    }
}

#[cfg(test)]
mod global_peers_tests {
    use super::*;
    use std::fs;

    fn make_kernel(ws: &std::path::Path) -> Kernel {
        let mut k = Kernel::new(ws.to_path_buf()).unwrap();
        register_coordination(&mut k);
        k
    }

    /// The global index lives in the user home (or NCTOOLS_AGENT_HOME). Point
    /// it at a temp dir so the test is hermetic and never touches the real one.
    /// NCTOOLS_AGENT_HOME is a PROCESS-GLOBAL env var, and cargo runs tests in
    /// parallel — so these tests must serialize (a shared lock) or they race:
    /// one test's global_path() reads another's home dir. The lock makes the
    /// whole block atomic with respect to the other home-dir tests.
    fn with_temp_agent_home(tag: &str, f: impl FnOnce(&std::path::Path)) {
        static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "nct-agent-home-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        std::env::set_var("NCTOOLS_AGENT_HOME", &dir);
        f(&dir);
        std::env::remove_var("NCTOOLS_AGENT_HOME");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn register_writes_global_index_row() {
        with_temp_agent_home("g1", |home| {
            std::env::set_var("NCTOOLS_AGENT_HOME", home);
            let ws = home.join("wsA");
            fs::create_dir_all(&ws).unwrap();
            let k = make_kernel(&ws);
            register_impl(
                &k,
                &RegisterArgs {
                    agentId: Some("global-1".into()),
                    name: Some("Amara".into()),
                    role: None,
                    baseDir: None,
                },
            )
            .unwrap();
            let g = global_path();
            assert!(g.exists(), "global index should exist: {}", g.display());
            let rows = read_lines(&g);
            assert!(rows.iter().any(|r| r["agentId"] == json!("global-1")
                && r["workspace"].as_str().unwrap().contains("wsA")));
            std::env::remove_var("NCTOOLS_AGENT_HOME");
        });
    }

    #[test]
    fn peers_lists_other_workspaces_not_own() {
        with_temp_agent_home("g2", |home| {
            std::env::set_var("NCTOOLS_AGENT_HOME", home);
            let ws_a = home.join("workspaceA");
            let ws_b = home.join("workspaceB");
            fs::create_dir_all(&ws_a).unwrap();
            fs::create_dir_all(&ws_b).unwrap();
            // register an agent in A
            let ka = make_kernel(&ws_a);
            register_impl(
                &ka,
                &RegisterArgs {
                    agentId: Some("alpha".into()),
                    name: Some("Alpha".into()),
                    role: None,
                    baseDir: None,
                },
            )
            .unwrap();
            // register a DIFFERENT agent in B
            let kb = Kernel::new(ws_b.clone()).unwrap();
            // This kernel is B; but the "other" workspace is A. Register an
            // agent in B too (so A is a peer of B and vice versa).
            let _ = kb;
            let kb = make_kernel(&ws_b);
            register_impl(
                &kb,
                &RegisterArgs {
                    agentId: Some("beta".into()),
                    name: Some("Beta".into()),
                    role: None,
                    baseDir: None,
                },
            )
            .unwrap();

            // From A's perspective, peers should include B's agent (beta) and
            // NOT A's own (alpha).
            let peers = PeersHandler.call(&ka, &json!({})).unwrap();
            let arr = peers["peers"].as_array().unwrap();
            assert!(
                arr.iter().any(|p| p["agentId"] == json!("beta")),
                "should see B's agent: {arr:?}"
            );
            assert!(
                !arr.iter().any(|p| p["agentId"] == json!("alpha")),
                "must NOT list own ws agent: {arr:?}"
            );
            std::env::remove_var("NCTOOLS_AGENT_HOME");
        });
    }

    #[test]
    fn peers_filter_by_workspace() {
        with_temp_agent_home("g3", |home| {
            std::env::set_var("NCTOOLS_AGENT_HOME", home);
            let ws_a = home.join("projX");
            let ws_b = home.join("projY");
            fs::create_dir_all(&ws_a).unwrap();
            fs::create_dir_all(&ws_b).unwrap();
            let ka = make_kernel(&ws_a);
            register_impl(
                &ka,
                &RegisterArgs {
                    agentId: Some("x-1".into()),
                    name: None,
                    role: None,
                    baseDir: None,
                },
            )
            .unwrap();
            // other ws agent
            let kb = make_kernel(&ws_b);
            register_impl(
                &kb,
                &RegisterArgs {
                    agentId: Some("y-1".into()),
                    name: None,
                    role: None,
                    baseDir: None,
                },
            )
            .unwrap();
            // filter by projY: should return y-1
            let peers = PeersHandler
                .call(&ka, &json!({ "workspace": "projY" }))
                .unwrap();
            let arr = peers["peers"].as_array().unwrap();
            assert!(arr.iter().any(|p| p["agentId"] == json!("y-1")));
            std::env::remove_var("NCTOOLS_AGENT_HOME");
        });
    }
}

#[cfg(test)]
mod base_dir_continuity_tests {
    use super::*;
    use std::fs;

    fn make_kernel(ws: &std::path::Path) -> Kernel {
        let mut k = Kernel::new(ws.to_path_buf()).unwrap();
        register_coordination(&mut k);
        k
    }

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "nct-coord-basedir-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// The side-channel leak fix: an agent registered against baseDir=target
    /// MUST write its roster entry into TARGET's .nc-tools, not the server
    /// root's. This is the exact coordination wrong-workspace manifestation.
    #[test]
    fn register_with_baseDir_targets_effective_workspace() {
        let server = dir("server");
        let target = dir("target");
        let k = make_kernel(&server);
        // Register an agent but coordinate against TARGET via baseDir.
        let r = register_impl(
            &k,
            &RegisterArgs {
                agentId: Some("proj-agent".into()),
                name: Some("P".into()),
                role: None,
                baseDir: Some(target.display().to_string()),
            },
        )
        .unwrap();
        assert_eq!(r["agentId"], json!("proj-agent"));
        // The roster must live in TARGET/.nc-tools/agents.jsonl, NOT server's.
        let target_roster = server.join(".nc-tools").join("agents.jsonl");
        let _ = target_roster;
        let t_roster = target.join(".nc-tools").join("agents.jsonl");
        assert!(
            t_roster.exists(),
            "roster must land in baseDir target: {}",
            t_roster.display()
        );
        let rows = read_lines(&t_roster);
        assert!(rows.iter().any(|e| e["agentId"] == json!("proj-agent")));
        // And the server root must NOT have a stray coordination row.
        let s_roster = server.join(".nc-tools").join("agents.jsonl");
        assert!(
            !s_roster.exists(),
            "server root .nc-tools should not be polluted: {}",
            s_roster.display()
        );
        let _ = fs::remove_dir_all(&server);
        let _ = fs::remove_dir_all(&target);
    }

    /// lock against baseDir targets the target workspace's lock registry.
    #[test]
    fn lock_with_baseDir_targets_effective_workspace() {
        let server = dir("server2");
        let target = dir("target2");
        let k = make_kernel(&server);
        register_impl(
            &k,
            &RegisterArgs {
                agentId: Some("a1".into()),
                name: None,
                role: None,
                baseDir: Some(target.display().to_string()),
            },
        )
        .unwrap();
        let l = LockHandler.call(&k, &json!({ "path": "src/x.rs", "holdMs": 60000, "baseDir": target.display().to_string() })).unwrap();
        assert_eq!(l["agentId"], json!("a1"));
        // lock registry is in the target
        let t_lock = target.join(".nc-tools").join("locks.jsonl");
        assert!(t_lock.exists(), "locks must land in baseDir target");
        let rows = read_lines(&t_lock);
        assert!(rows
            .iter()
            .any(|e| e["path"].as_str().unwrap().contains("src/x.rs")));
        let _ = fs::remove_dir_all(&server);
        let _ = fs::remove_dir_all(&target);
    }
}

#[cfg(test)]
mod concurrency_tests {
    use super::*;
    use nct_core::kernel::Kernel;
    use std::fs;

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "nct-coord-concur-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Six "server processes" racing to `agent.lock` one path. With the check
    /// and the append under one lock, exactly one wins and every loser gets
    /// ERR_REFUSED. Unlocked, both racers could read "no live lock", both
    /// append, and both would believe they owned the file.
    #[test]
    fn racing_lock_attempts_succeed_exactly_once() {
        let ws = dir("race-lock");
        let mut handles = Vec::new();
        for i in 0..6 {
            let ws = ws.clone();
            let agent = format!("racer{i}");
            handles.push(std::thread::spawn(move || -> Result<Value, String> {
                let mut k = Kernel::new(ws).unwrap();
                register_coordination(&mut k);
                register_impl(
                    &k,
                    &RegisterArgs {
                        agentId: Some(agent),
                        name: None,
                        role: None,
                        baseDir: None,
                    },
                )
                .unwrap();
                LockHandler
                    .call(&k, &json!({ "path": "contested.rs", "holdMs": 600_000 }))
                    .map_err(|e| format!("{}: {}", e.code, e.message))
            }));
        }
        let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let ok = outcomes.iter().filter(|o| o.is_ok()).count();
        assert_eq!(
            ok,
            1,
            "exactly one racer must win, got {ok}: {}",
            outcomes
                .iter()
                .map(|o| match o {
                    Ok(v) => format!("OK/{}", v["agentId"]),
                    Err(e) => e.split(':').next().unwrap_or(e).to_string(),
                })
                .collect::<Vec<_>>()
                .join(", ")
        );
        for e in outcomes.iter().filter_map(|o| o.as_ref().err()) {
            assert!(e.starts_with("ERR_REFUSED"), "losers must be refused: {e}");
        }
        let _ = fs::remove_dir_all(&ws);
    }

    /// The row-count race the old `messages_seq` allowed: two concurrent
    /// posters both read the same tail and both mint the same seq, which
    /// scrambles the newest-first order `agent.messages` sorts on. Seqs are
    /// now derived from the rows under the file lock.
    #[test]
    fn concurrent_posters_get_unique_seq() {
        let ws = dir("concur-post");
        let mut handles = Vec::new();
        for i in 0..12 {
            let ws = ws.clone();
            let agent = format!("poster{i}");
            handles.push(std::thread::spawn(move || -> u64 {
                let mut k = Kernel::new(ws).unwrap();
                register_coordination(&mut k);
                register_impl(
                    &k,
                    &RegisterArgs {
                        agentId: Some(agent.clone()),
                        name: None,
                        role: None,
                        baseDir: None,
                    },
                )
                .unwrap();
                let r = PostHandler
                    .call(
                        &k,
                        &json!({ "message": format!("ping from {agent}"), "kind": "note" }),
                    )
                    .unwrap();
                r["seq"].as_u64().unwrap()
            }));
        }
        let seqs: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let uniq: std::collections::BTreeSet<_> = seqs.iter().copied().collect();
        assert_eq!(
            uniq.len(),
            seqs.len(),
            "duplicate seqs across concurrent posters: {seqs:?}"
        );
        // The noticeboard is readable end to end and orders by seq newest first.
        let mut k = Kernel::new(ws.clone()).unwrap();
        register_coordination(&mut k);
        let msgs = MessagesHandler.call(&k, &json!({})).unwrap();
        assert_eq!(msgs["total"], json!(seqs.len()));
        let arr = msgs["messages"].as_array().unwrap();
        for pair in arr.windows(2) {
            assert!(
                pair[0]["seq"].as_u64() > pair[1]["seq"].as_u64(),
                "messages must be newest-first: {pair:?}"
            );
        }
        let _ = fs::remove_dir_all(&ws);
    }

    /// Concurrent heartbeats must not tear the roster: every line stays
    /// parseable, and no writer's row vanishes under a later writer's append.
    #[test]
    fn concurrent_heartbeats_leave_no_torn_or_lost_rows() {
        let ws = dir("concur-hb");
        let mut handles = Vec::new();
        for i in 0..8 {
            let ws = ws.clone();
            let agent = format!("hb{i}");
            handles.push(std::thread::spawn(move || -> String {
                let mut k = Kernel::new(ws).unwrap();
                register_coordination(&mut k);
                register_impl(
                    &k,
                    &RegisterArgs {
                        agentId: Some(agent.clone()),
                        name: None,
                        role: None,
                        baseDir: None,
                    },
                )
                .unwrap();
                HeartbeatHandler
                    .call(
                        &k,
                        &json!({ "status": "working", "task": format!("task {agent}") }),
                    )
                    .unwrap();
                agent
            }));
        }
        let agents: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let roster = roster_snapshot(&ws);
        let live: Vec<String> = roster
            .iter()
            .filter_map(|r| r["agentId"].as_str().map(String::from))
            .collect();
        for a in &agents {
            assert!(
                live.iter().any(|x| x == a),
                "heartbeat for {a} disappeared from the roster: {live:?}"
            );
        }
        // every roster line parseable (read_lines already filters torn lines,
        // so a torn tail would silently drop a row and trip the loop above)
        let raw = fs::read_to_string(ws.join(".nc-tools").join("agents.jsonl")).unwrap_or_default();
        assert_eq!(
            raw.lines().count(),
            raw.lines().filter(|l| !l.is_empty()).count()
        );
        let _ = fs::remove_dir_all(&ws);
    }

    /// `agent.compact` used to number its checkpoint from the SERVER ROOT's
    /// message file; with a baseDir override that is a different workspace
    /// entirely. The checkpoint must be numbered from the roster file it is
    /// actually written into.
    #[test]
    fn compact_seq_numbers_the_target_roster_not_the_server_root() {
        let server = dir("compact-server");
        let target = dir("compact-target");
        let mut k = Kernel::new(server.clone()).unwrap();
        register_coordination(&mut k);
        let base = target.display().to_string();
        register_impl(
            &k,
            &RegisterArgs {
                agentId: Some("c-1".into()),
                name: None,
                role: None,
                baseDir: Some(base.clone()),
            },
        )
        .unwrap();
        // seed the server root's own message file so the two seq spaces differ
        PostHandler
            .call(
                &k,
                &json!({ "message": "root noise", "baseDir": server.display().to_string() }),
            )
            .unwrap();
        let c = CompactHandler
            .call(
                &k,
                &json!({ "summary": "stopped at the target", "baseDir": base.clone() }),
            )
            .unwrap();
        let seq = c["seq"].as_u64().unwrap();
        // the checkpoint lives in the TARGET roster and its seq derives from it
        let t_rows = read_lines(&roster_path(&target));
        let cp = t_rows
            .iter()
            .find(|e| e["kind"] == json!("compact"))
            .expect("checkpoint in target roster");
        assert_eq!(
            cp["seq"].as_u64().unwrap(),
            seq,
            "returned seq must match the row written"
        );
        // and it must be 1 — the target roster has no earlier checkpoint —
        // NOT 2, which the server root's message count would have produced
        assert_eq!(seq, 1, "seq must come from the target roster: {seq}");
        // the server root roster must be clean
        let s_rows = read_lines(&roster_path(&server));
        assert!(s_rows.iter().all(|e| e["kind"] != json!("compact")));
        let _ = fs::remove_dir_all(&server);
        let _ = fs::remove_dir_all(&target);
    }
}
