//! A kernel given a base environment (a session's own) starts from it instead of this process's: its variables
//! (through the credential filter), its PATH, and the `ULTRON_*` settings the supervisor reads (`ULTRON_PYTHON`, the
//! resource limits). And the process-group kill guard, tested as a pure function only: no signal is ever sent here.

use kernel::{HostHandler, Kernel, KernelOptions, Signal, Status, command_group_target};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

fn handler() -> Arc<dyn HostHandler> {
    Arc::new(|_: &str, _: Map<String, Value>, _: &Signal| Ok(Value::Null))
}

fn options(base: &[(&str, &str)]) -> KernelOptions {
    let mut options = KernelOptions::new(env!("CARGO_MANIFEST_DIR"), runtime());
    options.base_env = Some(base.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect());
    options
}

#[test]
fn kernel_w16f_base_env_replaces_the_process_environment_of_the_kernel() {
    let kernel = Kernel::new(
        options(&[
            ("PATH", "/w16f/session/bin"),
            ("W16F_SESSION_ONLY", "from-session"),
            ("W16F_API_TOKEN", "hidden"),
            ("ULTRON_RLM_MAX_MEMORY_MB", "1234"),
            ("ULTRON_RLM_MAX_CPU_SECONDS", "77"),
        ]),
        handler(),
    )
    .unwrap();
    // The limits are read from the base environment.
    assert_eq!(
        (kernel.limits().max_memory_mb, kernel.limits().max_cpu_seconds),
        (1234, 77)
    );
    let result = kernel
        .execute(
            "import os\n(os.environ.get('PATH'), os.environ.get('W16F_SESSION_ONLY'), 'W16F_API_TOKEN' in os.environ, \
             os.environ.get('CARGO_MANIFEST_DIR'), os.environ['ULTRON_RLM_MAX_MEMORY_MB'])",
            None,
            None,
        )
        .unwrap();
    assert_eq!(result.status, Status::Ok, "{result:?}");
    // CARGO_MANIFEST_DIR is in this process's environment (cargo sets it for tests), never in the kernel's.
    assert_eq!(
        result.result.as_deref(),
        Some("('/w16f/session/bin', 'from-session', False, None, '1234')")
    );
    kernel.shutdown();
}

#[test]
fn kernel_w16f_the_interpreter_comes_from_the_base_envs_ultron_python() {
    let kernel = Kernel::new(options(&[("ULTRON_PYTHON", "/w16f/missing/python3")]), handler()).unwrap();
    let error = kernel.start().unwrap_err();
    assert!(error.message().starts_with("RLM kernel spawn failed"), "{error:?}");
    kernel.shutdown();
}

#[test]
fn kernel_w16f_only_a_real_command_group_other_than_the_hosts_own_may_be_signalled() {
    let (own_pid, own_group) = (4242, 4000);
    for refused in [0, -1, -4242, 1, own_pid, own_group, i32::MIN] {
        assert_eq!(command_group_target(refused, own_pid, own_group), None, "{refused}");
    }
    for allowed in [2, 4001, 4243, i32::MAX] {
        assert_eq!(
            command_group_target(allowed, own_pid, own_group).map(|pid| pid.as_raw_nonzero().get()),
            Some(allowed)
        );
    }
}
