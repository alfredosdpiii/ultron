//! A worktree subagent's shell (session-worker.ts `hostHandler` `bash`, shell-jobs.ts `laneWorkspace`): its
//! commands and jobs run in its worktree with `ULTRON_WORKTREE`, `ULTRON_PARENT_REPO`, `ULTRON_WORKTREE_BRANCH` and
//! `PWD` set, and its jobs are the session's one table: the root lane lists them, even after the child ended.

mod p_session_support;

use p_session_support::{Setup, actor, text, tool, tool_results, user_texts};
use serde_json::Value;

fn first_user(request: &Value) -> String {
    user_texts(request)
        .iter()
        .filter_map(Value::as_str)
        .find(|text| !text.starts_with("[clock]"))
        .unwrap_or_default()
        .to_string()
}

fn results_text(request: &Value) -> String {
    tool_results(request)
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

const CHILD_CELL: &str = "print(await bash(\"echo W=$ULTRON_WORKTREE B=$ULTRON_WORKTREE_BRANCH R=$ULTRON_PARENT_REPO P=$PWD\", yield_after=None))\nj = await bash(\"echo JOB=$ULTRON_WORKTREE\", yield_after=0)\nprint(\"JOB\", j.id)";

/// Spawn the worktree child, wait in the cell until it ended (bounded: a hang guard of about 120 s), then list the
/// session's jobs from the root lane.
const ROOT_CELL: &str = "import asyncio\nh = await rlm.spawn(\"CHILD: work\", name=\"w\", worktree=True)\nfor _ in range(12000):\n    try:\n        r = (await rlm.collect([h]))[0]\n        break\n    except RuntimeError:\n        await asyncio.sleep(0.01)\nfor j in await rlm.jobs():\n    j = await j.result(wait=60)\n    print(\"ROOTJOB\", j.status, (j.text or \"\").strip())";

fn git(cwd: &std::path::Path, home: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn session_core_worktree_subagent_shell_has_its_worktree_env_and_its_jobs_are_the_sessions() {
    let a = actor(
        Box::new(|request, _| {
            let child = first_user(request).starts_with("CHILD");
            match (child, tool_results(request).is_empty()) {
                (true, true) => tool("toolu_c", CHILD_CELL),
                (true, false) => text("child done"),
                (false, true) => tool("toolu_r", ROOT_CELL),
                (false, false) => text("root done"),
            }
        }),
        Setup::default(),
    );
    git(&a.cwd, &a.root, &["init", "-q"]);
    std::fs::write(a.cwd.join("README.md"), "repo\n").unwrap();
    git(&a.cwd, &a.root, &["add", "README.md"]);
    git(
        &a.cwd,
        &a.root,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    );
    a.prompt("ROOT: delegate");
    let requests = a.requests();
    let child = requests
        .iter()
        .rev()
        .find(|r| first_user(r).starts_with("CHILD"))
        .expect("the child ran");
    let child_results = results_text(child);
    let field = |name: &str| {
        let start = child_results
            .find(&format!("{name}="))
            .unwrap_or_else(|| panic!("{child_results}"))
            + name.len()
            + 1;
        child_results[start..]
            .split([' ', '\\'])
            .next()
            .unwrap_or_default()
            .to_string()
    };
    let (worktree, branch, repo, pwd) = (field("W"), field("B"), field("R"), field("P"));
    assert!(
        !worktree.is_empty() && worktree != a.cwd.display().to_string(),
        "{child_results}"
    );
    assert!(std::path::Path::new(&worktree).is_absolute(), "{worktree}");
    assert!(!branch.is_empty(), "{child_results}");
    assert_eq!(repo, a.cwd.display().to_string(), "{child_results}");
    assert_eq!(pwd, worktree, "the worktree child's PWD is its working directory");
    // The root lane lists the child's job (the child has ended), which ran in the worktree's environment.
    let root = results_text(requests.last().unwrap());
    assert!(root.contains(&format!("ROOTJOB completed JOB={worktree}")), "{root}");
}
