//! A code skill's test kernel starts from the session's environment, as the lanes' kernels do: a session whose PATH
//! holds a fake tool the server process's PATH lacks proposes a skill from its root REPL (`skills.propose_code`), and
//! the skill's test, run by the host in a fresh kernel, finds the fake tool and not a tool only the process PATH has.
//! The session actor against a scripted provider on loopback. New in Rust: literal expectations.

mod p_session_support;

use p_session_support::{Setup, actor, text, tool, tool_results};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;

const RESULT: &str = "skill-result.json";
const TOOL: &str = "w17e-fake-tool";

/// The test: the session's tool is found, `sh` (on this process's PATH, not the session's) is not.
const TEST_SOURCE: &str = "import shutil\ndef test_env():\n    assert shutil.which('w17e-fake-tool'), 'session tool missing'\n    assert shutil.which('sh') is None, shutil.which('sh')\n";

#[test]
fn session_core_w17e_a_code_skill_test_kernel_sees_the_session_path_not_the_process_path() {
    // The precondition that makes the second assertion of the test meaningful.
    let process_path = std::env::var("PATH").unwrap_or_default();
    assert!(
        std::env::split_paths(&process_path).any(|dir| dir.join("sh").is_file()),
        "this process's PATH has no sh: {process_path}"
    );
    let bin = tempfile::tempdir().unwrap();
    let bin_dir = bin.path().canonicalize().unwrap();
    let fake = bin_dir.join(TOOL);
    std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let cell = format!(
        "import json\nr = await skills.propose_code('w17e_env', 'def f():\\n    return 1\\n', {}, 'session env')\nopen({}, 'w').write(json.dumps(r, default=str))\nprint(r['status'])",
        json!(TEST_SOURCE),
        json!(RESULT)
    );
    let script = Box::new(move |request: &Value, _| match tool_results(request).is_empty() {
        true => tool("toolu_skill", &cell),
        false => text("done"),
    });
    let a = actor(
        script,
        Setup {
            vars: vec![("PATH", bin_dir.display().to_string())],
            ..Setup::default()
        },
    );
    let ended = a.prompt("propose a skill");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    let written = std::fs::read_to_string(a.cwd.join(RESULT)).unwrap_or_else(|_| {
        let results: Vec<Value> = a.requests().iter().flat_map(tool_results).collect();
        panic!("the cell wrote no result: {results:?}")
    });
    let result: Value = serde_json::from_str(&written).unwrap();
    assert_eq!(result["status"], "active", "{result}");
    assert_eq!(result["test"]["passed"], true, "{result}");
}
