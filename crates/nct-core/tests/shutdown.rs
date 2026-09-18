// Process-wide shutdown semantics live in their own integration-test binary:
// the flag is global, and unit tests inside the lib target run on shared
// threads, so a sticky global must not leak into sibling tests.
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[test]
fn shutdown_flag_turns_cancel_true_and_is_sticky() {
    assert!(!nct_core::cancel::shutdown_requested());
    let flag = Arc::new(AtomicBool::new(false));
    nct_core::set_cancel_flag(Some(flag));
    assert!(!nct_core::is_cancelled());

    nct_core::cancel::request_shutdown();
    assert!(nct_core::cancel::shutdown_requested());
    // A per-thread flag that is still false must not mask the global shutdown:
    // in-flight wait loops have to see it from any thread.
    assert!(nct_core::is_cancelled());

    nct_core::request_shutdown(); // idempotent
    assert!(nct_core::is_cancelled());
}
