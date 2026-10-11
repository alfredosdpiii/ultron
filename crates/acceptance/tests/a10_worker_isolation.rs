//! A10: the worker receives no ambient credentials or unauthorized host capability.
//! Ported from packages/coding-agent/test/ultron-worker-isolation.test.ts. The TypeScript test plants variables in
//! the test process and starts a kernel; here they are planted in the environment of the `ultron` binary itself, so
//! the filter is checked on the real path (process env -> print mode -> kernel), and the model-written cell runs
//! through the scripted provider.

mod common;

use common::{Install, Provider, Reply, stdout_of};

const ENV_CELL: &str =
    "import os\nsorted(k for k, v in os.environ.items() if 'planted' in v), os.environ.get('ULTRON_HARMLESS_SETTING')";
const CAPABILITY_CELL: &str = "await rlm.host_request('secrets.read', {})";

fn process_part() {
    let provider = Provider::start(|request| match request.tool_results().len() {
        0 => Reply::Rlm(ENV_CELL.into()),
        1 => Reply::Rlm(CAPABILITY_CELL.into()),
        _ => Reply::Text("A10_DONE".into()),
    });
    let install = Install::new(&provider);
    let output = install.prompt(
        "inspect the environment",
        &[
            ("ANTHROPIC_API_KEY", "sk-ant-planted"),
            ("GITHUB_TOKEN", "ghp_planted"),
            ("AWS_SECRET_ACCESS_KEY", "aws-planted"),
            ("PI_SESSION_WORKER_CONTROL_TOKEN", "control-planted"),
            // A control variable without a credential-like name is removed by the control-channel rule itself.
            ("PI_SESSION_WORKER_PEER_ID", "peer-planted"),
            ("ULTRON_SESSION_WORKER_PEER", "peer2-planted"),
            ("ULTRON_HARMLESS_SETTING", "kept"),
            // The provider key the binary itself uses must not reach Python either.
            ("GROQ_API_KEY", "groq-planted"),
        ],
    );
    assert_eq!(stdout_of(&output), "A10_DONE\n");
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    let results = requests[2].tool_results();
    assert_eq!(results[0], "([], 'kept')");
    // Host capabilities exist only as host requests, and an ungranted one fails in Python.
    assert!(
        results[1].contains("RuntimeError: Ultron RLM host request is not available in this host yet: secrets.read"),
        "{}",
        results[1]
    );
    for request in &requests {
        for planted in [
            "sk-ant-planted",
            "ghp_planted",
            "aws-planted",
            "control-planted",
            "groq-planted",
        ] {
            assert!(!request.raw.contains(planted), "{planted} reached the model");
        }
    }
}

fn allow_list_part() {
    let environment = kernel::kernel_environment([
        ("GITHUB_TOKEN", "t"),
        ("OPENAI_API_KEY", "k"),
        ("ULTRON_RLM_ENV_ALLOW", "GITHUB_TOKEN"),
        ("PATH", "/bin"),
    ]);
    assert_eq!(
        environment,
        vec![
            ("GITHUB_TOKEN", "t"),
            ("ULTRON_RLM_ENV_ALLOW", "GITHUB_TOKEN"),
            ("PATH", "/bin")
        ]
    );
}

#[test]
fn acceptance_a10_kernel_gets_no_ambient_credentials_or_unauthorized_host_capability() {
    process_part();
    allow_list_part();
}
