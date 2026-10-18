//! Which sandbox backends the process tests can run on this machine, shared by the sandbox and host test binaries.
//!
//! A backend that is not installed, not permitted, or (Docker) has no local image is skipped with a `SKIPPED` line
//! naming it and the reason (visible with `--nocapture`). A gate that must exercise a backend names it in
//! `ULTRON_SANDBOX_TEST_REQUIRE` (comma-separated, e.g. `bwrap,unshare,docker`): its absence then fails the test
//! instead of skipping.
//!
//! Docker is opt-in (`ULTRON_SANDBOX_TEST_DOCKER=1`, or `docker` in `ULTRON_SANDBOX_TEST_REQUIRE`): it is the user's
//! own daemon, so no default run touches it (2026-10-18: a stuck daemon hung the gates). It runs on an image that is
//! already on this machine, never pulled: `ULTRON_SANDBOX_TEST_IMAGE`, else the first
//! of [`DOCKER_CANDIDATES`] present locally (each has bash, coreutils and util-linux).

#![allow(dead_code)]

use std::process::{Command, Stdio};
use std::sync::OnceLock;

pub const DOCKER_CANDIDATES: &[&str] = &[
    "debian:bookworm-slim",
    "debian:bookworm",
    "debian:12",
    "debian:13",
    "ubuntu:24.04",
    "ubuntu:22.04",
    "python:3.12-slim",
    "python:3.13-slim",
];

/// Whether the machine itself can run `mechanism` (bwrap, unshare or docker with `image`), probed with the bare tool
/// rather than through the launcher: a launcher bug must fail the tests, not turn into a skip.
pub fn machine_runs(mechanism: &str, image: Option<&str>) -> Result<(), String> {
    let argv: Vec<&str> = match (mechanism, image) {
        ("bwrap", _) => vec![
            "bwrap",
            "--unshare-all",
            "--die-with-parent",
            "--ro-bind",
            "/",
            "/",
            "true",
        ],
        ("unshare", _) => vec![
            "unshare",
            "--user",
            "--map-root-user",
            "--mount",
            "--pid",
            "--fork",
            "true",
        ],
        ("docker", Some(image)) => vec![
            "docker",
            "run",
            "--rm",
            "--pull",
            "never",
            "--network",
            "none",
            image,
            "true",
        ],
        _ => return Err(format!("{mechanism}: nothing to probe")),
    };
    let ran = Command::new(argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if ran {
        Ok(())
    } else {
        Err(format!("`{}` does not run on this machine", argv.join(" ")))
    }
}

/// The mechanisms (by name) this machine runs, strongest first; the others are skipped with their reason. Docker
/// runs on [`docker_image`].
pub fn machine_backends() -> Vec<&'static str> {
    let mut out = Vec::new();
    for name in ["bwrap", "unshare", "docker"] {
        // Docker is the user's own daemon (its containers, its load): tests use it only when asked
        // (`ULTRON_SANDBOX_TEST_DOCKER=1`, or `docker` in `ULTRON_SANDBOX_TEST_REQUIRE`), never by default.
        if name == "docker" && !docker_opted_in() {
            skip_quietly(name, "not opted in (set ULTRON_SANDBOX_TEST_DOCKER=1)");
            continue;
        }
        let image = if name == "docker" {
            match docker_image() {
                Ok(image) => Some(image),
                Err(reason) => {
                    skip(name, &reason);
                    continue;
                }
            }
        } else {
            None
        };
        match machine_runs(name, image.as_deref()) {
            Ok(()) => out.push(name),
            Err(reason) => skip(name, &reason),
        }
    }
    out
}

/// Print that `mechanism` is skipped and why; fail instead when `ULTRON_SANDBOX_TEST_REQUIRE` names it.
pub fn skip(mechanism: &str, reason: &str) {
    let required = std::env::var("ULTRON_SANDBOX_TEST_REQUIRE").unwrap_or_default();
    if required.split(',').any(|name| name.trim() == mechanism) {
        panic!("{mechanism} is required by ULTRON_SANDBOX_TEST_REQUIRE but unavailable: {reason}");
    }
    println!("SKIPPED {mechanism}: {reason}");
}

fn image_present(image: &str) -> bool {
    Command::new("docker")
        .args(["image", "inspect", "--format", "{{.Id}}", image])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The first of `candidates` that is a local image (never pulled).
pub fn local_image(candidates: &[&str]) -> Result<String, String> {
    candidates
        .iter()
        .find(|image| image_present(image))
        .map(|image| image.to_string())
        .ok_or_else(|| {
            format!(
                "no local image among {} (images are never pulled), or no docker daemon",
                candidates.join(", ")
            )
        })
}

/// The local Docker image the tests use, or why there is none.
pub fn docker_image() -> Result<String, String> {
    static IMAGE: OnceLock<Result<String, String>> = OnceLock::new();
    IMAGE
        .get_or_init(|| {
            // Not even `docker images` runs unless the run asked for Docker.
            if !docker_opted_in() {
                return Err("Docker not opted in (set ULTRON_SANDBOX_TEST_DOCKER=1)".into());
            }
            if let Ok(image) = std::env::var("ULTRON_SANDBOX_TEST_IMAGE")
                && !image.is_empty()
            {
                return if image_present(&image) {
                    Ok(image)
                } else {
                    Err(format!(
                        "ULTRON_SANDBOX_TEST_IMAGE={image} is not a local image (images are never pulled)"
                    ))
                };
            }
            local_image(DOCKER_CANDIDATES).map_err(|reason| format!("{reason}; ULTRON_SANDBOX_TEST_IMAGE is not set"))
        })
        .clone()
}

/// Whether the run asked for the Docker backend: `ULTRON_SANDBOX_TEST_DOCKER=1` or `docker` in
/// `ULTRON_SANDBOX_TEST_REQUIRE`.
pub fn docker_opted_in() -> bool {
    std::env::var("ULTRON_SANDBOX_TEST_DOCKER").is_ok_and(|v| v == "1")
        || std::env::var("ULTRON_SANDBOX_TEST_REQUIRE")
            .unwrap_or_default()
            .split(',')
            .any(|m| m.trim() == "docker")
}

/// Print that `mechanism` is skipped (never a failure: it was not asked for).
fn skip_quietly(mechanism: &str, reason: &str) {
    println!("SKIPPED {mechanism}: {reason}");
}
