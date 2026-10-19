//! The model's `bash` result conventions (host::bash_notes), through the real runtime.py under the Rust kernel with
//! `host::Host` serving its requests, as the session worker routes them: exit code 1 of a simple pipeline ending in
//! a read-only comparison command reads as a result (`[No matches found (exit code 1)]`, not the failure line), on
//! the blocking bash, the yielding bash (the default) and shell jobs; anything else keeps `[exit code N]`.

use host::bash_notes::{annotate, exit_note};
use host::{AllowAll, Host};
use kernel::{Kernel, KernelOptions, Status};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

#[test]
fn host_bash_notes_exit_note_only_for_simple_pipelines_ending_in_a_comparison() {
    let noted = [
        ("grep nomatch f.txt", "No matches found"),
        ("egrep 'a|b' f.txt", "No matches found"),
        ("fgrep x f.txt", "No matches found"),
        ("rg -n pattern src", "No matches found"),
        ("cat f.txt | sort | grep -c x", "No matches found"),
        ("cat f.txt |& grep x", "No matches found"),
        ("LC_ALL=C grep x f.txt", "No matches found"),
        ("/usr/bin/grep x f.txt", "No matches found"),
        ("grep x f.txt 2>&1", "No matches found"),
        ("grep x f.txt &>/dev/null", "No matches found"),
        ("grep 'a;b && c' f.txt", "No matches found"),
        ("grep \"x|y\" f.txt", "No matches found"),
        ("grep a#b f.txt", "No matches found"),
        ("diff -u a b", "Files differ"),
        ("cmp a b", "Files differ"),
        ("test -f missing", "Condition is false"),
        ("[ -e missing ]", "Condition is false"),
    ];
    for (command, note) in noted {
        assert_eq!(exit_note(command, Some(1)), Some(note), "{command}");
        assert_eq!(exit_note(command, Some(2)), None, "{command}: only exit code 1");
        assert_eq!(exit_note(command, Some(0)), None, "{command}");
        assert_eq!(exit_note(command, None), None, "{command}");
    }
    let plain = [
        // The status may come from another command.
        "grep x f.txt; echo done",
        "cd sub && grep x f.txt",
        "false || grep x f.txt",
        "grep x f.txt &",
        "echo a\ngrep x f.txt",
        "grep x f.txt\nfalse",
        "(grep x f.txt)",
        "{ grep x f.txt; }",
        "grep x $(ls)",
        "grep x `ls`",
        "grep x \"$(ls)\"",
        "echo x # | grep y",
        // A negation: exit code 1 means grep matched.
        "! grep x f.txt",
        // The last command is not a comparison.
        "grep x f.txt | head -1",
        "find . -name missing",
        "ls missing",
        "xargs grep x",
        // Incomplete or empty.
        "grep 'x f.txt",
        "grep x |",
        "",
    ];
    for command in plain {
        assert_eq!(exit_note(command, Some(1)), None, "{command:?}");
    }
}

#[test]
fn host_bash_notes_annotate_skips_unfinished_results_and_needs_a_sandbox_for_refusals() {
    let mut finished = json!({"output": "", "exit_code": 1, "cancelled": false, "timed_out": false});
    annotate(&mut finished, "grep x f", None);
    assert_eq!(finished["exit_note"], "No matches found");
    for unfinished in [
        json!({"output": "", "exit_code": 1, "running": true}),
        json!({"output": "", "exit_code": 1, "cancelled": true}),
        json!({"output": "", "exit_code": 1, "timed_out": true}),
        json!({"text": "", "exit_code": 1, "status": "failed"}),
    ] {
        let mut result = unfinished.clone();
        annotate(&mut result, "grep x f", None);
        assert_eq!(result, unfinished);
    }
    // No sandbox in use: an output that looks like a refusal is the command's own business.
    let mut unsandboxed = json!({"output": "bash: connect: Network is unreachable", "exit_code": 1});
    annotate(&mut unsandboxed, "exec 3<>/dev/tcp/192.0.2.1/80", None);
    assert!(unsandboxed.get("sandbox_note").is_none(), "{unsandboxed}");
}

fn ok(kernel: &Kernel, code: &str) -> String {
    let result = kernel
        .execute(code, None, None)
        .unwrap_or_else(|e| panic!("{code}: {e}"));
    assert_eq!(result.status, Status::Ok, "{code}: {result:?}");
    result.result.unwrap_or_default()
}

#[test]
fn host_bash_notes_no_match_reads_as_a_result_through_runtime_py() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    std::fs::write(cwd.join("f.txt"), "alpha\nbeta\n").unwrap();
    std::fs::write(cwd.join("a"), "one\n").unwrap();
    std::fs::write(cwd.join("b"), "two\n").unwrap();
    // The default bash (yields after 30 s: the host's job-backed `bash`) and the blocking one (yield off).
    for yield_after in ["", "off"] {
        let host = Arc::new(Host::new(&cwd, cwd.join(".jobs"), &cwd, Box::new(AllowAll)));
        let mut options = KernelOptions::new(&cwd, runtime());
        options.env.push(("ULTRON_BASH_YIELD_AFTER".into(), yield_after.into()));
        let kernel = Kernel::new(options, host).unwrap();
        let cases = [
            (
                "r = await bash('''grep nomatch f.txt''')\n(str(r), r.exit_code, r.ok, r.exit_note)",
                "('[No matches found (exit code 1)]', 1, False, 'No matches found')",
            ),
            (
                "str(await bash('''cat f.txt | grep -c nomatch'''))",
                "'0\\n[No matches found (exit code 1)]'",
            ),
            (
                "str(await bash('''diff a b'''))",
                "'1c1\\n< one\\n---\\n> two\\n[Files differ (exit code 1)]'",
            ),
            (
                "str(await bash('''[ -e missing ]'''))",
                "'[Condition is false (exit code 1)]'",
            ),
            // A match is an ordinary success.
            (
                "r = await bash('''grep alpha f.txt''')\n(str(r), r.ok, r.exit_note)",
                "('alpha', True, None)",
            ),
            // Not a simple pipeline, or another exit code: the failure line stays.
            (
                "r = await bash('''grep nomatch f.txt && echo found''')\n(str(r), r.exit_note)",
                "('[exit code 1]', None)",
            ),
            (
                "str(await bash('''grep x missing.txt'''))",
                "'grep: missing.txt: No such file or directory\\n[exit code 2]'",
            ),
            ("str(await bash('''ls missing.txt 2>/dev/null'''))", "'[exit code 2]'"),
        ];
        for (code, want) in cases {
            assert_eq!(ok(&kernel, code), want, "yield_after={yield_after:?} {code}");
        }
        // A shell job (`yield_after` number): its status line carries the note too.
        let job = ok(
            &kernel,
            "j = await bash('''grep nomatch f.txt''', yield_after=120)\nawait j.result()\n(j.exit_code, j.ok, j.exit_note, str(j).startswith('[job '), ' No matches found (exit code 1) after ' in str(j))",
        );
        assert_eq!(
            job, "(1, False, 'No matches found', True, True)",
            "yield_after={yield_after:?}"
        );
        let failed_job = ok(
            &kernel,
            "j = await bash('''grep x missing.txt''', yield_after=120)\nawait j.result()\n(j.exit_note, ' exit code 2 after ' in str(j))",
        );
        assert_eq!(failed_job, "(None, True)");
        kernel.shutdown();
    }
}
