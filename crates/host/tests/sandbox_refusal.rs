//! The sandbox refusal note on the model's `bash` results (host::bash_notes over sandbox::refusal), under every
//! sandbox backend that works on this machine (bwrap, rootless unshare; Docker only when opted in), through the real
//! runtime.py with `host::Host` serving its requests: a write to the read-only home, a hidden credential, and a
//! connection with the network off each end with one `[sandbox: ...]` line before `[exit code N]`; a failure the
//! sandbox did not cause (a path read-only on the host too, a missing file), and a command that succeeded, get none.
//! A backend that does not work here is skipped with a `SKIPPED` line (`backends_support`).
//!
//! The scratch tree lives under the cargo target dir (not /tmp, so the read-only home is exercised):
//!   base/home              fake HOME: notes.txt, .ssh/id_test (hidden)
//!   base/home/project      the project

#[path = "../../sandbox/tests/backends_support/mod.rs"]
mod backends_support;

use host::{AllowAll, Host};
use kernel::{Kernel, KernelOptions, Status};
use sandbox::launcher::Mechanism;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

fn ok(kernel: &Kernel, code: &str) -> String {
    let result = kernel
        .execute(code, None, None)
        .unwrap_or_else(|e| panic!("{code}: {e}"));
    assert_eq!(result.status, Status::Ok, "{code}: {result:?}");
    result.result.unwrap_or_default()
}

/// The text of `bash(command)` as the model prints it.
fn bash(kernel: &Kernel, command: &str) -> String {
    let code = format!("print(await bash({command:?}))");
    let result = kernel
        .execute(&code, None, None)
        .unwrap_or_else(|e| panic!("{code}: {e}"));
    assert_eq!(result.status, Status::Ok, "{code}: {result:?}");
    result.stdout
}

#[test]
fn host_sandbox_refusal_note_under_each_backend() {
    for mechanism in backends_support::machine_backends()
        .into_iter()
        .filter_map(Mechanism::parse)
    {
        let name = mechanism.name();
        let dir = tempfile::Builder::new()
            .prefix("host-refusal-")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        let base = dir.path().canonicalize().unwrap();
        let (home, project) = (base.join("home"), base.join("home/project"));
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/id_test"), "SECRET-KEY\n").unwrap();
        std::fs::write(home.join("notes.txt"), "original\n").unwrap();
        let settings = json!({ "sandbox": {
            "mode": "required",
            "network": false,
            "image": backends_support::docker_image().ok(),
        } });
        let host = Host::new(&project, project.join(".jobs"), &project, Box::new(AllowAll)).sandboxed_with_mechanism(
            &settings,
            &home,
            None,
            Some(mechanism),
        );
        assert_eq!(host.launcher().unwrap().mechanism(), Some(mechanism));
        let kernel = Kernel::new(KernelOptions::new(&project, runtime()), Arc::new(host)).unwrap();
        let (home_text, project_text) = (home.display().to_string(), project.display().to_string());
        let read_only = |path: &str| {
            format!(
                "[sandbox: writing {path} was blocked: writes outside the project are blocked by the sandbox. Writable: {project_text}, {project_text}/.jobs, /tmp; the sandbox.writable setting adds paths]\n[exit code 1]\n"
            )
        };

        let redirect = bash(&kernel, &format!("echo x > {home_text}/notes.txt"));
        assert!(redirect.contains("Read-only file system"), "{name}: {redirect}");
        assert!(
            redirect.ends_with(&read_only(&format!("{home_text}/notes.txt"))),
            "{name}: {redirect}"
        );
        let touch = bash(&kernel, &format!("touch {home_text}/new.txt"));
        assert!(
            touch.ends_with(&read_only(&format!("{home_text}/new.txt"))),
            "{name}: {touch}"
        );

        let hidden = bash(&kernel, &format!("cat {home_text}/.ssh/id_test"));
        assert!(!hidden.contains("SECRET"), "{name}: {hidden}");
        assert!(
            hidden.ends_with(&format!(
                "[sandbox: {home_text}/.ssh/id_test is hidden by the sandbox (credentials and sandbox.hidden paths are invisible to commands); it cannot be read or written from here]\n[exit code 1]\n"
            )),
            "{name}: {hidden}"
        );

        let network = bash(&kernel, "exec 3<>/dev/tcp/192.0.2.1/80");
        assert!(
            network.ends_with(
                "[sandbox: commands have no network access (sandbox.network is off); retrying will not help]\n[exit code 1]\n"
            ),
            "{name}: {network}"
        );

        // Not the sandbox's doing: /usr is read-only on the host too; a missing file; a success.
        for command in [
            "echo x > /usr/ultron-refusal-test".to_string(),
            format!("ls {home_text}/missing"),
            format!("echo x > {home_text}/notes.txt; true"),
        ] {
            let text = bash(&kernel, &command);
            assert!(!text.contains("[sandbox:"), "{name}: {command}: {text}");
        }

        // Only a failed command is checked for a refusal.
        let succeeded = ok(
            &kernel,
            &format!("r = await bash('''echo x > {home_text}/notes.txt; true''')\n(r.exit_code, r.sandbox_note)"),
        );
        assert_eq!(succeeded, "(0, None)", "{name}");

        // A shell job's result carries the note too.
        let job = ok(
            &kernel,
            &format!(
                "j = await bash('''echo x > {home_text}/notes.txt''', yield_after=120)\nawait j.result()\n(j.exit_code, str(j).count('[sandbox: writing {home_text}/notes.txt was blocked'), str(j).splitlines()[-1].startswith('[job '))"
            ),
        );
        assert_eq!(job, "(1, 1, True)", "{name}");
        kernel.shutdown();
        assert_eq!(std::fs::read_to_string(home.join("notes.txt")).unwrap(), "original\n");
    }
}
