//! A user's own shell command (RPC `bash`, the TUI's `!`/`!!`) as Pi runs it (rpc-mode.ts `bash`,
//! interactive-mode.ts `handleBashCommand`, AgentSession `executeBash`/`recordBashResult`): the host's blocking
//! bash, each output chunk streamed (`bash_execution_update`). The result is recorded as a `bashExecution` message.
//! (Pi extensions' `user_bash` handlers are not supported by ultron-rs.)

use host::Host;
use kernel::Signal;
use serde_json::{Map, Value};

/// Run `command` as a user's command: the shell runs it after the `shellCommandPrefix` setting (`prefix`; output
/// chunks to `on_chunk`); its `BashResult`.
pub(super) fn run(
    host: &Host,
    command: &str,
    prefix: Option<&str>,
    signal: &Signal,
    on_chunk: &mut dyn FnMut(&str),
) -> Result<Value, String> {
    let result = host.user_bash(&host::shell::prefixed_command(prefix, command), signal, on_chunk)?;
    Ok(pi_result(&result))
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
    use serde_json::json;

    /// recordBashResult over a local result (rpc_pi_events.json `m1`): the cancelled command's absent exit code
    /// stays absent; a given result's `null` exit code and path are kept as given.
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
