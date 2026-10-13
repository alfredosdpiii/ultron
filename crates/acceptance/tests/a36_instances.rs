//! A36 / A40 (partial): invocation scratch is isolated while declared instance state persists; retained instances
//! accept new invocations without rewriting old terminal outcomes.
//! Ported from packages/coding-agent/test/ultron-instances.test.ts: "reset clears scratch and restores bindings while
//! declared state persists", against the real runtime.py under the Rust kernel supervisor (`Kernel::reset_scratch`).
//! Not ported: the three retained-instance cases ("continue on the same lane with fresh scratch...", "reject
//! concurrent use, closed instances, non-parents, and laneless tasks", "an instance survives an owner restart and
//! keeps its lane"). They run `agents.spawn` of `rlm-child@1` and the deterministic `identity@1` through
//! `NativeRlmHost`, which prompts a named task lane, reads its reply as the task result, reuses the lane for
//! `instances.invoke` and resets its scratch first (`beforeLaneReuse`). Rust has `kernel_apis::InstancesModule` but no
//! task runtime that runs definitions on named lanes (definition registry, lane reuse, deterministic adapters).

mod common;

use common::{kernel_in, null_handler, value};
use kernel::Status;

#[test]
fn acceptance_a36_reset_clears_scratch_and_restores_bindings_while_declared_state_persists_ported_part() {
    let dir = tempfile::tempdir().unwrap();
    let kernel = kernel_in(dir.path(), null_handler());
    value(
        &kernel,
        "scratch = 'first call'\nstate['calls'] = state.get('calls', 0) + 1\nagents = None",
    );
    let reset = kernel.reset_scratch().unwrap();
    assert_eq!(reset.status, Status::Ok, "{reset:?}");
    value(&kernel, "state['calls'] = state.get('calls', 0) + 1");
    assert_eq!(
        value(&kernel, "('scratch' in globals(), state['calls'], agents is not None)"),
        "(False, 2, True)"
    );
    kernel.shutdown();
}
