//! How the session runs a spawn contract's `run` checks (`rlm.spawn(contract={"checks": [{"run": ...}]})`, see
//! `delegation::spawn_contract`): `/bin/sh -c` in the child's working directory with its environment, under the
//! session's shell sandbox (the policy of its bash, with the child's worktree writable too), never inside the child.

use crate::compose::Stack;
use delegation::spawn_contract::{CheckRunner, CommandCheck, CommandOutcome, run_command};
use std::process::Command;
use std::sync::{Arc, Weak};

/// The session's check runner: its sandbox launcher, read per check.
pub fn check_runner(stack: Weak<Stack>) -> CheckRunner {
    Arc::new(move |check: &CommandCheck| {
        let launcher = stack.upgrade().and_then(|stack| stack.host.launcher().cloned());
        run_check(check, launcher.as_deref())
    })
}

/// Run one check command, isolated by `launcher` when there is one.
pub fn run_check(check: &CommandCheck, launcher: Option<&sandbox::Launcher>) -> CommandOutcome {
    let argv = vec!["/bin/sh".to_string(), "-c".to_string(), check.command.clone()];
    let mut command = match launcher {
        Some(launcher) => {
            let cwd = std::fs::canonicalize(&check.cwd).unwrap_or_else(|_| check.cwd.clone());
            // A worktree child's directory is outside the session's project: it is writable for its checks too.
            let widened = (!launcher.policy.may_write(&cwd)).then(|| {
                let mut policy = launcher.policy.clone();
                policy.writable.push(cwd);
                sandbox::Launcher::new(launcher.mode, policy)
            });
            let launcher = widened.as_ref().unwrap_or(launcher);
            match launcher.command(&host::sandboxed_argv(argv, &check.env), &check.cwd) {
                Ok(command) => command,
                Err(error) => {
                    return CommandOutcome {
                        error: Some(error),
                        ..CommandOutcome::default()
                    };
                }
            }
        }
        None => {
            let mut command = Command::new(&argv[0]);
            command.args(&argv[1..]).current_dir(&check.cwd);
            command
        }
    };
    command.envs(check.env.iter().map(|(key, value)| (key, value)));
    // A child server's verdict token authenticates only its own `rlm.finish`; checks never see it.
    command.env_remove(host::PARENT_TOKEN_ENV);
    run_command(command, check.timeout, &check.signal)
}
