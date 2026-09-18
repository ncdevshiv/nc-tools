// Per-call cancellation + progress reporting for long-running handlers.
//
// The MCP front-end runs each `tools/call` on its own worker thread and
// installs two thread-local contexts before dispatching:
//   * a cancellation flag, flipped when the client sends
//     `notifications/cancelled` for that request;
//   * a progress sink, present only when the request carried
//     `_meta.progressToken` (the MCP spec way of asking for progress).
//
// Handlers poll `is_cancelled()` from their wait loops and call
// `report_progress(...)`; they never touch the transport. Scoped sub-workers
// (batch.execute) re-install the parent call's flag and progress context via
// `current_cancel_flag()` / `current_progress_ctx()` so cancellation covers
// every call path. Callers with no context installed (replay, direct kernel
// use) simply observe false / no-op.
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

/// Transport callback: receives the progress notification `params` object.
pub type ProgressSink = Arc<dyn Fn(Value) + Send + Sync>;

#[derive(Clone)]
pub struct ProgressCtx {
    /// The client's `_meta.progressToken`, preserved as JSON so numeric
    /// tokens stay numeric on the wire (the SDK correlates by Number()).
    pub token: Value,
    pub sink: ProgressSink,
}

thread_local! {
    static CANCEL: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
    static PROGRESS: RefCell<Option<ProgressCtx>> = const { RefCell::new(None) };
}

/// Process-wide shutdown flag, set when the MCP transport closes (stdin EOF)
/// or the idle watcher decides to exit. In-flight wait loops observe it
/// through `is_cancelled()` and stop their children instead of orphaning
/// them, so process exit cannot leak a running build/server.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Arm the process-wide shutdown. Idempotent; there is no un-arm — the
/// process is going away.
pub fn request_shutdown() {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Undo an armed shutdown when the arming decision is later abandoned (the
/// idle watcher may arm, then find a worker started in the gap and continue
/// serving). Only the idle watcher uses this; EOF never un-arms.
pub fn clear_shutdown() {
    SHUTDOWN.store(false, Ordering::SeqCst);
}

pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

/// Install (or clear) the cancellation flag for the current worker thread.
pub fn set_cancel_flag(flag: Option<Arc<AtomicBool>>) {
    CANCEL.with(|c| *c.borrow_mut() = flag);
}

/// Install (or clear) the progress context for the current worker thread.
pub fn set_progress_ctx(ctx: Option<ProgressCtx>) {
    PROGRESS.with(|p| *p.borrow_mut() = ctx);
}

/// The current thread's cancellation flag, if one is installed. Scoped
/// sub-workers (batch.execute) re-install the parent's flag so cancellation
/// and progress cover every call path, not just the top-level tool call.
pub fn current_cancel_flag() -> Option<Arc<AtomicBool>> {
    CANCEL.with(|c| c.borrow().clone())
}

/// The current thread's progress context, if one is installed (see
/// `current_cancel_flag` for why scoped workers need it).
pub fn current_progress_ctx() -> Option<ProgressCtx> {
    PROGRESS.with(|p| p.borrow().clone())
}

/// True when the client cancelled the call currently running on this thread,
/// or the whole process has begun shutting down.
pub fn is_cancelled() -> bool {
    if SHUTDOWN.load(Ordering::SeqCst) {
        return true;
    }
    CANCEL.with(|c| {
        c.borrow()
            .as_ref()
            .map(|f| f.load(Ordering::SeqCst))
            .unwrap_or(false)
    })
}

/// Emit a progress notification for the current call. No-op when the client
/// did not ask for progress or no transport context is installed (batch
/// sub-calls, direct kernel use).
pub fn report_progress(progress: f64, total: Option<f64>, message: Option<&str>) {
    PROGRESS.with(|p| {
        let borrow = p.borrow();
        let Some(ctx) = borrow.as_ref() else {
            return;
        };
        let mut params = json!({ "progressToken": ctx.token, "progress": progress });
        if let Some(t) = total {
            params["total"] = json!(t);
        }
        if let Some(m) = message {
            params["message"] = json!(m);
        }
        (ctx.sink)(params);
    });
}

/// Standard error for a cancelled call: structured, with the remediation note
/// that only journaled effects before the cancellation are guaranteed.
pub fn cancelled_error(tool: &str) -> crate::errors::ToolError {
    crate::errors::ToolError::with_hint(
        crate::errors::codes::CANCELLED,
        format!("tool '{tool}' was cancelled by the client"),
        json!({
            "tool": tool,
            "note": "notifications/cancelled received; work stopped. Only effects journaled before the cancellation are guaranteed."
        }),
    )
}

/// Kill a child and its descendant tree, then reap it.
///
/// `Child::kill` alone leaves grandchildren running — a test runner's per-file
/// child process, an npm script's `node`, a build's compiler — which is the
/// orphan class the cancel/shutdown paths must not leak. Windows ships
/// `taskkill /T /F` for exactly this; on Unix we fall back to the direct
/// kill because we do not spawn children into their own process group (and
/// must not signal our own group).
pub fn kill_child_tree(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let pid = child.id().to_string();
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid, "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(crate::CREATE_NO_WINDOW)
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_context_means_no_cancel_and_no_progress() {
        assert!(!is_cancelled());
        // no sink installed: must not panic
        report_progress(1.0, Some(2.0), Some("x"));
    }

    #[test]
    fn cancel_flag_is_observed() {
        let flag = Arc::new(AtomicBool::new(false));
        set_cancel_flag(Some(flag.clone()));
        assert!(!is_cancelled());
        flag.store(true, Ordering::SeqCst);
        assert!(is_cancelled());
        set_cancel_flag(None);
        assert!(!is_cancelled());
    }

    #[test]
    fn accessors_round_trip_installed_context() {
        let flag = Arc::new(AtomicBool::new(false));
        let seen: Arc<std::sync::Mutex<Vec<Value>>> = Default::default();
        let sink: ProgressSink = {
            let seen = seen.clone();
            Arc::new(move |v| seen.lock().unwrap().push(v))
        };
        set_cancel_flag(Some(flag.clone()));
        set_progress_ctx(Some(ProgressCtx {
            token: json!(7),
            sink,
        }));
        let got = current_cancel_flag().expect("flag installed");
        assert!(Arc::ptr_eq(&got, &flag));
        assert_eq!(
            current_progress_ctx().expect("ctx installed").token,
            json!(7)
        );
        set_cancel_flag(None);
        set_progress_ctx(None);
        assert!(current_cancel_flag().is_none());
        assert!(current_progress_ctx().is_none());
    }

    #[test]
    fn progress_reaches_the_sink_with_token_and_message() {
        let seen: Arc<std::sync::Mutex<Vec<Value>>> = Default::default();
        let sink: ProgressSink = {
            let seen = seen.clone();
            Arc::new(move |v| seen.lock().unwrap().push(v))
        };
        set_progress_ctx(Some(ProgressCtx {
            token: json!("tok-1"),
            sink,
        }));
        report_progress(3.0, Some(10.0), Some("running"));
        set_progress_ctx(None);
        let events = seen.lock().unwrap().clone();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["progressToken"], json!("tok-1"));
        assert_eq!(events[0]["progress"], json!(3.0));
        assert_eq!(events[0]["total"], json!(10.0));
        assert_eq!(events[0]["message"], json!("running"));
    }
}
