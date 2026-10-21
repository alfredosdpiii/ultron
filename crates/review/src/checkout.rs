//! Source for the verifier (`autoreview/checkout.ts`): a cached blob-less bare clone per repository
//! (`<cache>/<host>/<owner>/<repo>.git`), the pull request's head and base fetched into it, and a detached worktree
//! at the head commit for each review, removed afterwards. Private repositories authenticate through the child's
//! environment (the account's token) and `gh auth git-credential` as a per-command credential helper: nothing is
//! written to any git config. Git operations on one clone run one at a time (a per-clone lock, where the
//! TypeScript chained promises).

use std::collections::HashMap;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use regex::Regex;

use crate::github::{Error, PullRef};
use crate::js::{head16, path_join, trim};
use crate::promotion::{CommitMessage, parse_commits};
use crate::runner::{Env, RunOptions, RunResult, Runner};

pub const CLONE_TIMEOUT_MS: u64 = 5 * 60_000;
pub const FETCH_TIMEOUT_MS: u64 = 3 * 60_000;
pub const GIT_TIMEOUT_MS: u64 = 2 * 60_000;

/// Per-command git options: credentials from `gh`, no stored helper, no prompts.
pub const CREDENTIALS: [&str; 4] = [
    "-c",
    "credential.helper=",
    "-c",
    "credential.helper=!gh auth git-credential",
];

/// What the checkout needs of the pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullHeads {
    pub head_sha: String,
    pub base_ref: String,
    pub base_sha: String,
}

/// `n` random bytes as hex.
pub fn random_hex(n: usize) -> String {
    use ring::rand::SecureRandom;
    let mut bytes = vec![0u8; n];
    let _ = ring::rand::SystemRandom::new().fill(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `mkdirSync(dir, { recursive: true, mode: 0o700 })`.
pub fn make_private_dir(dir: &str) -> std::io::Result<()> {
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

fn parent(path: &str) -> String {
    Path::new(path)
        .parent()
        .map_or_else(|| ".".into(), |p| p.to_string_lossy().into_owned())
}

struct Git {
    runner: Arc<dyn Runner>,
    cache: String,
}

impl Git {
    fn run(&self, args: &[&str], env: &Env, timeout_ms: u64) -> RunResult {
        let argv: Vec<String> = std::iter::once("git")
            .chain(CREDENTIALS)
            .chain(args.iter().copied())
            .map(String::from)
            .collect();
        let mut env = env.clone();
        env.insert("GIT_LFS_SKIP_SMUDGE".into(), "1".into());
        env.insert("GIT_OPTIONAL_LOCKS".into(), "0".into());
        self.runner.run(
            &argv,
            &RunOptions {
                env,
                timeout_ms: Some(timeout_ms),
                ..RunOptions::default()
            },
        )
    }

    fn must(&self, args: &[&str], env: &Env, timeout_ms: u64) -> Result<String, Error> {
        let result = self.run(args, env, timeout_ms);
        if result.code != 0 {
            let name = args
                .iter()
                .find(|arg| !arg.starts_with('-') && **arg != self.cache)
                .copied()
                .unwrap_or("");
            let detail = if result.stderr.is_empty() {
                &result.stdout
            } else {
                &result.stderr
            };
            return Err(Error::Other(format!(
                "git {name} failed: {}",
                head16(trim(detail), 300)
            )));
        }
        Ok(result.stdout)
    }
}

/// A worktree at the pull request's head.
pub struct Checkout {
    /// A worktree at the head commit.
    pub work_dir: String,
    /// The merge base of the base branch and the head: the pull request diff starts here.
    pub merge_base: String,
    git: Arc<Git>,
    dir: String,
    env: Env,
    lock: Arc<Mutex<()>>,
}

impl Checkout {
    /// `git diff from to`, with the prefixes the diff parser expects.
    pub fn diff(&self, from: &str, to: &str) -> Result<String, Error> {
        self.git.must(
            &[
                "-C",
                &self.dir,
                "-c",
                "diff.noprefix=false",
                "diff",
                "--no-color",
                "--no-ext-diff",
                "-M",
                "-U3",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                from,
                to,
                "--",
            ],
            &self.env,
            GIT_TIMEOUT_MS,
        )
    }

    /// Whether `ancestor` is in the clone and an ancestor of `descendant`.
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        let sha = Regex::new(r"^[0-9a-f]{7,64}$").expect("static pattern");
        sha.is_match(ancestor)
            && self
                .git
                .run(
                    &["-C", &self.dir, "merge-base", "--is-ancestor", ancestor, descendant],
                    &self.env,
                    GIT_TIMEOUT_MS,
                )
                .code
                == 0
    }

    /// The commits after `from` up to `to`, oldest first: sha, subject and message.
    pub fn commits(&self, from: &str, to: &str) -> Result<Vec<CommitMessage>, Error> {
        let range = format!("{from}..{to}");
        let out = self.git.must(
            &[
                "-C",
                &self.dir,
                "log",
                "--reverse",
                "--format=%H%x1f%s%x1f%B%x1e",
                &range,
                "--",
            ],
            &self.env,
            GIT_TIMEOUT_MS,
        )?;
        Ok(parse_commits(&out))
    }

    /// Fetch the commits the clone does not have, by sha (GitHub serves any commit of the repository). A commit that
    /// cannot be fetched is left out by the caller's diffs.
    pub fn fetch_commits(&self, shas: &[String]) {
        let sha = Regex::new(r"^[0-9a-f]{7,64}$").expect("static pattern");
        let missing: Vec<&str> = shas
            .iter()
            .filter(|candidate| sha.is_match(candidate))
            .filter(|candidate| {
                self.git
                    .run(
                        &["-C", &self.dir, "cat-file", "-e", &format!("{candidate}^{{commit}}")],
                        &self.env,
                        GIT_TIMEOUT_MS,
                    )
                    .code
                    != 0
            })
            .map(String::as_str)
            .collect();
        if missing.is_empty() {
            return;
        }
        let _serial = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut args = vec!["-C", &self.dir, "fetch", "--quiet", "--no-tags", "origin"];
        args.extend(missing);
        self.git.run(&args, &self.env, FETCH_TIMEOUT_MS);
    }

    /// Remove the worktree (never fails).
    pub fn cleanup(&self) {
        let _serial = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.git.run(
            &["-C", &self.dir, "worktree", "remove", "--force", &self.work_dir],
            &self.env,
            GIT_TIMEOUT_MS,
        );
        let _ = std::fs::remove_dir_all(&self.work_dir);
        self.git
            .run(&["-C", &self.dir, "worktree", "prune"], &self.env, GIT_TIMEOUT_MS);
    }
}

pub struct CheckoutManager {
    git: Arc<Git>,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl CheckoutManager {
    pub fn new(runner: Arc<dyn Runner>, cache_dir: &str) -> Self {
        Self {
            git: Arc::new(Git {
                runner,
                cache: cache_dir.into(),
            }),
            locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn repo_dir(&self, r: &PullRef) -> String {
        path_join(&[&self.git.cache, &r.host, &r.owner, &format!("{}.git", r.repo)])
    }

    fn lock(&self, dir: &str) -> Arc<Mutex<()>> {
        self.locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(dir.into())
            .or_default()
            .clone()
    }

    /// Clone or update the cached repository, fetch the pull request, and check out its head.
    pub fn prepare(&self, r: &PullRef, pull: &PullHeads, env: &Env) -> Result<Checkout, Error> {
        let safe = Regex::new(r"^[A-Za-z0-9._-]+$").expect("static pattern");
        for part in [&r.host, &r.owner, &r.repo] {
            if !safe.is_match(part) || part == "." || part == ".." {
                return Err(Error::Other(format!("unsafe repository name: {part}")));
            }
        }
        let dir = self.repo_dir(r);
        let pull_ref = format!("refs/autoreview/pull/{}", r.number);
        let base_ref = format!("refs/autoreview/base/{}", r.number);
        let short: String = pull.head_sha.chars().take(7).collect();
        let work_dir = path_join(&[
            &self.git.cache,
            "worktrees",
            &format!("{}-{}-{}-{short}-{}", r.owner, r.repo, r.number, random_hex(3)),
        ]);
        let lock = self.lock(&dir);
        let merge_base = {
            let _serial = lock.lock().unwrap_or_else(|e| e.into_inner());
            if !Path::new(&dir).join("HEAD").exists() {
                make_private_dir(&parent(&dir)).map_err(|e| Error::Other(e.to_string()))?;
                let temporary = format!("{dir}.{}.{}.tmp", std::process::id(), random_hex(3));
                let url = format!("https://{}/{}/{}.git", r.host, r.owner, r.repo);
                let cloned = self
                    .git
                    .must(
                        &["clone", "--quiet", "--filter=blob:none", "--bare", &url, &temporary],
                        env,
                        CLONE_TIMEOUT_MS,
                    )
                    .and_then(|_| std::fs::rename(&temporary, &dir).map_err(|e| Error::Other(e.to_string())));
                let _ = std::fs::remove_dir_all(&temporary);
                cloned?;
            }
            self.git.must(
                &[
                    "-C",
                    &dir,
                    "fetch",
                    "--quiet",
                    "--no-tags",
                    "--force",
                    "origin",
                    &format!("+refs/pull/{}/head:{pull_ref}", r.number),
                    &format!("+refs/heads/{}:{base_ref}", pull.base_ref),
                ],
                env,
                FETCH_TIMEOUT_MS,
            )?;
            let fetched = trim(
                &self
                    .git
                    .must(&["-C", &dir, "rev-parse", &pull_ref], env, GIT_TIMEOUT_MS)?,
            )
            .to_string();
            if fetched != pull.head_sha {
                let moved: String = fetched.chars().take(7).collect();
                return Err(Error::StaleHead(format!(
                    "the pull request head moved to {moved} while it was being fetched"
                )));
            }
            let base = self.git.run(
                &["-C", &dir, "merge-base", &base_ref, &pull.head_sha],
                env,
                GIT_TIMEOUT_MS,
            );
            make_private_dir(&parent(&work_dir)).map_err(|e| Error::Other(e.to_string()))?;
            self.git.must(
                &[
                    "-C",
                    &dir,
                    "worktree",
                    "add",
                    "--quiet",
                    "--detach",
                    &work_dir,
                    &pull.head_sha,
                ],
                env,
                FETCH_TIMEOUT_MS,
            )?;
            let found = trim(&base.stdout);
            if base.code == 0 && !found.is_empty() {
                found.to_string()
            } else {
                pull.base_sha.clone()
            }
        };
        Ok(Checkout {
            work_dir,
            merge_base,
            git: self.git.clone(),
            dir,
            env: env.clone(),
            lock,
        })
    }
}
