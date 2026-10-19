//! Isolation for the host's operations, from the `sandbox` crate.
//!
//! [`Host::sandboxed`] turns it on: shell commands (blocking bash and shell jobs) run under the session's
//! [`sandbox::Launcher`]. Writes (runtime.py's `write`/`edit`, through `files.before_write`) are also contained:
//! outside the sandbox's writable paths they are blocked, as the shell's would be. There are no approvals: every
//! operation the sandbox contains is allowed.

use crate::{Guard, Host};
use sandbox::Settings;
use sandbox::launcher::{Launcher, Mechanism, Policy, SANDBOX_PATH};
use sandbox::settings::Mode;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;

/// The write containment check in front of another guard (Loki, file hooks), which still runs after it.
pub struct SandboxGuard {
    pub inner: Box<dyn Guard>,
    pub launcher: Arc<Launcher>,
}

impl Guard for SandboxGuard {
    fn before_shell(&self, command: &str) -> Option<String> {
        self.before_shell_on("main", command)
    }

    fn before_shell_on(&self, lane: &str, command: &str) -> Option<String> {
        self.inner.before_shell_on(lane, command)
    }

    fn before_write(&self, path: &str, content: &str) -> Value {
        self.before_write_on("main", path, content)
    }

    fn before_write_on(&self, lane: &str, path: &str, content: &str) -> Value {
        if self.launcher.mode != Mode::Off {
            let target = self.launcher.policy.cwd.join(path);
            if !self.launcher.policy.may_write(&target) {
                return json!({ "blocked": true, "reason": outside_message(&self.launcher.policy) });
            }
        }
        self.inner.before_write_on(lane, path, content)
    }

    fn guard_stats(&self) -> Vec<Value> {
        self.inner.guard_stats()
    }

    fn begin_turn(&self) {
        self.inner.begin_turn();
    }

    fn cell_started(&self, lane: &str) {
        self.inner.cell_started(lane);
    }

    fn cell_ended(&self, lane: &str) {
        self.inner.cell_ended(lane);
    }

    fn take_pending(&self, lane: &str) -> Option<String> {
        self.inner.take_pending(lane)
    }
}

/// The environment a sandbox policy is built from: the host's base environment (the session's, see
/// [`Host::with_base_env`]); a host without one gets an empty environment plus `PATH` = [`SANDBOX_PATH`] and `HOME` =
/// the sandbox's `home`, never this process's environment (its PATH, credentials or `DOCKER_HOST` would otherwise
/// reach the sandbox and its launcher).
pub fn sandbox_host_env(base: Option<&[(String, String)]>, home: &Path) -> Vec<(String, String)> {
    match base {
        Some(env) => env.to_vec(),
        None => vec![
            ("PATH".to_string(), SANDBOX_PATH.to_string()),
            ("HOME".to_string(), home.display().to_string()),
        ],
    }
}

fn outside_message(policy: &Policy) -> String {
    let roots: Vec<String> = policy.writable.iter().map(|p| p.display().to_string()).collect();
    format!(
        "it is outside the sandbox's writable paths ({}); add the path to sandbox.writable to allow it",
        roots.join(", ")
    )
}

impl Host {
    /// Run this host's shell commands isolated and contain its writes by `settings` (`sandbox.*`). `home` is the
    /// user's home, `tmp` the session temp dir seen as /tmp inside (None: a private /tmp per command).
    pub fn sandboxed(self, settings: &Value, home: &Path, tmp: Option<&Path>) -> Self {
        self.sandboxed_under(settings, home, tmp, None)
    }

    /// [`Host::sandboxed`] under `mechanism` instead of the strongest one that works here (tests run every backend
    /// this way; None runs unisolated, as `auto` does when nothing works).
    pub fn sandboxed_with_mechanism(
        self,
        settings: &Value,
        home: &Path,
        tmp: Option<&Path>,
        mechanism: Option<Mechanism>,
    ) -> Self {
        self.sandboxed_under(settings, home, tmp, Some(mechanism))
    }

    fn sandboxed_under(
        mut self,
        settings: &Value,
        home: &Path,
        tmp: Option<&Path>,
        mechanism: Option<Option<Mechanism>>,
    ) -> Self {
        let sandbox_settings = Settings::from_settings(settings);
        let host_env = sandbox_host_env(self.base_env.as_deref().map(Vec::as_slice), home);
        let policy = Policy::new(
            &sandbox_settings,
            &self.cwd,
            home,
            &[self.jobs_dir.clone(), self.spill_dir.clone()],
            tmp,
            &host_env,
        );
        let launcher = Launcher::new(sandbox_settings.mode, policy);
        let launcher = Arc::new(match mechanism {
            Some(mechanism) => launcher.with_mechanism(mechanism),
            None => launcher,
        });
        let inner = std::mem::replace(&mut self.guard, Box::new(crate::AllowAll));
        self.guard = Box::new(SandboxGuard {
            inner,
            launcher: launcher.clone(),
        });
        self.launcher = Some(launcher);
        self
    }

    /// The launcher of a sandboxed host (for `doctor` and status lines).
    pub fn launcher(&self) -> Option<&Arc<Launcher>> {
        self.launcher.as_ref()
    }
}
