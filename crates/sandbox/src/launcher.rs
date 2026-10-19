//! Running a command under the strongest isolation available: bubblewrap, then rootless `unshare` namespaces, then
//! Docker (only with a configured image), else none.
//!
//! [`Policy`] is the interactive profile (autoreview's test launcher, `autoreview_tests.py` `Sandbox.wrap`, stays the
//! Python reviews run): the project and session dirs writable, the home read-only with credentials hidden, network on unless
//! configured off, the environment cleared to an allowlist, the sandbox dying with its parent.

use crate::settings::{DEFAULT_ENV, Mode, Settings, expand};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mechanism {
    Bwrap,
    Unshare,
    Docker,
}

impl Mechanism {
    pub const STRONGEST_FIRST: [Mechanism; 3] = [Mechanism::Bwrap, Mechanism::Unshare, Mechanism::Docker];
    pub fn name(self) -> &'static str {
        match self {
            Mechanism::Bwrap => "bwrap",
            Mechanism::Unshare => "unshare",
            Mechanism::Docker => "docker",
        }
    }
    pub fn parse(name: &str) -> Option<Mechanism> {
        Mechanism::STRONGEST_FIRST.into_iter().find(|m| m.name() == name)
    }
}

/// What a path is on the host (not following a final symlink).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Symlink(String),
    Dir,
    File,
    Missing,
}

/// The host filesystem as the launcher sees it; tests substitute a fixed layout.
pub trait Layout {
    fn entry(&self, path: &Path) -> Entry;
    /// Whether `name` is an executable on PATH.
    fn which(&self, name: &str) -> bool;
}

/// The host filesystem, with executables looked up on [`SANDBOX_PATH`] (never this process's PATH); [`PathLayout`]
/// looks them up on a session's PATH.
pub struct RealLayout;

/// Whether `name` is an executable file in one of the `path` directories.
fn on_path(path: &str, name: &str) -> bool {
    std::env::split_paths(path).any(|dir| {
        let candidate = dir.join(name);
        std::fs::metadata(&candidate).is_ok_and(|meta| {
            use std::os::unix::fs::PermissionsExt;
            meta.is_file() && meta.permissions().mode() & 0o111 != 0
        })
    })
}

impl Layout for RealLayout {
    fn entry(&self, path: &Path) -> Entry {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => std::fs::read_link(path)
                .map(|target| Entry::Symlink(target.to_string_lossy().into_owned()))
                .unwrap_or(Entry::Missing),
            Ok(meta) if meta.is_dir() => Entry::Dir,
            Ok(_) => Entry::File,
            Err(_) => Entry::Missing,
        }
    }
    fn which(&self, name: &str) -> bool {
        on_path(SANDBOX_PATH, name)
    }
}

/// The host filesystem, with executables looked up on `path` (a session's PATH: [`Policy::layout`]).
pub struct PathLayout {
    pub path: String,
}

impl Layout for PathLayout {
    fn entry(&self, path: &Path) -> Entry {
        RealLayout.entry(path)
    }
    fn which(&self, name: &str) -> bool {
        on_path(&self.path, name)
    }
}

/// `os.path.isdir` (follows symlinks) over a [`Layout`].
fn is_dir(layout: &dyn Layout, path: &Path) -> bool {
    match layout.entry(path) {
        Entry::Dir => true,
        Entry::Symlink(target) => {
            let base = path.parent().unwrap_or(Path::new("/"));
            let resolved = base.join(target);
            resolved != path && is_dir(layout, &resolved)
        }
        _ => false,
    }
}

fn s(text: &str) -> String {
    text.to_owned()
}

fn p(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub const SANDBOX_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
const SYSTEM_DIRS: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib64", "/lib32"];

/// `detect_sandbox`: the first mechanism, strongest first, that is installed (Docker also needs an image) and whose
/// probe works; `only` restricts the search to one.
pub fn detect(
    image: Option<&str>,
    only: Option<Mechanism>,
    which: impl Fn(&str) -> bool,
    mut works: impl FnMut(Mechanism) -> bool,
) -> Option<Mechanism> {
    Mechanism::STRONGEST_FIRST.into_iter().find(|&mechanism| {
        if only.is_some_and(|only| only != mechanism)
            || !which(mechanism.name())
            || (mechanism == Mechanism::Docker && image.is_none_or(str::is_empty))
        {
            return false;
        }
        works(mechanism)
    })
}

// --- Interactive profile ---------------------------------------------------------------------------------------

/// Extra directories bubblewrap shows read-only besides the system ones (when present).
const INTERACTIVE_SYSTEM_EXTRA: &[&str] = &["/opt", "/nix"];
// Rootless `unshare` replaces /tmp, /var/tmp, /dev/shm and /run/user by private empty tmpfs mounts: shared sockets
// (tmux, ssh-agent, D-Bus) live there.
const INTERACTIVE_UNSHARE_SCRIPT: &str = r#"set -e
PATH=/usr/bin:/bin:/usr/sbin:/sbin:$PATH
uid=$1; gid=$2; home=$3; cwd=$4; tmp=$5; shift 5
stage=/mnt
mount -t tmpfs -o mode=700 tmpfs "$stage"
: > "$stage/w"; : > "$stage/r"
n=$1; shift; i=0
while [ "$i" -lt "$n" ]; do mkdir "$stage/w$i"; mount --rbind "$1" "$stage/w$i"; printf '%s\n' "$1" >> "$stage/w"; shift; i=$((i+1)); done
n=$1; shift; i=0
while [ "$i" -lt "$n" ]; do mkdir "$stage/r$i"; mount --rbind "$1" "$stage/r$i"; printf '%s\n' "$1" >> "$stage/r"; shift; i=$((i+1)); done
if [ -n "$tmp" ]; then mkdir "$stage/t"; mount --rbind "$tmp" "$stage/t"; fi
for root in /home "$home"; do
  if [ -d "$root" ]; then
    mount --rbind "$root" "$root"
    while read -r _ _ _ _ point _; do
      case "$point" in "$root"|"$root"/*) mount -o remount,bind,ro "$point";; esac
    done < /proc/self/mountinfo
  fi
done
for d in /tmp /var/tmp /dev/shm /run/user; do
  if [ -d "$d" ]; then mount -t tmpfs -o mode=1777 tmpfs "$d"; fi
done
if [ -n "$tmp" ]; then mount --rbind "$stage/t" /tmp; fi
i=0
while IFS= read -r p; do mkdir -p "$p" 2>/dev/null || true; mount --rbind "$stage/w$i" "$p"; i=$((i+1)); done < "$stage/w"
i=0
while IFS= read -r p; do mkdir -p "$p" 2>/dev/null || true; mount --rbind "$stage/r$i" "$p"; mount -o remount,bind,ro "$p"; i=$((i+1)); done < "$stage/r"
n=$1; shift; i=0
while [ "$i" -lt "$n" ]; do
  if [ -d "$1" ]; then mount -t tmpfs -o mode=700 tmpfs "$1"; elif [ -e "$1" ]; then mount --bind /dev/null "$1"; fi
  shift; i=$((i+1))
done
n=$1; shift; i=0
while [ "$i" -lt "$n" ]; do
  mkdir -p "$2" 2>/dev/null || true; mount --rbind "$stage/$1" "$2"
  case "$1" in r*) mount -o remount,bind,ro "$2";; esac
  shift 2; i=$((i+1))
done
shift
umount -l "$stage"
cd "$cwd"
exec unshare --user --map-user="$uid" --map-group="$gid" -- "$@"
"#;

/// The Docker client's stdin: a pipe nobody writes, open exactly as long as the client runs (the reader of
/// [`DOCKER_TIED`] inside sees end-of-input when the client is gone). `$$` is the shell that became the client.
const DOCKER_CLIENT: &str = r#"exec "$@" < <(exec setpriv --pdeathsig KILL -- sleep infinity)"#;
const DOCKER_CLIENT_POLLING: &str = r#"exec "$@" < <(while kill -0 $$ 2>/dev/null; do sleep 1; done)"#;
/// Inside the container: the command runs with no stdin while a watcher reads the container's stdin (fd 3), which
/// `docker run -i` closes when its client goes away; the watcher then kills the command, the shell exits, and with
/// it `--init` and the whole container. A command that ends on its own ends the watcher.
const DOCKER_TIED: &str = r#"exec 3<&0; "$@" </dev/null 3<&- & c=$!; { cat <&3 >/dev/null; kill -KILL $c 2>/dev/null; } & w=$!; exec 3<&-; wait $c; s=$?; kill $w 2>/dev/null; exit $s"#;

/// Writable (true) and readable paths strictly inside a hidden one, with their index in their list: mounted again
/// after the hidden ones, so the most specific rule wins (a session dir under a hidden `~/.ultron` stays usable).
fn nested<'a>(
    writable: &[&'a PathBuf],
    readable: &[&'a PathBuf],
    hidden: &[&PathBuf],
) -> Vec<(bool, usize, &'a PathBuf)> {
    let inside = |path: &PathBuf| hidden.iter().any(|h| path != *h && path.starts_with(h));
    let tagged = |list: &[&'a PathBuf], kind: bool| -> Vec<(bool, usize, &'a PathBuf)> {
        list.iter()
            .enumerate()
            .filter(|(_, path)| inside(path))
            .map(|(index, path)| (kind, index, *path))
            .collect()
    };
    let mut out = tagged(readable, false);
    out.extend(tagged(writable, true));
    // Parents before children, so a nested path inside another nested one lands on top of it.
    out.sort_by_key(|(_, _, path)| path.components().count());
    out
}

/// The interactive isolation of one session: what is writable, readable, hidden, and what the command sees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    pub cwd: PathBuf,
    pub home: PathBuf,
    /// Writable inside (the project first); everything else is read-only or invisible.
    pub writable: Vec<PathBuf>,
    /// Extra read-only paths.
    pub readable: Vec<PathBuf>,
    /// Replaced by an empty directory or file (credentials).
    pub hidden: Vec<PathBuf>,
    /// The session temp dir seen as /tmp inside; None: a private empty /tmp per command.
    pub tmp: Option<PathBuf>,
    pub network: bool,
    /// The whole environment inside.
    pub env: Vec<(String, String)>,
    pub image: Option<String>,
    pub uid: u32,
    pub gid: u32,
    /// The environment of the launcher itself (bwrap, unshare, docker, setpriv), from the session's environment
    /// ([`launch_env`]): where they are found and which Docker daemon is asked. Never this process's environment.
    pub launch: Vec<(String, String)>,
}

/// `path` with symlinks resolved when it exists; None when it does not.
fn existing(path: PathBuf) -> Option<PathBuf> {
    std::fs::canonicalize(&path).ok()
}

impl Policy {
    /// The policy for a session in `cwd`: the project, `extra_writable` (job and spill dirs) and `sandbox.writable`
    /// writable; `host_env` (the session's environment) filtered to the allowlist, and its PATH and DOCKER_HOST for
    /// the launcher ([`launch_env`]). Paths that do not exist are left out.
    pub fn new(
        settings: &Settings,
        cwd: &Path,
        home: &Path,
        extra_writable: &[PathBuf],
        tmp: Option<&Path>,
        host_env: &[(String, String)],
    ) -> Policy {
        let resolve = |list: &[String]| -> Vec<PathBuf> {
            list.iter()
                .filter_map(|path| existing(expand(path, home, cwd)))
                .collect()
        };
        let mut writable: Vec<PathBuf> = Vec::new();
        for path in std::iter::once(cwd.to_path_buf())
            .chain(extra_writable.iter().cloned())
            .filter_map(existing)
            .chain(resolve(&settings.writable))
        {
            if !writable.contains(&path) {
                writable.push(path);
            }
        }
        let tmp = tmp.and_then(|tmp| existing(tmp.to_path_buf()));
        let mut env: Vec<(String, String)> = host_env
            .iter()
            .filter(|(name, _)| {
                DEFAULT_ENV.contains(&name.as_str()) || name.starts_with("LC_") || settings.env.contains(name)
            })
            .filter(|(name, _)| name != "TMPDIR")
            .cloned()
            .collect();
        env.push((s("TMPDIR"), s("/tmp")));
        Policy {
            cwd: existing(cwd.to_path_buf()).unwrap_or_else(|| cwd.to_path_buf()),
            home: existing(home.to_path_buf()).unwrap_or_else(|| home.to_path_buf()),
            writable,
            readable: resolve(&settings.readable),
            hidden: resolve(&settings.hidden),
            tmp,
            network: settings.network,
            env,
            image: settings.image.clone(),
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            launch: launch_env(host_env),
        }
    }

    /// The host filesystem with the launcher's PATH: what [`Policy::wrap`] looks executables up on.
    pub fn layout(&self) -> PathLayout {
        PathLayout {
            path: self
                .launch
                .iter()
                .find(|(name, _)| name == "PATH")
                .map_or_else(|| s(SANDBOX_PATH), |(_, value)| value.clone()),
        }
    }

    /// [`Policy::wrap`] on the host filesystem as a [`Command`]: started from the launcher's environment only, in
    /// `cwd`. The launcher binary (`bwrap`, `unshare`, `setpriv`, `bash`) is found on that environment's PATH.
    pub fn command(&self, mechanism: Mechanism, argv: &[String], cwd: &Path) -> Command {
        self.spawnable(self.wrap(mechanism, &self.layout(), argv, cwd), cwd)
    }

    fn spawnable(&self, wrapped: Vec<String>, cwd: &Path) -> Command {
        let layout = self.layout();
        let program = resolve_program(&wrapped[0], &layout.path);
        let mut command = Command::new(program);
        command
            .args(&wrapped[1..])
            .env_clear()
            .envs(self.launch.iter().map(|(k, v)| (k, v)))
            .current_dir(cwd);
        command
    }

    /// Whether `path` (absolute, lexically normal) may be written: inside a writable path and not hidden, the most
    /// specific of the two winning (as the mounts do).
    pub fn may_write(&self, path: &Path) -> bool {
        let resolved = resolve_for_write(path);
        let deepest = |roots: &[PathBuf]| {
            roots
                .iter()
                .filter(|root| resolved.starts_with(root))
                .map(|root| root.components().count())
                .max()
        };
        match (deepest(&self.writable), deepest(&self.hidden)) {
            (Some(writable), Some(hidden)) => writable > hidden,
            (writable, _) => writable.is_some(),
        }
    }

    /// [`Policy::wrap`] for a process that must outlive the one starting it (a shell job a successor worker
    /// adopts). bwrap's `--die-with-parent` and the `setpriv --pdeathsig` of unshare and the Docker client kill the
    /// sandbox when its parent ends, and they stay: they are what makes killing the job's process group end every
    /// process inside (the sandbox's pid-1 is in a session of its own; a container is not a process of ours at all).
    /// The parent they watch becomes a small `sh` that is not tied to the starter, so the sandbox lives as long as
    /// that `sh`: until the job ends or its process group is killed.
    pub fn wrap_persistent(
        &self,
        mechanism: Mechanism,
        layout: &dyn Layout,
        argv: &[String],
        cwd: &Path,
    ) -> Vec<String> {
        // `; exit $?` keeps the shell from replacing itself with the sandbox (it must stay its parent).
        [s("sh"), s("-c"), s(r#""$@"; exit $?"#), s("ultron-job")]
            .into_iter()
            .chain(self.wrap(mechanism, layout, argv, cwd))
            .collect()
    }

    /// The command line running `argv` in `cwd` under `mechanism`.
    pub fn wrap(&self, mechanism: Mechanism, layout: &dyn Layout, argv: &[String], cwd: &Path) -> Vec<String> {
        let exists = |path: &PathBuf| layout.entry(path) != Entry::Missing;
        let writable: Vec<&PathBuf> = self.writable.iter().filter(|p| exists(p)).collect();
        let readable: Vec<&PathBuf> = self.readable.iter().filter(|p| exists(p)).collect();
        let hidden: Vec<&PathBuf> = self.hidden.iter().filter(|p| exists(p)).collect();
        match mechanism {
            Mechanism::Bwrap => {
                let mut out: Vec<String> = vec![s("bwrap"), s("--unshare-all")];
                if self.network {
                    out.push(s("--share-net"));
                }
                out.extend(["--die-with-parent", "--new-session", "--cap-drop", "ALL", "--clearenv"].map(s));
                for (name, value) in &self.env {
                    out.extend([s("--setenv"), name.clone(), value.clone()]);
                }
                for dir in SYSTEM_DIRS.iter().chain(INTERACTIVE_SYSTEM_EXTRA) {
                    match layout.entry(Path::new(dir)) {
                        Entry::Symlink(target) => out.extend([s("--symlink"), target, s(dir)]),
                        _ if is_dir(layout, Path::new(dir)) => out.extend([s("--ro-bind"), s(dir), s(dir)]),
                        _ => {}
                    }
                }
                out.extend(["--ro-bind", "/etc", "/etc"].map(s));
                if self.network {
                    out.extend(["--ro-bind-try", "/run/systemd/resolve", "/run/systemd/resolve"].map(s));
                }
                out.extend(["--proc", "/proc", "--dev", "/dev"].map(s));
                match &self.tmp {
                    Some(tmp) => out.extend([s("--bind"), p(tmp), s("/tmp")]),
                    None => out.extend([s("--tmpfs"), s("/tmp")]),
                }
                if is_dir(layout, &self.home) {
                    out.extend([s("--ro-bind"), p(&self.home), p(&self.home)]);
                }
                for path in &readable {
                    out.extend([s("--ro-bind"), p(path), p(path)]);
                }
                for path in &writable {
                    out.extend([s("--bind"), p(path), p(path)]);
                }
                for path in &hidden {
                    if is_dir(layout, path) {
                        out.extend([s("--tmpfs"), p(path)]);
                    } else {
                        out.extend([s("--ro-bind"), s("/dev/null"), p(path)]);
                    }
                }
                for (is_writable, _, path) in nested(&writable, &readable, &hidden) {
                    out.extend([s(if is_writable { "--bind" } else { "--ro-bind" }), p(path), p(path)]);
                }
                out.extend([s("--chdir"), p(cwd), s("--")]);
                out.extend(argv.iter().cloned());
                out
            }
            Mechanism::Unshare => {
                let mut out: Vec<String> = Vec::new();
                if layout.which("setpriv") {
                    out.extend(["setpriv", "--pdeathsig", "KILL", "--"].map(s));
                }
                out.extend(["unshare", "--user", "--map-root-user", "--mount"].map(s));
                if !self.network {
                    out.push(s("--net"));
                }
                out.extend(["--pid", "--fork", "--ipc", "--uts", "--kill-child", "env", "-i"].map(s));
                out.extend(self.env.iter().map(|(name, value)| format!("{name}={value}")));
                out.extend([s("sh"), s("-c"), s(INTERACTIVE_UNSHARE_SCRIPT), s("sandbox")]);
                out.extend([self.uid.to_string(), self.gid.to_string(), p(&self.home), p(cwd)]);
                out.push(self.tmp.as_deref().map(p).unwrap_or_default());
                for list in [&writable, &readable, &hidden] {
                    out.push(list.len().to_string());
                    out.extend(list.iter().map(|path| p(path)));
                }
                let nested = nested(&writable, &readable, &hidden);
                out.push(nested.len().to_string());
                for (is_writable, index, path) in nested {
                    out.extend([format!("{}{index}", if is_writable { "w" } else { "r" }), p(path)]);
                }
                out.push(s("--"));
                out.extend(argv.iter().cloned());
                out
            }
            Mechanism::Docker => {
                // A container is not a process of ours: killing the `docker run` client, or its process group, leaves
                // it running. It is tied to the client instead (DOCKER_CLIENT, DOCKER_TIED), and the client to its
                // parent as unshare is.
                let mut out: Vec<String> = Vec::new();
                let setpriv = layout.which("setpriv");
                if setpriv {
                    out.extend(["setpriv", "--pdeathsig", "KILL", "--"].map(s));
                }
                let client = if setpriv { DOCKER_CLIENT } else { DOCKER_CLIENT_POLLING };
                out.extend([s("bash"), s("-c"), s(client), s("ultron-docker")]);
                out.extend(["docker", "run", "--rm", "--init", "-i", "--pull", "never"].map(s));
                if !self.network {
                    out.extend([s("--network"), s("none")]);
                }
                out.extend(
                    [
                        "--cap-drop",
                        "ALL",
                        "--security-opt",
                        "no-new-privileges",
                        "--pids-limit",
                        "1024",
                    ]
                    .map(s),
                );
                out.extend([s("--user"), format!("{}:{}", self.uid, self.gid)]);
                match &self.tmp {
                    Some(tmp) => out.extend([s("-v"), format!("{}:/tmp:rw", p(tmp))]),
                    None => out.extend([s("--tmpfs"), s("/tmp")]),
                }
                if is_dir(layout, &self.home) {
                    out.extend([s("-v"), format!("{0}:{0}:ro", p(&self.home))]);
                }
                for path in readable {
                    out.extend([s("-v"), format!("{0}:{0}:ro", p(path))]);
                }
                for path in writable {
                    out.extend([s("-v"), format!("{0}:{0}:rw", p(path))]);
                }
                for path in hidden {
                    if is_dir(layout, path) {
                        out.extend([s("--tmpfs"), p(path)]);
                    } else {
                        out.extend([s("-v"), format!("/dev/null:{}:ro", p(path))]);
                    }
                }
                out.extend([s("-w"), p(cwd)]);
                out.push(self.image.clone().unwrap_or_default());
                out.extend([s("sh"), s("-c"), s(DOCKER_TIED), s("sh")]);
                // `env -i`, not `docker run -e`: `-e` adds to the image's own ENV, which then reaches the command.
                out.extend([s("env"), s("-i")]);
                out.extend(self.env.iter().map(|(name, value)| format!("{name}={value}")));
                out.extend(argv.iter().cloned());
                out
            }
        }
    }

    pub fn describe(&self, mechanism: Option<Mechanism>) -> String {
        let network = if self.network { "network on" } else { "no network" };
        match mechanism {
            Some(Mechanism::Bwrap) => format!(
                "bubblewrap: new user, mount, pid, ipc, uts and cgroup namespaces, {network}; an empty root with the system directories and /etc read-only, the home read-only with credentials hidden, the project and session directories writable, a private /tmp; all capabilities dropped; environment cleared to an allowlist; dies with its parent"
            ),
            Some(Mechanism::Unshare) => format!(
                "unshare: new user, mount, pid, ipc and uts namespaces, {network}; /home and the home read-only with credentials hidden, the project and session directories writable, private /tmp, /var/tmp, /dev/shm and /run/user; capabilities dropped; environment cleared to an allowlist; dies with its parent"
            ),
            Some(Mechanism::Docker) => format!(
                "docker ({}): {network}, all capabilities dropped, no new privileges, the home read-only with credentials hidden, the project and session directories writable; environment cleared to an allowlist; dies with its parent",
                self.image.as_deref().unwrap_or_default()
            ),
            None => s("none: commands run without isolation"),
        }
    }
}

/// The real location a write to `path` would reach: the nearest existing ancestor with symlinks resolved, plus the
/// rest. A symlink inside the project pointing outside therefore counts as outside.
pub fn resolve_for_write(path: &Path) -> PathBuf {
    let path = crate::settings::normalize(path);
    let mut existing = path.as_path();
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(existing) {
            let mut out = real;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name);
                existing = parent;
            }
            _ => return path,
        }
    }
}

/// The environment of the launcher itself (bwrap, unshare, docker) from a session's environment `env`: its PATH
/// (else [`SANDBOX_PATH`]) and DOCKER_HOST, nothing else. Never this process's environment.
pub fn launch_env(env: &[(String, String)]) -> Vec<(String, String)> {
    let var = |name: &str| {
        env.iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    let mut out = vec![(s("PATH"), var("PATH").unwrap_or_else(|| s(SANDBOX_PATH)))];
    if let Some(host) = var("DOCKER_HOST") {
        out.push((s("DOCKER_HOST"), host));
    }
    out
}

/// `program` as the launcher's PATH finds it: a bare name becomes the first executable of that name in `path`, so
/// the spawn does not search this process's PATH; a name found nowhere (or a path) stays as it is.
pub fn resolve_program(program: &str, path: &str) -> PathBuf {
    if program.contains('/') {
        return PathBuf::from(program);
    }
    std::env::split_paths(path)
        .map(|dir| dir.join(program))
        .find(|candidate| {
            std::fs::metadata(candidate).is_ok_and(|meta| {
                use std::os::unix::fs::PermissionsExt;
                meta.is_file() && meta.permissions().mode() & 0o111 != 0
            })
        })
        .unwrap_or_else(|| PathBuf::from(program))
}

/// A session's launcher: the mode, the policy, and the mechanism found on first use.
pub struct Launcher {
    pub mode: Mode,
    pub policy: Policy,
    mechanism: OnceLock<Option<Mechanism>>,
}

impl Launcher {
    pub fn new(mode: Mode, policy: Policy) -> Self {
        Launcher {
            mode,
            policy,
            mechanism: OnceLock::new(),
        }
    }

    /// Use `mechanism` instead of detecting one (tests, `doctor --mechanism`).
    pub fn with_mechanism(self, mechanism: Option<Mechanism>) -> Self {
        let _ = self.mechanism.set(mechanism);
        self
    }

    /// The mechanism in use: None when off, or when nothing works here.
    pub fn mechanism(&self) -> Option<Mechanism> {
        if self.mode == Mode::Off {
            return None;
        }
        *self.mechanism.get_or_init(|| {
            let layout = self.policy.layout();
            detect(
                self.policy.image.as_deref(),
                None,
                |name| layout.which(name),
                |mechanism| self.probe(mechanism),
            )
        })
    }

    /// Whether `true` runs under `mechanism` here.
    pub fn probe(&self, mechanism: Mechanism) -> bool {
        let mut command = self.policy.command(mechanism, &[s("true")], &self.policy.cwd);
        command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        command.status().is_ok_and(|status| status.success())
    }

    /// The process running `argv` in `cwd`: isolated when a mechanism is in use, as before when off or (auto)
    /// when nothing works, refused when required and nothing works.
    pub fn command(&self, argv: &[String], cwd: &Path) -> Result<Command, String> {
        let Some(mechanism) = self.mechanism() else {
            if self.mode == Mode::Required {
                return Err(s(
                    "sandbox.mode is required but no sandbox works here (install bubblewrap, allow unprivileged user namespaces, or set sandbox.image for Docker)",
                ));
            }
            let mut command = Command::new(&argv[0]);
            command.args(&argv[1..]).current_dir(cwd);
            return Ok(command);
        };
        Ok(self.policy.command(mechanism, argv, cwd))
    }

    /// [`Launcher::command`] for a process that outlives its starter ([`Policy::wrap_persistent`]): shell jobs,
    /// which a successor worker adopts after a hand-over.
    pub fn command_persistent(&self, argv: &[String], cwd: &Path) -> Result<Command, String> {
        let Some(mechanism) = self.mechanism() else {
            return self.command(argv, cwd);
        };
        let wrapped = self.policy.wrap_persistent(mechanism, &self.policy.layout(), argv, cwd);
        Ok(self.policy.spawnable(wrapped, cwd))
    }

    /// Which isolation is in use, for status lines and `doctor`.
    pub fn describe(&self) -> String {
        match (self.mode, self.mechanism()) {
            (Mode::Off, _) => s("off: commands run without isolation"),
            (_, mechanism) => self.policy.describe(mechanism),
        }
    }
}
