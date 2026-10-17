//! The project's interpreter reaches the session's Loki post-write checks (compose.rs `file_hooks_in`:
//! `LokiGuard::with_project_python(project_python(...))`; loki.ts passes `projectPython` to the engine): in a
//! repository with a committed `.loki/` and ULTRON_LOKI_PROJECT_PYTHON set, a cell that adds an import the project's interpreter cannot
//! resolve gets the engine's `unresolved import` finding (with a later cell's result), and the interpreter was the
//! one asked (without it the engine would resolve imports with its own interpreter and leave no mark).
//! The interpreter is a fake script that answers the engine's import probe and records that it ran.

#[path = "../../cli/tests/executable_support/mod.rs"]
mod executable_support;

mod p_session_support;
mod reopen_support;

use p_session_support::{text, tool, tool_results};
use reopen_support::Home;
use session_core::core::Op;

fn git(home: &Home, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(&home.cwd)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", &home.root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

const CELL: &str = "import os\nos.makedirs('app', exist_ok=True)\nopen('app/a.py', 'w').write('import os\\nimport nonexistent_pkg_q\\n')\nprint('WROTE')";

fn finding(missing: &str) -> (String, bool) {
    let home = Home::new(None);
    git(&home, &["init", "-q"]);
    std::fs::create_dir_all(home.cwd.join(".loki")).unwrap();
    std::fs::write(home.cwd.join(".loki/loki.json"), "{}\n").unwrap();
    std::fs::write(home.cwd.join(".loki/loki.py"), "# engine\n").unwrap();
    std::fs::create_dir_all(home.cwd.join("app")).unwrap();
    std::fs::write(home.cwd.join("app/a.py"), "").unwrap();
    git(&home, &["add", "."]);
    git(&home, &["commit", "-q", "-m", "base"]);
    // The project's interpreter, named by ULTRON_LOKI_PROJECT_PYTHON (outside the checkout, so only the session's
    // detection finds it), answering the engine's import probe.
    let python = home.root.join("project-python");
    let ran = home.root.join("python.ran");
    // Written without a write descriptor in this process (ETXTBSY under parallel tests).
    executable_support::install(
        &python,
        &format!("#!/bin/sh\n: > '{}'\necho '{missing}'\n", ran.display()),
    );
    let w = home.open(
        // The check runs in the background after the writing cell; later cells run until one carries its finding
        // (a hang guard of 500 cells, never a timing expectation).
        Box::new(|request, _| {
            let results = tool_results(request);
            if results.is_empty() {
                return tool("toolu_0", CELL);
            }
            if results.iter().any(|r| r.to_string().contains("[Loki]")) || results.len() > 500 {
                return text("done");
            }
            tool(&format!("toolu_{}", results.len()), "print('NEXT')")
        }),
        &[("ULTRON_LOKI_PROJECT_PYTHON", &python.display().to_string())],
        None,
    );
    w.call(Op::PromptAndWait {
        message: "write it".into(),
        images: Vec::new(),
    })
    .unwrap();
    let requests = w.requests();
    w.close();
    let result = tool_results(requests.last().unwrap())
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(result.contains("WROTE"), "{result}");
    (result, ran.exists())
}

#[test]
fn session_core_loki_post_write_checks_ask_the_projects_interpreter() {
    let (result, asked) = finding(r#"["nonexistent_pkg_q"]"#);
    assert!(asked, "the project's interpreter was asked: {result}");
    assert!(
        result.contains("app/a.py:2: python-ast: unresolved import nonexistent_pkg_q"),
        "{result}"
    );
}
