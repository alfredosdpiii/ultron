//! A user's own shell command (RPC `bash`, the TUI's `!`/`!!`) as Pi runs it (rpc-mode.ts `bash`,
//! interactive-mode.ts `handleBashCommand`, AgentSession `executeBash`/`recordBashResult`):
//!
//! 1. the extensions' `user_bash` (`{ command, excludeFromContext, cwd }`): the first `{ result }` replaces the
//!    command (it is recorded as if it ran, and answered as the extension gave it); a handler that throws or answers
//!    an invalid result fails the command closed (it does not run, nothing is recorded; the error was reported to the
//!    error listeners as `extension_error`); `undefined` everywhere runs it;
//! 2. otherwise the host's blocking bash, each output chunk streamed (`bash_execution_update`).
//!
//! The result is recorded as a `bashExecution` message. `{ operations }` (custom execution functions) cannot cross the
//! extension process boundary and is an invalid result here (crates/extensions PROTOCOL.md).

use super::BashOutcome;
use extensions::ExtensionHost;
use host::Host;
use kernel::Signal;
use serde_json::{Map, Value, json};

/// The answer member of a command an extension failed closed: `{ "userBashFailed": <error> }`.
pub const USER_BASH_FAILED: &str = "userBashFailed";

/// Run `command` as a user's command: `user_bash`, then the shell (output chunks to `on_chunk`).
pub(super) fn run(
    extensions: Option<&ExtensionHost>,
    host: &Host,
    command: &str,
    exclude: bool,
    cwd: &str,
    signal: &Signal,
    on_chunk: &mut dyn FnMut(&str),
) -> Result<BashOutcome, String> {
    if let Some(extensions) = extensions.filter(|e| e.router().has_handlers("user_bash")) {
        let event = json!({"type": "user_bash", "command": command, "excludeFromContext": exclude, "cwd": cwd});
        match extensions.router().emit_user_bash(&event) {
            // Already reported to the error listeners; TypeScript does not fall back to running it.
            Err(error) => return Ok(BashOutcome::FailedClosed(error.error)),
            Ok(Some(answer)) => return Ok(BashOutcome::Ran(answer["result"].clone())),
            Ok(None) => {}
        }
    }
    let result = host.user_bash(command, signal, on_chunk)?;
    Ok(BashOutcome::Ran(pi_result(&result)))
}

/// The host's bash result as bash-executor.ts's `BashResult` (`exitCode` and `fullOutputPath` only when set).
pub(super) fn pi_result(result: &Value) -> Value {
    let mut data = Map::new();
    for (from, to) in [
        ("output", "output"),
        ("exit_code", "exitCode"),
        ("cancelled", "cancelled"),
        ("truncated", "truncated"),
        ("full_output_path", "fullOutputPath"),
    ] {
        if !result[from].is_null() {
            data.insert(to.into(), result[from].clone());
        }
    }
    Value::Object(data)
}

/// `recordBashResult`'s `BashExecutionMessage` for a `BashResult`: members the result leaves out (`undefined`) are
/// left out; `excludeFromContext` only for `!!`.
pub(super) fn bash_message(command: &str, result: &Value, exclude: bool, timestamp: i64) -> Value {
    let mut message = Map::new();
    message.insert("role".into(), "bashExecution".into());
    message.insert("command".into(), command.into());
    message.insert("output".into(), result["output"].clone());
    if let Some(code) = result.get("exitCode") {
        message.insert("exitCode".into(), code.clone());
    }
    message.insert("cancelled".into(), result["cancelled"].clone());
    message.insert("truncated".into(), result["truncated"].clone());
    if let Some(path) = result.get("fullOutputPath") {
        message.insert("fullOutputPath".into(), path.clone());
    }
    message.insert("timestamp".into(), timestamp.into());
    if exclude {
        message.insert("excludeFromContext".into(), true.into());
    }
    Value::Object(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// recordBashResult over a local result (rpc_pi_events.json `m1`): the cancelled command's absent exit code
    /// stays absent; an extension's `null` exit code and path are kept as given.
    #[test]
    fn session_core_user_bash_message_keeps_the_result_members_it_was_given() {
        let local = pi_result(
            &json!({"output": "x", "exit_code": null, "cancelled": true, "truncated": false,
            "timed_out": false, "full_output_path": null}),
        );
        assert_eq!(local, json!({"output": "x", "cancelled": true, "truncated": false}));
        assert_eq!(
            bash_message("c", &local, false, 5),
            json!({"role": "bashExecution", "command": "c", "output": "x", "cancelled": true, "truncated": false,
                "timestamp": 5})
        );
        let given = json!({"output": "y", "exitCode": null, "cancelled": false, "truncated": true,
            "fullOutputPath": null});
        assert_eq!(
            bash_message("d", &given, true, 6),
            json!({"role": "bashExecution", "command": "d", "output": "y", "exitCode": null, "cancelled": false,
                "truncated": true, "fullOutputPath": null, "timestamp": 6, "excludeFromContext": true})
        );
    }
}
