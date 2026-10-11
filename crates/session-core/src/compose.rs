//! The composition root of a session's host stack. A cell's host request passes, outermost first:
//!
//! 1. [`kernel_apis::KernelApis`]: the kernel-API modules (`skills.*` with code skills, `memory.*`, `grants.*`,
//!    `hints.*`), matched by prefix;
//! 2. [`inference::Inference`]: `rlm.infer`, `rlm.load`, `rlm.frames`;
//! 3. [`Router`]: subagents (`rlm.spawn`, `rlm.collect`, ... served by [`delegation::host::Delegation`] for the
//!    calling lane, in host collect mode: a cell never waits) and extension functions (`ext.call`, `ext.list`);
//!    the lane API (`rlm.lanes`, `rlm.lane.*`, `rlm.cells`, `rlm.cell.cancel`, `rlm.recovery`) is answered by the
//!    session's REPL lanes ([`delegation::lanes::Lanes`]) first;
//! 4. [`host::Host`], sandboxed ([`host::Host::sandboxed`] with the `sandbox.*` and `approval.*` settings) over the
//!    session's file hooks (Loki): shell, jobs and the before-write check.
//!
//! Each lane's kernel runs inside the session's shell sandbox (`KernelOptions::sandbox`), with `ULTRON_FILE_HOOKS=1`
//! (its writes ask the host first), the extension functions listed for runtime.py's `ext` (`ULTRON_EXT_FUNCTIONS`),
//! a per-session spill dir, a signed snapshot under the profile's `rlm-snapshots/<session>/` with the profile
//! snapshot key, and the continuity policy (checkpoint after each cell, restore on restart). Subagent and detached
//! cell completions continue the run between turns ([`RuntimeEvents`]). The model sees one tool, `rlm`.

use crate::RunEnv as Env;
use crate::lane::{Provider, Rlm, rlm_over};
use agent::{AgentOptions, AgentTool, StreamFn};
use ai::http::ReqwestTransport;
use ai::{Message, Model, UserMessage, UserMessageContent};
use delegation::child::agent_runner;
use delegation::host::{ChildRun, ChildRunner, CollectMode, Delegation, DelegationOptions, DetachedEnd, Park};
use delegation::lanes::{CellEnd, Lanes, LanesOptions};
use extensions::{ExtensionHost, NoHost, SessionInfo};
use host::Host;
use host::file_hooks::FileHooks;
use host::loki_guard::{LokiGuard, LokiMode, git_root, loki_mode};
use inference::Inference;
use kernel::{ContinuityPolicy, HostHandler, Kernel, KernelOptions, Signal};
use kernel_apis::{
    CodeSkills, CodeSkillsOptions, FileDocumentStore, GrantsModule, HintsModule, KernelApis, MemoryApi, MemoryBackend,
    MemoryService, SkillSource, SkillsModule,
};
use sandbox::Approver;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

/// Host requests the subagent host serves.
const DELEGATION_REQUESTS: [&str; 7] = [
    "rlm.spawn",
    "rlm.finish",
    "rlm.list_subagents",
    "rlm.collect",
    "rlm.results",
    "rlm.delete_subagent",
    "rlm.merge",
];

/// Collaborators a caller may supply in place of the defaults (tests, embedders).
#[derive(Clone, Default)]
pub struct Services {
    /// The memory backend behind `memory.*`; none: the Hindsight server and bank this process resolves
    /// ([`hindsight::Endpoint`]), or memory off ("Hindsight is not configured") when that resolves to off.
    pub memory: Option<Arc<dyn MemoryBackend>>,
    /// Answers `prompt` approval decisions (none: they fail closed).
    pub approver: Option<Arc<dyn Approver>>,
}

/// What every lane of this process shares: settings, locations, guards, extensions and the snapshot key. Owned
/// values only, so subagent lanes can be built on their own threads.
pub struct Config {
    pub cwd: PathBuf,
    pub home: PathBuf,
    pub agent_dir: PathBuf,
    /// The merged settings (`sandbox.*`, `approval.*`, `loki`, ...).
    pub settings: Value,
    pub loki: LokiMode,
    /// Where Loki's policy is read: the Git work tree containing the cwd, else the cwd (None when Loki is off).
    pub loki_root: Option<PathBuf>,
    pub runtime_path: PathBuf,
    pub skills_dir: PathBuf,
    pub mask_secrets: Option<String>,
    pub output_bytes: Option<String>,
    pub snapshot_key: Option<Vec<u8>>,
    pub spawn_depth: u32,
    /// `ULTRON_ASYNC_EVENTS` is not off: completions of detached work continue the run.
    pub async_events: bool,
}

impl Config {
    /// The kernel environment and output budget a lane reads.
    pub fn from_env(env: &Env) -> Config {
        let agent_dir = env.agent_dir();
        let manager = config::settings::SettingsManager::create(&env.cwd, &agent_dir, false);
        let settings = Value::Object(manager.settings().clone());
        let loki = loki_mode((env.var)("ULTRON_LOKI").as_deref(), &manager.loki_settings());
        let loki_root = (loki != LokiMode::Off).then(|| git_root(&env.cwd).unwrap_or_else(|| env.cwd.clone()));
        let skills_dir = (env.var)("ULTRON_CODE_SKILLS_DIR")
            .map(|dir| dir.trim().to_string())
            .filter(|dir| !dir.is_empty())
            .map_or_else(|| agent_dir.join("skills"), PathBuf::from);
        Config {
            cwd: env.cwd.clone(),
            home: env.home.clone(),
            snapshot_key: kernel::snapshot::load_or_create_snapshot_key(&agent_dir).ok(),
            agent_dir,
            settings,
            loki,
            loki_root,
            runtime_path: config::paths::normalize(&env.docs.join("../src/ultron/rlm/runtime.py")),
            skills_dir,
            mask_secrets: (env.var)("ULTRON_MASK_SECRETS"),
            output_bytes: (env.var)("ULTRON_RLM_OUTPUT_BYTES"),
            spawn_depth: delegation::host::spawn_depth_limit((env.var)("ULTRON_SPAWN_DEPTH").as_deref()),
            async_events: (env.var)("ULTRON_ASYNC_EVENTS")
                .is_none_or(|v| !matches!(v.trim().to_lowercase().as_str(), "off" | "0" | "false" | "no")),
        }
    }

    /// The session's file hooks (session-worker.ts): Loki's guard unless `ULTRON_LOKI` or the global `loki.mode`
    /// setting turns it off.
    /// In `cwd` (a worktree subagent's own), Loki checks that tree.
    fn file_hooks_in(&self, cwd: &Path) -> FileHooks {
        let mut hooks = FileHooks::new(cwd);
        if let Some(root) = &self.loki_root {
            let root = if cwd == self.cwd {
                root.clone()
            } else {
                git_root(cwd).unwrap_or_else(|| cwd.to_path_buf())
            };
            hooks.add(Box::new(LokiGuard::new(&root, self.loki)));
        }
        hooks
    }

    /// Per-session directories under the profile, private like snapshots.
    fn session_dir(&self, kind: &str, session_id: &str) -> PathBuf {
        self.agent_dir.join(kind).join(session_id)
    }
}

/// Lane names as file names (`laneDir` of session-worker.ts).
fn lane_dir(lane: &str) -> String {
    lane.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The process-wide composition: configuration, loaded extensions and injected services.
pub struct Runtime {
    pub config: Arc<Config>,
    pub extensions: Option<Arc<ExtensionHost>>,
    pub services: Services,
    /// Problems found loading extensions (reported once, as warnings).
    pub diagnostics: Vec<String>,
    /// The root lane's system prompt (subagents use it too).
    pub system: String,
    /// Where session-scoped host files (job logs, inference context) live: the cwd's session directory.
    pub sessions_dir: PathBuf,
    /// `print`, `json`, `rpc` or `interactive`.
    pub mode: String,
    /// The Hindsight server and bank every instance shares (`ULTRON_HINDSIGHT_URL`, the global `hindsightUrl`
    /// setting, else localhost:8888; `ULTRON_HINDSIGHT_BANK`, else "ultron"); `None`: memory is off.
    pub hindsight: Option<hindsight::Endpoint>,
}

impl Runtime {
    /// Read the settings, start the extensions configured for `cwd` (`mode`: "print", "json" or "rpc") and build
    /// the root system prompt.
    pub fn new(env: &Env, mode: &str, sessions_dir: PathBuf, services: Services) -> Result<Runtime, String> {
        let config = Config::from_env(env);
        let manager = config::settings::SettingsManager::create(&env.cwd, &config.agent_dir, false);
        let discovery = extensions::discover_with_settings(&env.cwd, &config.agent_dir, &env.home, &manager);
        let mut diagnostics: Vec<String> = discovery
            .diagnostics
            .iter()
            .map(|d| format!("{}: {}", d.path.display(), d.message))
            .collect();
        let extensions = (!discovery.manifests.is_empty()).then(|| {
            let session = SessionInfo {
                cwd: env.cwd.display().to_string(),
                mode: mode.into(),
                has_ui: false,
                flags: Map::new(),
            };
            Arc::new(ExtensionHost::load(discovery.manifests, session, Arc::new(NoHost)))
        });
        if let Some(host) = &extensions {
            diagnostics.extend(
                host.diagnostics
                    .iter()
                    .map(|d| format!("{}: {}", d.path.display(), d.message)),
            );
        }
        let hindsight = hindsight::Endpoint::resolve(&|key| (env.var)(key), hindsight::resolve::setting_url(&manager));
        let guide = extensions
            .as_ref()
            .and_then(|host| extensions::kernel_api::guide_section(&host.functions()));
        let system = crate::lane::system_prompt(
            env,
            &env.cwd.display().to_string(),
            config.loki_root.as_deref(),
            guide.as_deref(),
        )?;
        Ok(Runtime {
            config: Arc::new(config),
            extensions,
            services,
            diagnostics,
            system,
            sessions_dir,
            mode: mode.to_string(),
            hindsight,
        })
    }

    /// The host stack of one session, whose lanes call `model` with `api_key`.
    pub fn stack(&self, model: &Model, api_key: Option<String>, session_id: &str) -> Result<Arc<Stack>, String> {
        let config = self.config.clone();
        let host = Arc::new(self.host_in(&config.cwd, session_id, "bash")?);
        let modules = |name: &str| -> Arc<FileDocumentStore> {
            Arc::new(FileDocumentStore::new(
                config
                    .session_dir("rlm-modules", session_id)
                    .join(format!("{name}.json")),
            ))
        };
        // TS createWorkerServices: a configured server gives every session a backend tagged with the session, its
        // project directory and the home directory; an invalid URL fails the session as it does there.
        let backend = match (&self.services.memory, &self.hindsight) {
            (Some(backend), _) => Some(backend.clone()),
            (None, Some(endpoint)) => Some(Arc::new(
                endpoint
                    .session_backend(
                        session_id,
                        &config.cwd.display().to_string(),
                        &config.home.display().to_string(),
                    )
                    .map_err(|e| e.to_string())?,
            ) as Arc<dyn MemoryBackend>),
            (None, None) => None,
        };
        let memory = match &backend {
            Some(backend) => Some(
                MemoryService::new(modules("memory"), backend.clone(), kernel_apis::worker_gate())
                    .map_err(|e| format!("memory: {e:?}"))?,
            ),
            None => None,
        };
        let skills_config = config.clone();
        let loader: kernel_apis::SkillLoader = Arc::new(move || Ok(skill_sources(&skills_config)));
        let code = CodeSkills::new(CodeSkillsOptions::new(&config.skills_dir, &config.runtime_path));
        let clock = kernel_apis::util::system_clock();
        let apis = ApiModules {
            skills: Arc::new(SkillsModule::new(modules("skills"), loader, clock.clone(), Some(code))),
            memory: Arc::new(MemoryApi { service: memory }),
            grants: Arc::new(GrantsModule::new(modules("grants"), clock, false)),
            hints: Arc::new(HintsModule::new(modules("hints"))),
        };
        let session_id = session_id.to_string();
        let system = self.system.clone();
        let extensions = self.extensions.clone();
        let sessions_dir = self.sessions_dir.clone();
        let services = self.services.clone();
        let model = model.clone();
        let child = ChildDefaults {
            model: model.clone(),
            api_key: api_key.clone(),
        };
        let mut built: Result<(), String> = Ok(());
        let stack = Arc::new_cyclic(|me: &Weak<Stack>| {
            let runner_stack = me.clone();
            let inner = agent_runner(Arc::new(move |run: &ChildRun| child_agent(&runner_stack, &child, run)));
            let closer = me.clone();
            // A subagent's lane closes when its run ends.
            let runner: ChildRunner = Arc::new(move |run: ChildRun| {
                let lane = run.lane_name.clone();
                let result = inner(run);
                if let Some(stack) = closer.upgrade() {
                    let _ = stack.lanes.close_lane(&lane);
                    lock(&stack.workspaces).remove(&lane);
                }
                result
            });
            let mut options = DelegationOptions::new(runner);
            // A cell never waits for subagents: rlm.collect returns what has ended, and the host waits between
            // turns, delivering each end as a child_done event.
            options.collect = CollectMode::Host;
            options.spawn_depth = config.spawn_depth;
            options.session_id = session_id.clone();
            options.workspace = Some(config.cwd.clone());
            options.now = Box::new(|| crate::lane::now_ms() as f64);
            let (delegation, child_ends) = Delegation::new(options);
            let factory_stack = me.clone();
            let handler_stack = me.clone();
            let lanes = Lanes::new(LanesOptions {
                create: Arc::new(move |lane| {
                    let stack = factory_stack.upgrade().ok_or("the session is closed")?;
                    stack.kernel(lane)
                }),
                handlers: Arc::new(move |lane, signal, park| match handler_stack.upgrade() {
                    Some(stack) => stack.cell_handler(lane, Some((signal, park))),
                    None => Arc::new(Closed) as Arc<dyn HostHandler>,
                }),
                max_lanes: 64,
                cancel_grace: Duration::from_secs(5),
            });
            let (lanes, cell_ends) = match lanes {
                Ok(pair) => pair,
                Err(error) => {
                    built = Err(error);
                    let (lanes, ends) = Lanes::new(LanesOptions {
                        create: Arc::new(|_| Err("no lanes".into())),
                        handlers: Arc::new(|_, _, _| Arc::new(Closed) as Arc<dyn HostHandler>),
                        max_lanes: 1,
                        cancel_grace: Duration::ZERO,
                    })
                    .expect("one lane is a valid limit");
                    (lanes, ends)
                }
            };
            let events = Arc::new(RuntimeEvents {
                child_ends: Mutex::new(child_ends),
                cell_ends: Mutex::new(cell_ends),
                delegation: delegation.clone(),
                lanes: lanes.clone(),
                enabled: config.async_events,
            });
            Stack {
                config,
                host,
                delegation,
                extensions,
                apis,
                model,
                api_key,
                session_id,
                sessions_dir,
                system,
                services,
                lanes,
                workspaces: Mutex::new(HashMap::new()),
                kernel_sandbox: std::sync::OnceLock::new(),
                events,
            }
        });
        built?;
        stack.lanes.open(crate::lane::MAIN)?;
        Ok(stack)
    }

    /// A sandboxed host with its working directory at `cwd` (the session's, or a worktree subagent's).
    fn host_in(&self, cwd: &Path, session_id: &str, spill: &str) -> Result<Host, String> {
        let config = &self.config;
        let jobs_dir = self.sessions_dir.join("rlm-jobs").join(session_id);
        // Blocking-bash spill files stay with the session (TS: the OS temp dir), so the sandbox keeps only
        // session paths writable.
        let spill_dir = config.session_dir("rlm-output", session_id).join(spill);
        let tmp_dir = config.session_dir("rlm-tmp", session_id);
        for dir in [&spill_dir, &tmp_dir] {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        Ok(
            Host::new(cwd, jobs_dir, spill_dir, Box::new(config.file_hooks_in(cwd))).sandboxed(
                &config.settings,
                &config.home,
                Some(&tmp_dir),
                self.services.approver.clone(),
            ),
        )
    }
}

/// The skill catalog `skills.*` serves: the session's skills with their file contents.
fn skill_sources(config: &Config) -> Vec<SkillSource> {
    let cwd = config.cwd.display().to_string();
    let agent_dir = config.agent_dir.display().to_string();
    resources::skills::load_skills(&resources::skills::LoadSkillsOptions {
        cwd: &cwd,
        agent_dir: &agent_dir,
        skill_paths: &[],
        include_defaults: true,
    })
    .skills
    .into_iter()
    .map(|skill| SkillSource {
        content: std::fs::read_to_string(&skill.file_path).unwrap_or_default(),
        name: skill.name,
        description: skill.description,
        file_path: skill.file_path,
        disable_model_invocation: skill.disable_model_invocation,
    })
    .collect()
}

struct ApiModules {
    skills: Arc<SkillsModule>,
    memory: Arc<MemoryApi>,
    grants: Arc<GrantsModule>,
    hints: Arc<HintsModule>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A worktree subagent's lane: its working directory, extra environment and host.
struct Workspace {
    cwd: PathBuf,
    env: Vec<(String, String)>,
    host: Arc<Host>,
}

/// A handler for a closed session.
struct Closed;

impl HostHandler for Closed {
    fn handle(&self, kind: &str, _: Map<String, Value>, _: &Signal) -> Result<Value, String> {
        Err(format!("The session is closed ({kind})"))
    }
}

/// One session's host stack: the sandboxed host, the REPL lanes, the subagent host, the extensions and the
/// kernel-API modules.
pub struct Stack {
    config: Arc<Config>,
    pub host: Arc<Host>,
    pub delegation: Delegation,
    pub lanes: Lanes,
    pub events: Arc<RuntimeEvents>,
    extensions: Option<Arc<ExtensionHost>>,
    apis: ApiModules,
    model: Model,
    api_key: Option<String>,
    session_id: String,
    sessions_dir: PathBuf,
    system: String,
    services: Services,
    workspaces: Mutex<HashMap<String, Workspace>>,
    /// The root lane kernels' sandbox, once built.
    kernel_sandbox: std::sync::OnceLock<Option<Arc<sandbox::Launcher>>>,
}

impl Stack {
    /// The host `lane` runs its shell in: a worktree subagent's own, else the session's.
    fn host_of(&self, lane: &str) -> Arc<Host> {
        lock(&self.workspaces)
            .get(lane)
            .map_or_else(|| self.host.clone(), |w| w.host.clone())
    }

    /// The handler `lane`'s requests go to (outermost layer first). `cell`: the cell's wait signal and park
    /// callback (subagent waits end with the cell); none for requests outside a cell.
    pub fn cell_handler(&self, lane: &str, cell: Option<(Signal, Park)>) -> Arc<dyn HostHandler> {
        let delegation: Arc<dyn HostHandler> = match cell {
            Some((signal, park)) => Arc::new(self.delegation.cell_handler(lane, signal, park)),
            None => Arc::new(self.delegation.lane_handler(lane)),
        };
        let router = Router {
            delegation,
            extensions: self.extensions.clone(),
            host: self.host_of(lane),
        };
        let inference = Inference::new(
            Arc::new(router),
            self.model.clone(),
            self.api_key.clone(),
            // Frames run on their handler threads' own runtimes, so they get their own HTTP client.
            Arc::new(ReqwestTransport::new(None)),
            self.sessions_dir.join("rlm-context").join(&self.session_id),
        );
        let apis = KernelApis::new(Arc::new(inference))
            .with(self.apis.skills.clone())
            .with(self.apis.memory.clone())
            .with(self.apis.grants.clone())
            .with(self.apis.hints.clone());
        Arc::new(apis)
    }

    /// `lane`'s kernel (not started): in the lane's working directory, inside the shell sandbox, with the
    /// extension functions listed, its snapshot under the profile and the continuity policy.
    fn kernel(&self, lane: &str) -> Result<Kernel, String> {
        let config = &self.config;
        let (cwd, extra) = match lock(&self.workspaces).get(lane) {
            Some(workspace) => (workspace.cwd.clone(), workspace.env.clone()),
            None => (config.cwd.clone(), Vec::new()),
        };
        let spill_dir = config.session_dir("rlm-output", &self.session_id).join(lane_dir(lane));
        let mut options = KernelOptions::new(&cwd, &config.runtime_path);
        options.env = vec![
            ("ULTRON_CODE_SKILLS_DIR".into(), config.skills_dir.display().to_string()),
            ("ULTRON_RLM_SPILL_DIR".into(), spill_dir.display().to_string()),
            // The kernel's write skills ask the host's file hooks (Loki, the sandbox's containment) first.
            ("ULTRON_FILE_HOOKS".into(), "1".into()),
            ("HOME".into(), config.home.display().to_string()),
        ];
        if let Some(host) = &self.extensions
            && let Some(listing) = extensions::kernel_api::kernel_env(&host.functions())
        {
            options.env.push(listing);
        }
        options.env.extend(extra);
        let snapshots = config.session_dir("rlm-snapshots", &self.session_id);
        std::fs::DirBuilder::new()
            .recursive(true)
            .create(&snapshots)
            .map_err(|e| format!("{}: {e}", snapshots.display()))?;
        options.snapshot_path = Some(snapshots.join(format!("{}.snapshot", lane_dir(lane))));
        options.snapshot_key = config.snapshot_key.clone();
        options.continuity = Some(ContinuityPolicy::default());
        // The Python process itself runs inside the session's shell sandbox, so direct writes are contained too.
        options.sandbox = self.kernel_launcher(lane)?;
        Kernel::new(options, self.cell_handler(lane, None)).map_err(|e| e.to_string())
    }

    /// The sandbox a lane's kernel runs in: its host's shell policy plus the inference context directory, which
    /// `rlm.load` writes from inside the kernel (the kernel adds its snapshot and spill paths itself). The root
    /// lane's is built once.
    fn kernel_launcher(&self, lane: &str) -> Result<Option<Arc<sandbox::Launcher>>, String> {
        let host = self.host_of(lane);
        let workspace = lock(&self.workspaces).contains_key(lane);
        if !workspace && let Some(launcher) = self.kernel_sandbox.get() {
            return Ok(launcher.clone());
        }
        let context = self.sessions_dir.join("rlm-context").join(&self.session_id);
        std::fs::create_dir_all(&context).map_err(|e| format!("{}: {e}", context.display()))?;
        let launcher = host.launcher().map(|launcher| {
            let mut policy = launcher.policy.clone();
            policy.writable.push(std::fs::canonicalize(&context).unwrap_or(context));
            Arc::new(sandbox::Launcher::new(launcher.mode, policy))
        });
        if !workspace {
            let _ = self.kernel_sandbox.set(launcher.clone());
        }
        Ok(launcher)
    }

    /// The `rlm` tool over `lane` (opened when needed).
    pub fn rlm(&self, lane: &str) -> Result<Rlm, String> {
        self.lanes.open(lane)?;
        let spill_dir = self
            .config
            .session_dir("rlm-output", &self.session_id)
            .join(lane_dir(lane));
        Ok(rlm_over(&self.config, self.lanes.clone(), lane, spill_dir))
    }

    /// Stop the session's lanes and subagents.
    pub fn close(&self) {
        self.delegation.close();
        self.lanes.close();
    }
}

/// A subagent's agent: its own lane (in its worktree when it has one, with the system prompt naming that working
/// directory, as `session-worker.ts` rewrites it), the session's model unless the spawn named one.
fn child_agent(stack: &Weak<Stack>, defaults: &ChildDefaults, run: &ChildRun) -> (AgentOptions, Box<dyn StreamFn>) {
    let Some(stack) = stack.upgrade() else {
        // The session is closing: a loop without tools whose provider reports no work.
        let mut options = AgentOptions::new(defaults.model.clone());
        options.system_prompt = Some("The session ended before this subagent started.".into());
        return (options, Box::new(Provider::with_key(defaults.api_key.clone())));
    };
    let model = run
        .model
        .as_deref()
        .and_then(|spec| spec.split_once('/'))
        .and_then(|(provider, id)| ai::models::builtin_model(provider, id).cloned())
        .map(|mut model| {
            if model.provider == stack.model.provider {
                model.base_url = stack.model.base_url.clone();
            }
            model
        })
        .unwrap_or_else(|| stack.model.clone());
    let mut options = AgentOptions::new(model);
    let root_cwd = stack.config.cwd.display().to_string();
    let mut system = stack.system.clone();
    if let Some(cwd) = run.cwd.as_deref() {
        // A worktree subagent works in its worktree: its shell, kernel and prompt name it, not the checkout.
        let host = Runtime {
            config: stack.config.clone(),
            extensions: None,
            services: stack.services.clone(),
            diagnostics: Vec::new(),
            system: String::new(),
            sessions_dir: stack.sessions_dir.clone(),
            mode: String::new(),
            hindsight: None,
        }
        .host_in(Path::new(cwd), &stack.session_id, &lane_dir(&run.lane_name));
        match host {
            Ok(host) => {
                lock(&stack.workspaces).insert(
                    run.lane_name.clone(),
                    Workspace {
                        cwd: PathBuf::from(cwd),
                        env: run.env.clone(),
                        host: Arc::new(host),
                    },
                );
                system = system.split(&root_cwd).collect::<Vec<_>>().join(cwd);
            }
            Err(error) => system.push_str(&format!("\n\nThe worktree could not be prepared: {error}")),
        }
    }
    options.system_prompt = Some(system.clone());
    options.thinking_level = run.thinking.clone().unwrap_or_else(|| "off".into());
    match stack.rlm(&run.lane_name) {
        Ok(tool) => options.tools = vec![Arc::new(tool) as Arc<dyn AgentTool>],
        // The child's loop then runs without its REPL and reports the failure in its reply.
        Err(error) => options.system_prompt = Some(format!("{system}\n\nThe REPL failed to start: {error}")),
    }
    (options, Box::new(Provider::with_key(stack.api_key.clone())))
}

/// The model and key a subagent falls back to.
struct ChildDefaults {
    model: Model,
    api_key: Option<String>,
}

/// Routes a cell's requests: subagents to the delegation host (as that lane), extension functions to their
/// processes, everything else to the host operations. (The lane API — `rlm.lanes`, `rlm.lane.*`, `rlm.cells`,
/// `rlm.cell.cancel`, `rlm.recovery` — is answered by the lanes before a request gets here.)
struct Router {
    delegation: Arc<dyn HostHandler>,
    extensions: Option<Arc<ExtensionHost>>,
    host: Arc<Host>,
}

impl HostHandler for Router {
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        if DELEGATION_REQUESTS.contains(&kind) || kind.starts_with("rlm.worktrees.") {
            return self.delegation.handle(kind, payload, signal);
        }
        if kind == "ext.call" || kind == "ext.list" {
            let host = self
                .extensions
                .as_ref()
                .ok_or("No extensions are loaded in this session")?;
            let cancel = extensions::CancelToken::new();
            let watched = cancel.clone();
            let ended = signal.clone();
            // A cell that ends (or is cancelled) cancels its call.
            let watcher = std::thread::spawn(move || {
                while !ended.wait(Duration::from_millis(200)) {
                    if watched.is_cancelled() {
                        return;
                    }
                }
                watched.cancel(Some("the cell ended"));
            });
            let result = extensions::kernel_api::handle(host, kind, &Value::Object(payload), None, Some(&cancel))
                .unwrap_or_else(|| {
                    Err(extensions::RpcError::Other(format!(
                        "Unknown extension request: {kind}"
                    )))
                })
                .map_err(|e| e.to_string());
            cancel.cancel(None);
            let _ = watcher.join();
            return result;
        }
        self.host.handle(kind, payload, signal)
    }
}

/// `<runtime_event>` attribute text (`attribute` of async-events.ts).
fn attribute(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `boundedSummary`: one line of at most 240 characters.
fn bounded(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > 240 {
        format!("{}…", line.chars().take(239).collect::<String>())
    } else {
        line
    }
}

/// One `<runtime_event>` line (`runtimeEventText` of async-events.ts).
pub fn runtime_event_line(kind: &str, id: &str, status: &str, summary: &str, fetch: &str) -> String {
    format!(
        "<runtime_event kind=\"{}\" id=\"{}\" status=\"{}\" summary=\"{}\" fetch=\"{}\" />",
        attribute(kind),
        attribute(id),
        attribute(status),
        attribute(&bounded(
            &host::secrets::OutputMasker::default().mask_counted(summary, None).0
        )),
        attribute(fetch)
    )
}

/// Completions of work a run started that outlives its cell: subagents (`child_done`) and detached cells
/// (`cell_done`) of the root lane. Between turns the run waits here (in the host, never in a cell) while such work
/// is running, and each completion continues the run as a `<runtime_event>` message. `ULTRON_ASYNC_EVENTS=off`
/// turns this off: nothing waits and nothing is announced.
pub struct RuntimeEvents {
    child_ends: Mutex<Receiver<DetachedEnd>>,
    cell_ends: Mutex<Receiver<CellEnd>>,
    delegation: Delegation,
    lanes: Lanes,
    enabled: bool,
}

impl RuntimeEvents {
    fn pending_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        while let Ok(end) = lock(&self.child_ends).try_recv() {
            if end.owner_lane != crate::lane::MAIN {
                continue;
            }
            let task = &end.task;
            let result = &task["result"];
            let status = result["status"].as_str().or(task["status"].as_str()).unwrap_or("ended");
            let summary = match &result["value"] {
                Value::String(text) => text.clone(),
                Value::Null => result["error"].as_str().unwrap_or_default().to_string(),
                other => other.to_string(),
            };
            let id = task["id"].as_str().unwrap_or_default();
            lines.push(runtime_event_line(end.kind, id, status, &summary, &end.fetch));
        }
        while let Ok(end) = lock(&self.cell_ends).try_recv() {
            if end.lane != crate::lane::MAIN {
                continue;
            }
            let (status, summary) = match &end.outcome {
                Ok(result) => (
                    if result.status == kernel::Status::Ok {
                        "completed"
                    } else {
                        "failed"
                    },
                    [result.stdout.as_str(), result.result.as_deref().unwrap_or_default()].join(" "),
                ),
                Err(error) => ("failed", error.clone()),
            };
            lines.push(runtime_event_line(
                "cell_done",
                &end.cell,
                status,
                &summary,
                "await rlm.cells()",
            ));
        }
        lines
    }

    /// Whether work of the root lane is still running: subagents, or detached cells.
    fn running(&self) -> bool {
        if !self.delegation.running(crate::lane::MAIN).is_empty() {
            return true;
        }
        self.lanes
            .request("rlm.cells", Map::new(), None)
            .ok()
            .and_then(|v| v["cells"].as_array().cloned())
            .is_some_and(|cells| {
                cells
                    .iter()
                    .any(|c| c["lane"] == crate::lane::MAIN && c["detached"] == true)
            })
    }

    /// The next completion message: at once when one waits, else after waiting (until `signal` aborts) while work
    /// is running; none when nothing runs.
    pub fn next_message(&self, signal: &ai::AbortSignal) -> Vec<Message> {
        if !self.enabled {
            return Vec::new();
        }
        loop {
            let lines = self.pending_lines();
            if !lines.is_empty() {
                let text = lines.join("\n");
                return vec![Message::User(UserMessage {
                    content: UserMessageContent::Blocks(vec![crate::lane::text_block(text)]),
                    timestamp: crate::lane::now_ms(),
                })];
            }
            if signal.aborted() || !self.running() {
                return Vec::new();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Whether a value lists exactly the `rlm` tool: the invariant every request body keeps.
pub fn only_rlm(tools: &[Value]) -> bool {
    tools.len() == 1 && tools[0]["name"] == json!("rlm")
}
