//! Isolation and approvals for the host's operations, from the `sandbox` crate.
//!
//! [`Host::sandboxed`] turns them on: shell commands (blocking bash and shell jobs) run under the session's
//! [`sandbox::Launcher`], and the guard decides every shell and write request before it runs. A denied request fails
//! the host call, so the cell raises with the message; a `prompt` goes to the [`sandbox::Approver`] (the UI) and with
//! none fails closed. Writes (runtime.py's `write`/`edit`, through `files.before_write`) are also contained: outside
//! the sandbox's writable paths they are blocked, as the shell's would be.

use crate::{Guard, Host};
use sandbox::launcher::{Launcher, Mechanism, Policy};
use sandbox::settings::Mode;
use sandbox::{Approvals, Approver, Settings};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;

/// The approval and containment checks in front of another guard (Loki, file hooks), which still runs after them.
pub struct SandboxGuard {
    pub inner: Box<dyn Guard>,
    pub approvals: Approvals,
    pub launcher: Arc<Launcher>,
}

impl Guard for SandboxGuard {
    fn before_shell(&self, command: &str) -> Option<String> {
        self.before_shell_on("main", command)
    }

    fn before_shell_on(&self, lane: &str, command: &str) -> Option<String> {
        if let Err(reason) = self.approvals.check("bash", Some(command)) {
            return Some(reason);
        }
        self.inner.before_shell_on(lane, command)
    }

    fn before_write(&self, path: &str, content: &str) -> Value {
        self.before_write_on("main", path, content)
    }

    fn before_write_on(&self, lane: &str, path: &str, content: &str) -> Value {
        if let Err(reason) = self.approvals.check("files.before_write", Some(path)) {
            return json!({ "blocked": true, "reason": reason });
        }
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

fn outside_message(policy: &Policy) -> String {
    let roots: Vec<String> = policy.writable.iter().map(|p| p.display().to_string()).collect();
    format!(
        "it is outside the sandbox's writable paths ({}); add the path to sandbox.writable to allow it",
        roots.join(", ")
    )
}

impl Host {
    /// Run this host's shell commands isolated and decide its shell and write requests by `settings`
    /// (`sandbox.*`, `approval.*`). `home` is the user's home, `tmp` the session temp dir seen as /tmp inside (None:
    /// a private /tmp per command), `approver` the UI that answers `prompt` decisions (None: they fail closed).
    pub fn sandboxed(
        self,
        settings: &Value,
        home: &Path,
        tmp: Option<&Path>,
        approver: Option<Arc<dyn Approver>>,
    ) -> Self {
        self.sandboxed_under(settings, home, tmp, approver, None)
    }

    /// [`Host::sandboxed`] under `mechanism` instead of the strongest one that works here (tests run every backend
    /// this way; None runs unisolated, as `auto` does when nothing works).
    pub fn sandboxed_with_mechanism(
        self,
        settings: &Value,
        home: &Path,
        tmp: Option<&Path>,
        approver: Option<Arc<dyn Approver>>,
        mechanism: Option<Mechanism>,
    ) -> Self {
        self.sandboxed_under(settings, home, tmp, approver, Some(mechanism))
    }

    fn sandboxed_under(
        mut self,
        settings: &Value,
        home: &Path,
        tmp: Option<&Path>,
        approver: Option<Arc<dyn Approver>>,
        mechanism: Option<Option<Mechanism>>,
    ) -> Self {
        let sandbox_settings = Settings::from_settings(settings);
        let host_env: Vec<(String, String)> = std::env::vars().collect();
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
        let mut approvals = Approvals::from_settings(settings);
        approvals.approver = approver;
        let inner = std::mem::replace(&mut self.guard, Box::new(crate::AllowAll));
        self.guard = Box::new(SandboxGuard {
            inner,
            approvals,
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
