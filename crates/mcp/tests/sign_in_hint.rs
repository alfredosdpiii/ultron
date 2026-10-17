//! The `/mcp login` prompt's hint comes from the session's injected environment (`browserReach` of
//! utils/remote-browser.ts over the worker's env), never from this test process's own environment. Expected texts
//! are `unreachableCallbackHint`'s, literally.

use mcp::connection::ConnectionEnv;
use mcp::gateway::sign_in_hint;
use std::collections::HashMap;
use std::sync::Arc;

const LOCAL: &str = "If the browser ends on a page that can't be reached (localhost refused to connect), copy that page's full address from the address bar and paste it here.";
const SSH: &str = "Ultron is running over SSH, so the browser on your computer may not reach this login's callback. If it ends on a page that can't be reached (localhost refused to connect) after you sign in, that is expected: copy that page's full address from the address bar and paste it here.";
const CONTAINER: &str = "Ultron is running in a container, so the browser on your computer may not reach this login's callback. If it ends on a page that can't be reached (localhost refused to connect) after you sign in, that is expected: copy that page's full address from the address bar and paste it here.";

fn env_of(vars: &[(&str, &str)]) -> ConnectionEnv {
    let vars: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    ConnectionEnv {
        var: Some(Arc::new(move |key: &str| vars.get(key).cloned())),
        ..ConnectionEnv::default()
    }
}

/// The host's own container markers (files, not env): with one, an SSH session is still a container.
fn host_in_container() -> bool {
    std::path::Path::new("/.dockerenv").exists()
        || std::path::Path::new("/run/.containerenv").exists()
        || std::fs::read_to_string("/proc/1/cgroup").is_ok_and(|cgroup| {
            ["docker", "containerd", "kubepods", "libpod", "lxc"]
                .iter()
                .any(|marker| cgroup.contains(marker))
        })
}

#[test]
fn mcp_sign_in_hint_reads_the_injected_environment() {
    let base = if host_in_container() { CONTAINER } else { LOCAL };
    let ssh = if host_in_container() { CONTAINER } else { SSH };
    // No injected environment is an empty one.
    assert_eq!(sign_in_hint(&ConnectionEnv::default()), base);
    assert_eq!(sign_in_hint(&env_of(&[])), base);
    assert_eq!(
        sign_in_hint(&env_of(&[("SSH_CONNECTION", "10.0.0.1 5 10.0.0.2 22")])),
        ssh
    );
    assert_eq!(sign_in_hint(&env_of(&[("SSH_TTY", "/dev/pts/3")])), ssh);
    // Blank values are unset (`?.trim()`).
    assert_eq!(sign_in_hint(&env_of(&[("SSH_TTY", "  ")])), base);
    // The env's container markers.
    assert_eq!(sign_in_hint(&env_of(&[("container", "podman")])), CONTAINER);
    assert_eq!(
        sign_in_hint(&env_of(&[
            ("KUBERNETES_SERVICE_HOST", "10.96.0.1"),
            ("SSH_TTY", "/dev/pts/3")
        ])),
        CONTAINER
    );
    // A callback host put elsewhere on purpose: the redirect is expected to arrive.
    for key in ["ULTRON_OAUTH_CALLBACK_HOST", "PI_OAUTH_CALLBACK_HOST"] {
        assert_eq!(
            sign_in_hint(&env_of(&[
                (key, "0.0.0.0"),
                ("SSH_TTY", "/dev/pts/3"),
                ("container", "x")
            ])),
            LOCAL
        );
    }
}
