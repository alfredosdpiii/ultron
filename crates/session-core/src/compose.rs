//! The composition root of a session's host stack. A cell's host request passes, outermost first:
//!
//! 1. [`kernel_apis::KernelApis`]: the kernel-API modules (`skills.*` with code skills, `memory.*`,
//!    `hints.*`, `images.*`, and the local services of [`services`]: `refinements.*`, `experiments.*`, `gates.*`),
//!    matched by prefix;
//! 2. [`inference::Inference`]: `rlm.infer`, `rlm.load`, `rlm.frames`;
//! 3. [`Router`]: subagents (`rlm.spawn`, `rlm.collect`, ... served by [`delegation::host::Delegation`] for the
//!    calling lane, in host collect mode: a cell never waits); the lane API (`rlm.lanes`, `rlm.lane.*`, `rlm.cells`, `rlm.cell.cancel`, `rlm.recovery`) is answered by the
//!    session's REPL lanes ([`delegation::lanes::Lanes`]) first;
//! 4. [`host::Host`], sandboxed ([`host::Host::sandboxed`] with the `sandbox.*` settings) over the
//!    session's file hooks (Loki): shell, jobs and the before-write check.
//!
//! Each lane's kernel runs inside the session's shell sandbox (`KernelOptions::sandbox`), with `ULTRON_FILE_HOOKS=1`
//! (its writes ask the host first), a per-session spill dir, a signed snapshot under the profile's `rlm-snapshots/<session>/` with the profile
//! snapshot key, and the continuity policy (checkpoint after each cell, restore on restart). Subagent and detached
//! cell completions continue the run between turns ([`RuntimeEvents`]). The model sees one tool, `rlm`.

use crate::RunEnv as Env;
use crate::lane::{Provider, Rlm, rlm_over};
use crate::session_stats::{GuardOutcome, SessionStats};
use agent::{AgentOptions, AgentTool, StreamFn};
use ai::{Message, Model, UserMessage, UserMessageContent};
use delegation::agents::{Agents, AgentsOptions, Completion, PredictCall, predict_adapter};
use delegation::child::agent_runner;
use delegation::host::{ChildRun, ChildRunner, CollectMode, Delegation, DelegationOptions, DetachedEnd, Park};
use delegation::lanes::{CellEnd, Lanes, LanesOptions};
use delegation::usage::UsageLedger;
use host::Host;
use host::file_hooks::{FileHooks, GuardRecord, OnRecord};
use host::loki_guard::{LokiGuard, LokiMode, git_root, loki_mode};
use inference::Inference;
use kernel::{ContinuityPolicy, HostHandler, Kernel, KernelOptions, Signal};
use kernel_apis::{
    CodeSkills, CodeSkillsOptions, DurableWork, DurableWorkOptions, FileDocumentStore, GoalDriver, HintsModule,
    KernelApis, MemoryApi, MemoryBackend, MemoryService, SchedulesModule, SkillSource, SkillsModule, TaskGoalJobs,
};
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

/// What a cell's `ext.call` / `ext.list` (runtime.py's Pi extension functions) is answered.
pub const EXTENSIONS_UNSUPPORTED: &str = "Pi extensions are not supported by ultron-rs";

/// Told the session's MCP gateway when its stack opens.
pub type McpListener = Arc<dyn Fn(&Arc<mcp::gateway::Gateway>) + Send + Sync>;

/// Collaborators a caller may supply in place of the defaults (tests, embedders).
#[derive(Clone, Default)]
pub struct Services {
    /// The memory backend behind `memory.*`; none: the Hindsight server and bank this process resolves
    /// ([`hindsight::Endpoint`]), or memory off ("Hindsight is not configured") when that resolves to off.
    pub memory: Option<Arc<dyn MemoryBackend>>,
    /// Told the session's MCP gateway once its stack opens (the session worker serves `/mcp` with it).
    pub on_mcp: Option<McpListener>,
    /// Where the gateway's notices go when the session starts (configuration errors, an eager server that needs
    /// sign-in): the attached client's UI (`ctx.ui.notify`). None: nowhere, as without a UI in TypeScript.
    pub mcp_notices: Option<mcp::gateway::NoticeSink>,
    /// `--system-prompt`, `--no-skills`, `--no-context-files` (crate::session_flags).
    pub session: crate::session_flags::SessionFlags,
    /// The local skills and prompts (crate::session_resources), resolved by [`Runtime::new`].
    pub resources: Arc<crate::session_resources::SessionResources>,
    /// The session whose values the module stores are (`ultron.module/<name>`, `ultron.local/<key>`, the memory
    /// journal), set by the session actor: resuming restores them and a tree fork copies them (A07). None: files
    /// under `rlm-modules/<session id>/` (no session).
    pub values: Option<services::SessionValues>,
    /// Told the session's active schedules (`schedule.*`, kernel_apis::prompt_schedule) after every change: the
    /// session worker reports them to its server, which fires them.
    pub on_schedules: Option<kernel_apis::prompt_schedule::Listener>,
}

/// What every lane of this process shares: settings, locations, guards and the snapshot key. Owned
/// values only, so subagent lanes can be built on their own threads.
pub struct Config {
    pub cwd: PathBuf,
    pub home: PathBuf,
    pub agent_dir: PathBuf,
    /// The merged settings (`sandbox.*`, `loki`, ...).
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
    /// The frame settings `rlm.map` / `rlm.infer` read from the environment (`ULTRON_RLM_FRAME_MODEL`,
    /// `ULTRON_REVIEW_MODEL`, `ULTRON_RLM_MAP_TOKENS`).
    pub frame_env: HashMap<String, String>,
    /// The process environment, for models.json keys and headers (`$VAR` values).
    pub var: crate::VarLookup,
    /// Cell output masking for every lane: the values MCP servers resolve (env, headers) are registered here, as
    /// TypeScript's process-wide `registerSecretValues`.
    pub masker: Arc<host::secrets::OutputMasker>,
    /// Whether the project's own configuration (`.pi/mcp.json`) may be read.
    pub project_trusted: bool,
    /// The current key of a `/login` provider, for MCP servers with `auth.provider`.
    pub provider_token: mcp::connection::ProviderToken,
    /// Files Loki's own setup commit changed (crate::loki_notice): not the changes of the cell it ran beside.
    pub outside_changes: host::file_hooks::OutsideChanges,
}

/// After-cell checks picture the directory between cells (workspace-snapshot.ts).
pub fn workspace_snapshotter() -> host::file_hooks::Snapshotter {
    std::sync::Arc::new(|dir: &Path| {
        let picture =
            delegation::workspace::snapshot_workspace(dir, delegation::workspace::DEFAULT_MAX_SNAPSHOT_ENTRIES, true);
        Ok(host::file_hooks::Snapshot {
            root: picture.root,
            entries: picture.entries,
            complete: picture.complete,
        })
    })
}

/// Variables a session's commands and kernels receive from the session's environment even when this process has
/// none of that name (the session's lookup cannot be listed): the shell's essentials and the kernel's own settings.
const SESSION_ENV_NAMES: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "TMPDIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    "PYTHONPATH",
    "PYTHONHOME",
    "VIRTUAL_ENV",
    "PI_OFFLINE",
    "PI_SKIP_VERSION_CHECK",
    "ULTRON_PYTHON",
    "ULTRON_RLM_ENV_ALLOW",
    "ULTRON_BASH_YIELD_AFTER",
    "ULTRON_RLM_MAX_MEMORY_MB",
    "ULTRON_RLM_MAX_CPU_SECONDS",
    "ULTRON_RLM_OUTPUT_BYTES",
    "ULTRON_RLM_MAX_TREE_MEMORY_MB",
    "ULTRON_RLM_TREE_MEMORY_BACKEND",
];

/// The environment a session's kernels and commands start from (TS: the worker's `process.env`, which kernel.ts
/// `kernelEnvironment` filters and `getShellEnv` extends): every name this process has plus [`SESSION_ENV_NAMES`],
/// each with the value the session's `var` gives it (dropped when it gives none). Values come only from the session's
/// environment, never from this process's: a session whose PATH differs runs its tools from its own PATH.
pub fn session_environment(var: &dyn Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    let mut names: Vec<String> = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .collect();
    for name in SESSION_ENV_NAMES {
        if !names.iter().any(|known| known == name) {
            names.push((*name).to_string());
        }
    }
    names
        .into_iter()
        .filter_map(|name| var(&name).map(|value| (name, value)))
        .collect()
}

impl Config {
    /// [`session_environment`] of this session.
    pub fn child_env(&self) -> Vec<(String, String)> {
        session_environment(&*self.var)
    }

    /// The kernel environment and output budget a lane reads.
    pub fn from_env(env: &Env) -> Config {
        let agent_dir = env.agent_dir();
        // The worker's settings (`workerProjectTrusted`): a trusted project's `.pi/settings.json` applies too.
        let manager = config::settings::SettingsManager::for_worker(&env.cwd, &agent_dir, &env.home);
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
            snapshot_key: kernel::snapshot_store::session_snapshot_key(&agent_dir, &*env.var),
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
            frame_env: ["ULTRON_RLM_FRAME_MODEL", "ULTRON_REVIEW_MODEL", "ULTRON_RLM_MAP_TOKENS"]
                .into_iter()
                .filter_map(|key| (env.var)(key).map(|value| (key.to_string(), value)))
                .collect(),
            var: env.var.clone(),
            masker: Arc::default(),
            project_trusted: manager.is_project_trusted(),
            outside_changes: host::file_hooks::OutsideChanges::default(),
            provider_token: {
                let env = env.clone();
                Arc::new(move |provider: &str| crate::lane::api_key(&crate::ModelFlags::default(), &env, provider))
            },
        }
    }

    /// Reads the settings as they are now, with the worker's project trust (fixed when the session opened): what the
    /// TypeScript worker reads from its live `SettingsManager` per frame, spawn or request.
    pub fn settings_reader(&self) -> Arc<dyn Fn() -> config::settings::SettingsManager + Send + Sync> {
        let (cwd, agent_dir, trusted) = (self.cwd.clone(), self.agent_dir.clone(), self.project_trusted);
        Arc::new(move || config::settings::SettingsManager::create(&cwd, &agent_dir, trusted))
    }

    /// The bundled skills directory (packages/coding-agent/skills, beside the runtime), unless
    /// `ULTRON_BUNDLED_SKILLS` turns it off.
    pub fn bundled_skills(&self) -> Option<String> {
        let root = self.runtime_path.ancestors().nth(4)?;
        crate::session_resources::bundled_skills(&root.join("skills"), &*self.var)
    }

    /// The session's MCP gateway (TypeScript's extensions/mcp, built in): the servers of `mcp.json`, and of `.pi/mcp.json` in a trusted
    /// project, reached from the REPL through the `mcp` tool. Server log messages go to `<agentDir>/mcp.log`.
    pub fn mcp_gateway(&self) -> Arc<mcp::gateway::Gateway> {
        let masker = self.masker.clone();
        mcp::gateway::Gateway::new(mcp::gateway::GatewayOptions {
            agent_dir: self.agent_dir.clone(),
            project_trusted: self.project_trusted,
            servers: None,
            env: mcp::connection::ConnectionEnv {
                cwd: self.cwd.clone(),
                home: self.home.clone(),
                env_overlay: HashMap::new(),
                client_version: mcp::gateway::client_version(),
                log: Some(Arc::new(mcp::log::ServerLog::new(self.agent_dir.join("mcp.log")))),
                secrets: Some(Arc::new(move |values: &[String]| {
                    let values: Vec<Value> = values.iter().map(|v| Value::String(v.clone())).collect();
                    masker.register_secret_values(&values);
                })),
                provider_token: Some(self.provider_token.clone()),
                // The worker's proxies, with undici's semantics (TS: the worker's `configureHttpDispatcher`).
                proxy: Some(ai::net::ProxyEnv::from_lookup(&*self.var)),
                var: Some(self.var.clone()),
                ..mcp::connection::ConnectionEnv::default()
            },
            // The session's PATH finds the launcher (never the process's).
            open_url: Some(mcp::gateway::browser_opener_with(
                (self.var)("PATH"),
                (self.var)("HOME"),
                mcp::gateway::opener_vars(&*self.var),
            )),
        })
    }

    /// The model `provider`/`id` of the built-in catalog or models.json, ready for requests.
    pub fn model(&self, provider: &str, id: &str) -> Option<Model> {
        let env = Env {
            cwd: self.cwd.clone(),
            home: self.home.clone(),
            var: self.var.clone(),
            docs: PathBuf::new(),
        };
        crate::lane::catalog_model(provider, id, &env).ok()
    }

    /// `rlm.find_models` (session-worker.ts): the available models (`registry.getAvailable()`) as
    /// `{provider, id, name}`, whatever the query.
    pub fn find_models(&self) -> Value {
        let env = Env {
            cwd: self.cwd.clone(),
            home: self.home.clone(),
            var: self.var.clone(),
            docs: PathBuf::new(),
        };
        Value::Array(
            crate::lane::available_models(&env)
                .iter()
                .map(|m| json!({"provider": m.provider, "id": m.id, "name": m.name}))
                .collect(),
        )
    }

    /// Refuses a spawn's `model=` unless it names a model of the built-in catalog or models.json.
    fn model_check(self: &Arc<Self>) -> delegation::host::ModelCheck {
        let config = self.clone();
        Arc::new(move |spec: &str| {
            match spec
                .split_once('/')
                .and_then(|(provider, id)| config.model(provider, id))
            {
                Some(_) => Ok(()),
                None => Err(format!("model must be provider/model naming a known model, not {spec}")),
            }
        })
    }

    /// The session's file hooks (session-worker.ts): Loki's guard unless `ULTRON_LOKI` or the global `loki.mode`
    /// setting turns it off.
    /// In `cwd` (a worktree subagent's own), Loki checks that tree. Every guard outcome goes to `ULTRON_LOKI_LOG`, then
    /// to `record` (the session's counters).
    fn file_hooks_in(&self, cwd: &Path, record: Option<OnRecord>) -> FileHooks {
        let mut hooks = FileHooks::with_snapshots(cwd, workspace_snapshotter());
        hooks.ignore_outside(self.outside_changes.clone());
        match (
            crate::guard_log::loki_log((self.var)("ULTRON_LOKI_LOG").as_deref()),
            record,
        ) {
            (Some(log), Some(record)) => hooks.on_record(Arc::new(move |outcome: &GuardRecord| {
                log(outcome);
                record(outcome);
            })),
            (Some(only), None) | (None, Some(only)) => hooks.on_record(only),
            (None, None) => {}
        }
        if let Some(root) = &self.loki_root {
            let root = if cwd == self.cwd {
                root.clone()
            } else {
                git_root(cwd).unwrap_or_else(|| cwd.to_path_buf())
            };
            // Post-write checks compare against the committed policy and a base revision: only in a repository
            // with a `.loki/` policy (TS: after its auto-install there).
            let post_write = git_root(cwd).is_some() && root.join(".loki").is_dir();
            hooks.add(Box::new(
                LokiGuard::new(&root, self.loki)
                    .with_post_write(post_write)
                    .with_project_python(crate::guard_log::project_python(&*self.var, &self.cwd))
                    .with_timeout(host::loki_guard::loki_timeout(
                        (self.var)("ULTRON_LOKI_TIMEOUT_MS").as_deref(),
                    )),
            ));
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

/// Live kernels a session keeps (`maxLive` of the TS rlm tool, 16); `rlm.maxLiveKernels` overrides it.
fn max_live_kernels(settings: &Value) -> usize {
    settings["rlm"]["maxLiveKernels"]
        .as_u64()
        .filter(|n| *n >= 1)
        .map_or(16, |n| n.min(1024) as usize)
}

/// Where each lane's kernel snapshot lives (the path its kernel restores at startup): the pool snapshots an idle
/// kernel there before evicting it.
fn lane_snapshot_path(config: &Arc<Config>, session_id: &str) -> delegation::lanes::SnapshotPathFn {
    let snapshots = config.session_dir("rlm-snapshots", session_id);
    Arc::new(move |lane: &str| {
        Some(
            snapshots
                .join(format!("{}.snapshot", lane_dir(lane)))
                .display()
                .to_string(),
        )
    })
}

/// The runtime guide's view of the `mcp` gateway: the tool and its servers when `mcp.json` names any.
fn mcp_runtime_guide(config: &Config) -> resources::runtime_guide::RuntimeGuideOptions {
    let tool = config.mcp_gateway().tool();
    resources::runtime_guide::RuntimeGuideOptions {
        mcp_servers: tool
            .as_ref()
            .map(|t| mcp::gateway::server_names_of(&t.description))
            .unwrap_or_default(),
        // The REPL's only `tools.*` tool is the gateway (TypeScript lists it among its extension tools): without
        // `mcp.json` servers there is none, and the guide has no tools section.
        extension_tools: tool
            .into_iter()
            .map(|t| resources::runtime_guide::ExtensionToolSummary {
                name: t.name,
                description: t.description,
            })
            .collect(),
        // The session runtime serves `rlm.ask`, `swarm`, `board`, `notes` (swarm_ops.rs) and typed memory: the model
        // is told, unless `ULTRON_GUIDE=ts` asks for the TypeScript guide (the TS-recorded request replays use it).
        durable_apis: (config.var)("ULTRON_GUIDE").as_deref() != Some("ts"),
        ..Default::default()
    }
}

/// The root system prompt, rendered again only when the settings' [`crate::lane::PromptMode`] changes (engineering
/// mode, `alwaysSkills`), so the prompt-cache prefix stays stable (session-worker.ts `currentSystemPrompt`). A
/// change applies from the next request.
#[derive(Clone)]
pub struct SystemPrompt(Arc<PromptState>);

type RenderPrompt = dyn Fn(&crate::lane::PromptMode) -> Result<(String, Vec<String>), String> + Send + Sync;

struct PromptState {
    /// `None`: a fixed prompt.
    render: Option<Box<RenderPrompt>>,
    mode: Box<dyn Fn() -> crate::lane::PromptMode + Send + Sync>,
    current: Mutex<(crate::lane::PromptMode, String, Vec<String>)>,
}

impl SystemPrompt {
    /// A prompt that never changes.
    pub fn fixed(text: impl Into<String>) -> SystemPrompt {
        SystemPrompt(Arc::new(PromptState {
            render: None,
            mode: Box::new(crate::lane::PromptMode::default),
            current: Mutex::new((Default::default(), text.into(), Vec::new())),
        }))
    }

    /// The native worker's prompt for `env`'s working directory, in the mode its settings name now (read with the
    /// worker's project trust).
    fn rendered(
        env: &Env,
        config: &Config,
        trusted: bool,
        guide: resources::runtime_guide::RuntimeGuideOptions,
        flags: &crate::session_flags::SessionFlags,
        extra: Arc<crate::session_resources::SessionResources>,
    ) -> Result<SystemPrompt, String> {
        let (cwd, agent_dir) = (env.cwd.clone(), config.agent_dir.clone());
        let (no_skills, no_context_files) = (flags.no_skills, flags.no_context_files);
        let mode = move || {
            let settings = config::settings::SettingsManager::create(&cwd, &agent_dir, trusted);
            crate::lane::PromptMode {
                no_skills,
                no_context_files,
                ..crate::lane::PromptMode::from_settings(&settings)
            }
        };
        let (env, loki_root) = (env.clone(), config.loki_root.clone());
        let render = move |mode: &crate::lane::PromptMode| {
            crate::lane::system_prompt_with_resources(
                &env,
                &env.cwd.display().to_string(),
                loki_root.as_deref(),
                &guide,
                mode,
                &extra,
            )
        };
        let initial = mode();
        let (text, warnings) = render(&initial)?;
        Ok(SystemPrompt(Arc::new(PromptState {
            render: Some(Box::new(render)),
            mode: Box::new(mode),
            current: Mutex::new((initial, text, warnings)),
        })))
    }

    /// The prompt in the settings' current mode. A mode whose prompt does not render keeps the last one.
    pub fn current(&self) -> String {
        let state = &self.0;
        let Some(render) = &state.render else {
            return lock(&state.current).1.clone();
        };
        let mode = (state.mode)();
        let mut current = lock(&state.current);
        if current.0 != mode
            && let Ok((text, warnings)) = render(&mode)
        {
            *current = (mode, text, warnings);
        }
        current.1.clone()
    }

    /// The `alwaysSkills` problems of the prompt as last rendered.
    pub fn warnings(&self) -> Vec<String> {
        lock(&self.0.current).2.clone()
    }
}

/// The process-wide composition: configuration and injected services.
pub struct Runtime {
    pub config: Arc<Config>,
    pub services: Services,
    /// Problems found building the session (the `alwaysSkills` warnings of the system prompt), reported once.
    pub diagnostics: Vec<String>,
    /// The root lane's system prompt (subagents use it too).
    pub system: SystemPrompt,
    /// Where session-scoped host files (job logs, inference context) live: the cwd's session directory.
    pub sessions_dir: PathBuf,
    /// `print`, `json`, `rpc` or `interactive`.
    pub mode: String,
    /// The Hindsight server and bank every instance shares (`ULTRON_HINDSIGHT_URL`, the global `hindsightUrl`
    /// setting, else localhost:8888; `ULTRON_HINDSIGHT_BANK`, else "ultron"); `None`: memory is off.
    pub hindsight: Option<hindsight::Endpoint>,
}

impl Runtime {
    /// Read the settings and the local resources for `cwd` (`mode`: "print", "json" or "rpc") and build the root
    /// system prompt.
    pub fn new(env: &Env, mode: &str, sessions_dir: PathBuf, services: Services) -> Result<Runtime, String> {
        let mut services = services;
        let config = Config::from_env(env);
        // The worker's project trust (`workerProjectTrusted`, decided by Config).
        let trusted = config.project_trusted;
        let manager = config::settings::SettingsManager::create(&env.cwd, &config.agent_dir, trusted);
        // resource-loader.ts `reload`: the local skills and prompts.
        let resolved = crate::session_resources::resolve(
            env,
            config::settings::SettingsManager::create(&env.cwd, &config.agent_dir, trusted),
        );
        services.resources = Arc::new(resolved);
        let mut diagnostics: Vec<String> = Vec::new();
        let hindsight = hindsight::Endpoint::resolve(&|key| (env.var)(key), hindsight::resolve::setting_url(&manager));
        // The `mcp` gateway lives in the REPL: the runtime guide lists it with its servers (`mcpServerNames`).
        let runtime_guide = mcp_runtime_guide(&config);
        let system = match &services.session.system_prompt {
            // `--system-prompt`: exactly the given text (`currentSystemPrompt`).
            Some(text) => SystemPrompt::fixed(text.clone()),
            None => SystemPrompt::rendered(
                env,
                &config,
                trusted,
                runtime_guide,
                &services.session,
                services.resources.clone(),
            )?,
        };
        diagnostics.extend(system.warnings());
        Ok(Runtime {
            config: Arc::new(config),
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
        self.stack_with(model, api_key, session_id, None)
    }

    /// [`Runtime::stack`] whose subagents run as `subagents` says (default: each on its agent loop, with an
    /// in-memory task journal).
    pub fn stack_with(
        &self,
        model: &Model,
        api_key: Option<String>,
        session_id: &str,
        subagents: Option<SubagentHost>,
    ) -> Result<Arc<Stack>, String> {
        let mut subagents = subagents;
        // Without a subagent host, children run as in-memory agent loops, steerable by the agent hub and measured
        // for their context reminder (crate::memory_children).
        let memory_children = subagents
            .is_none()
            .then(|| Arc::new(crate::memory_children::MemoryChildren::default()));
        let config = self.config.clone();
        // Shell jobs are durable: their journal is in the session's module directory, and opening the host recovers
        // the jobs a dead worker left (from their processes).
        let values = self.services.values.clone();
        let jobs: Arc<dyn host::journal::JobJournal> = match &values {
            Some(values) => Arc::new(crate::session_modules::ModuleJobJournal(
                crate::session_modules::ModuleDocument(
                    values.document(crate::session_modules::MODULE_NAMESPACE, "jobs"),
                ),
            )),
            None => Arc::new(host::journal::FileJobJournal::new(
                config.session_dir("rlm-modules", session_id).join("jobs.json"),
            )),
        };
        // The session's runtime counters (`ultron.module/stats`): saved once the session actor attaches its journal.
        let stats = Arc::new(SessionStats::system("lane"));
        // Typed memory records (kernel_apis::memory_records) are shared by every session of the agent directory;
        // a cell's file changes that match one point at it with the next result (kernel_apis::memory_pointers).
        let memory_records = Arc::new(kernel_apis::memory_records::RecordIndex::for_agent_dir(
            &config.agent_dir,
        ));
        let pointers = self.memory_pointers(session_id, &memory_records);
        let host = Arc::new(
            self.host_in(&config.cwd, session_id, "bash", Some(&stats), pointers)?
                .with_journal(jobs)?,
        );
        crate::guard_log::loki_setup(&config);
        // `createSessionModuleStore(session, name)`: the session value `ultron.module/<name>`.
        let modules = |name: &str| -> Arc<dyn kernel_apis::DocumentStore> {
            match &values {
                Some(values) => Arc::new(crate::session_modules::ModuleDocument(
                    values.document(crate::session_modules::MODULE_NAMESPACE, name),
                )),
                None => Arc::new(FileDocumentStore::new(
                    config
                        .session_dir("rlm-modules", session_id)
                        .join(format!("{name}.json")),
                )),
            }
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
                MemoryService::new(
                    match &values {
                        Some(values) => Arc::new(crate::session_modules::ModuleDocument(
                            values.document("ultron.memory.state", "root"),
                        )),
                        None => modules("memory"),
                    },
                    backend.clone(),
                    kernel_apis::worker_gate(),
                )
                .map_err(|e| format!("memory: {e:?}"))?
                .with_records(memory_records.clone()),
            ),
            None => None,
        };
        let skills_config = config.clone();
        // `--no-skills`: the catalog is empty.
        let no_skills = self.services.session.no_skills;
        let extra = self.services.resources.clone();
        let loader: kernel_apis::SkillLoader = Arc::new(move || Ok(skill_sources(&skills_config, &extra, no_skills)));
        // Skill test kernels start from the session's environment, as the lanes' kernels do.
        let code = CodeSkills::new(CodeSkillsOptions::for_session(
            &config.skills_dir,
            &config.runtime_path,
            config.child_env(),
        ));
        let clock = kernel_apis::util::system_clock();
        let artifacts = kernel_apis::artifacts::Artifacts::new(
            Box::new(kernel_apis::artifacts::FileKeyedDocuments::new(
                config.session_dir("rlm-artifacts", session_id).join("artifacts.json"),
            )),
            kernel_apis::artifacts::ArtifactLimits::default(),
        )?;
        // The local services: refinements (lessons, checked by the worker's activation rules) and experiment history in
        // one keyed document file, release gates in their module document. Active instruction refinements reach
        // later typed-agent prompts through the host's refinement hook.
        let local = Arc::new(services::LocalServices::worker(match &values {
            Some(values) => Box::new(values.keyed("ultron.local")),
            None => Box::new(kernel_apis::artifacts::FileKeyedDocuments::new(
                config.session_dir("rlm-modules", session_id).join("local.json"),
            )),
        }));
        // Refinements follow the conversation branch (`mainBranch`): anchored at the main lane's tip, read back only
        // while the anchor is on the branch in use (A07).
        let branch = values.as_ref().map(crate::session_modules::main_branch);
        let refinements = services::instruction_hook_on(local.clone(), branch.clone());
        // `session_start` of the MCP extension: eager servers connect in the background; configuration errors were
        // reported with the runtime's diagnostics.
        let gateway = config.mcp_gateway();
        let notices = gateway.start(self.services.mcp_notices.clone());
        if let Some(notify) = &self.services.mcp_notices {
            for (message, level) in notices {
                notify(&message, &level);
            }
        }
        if let Some(on_mcp) = &self.services.on_mcp {
            on_mcp(&gateway);
        }
        let apis = ApiModules {
            local: match branch {
                Some(branch) => Arc::new(services::BranchedLocalServices {
                    services: local,
                    branch,
                }),
                None => Arc::new(services::LocalServicesModule { services: local }),
            },
            gates: Arc::new(services::ReleaseGateModule::new(modules("release-gates"), None)),
            artifacts: Arc::new(kernel_apis::artifacts::ArtifactsModule::new(artifacts)),
            skills: Arc::new(SkillsModule::new(modules("skills"), loader, clock.clone(), Some(code))),
            memory: Arc::new(MemoryApi { service: memory }),
            hints: Arc::new(HintsModule::new(modules("hints"))),
            images: Arc::new(crate::session_images::module(&config, session_id)),
            // `schedule.*`: the session's own prompts for later, kept as session values.
            schedule: Arc::new(kernel_apis::prompt_schedule::ScheduleModule::new(
                match &values {
                    Some(values) => Box::new(values.keyed(kernel_apis::prompt_schedule::NAMESPACE)),
                    None => Box::new(kernel_apis::artifacts::MemoryKeyedDocuments::default()),
                },
                session_id,
                kernel_apis::prompt_schedule::clock_for(&*config.var),
                self.services.on_schedules.clone(),
            )),
            // `tools.*`: the `mcp` gateway called from the REPL, detached after `yield_after`.
            tools: Arc::new(kernel_apis::ToolCalls::new(
                gateway.tool().is_some().then(|| {
                    Arc::new(crate::session_tools::SessionTools { mcp: gateway.clone() })
                        as Arc<dyn kernel_apis::ToolRunner>
                }),
                modules("tool-calls"),
            )),
            // The task modules of session-worker.ts over the typed-agent host: retained instances (their lanes
            // pinned in the kernel pool), progress with verifier runs, and family messages.
            instances: Arc::new(kernel_apis::InstancesModule::new(modules("instances"), None)),
            progress: Arc::new(kernel_apis::ProgressModule::new(modules("progress"))),
            family: Arc::new(kernel_apis::FamilyModule::new(
                modules("family"),
                kernel_apis::FamilyOptions::default(),
            )),
        };
        // Situational hints (`ULTRON_HINTS=on`): what a cell did, observed per lane, ends its result with one line.
        let var = |key: &str| (config.var)(key);
        let context_reminders = Arc::new(crate::context_reminder::ContextReminders::new(
            crate::context_reminder::ReminderSettings::from_vars(
                var("ULTRON_CONTEXT_REMINDER").as_deref(),
                var("ULTRON_CONTEXT_REMINDER_TOKENS").as_deref(),
            ),
        ));
        let cell_hints = Arc::new(kernel_apis::cell_hints::CellHints::new(
            apis.hints.clone(),
            kernel_apis::cell_hints::CellHintsOptions {
                enabled: kernel_apis::cell_hints::hints_enabled(var("ULTRON_HINTS").as_deref()),
                max_per_tag: kernel_apis::cell_hints::hint_max_per_tag(var("ULTRON_HINTS_MAX").as_deref()),
                async_events: config.async_events,
                read_handle_bytes: kernel_apis::cell_hints::read_handle_bytes(
                    var("ULTRON_READ_HANDLE_BYTES").as_deref(),
                ),
                ..Default::default()
            },
        ));
        let session_id = session_id.to_string();
        let system = self.system.clone();
        let sessions_dir = self.sessions_dir.clone();
        let services = self.services.clone();
        let model = model.clone();
        let child = ChildDefaults {
            model: model.clone(),
            api_key: api_key.clone(),
        };
        let mut built: Result<(), String> = Ok(());
        let (schedules_store, goal_store) = (modules("schedules"), modules("goal"));
        let images = Arc::new(crate::view_images::ViewImages::new(
            model.clone(),
            config.cwd.clone(),
            config.agent_dir.clone(),
            config.project_trusted,
        ));
        let stack = Arc::new_cyclic(|me: &Weak<Stack>| {
            let runner_stack = me.clone();
            let (store, inner, agent_stores) = match subagents.take() {
                Some(host) => (Some(host.store), (host.runner)(me.clone()), host.agents),
                None => {
                    let factory: delegation::child::ChildAgentFactory =
                        Arc::new(move |run: &ChildRun| child_agent(&runner_stack, &child, run));
                    let runner = match &memory_children {
                        Some(children) => children.runner(factory),
                        None => agent_runner(factory),
                    };
                    (None, runner, None)
                }
            };
            let goal_slot: Arc<std::sync::OnceLock<Arc<GoalDriver>>> = Arc::new(std::sync::OnceLock::new());
            // Bounded-inference frames: traces as session values (in memory without a session), run as
            // `rlm-frame@1` tasks of the typed-agent host.
            let frames = inference::FrameSession::new(match &agent_stores {
                Some(stores) => stores.frames.clone(),
                None => Arc::new(inference::MemoryTraceStore::default()),
            });
            let (agents, usage, task_ends) = typed_agents(
                &config,
                agent_stores,
                me.clone(),
                &model,
                api_key.clone(),
                goal_slot.clone(),
                Some(refinements),
                crate::frames::frame_runner(frames.clone()),
                &stats,
            );
            frames.set_tasks(Arc::new(crate::frames::StackFrameTasks {
                agents: agents.clone(),
                usage: usage.clone(),
                stack: me.clone(),
            }));
            // Schedules and the session goal are durable tasks of the typed-agent host.
            let schedules = Arc::new(SchedulesModule::new(schedules_store, None));
            let goal = Arc::new(GoalDriver::new(
                goal_store,
                Arc::new(TaskGoalJobs::new(Arc::new(agents.clone()), host.clone())),
                kernel_apis::util::system_clock(),
            ));
            let _ = goal_slot.set(goal.clone());
            let closer = me.clone();
            // A subagent's lane closes when its run ends.
            let runner: ChildRunner = Arc::new(move |run: ChildRun| {
                let lane = run.lane_name.clone();
                let result = inner(run);
                if let Some(stack) = closer.upgrade() {
                    let _ = stack.lanes.close_lane(&lane);
                    stack.retire_workspace(&lane);
                }
                result
            });
            let mut options = DelegationOptions::new(runner);
            if let Some(store) = store {
                options.store = store;
            }
            // A cell never waits for subagents: rlm.collect returns what has ended, and the host waits between
            // turns, delivering each end as a child_done event.
            options.collect = CollectMode::Host;
            options.spawn_depth = config.spawn_depth;
            options.session_id = session_id.clone();
            options.workspace = Some(config.cwd.clone());
            options.now = Box::new(|| crate::lane::now_ms() as f64);
            // `ULTRON_KEEP_WORKTREES`: worktrees and branches stay after merges, on close and at startup.
            options.keep_worktrees = delegation::worktrees::keep_worktrees(
                (config.var)(delegation::worktrees::KEEP_WORKTREES_ENV).as_deref(),
            );
            let (delegation, child_ends) = Delegation::new(options);
            // Read per spawn, as session-worker.ts passes them: `rlm.childModel` (/settings -> Models) and the
            // `worktrees` setting, so a changed setting applies to the next subagent.
            let settings = config.settings_reader();
            delegation.set_child_model_source(Arc::new(move || settings().rlm_model_settings().child_model));
            let settings = config.settings_reader();
            delegation.set_worktree_setup_source(Arc::new(move || {
                let configured = settings().worktree_settings();
                Some(delegation::worktrees::WorktreeSetup {
                    link: configured.link,
                    copy: configured.copy,
                    command: configured.command,
                    timeout_ms: None,
                })
            }));
            // Worktrees of sessions that ended without cleaning up (a crash): pruned in the background, their work
            // kept on its branch (native-host.ts `loadTasks`).
            let pruning = delegation.clone();
            std::thread::spawn(move || pruning.prune_orphan_worktrees());
            // How each worktree child's merge ended is counted (`onMerge`).
            let merges = stats.clone();
            delegation.on_merge(Arc::new(move |task: &str, status: &str| merges.merge(task, status)));
            // Workflow nodes with worktree=True run as subagents of the workflow's caller (worktrees live there).
            agents.set_worktree_children(Arc::new(delegation.clone()));
            // A spawn naming a model the session cannot run is refused, never run on the root's model (A04).
            delegation.set_model_check(config.model_check());
            // Spawn contract `run` checks run under the session's shell sandbox, in the child's working directory.
            delegation.set_check_runner(crate::contract_checks::check_runner(me.clone()));
            // Contract baselines run git with the session's environment (its PATH finds git).
            delegation.set_command_env(config.child_env());
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
                // Live kernels are bounded as the TS session's rlm tool bounds them (A43): an idle lane is
                // snapshotted to its own snapshot path and evicted, and its next cell restores it.
                pool: Some(delegation::lanes::PoolLimits::new(
                    max_live_kernels(&config.settings),
                    Some(lane_snapshot_path(&config, &session_id)),
                )),
            });
            let (lanes, cell_ends) = match lanes {
                Ok(pair) => {
                    // Every host request a kernel makes is counted; `rlm.pool`, inspected or asked by a cell, carries
                    // the session's guard statistics.
                    let calls = stats.clone();
                    pair.0
                        .observe_requests(Arc::new(move |kind: &str| calls.host_call(kind)));
                    let guards_of = me.clone();
                    pair.0.extend_pool_stats(Arc::new(move |pool: &mut Value| {
                        if let Some(stack) = guards_of.upgrade() {
                            let guards = stack.guard_stats();
                            if !guards.is_empty() {
                                pool["guards"] = Value::Array(guards);
                            }
                        }
                    }));
                    pair
                }
                Err(error) => {
                    built = Err(error);
                    let (lanes, ends) = Lanes::new(LanesOptions {
                        create: Arc::new(|_| Err("no lanes".into())),
                        handlers: Arc::new(|_, _, _| Arc::new(Closed) as Arc<dyn HostHandler>),
                        max_lanes: 1,
                        cancel_grace: Duration::ZERO,
                        pool: None,
                    })
                    .expect("one lane is a valid limit");
                    (lanes, ends)
                }
            };
            let events = Arc::new(RuntimeEvents {
                task_ends: Mutex::new(task_ends),
                child_ends: Mutex::new(child_ends),
                cell_ends: Mutex::new(cell_ends),
                delegation: delegation.clone(),
                lanes: lanes.clone(),
                enabled: config.async_events,
                mask: session_mask(&config),
            });
            Stack {
                config,
                mcp: gateway,
                host,
                delegation,
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
                cell_progress: Arc::new(Mutex::new(None)),
                agents,
                usage,
                frames,
                schedules,
                goal,
                work: std::sync::OnceLock::new(),
                cell_hints,
                context: std::sync::OnceLock::new(),
                images,
                swarm: std::sync::OnceLock::new(),
                budget_note: Arc::new(std::sync::OnceLock::new()),
                context_reminders,
                lane_steer: Arc::new(std::sync::OnceLock::new()),
                stats,
                retired_guards: Mutex::new(Vec::new()),
            }
        });
        built?;
        if let Some(children) = memory_children {
            children.attach_reminders(stack.context_reminders.clone());
            let _ = stack
                .lane_steer
                .set(Arc::new(move |lane: &str, message: &str| children.steer(lane, message)));
        }
        stack.lanes.open(crate::lane::MAIN)?;
        let work = DurableWork::open(
            DurableWorkOptions::new(Arc::new(stack.agents.clone()))
                .schedules(stack.schedules.clone())
                .goal(stack.goal.clone())
                .jobs(stack.host.clone()),
        )?;
        let _ = stack.work.set(work);
        Ok(stack)
    }

    /// The file hooks of a host in `cwd`: Loki's (see [`Config::file_hooks_in`]), with `observer` told each cell's
    /// changes.
    fn file_hooks_in(
        &self,
        cwd: &Path,
        stats: Option<&Arc<SessionStats>>,
        observer: Option<Arc<dyn host::file_hooks::CellObserver>>,
    ) -> FileHooks {
        let record = stats.map(|stats| {
            let stats = stats.clone();
            Arc::new(move |record: &GuardRecord| {
                stats.guard(&GuardOutcome {
                    guard: &record.guard,
                    phase: record.phase,
                    outcome: record.outcome,
                    ms: record.ms as f64,
                });
            }) as OnRecord
        });
        let hooks = self.config.file_hooks_in(cwd, record);
        if let Some(observer) = observer {
            hooks.observe(observer);
        }
        hooks
    }

    /// The session's memory pointers: on while memory is (an injected backend or a Hindsight server) and the
    /// `memory.filePointers` setting is not false; scoped by the session's identity tags.
    fn memory_pointers(
        &self,
        session_id: &str,
        records: &Arc<kernel_apis::memory_records::RecordIndex>,
    ) -> Option<Arc<dyn host::file_hooks::CellObserver>> {
        let config = &self.config;
        let filters = match (&self.services.memory, &self.hindsight) {
            (Some(backend), _) => backend.scope_tags(),
            (None, Some(_)) => hindsight::worker_scope_tags(
                session_id,
                &config.cwd.display().to_string(),
                &config.home.display().to_string(),
            ),
            (None, None) => return None,
        };
        kernel_apis::memory_pointers::session_pointers(&config.settings, records.clone(), filters)
    }

    /// A sandboxed host with its working directory at `cwd` (the session's, or a worktree subagent's); its guard
    /// outcomes are counted in `stats`.
    fn host_in(
        &self,
        cwd: &Path,
        session_id: &str,
        spill: &str,
        stats: Option<&Arc<SessionStats>>,
        observer: Option<Arc<dyn host::file_hooks::CellObserver>>,
    ) -> Result<Host, String> {
        let config = &self.config;
        let jobs_dir = self.sessions_dir.join("rlm-jobs").join(session_id);
        // Blocking-bash spill files stay with the session (TS: the OS temp dir), so the sandbox keeps only
        // session paths writable.
        let spill_dir = config.session_dir("rlm-output", session_id).join(spill);
        let tmp_dir = config.session_dir("rlm-tmp", session_id);
        for dir in [&spill_dir, &tmp_dir] {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        // `createLocalBashOperations({ shellPath: settingsManager.getShellPath() })`: the REPL's bash and its jobs.
        let shell_path = config
            .settings
            .as_object()
            .and_then(|settings| config::settings_keys::shell_path(settings, &config.home));
        Ok(Host::new(
            cwd,
            jobs_dir,
            spill_dir,
            Box::new(self.file_hooks_in(cwd, stats, observer)),
        )
        .with_shell_path(shell_path)
        // The session's environment, never the server's: its PATH finds the shell, the sandbox and the tools.
        .with_base_env(config.child_env())
        // Cell results carry the diff of what they wrote (crate::cell_file_changes), except in the TS-compatible
        // output the TS-recorded replays compare (`ULTRON_GUIDE=ts`).
        .recording_cell_writes((config.var)("ULTRON_GUIDE").as_deref() != Some("ts"))
        .sandboxed(&config.settings, &config.home, Some(&tmp_dir)))
    }
}

/// The skill catalog `skills.*` serves: the session's skills with their file contents.
fn skill_sources(
    config: &Config,
    extra: &crate::session_resources::SessionResources,
    no_skills: bool,
) -> Vec<SkillSource> {
    let cwd = config.cwd.display().to_string();
    let agent_dir = config.agent_dir.display().to_string();
    let bundled = config.bundled_skills();
    crate::session_resources::skills(&cwd, &agent_dir, extra, no_skills, bundled.as_deref())
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
    local: Arc<dyn kernel_apis::ApiModule>,
    gates: Arc<services::ReleaseGateModule>,
    artifacts: Arc<kernel_apis::artifacts::ArtifactsModule>,
    skills: Arc<SkillsModule>,
    memory: Arc<MemoryApi>,
    hints: Arc<HintsModule>,
    images: Arc<kernel_apis::ImagesModule>,
    schedule: Arc<kernel_apis::prompt_schedule::ScheduleModule>,
    tools: Arc<kernel_apis::ToolCalls>,
    instances: Arc<kernel_apis::InstancesModule>,
    progress: Arc<kernel_apis::ProgressModule>,
    family: Arc<kernel_apis::FamilyModule>,
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

/// One session's host stack: the sandboxed host, the REPL lanes, the subagent host and the kernel-API modules.
pub struct Stack {
    config: Arc<Config>,
    /// The session's MCP servers (closed with the session).
    mcp: Arc<mcp::gateway::Gateway>,
    pub host: Arc<Host>,
    pub delegation: Delegation,
    pub lanes: Lanes,
    pub events: Arc<RuntimeEvents>,
    apis: ApiModules,
    model: Model,
    api_key: Option<String>,
    session_id: String,
    sessions_dir: PathBuf,
    system: SystemPrompt,
    services: Services,
    workspaces: Mutex<HashMap<String, Workspace>>,
    /// The root lane kernels' sandbox, once built.
    kernel_sandbox: std::sync::OnceLock<Option<Arc<sandbox::Launcher>>>,
    /// The durable progress of the root lane's running cell: what its shell operations print is its partial
    /// output, kept if the worker dies before the cell ends.
    pub cell_progress: Arc<Mutex<Option<harness::chat::CellProgress>>>,
    /// The typed-agent host (`agents.*`, `background.*`, `workflows.run`) and its usage ledger.
    pub agents: Agents,
    pub usage: UsageLedger,
    /// The session's bounded-inference frames (`rlm.infer`, `rlm.map`, `rlm.frames`): budgets, traces, tasks.
    pub frames: Arc<inference::FrameSession>,
    /// `schedules.*` and the session goal (`goal.*`, `/goal`), on the root lane.
    pub schedules: Arc<SchedulesModule>,
    pub goal: Arc<GoalDriver>,
    /// Schedules, the goal and shell jobs as durable work: recovered when the stack opens, its tick running.
    pub work: std::sync::OnceLock<DurableWork>,
    /// Situational hints over every lane's cells (crate::lane::Rlm appends them).
    cell_hints: Arc<kernel_apis::cell_hints::CellHints>,
    /// Model-owned context of every lane (`ctx.*`; collapse on return on the root), once the session actor opened it.
    pub context: std::sync::OnceLock<Arc<kernel_apis::ContextControl>>,
    /// The images each lane's running cell attached with `view_image`, and the model each lane's results go to.
    pub images: Arc<crate::view_images::ViewImages>,
    /// `swarm.*`, `board.*`, `notes.*` and persistent children, once the session actor opened the durable runtime.
    pub swarm: std::sync::OnceLock<Arc<crate::swarm_ops::SwarmOps>>,
    /// The budget line every lane's cell results end with, once the session actor wired it (crate::budget_note).
    pub budget_note: Arc<std::sync::OnceLock<crate::budget_note::BudgetNote>>,
    /// The context-pressure reminder every lane's cell results may end with (crate::context_reminder): each lane's
    /// limit and, once the session actor wired it, the meter of its context use.
    pub context_reminders: Arc<crate::context_reminder::ContextReminders>,
    /// Steering into a child lane's running conversation (crate::agent_hub), once the session actor wired it.
    pub lane_steer: Arc<std::sync::OnceLock<crate::agent_hub::LaneSteer>>,
    /// The session's runtime counters (crate::session_stats), for `/usage`.
    pub stats: Arc<SessionStats>,
    /// The guard statistics of worktree subagent hosts whose runs ended (TypeScript has one set of file hooks for the
    /// whole session, so they stay in its totals).
    retired_guards: Mutex<Vec<Value>>,
}

impl Stack {
    /// The settings and locations the session's lanes share.
    pub(crate) fn config(&self) -> &Arc<Config> {
        &self.config
    }

    /// `schedule.*`: the session's scheduled prompts (fired by the server through the session actor).
    pub fn prompt_schedules(&self) -> &Arc<kernel_apis::prompt_schedule::ScheduleModule> {
        &self.apis.schedule
    }

    /// `poolStats()` as the worker answers an inspected `rlm.pool`: the lanes' kernel pool, plus the session's
    /// file-hook guard statistics (`guards`) when it has guards.
    pub fn pool_stats(&self) -> Value {
        self.lanes.pool_stats()
    }

    /// `fileHooks.stats()` over the whole session: the session host's guards, with the same guards of every worktree
    /// subagent's host (live or ended) added in.
    pub fn guard_stats(&self) -> Vec<Value> {
        let mut totals = self.host.guard_stats();
        let others: Vec<Vec<Value>> = lock(&self.workspaces)
            .values()
            .map(|w| w.host.guard_stats())
            .chain(std::iter::once(lock(&self.retired_guards).clone()))
            .collect();
        for stats in others {
            add_guard_stats(&mut totals, &stats);
        }
        totals
    }

    /// A new root turn began (`fileHooks.beginTurn()`): every host's per-turn guard times start from zero.
    pub fn begin_turn(&self) {
        self.host.begin_turn();
        for workspace in lock(&self.workspaces).values() {
            workspace.host.begin_turn();
        }
        for retired in lock(&self.retired_guards).iter_mut() {
            retired["turnMs"] = json!(0);
        }
    }

    /// A worktree subagent's lane ended: its host goes (its shell jobs are the session's and keep running), its guard
    /// statistics stay in the session's.
    fn retire_workspace(&self, lane: &str) {
        let Some(workspace) = lock(&self.workspaces).remove(lane) else {
            return;
        };
        add_guard_stats(&mut lock(&self.retired_guards), &workspace.host.guard_stats());
    }

    /// The host `lane` runs its shell in: a worktree subagent's own, else the session's.
    fn host_of(&self, lane: &str) -> Arc<Host> {
        lock(&self.workspaces)
            .get(lane)
            .map_or_else(|| self.host.clone(), |w| w.host.clone())
    }

    /// The handler `lane`'s requests go to (outermost layer first). `cell`: the cell's wait signal and park
    /// callback (subagent waits end with the cell); none for requests outside a cell.
    pub fn cell_handler(&self, lane: &str, cell: Option<(Signal, Park)>) -> Arc<dyn HostHandler> {
        let cell_signal = cell.as_ref().map(|(signal, _)| signal.clone());
        let delegation: Arc<dyn HostHandler> = match cell {
            Some((signal, park)) => Arc::new(self.delegation.cell_handler(lane, signal, park)),
            None => Arc::new(self.delegation.lane_handler(lane)),
        };
        let router = Router {
            delegation,
            host: self.host_of(lane),
            progress: (lane == crate::lane::MAIN).then(|| self.cell_progress.clone()),
            lane: lane.to_string(),
            config: self.config.clone(),
            swarm: self.swarm.get().cloned(),
            cell: cell_signal.clone().map(CellShell::new),
        };
        let inference = Inference::new(
            Arc::new(router),
            self.model.clone(),
            self.api_key.clone(),
            // Frames run on their handler threads' own runtimes, so they get their own HTTP client.
            Arc::new(crate::lane::process_transport()),
            self.sessions_dir.join("rlm-context").join(&self.session_id),
        )
        // A frame naming another model calls it as a lane of that model would (crate::frame_models).
        .with_models(crate::frame_models::resolver(self.config.clone()))
        .with_session(self.frames.clone(), lane);
        // `rlm.map` and the frame-model defaults wrap the frames.
        // Read per frame (session-worker.ts `modelSettings`): `/settings -> Models` applies to the next frame.
        let settings = self.config.settings_reader();
        let models = move || {
            let settings = settings();
            let rlm = settings.rlm_model_settings();
            inference::ModelSettings {
                frame_model: rlm.frame_model,
                frame_thinking: rlm.frame_thinking,
                child_model: rlm.child_model,
                review_model: settings.review_model(),
            }
        };
        let frames = inference::Frames::new(inference, self.config.frame_env.clone(), models);
        // Frames end with their cell: an aborted root turn cancels every in-flight frame at once.
        let frames: Arc<dyn HostHandler> = match cell_signal {
            Some(cell) => Arc::new(FramesOfCell {
                inner: Arc::new(frames),
                cell,
            }),
            None => Arc::new(frames),
        };
        // Typed agents answer their requests (and their own lanes' `rlm.finish`) outside the delegation router.
        let typed: Arc<dyn HostHandler> = Arc::new(self.agents.handler(lane, frames));
        // `schedules.*` and `goal.*` from every lane, as session-worker.ts registers them: the goal module itself
        // refuses lanes other than the root and the goal's job.
        let mut apis = KernelApis::new(typed)
            .with_host(Arc::new(self.agents.clone()), lane)
            .with(self.schedules.clone() as Arc<dyn kernel_apis::ApiModule>)
            .with(self.goal.clone() as Arc<dyn kernel_apis::ApiModule>);
        // `ctx.*` from any lane edits that lane's own branch (core::context_children).
        if let Some(context) = self.context.get() {
            apis = apis.with(context.clone() as Arc<dyn kernel_apis::ApiModule>);
        }
        let apis = apis
            .with(self.apis.artifacts.clone())
            .with(self.apis.skills.clone())
            .with(self.apis.memory.clone())
            .with(self.apis.hints.clone())
            .with(self.apis.images.clone())
            .with(self.apis.schedule.clone())
            .with(self.apis.tools.clone())
            .with(self.apis.local.clone())
            .with(self.apis.gates.clone())
            .with(self.apis.instances.clone())
            .with(self.apis.progress.clone())
            .with(Arc::new(crate::agent_hub::Family::new(
                self.apis.family.clone(),
                self.delegation.clone(),
                self.lane_steer.clone(),
            )) as Arc<dyn kernel_apis::ApiModule>);
        let observed: Arc<dyn HostHandler> = if self.cell_hints.enabled() {
            Arc::new(ObservedByHints {
                inner: Arc::new(apis),
                hints: self.cell_hints.clone(),
                lane: lane.to_string(),
            })
        } else {
            Arc::new(apis)
        };
        // `view_image` is the cell's own (`executeCell`), outside what the hints observe.
        Arc::new(crate::view_images::ViewImageLayer {
            inner: observed,
            images: self.images.clone(),
            lane: lane.to_string(),
        })
    }

    /// `lane`'s kernel (not started): in the lane's working directory, inside the shell sandbox, with its snapshot
    /// under the profile and the continuity policy.
    fn kernel(&self, lane: &str) -> Result<Kernel, String> {
        let config = &self.config;
        let (cwd, extra) = match lock(&self.workspaces).get(lane) {
            Some(workspace) => (workspace.cwd.clone(), workspace.env.clone()),
            None => (config.cwd.clone(), Vec::new()),
        };
        let spill_dir = config.session_dir("rlm-output", &self.session_id).join(lane_dir(lane));
        let mut options = KernelOptions::new(&cwd, &config.runtime_path);
        // The kernel starts from the session's environment (its PATH, its ULTRON_* settings), never the server's.
        options.base_env = Some(config.child_env());
        options.env = vec![
            ("ULTRON_CODE_SKILLS_DIR".into(), config.skills_dir.display().to_string()),
            ("ULTRON_RLM_SPILL_DIR".into(), spill_dir.display().to_string()),
            // The kernel's write skills ask the host's file hooks (Loki, the sandbox's containment) first.
            ("ULTRON_FILE_HOOKS".into(), "1".into()),
            // What a running cell prints reaches the host as it is written (`cell.output`): its partial output.
            ("ULTRON_RLM_STREAM_OUTPUT".into(), "1".into()),
            ("HOME".into(), config.home.display().to_string()),
        ];
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
        // The whole process tree is capped (ULTRON_RLM_MAX_TREE_MEMORY_MB, default twice the per-process limit) by the
        // host watchdog; ULTRON_RLM_TREE_MEMORY_BACKEND=auto|cgroup opts into a systemd scope where one is available.
        options.tree_memory = Some(kernel::TreeMemoryOptions {
            backend: Some(
                (config.var)("ULTRON_RLM_TREE_MEMORY_BACKEND")
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| "watchdog".into()),
            ),
            ..kernel::TreeMemoryOptions::default()
        });
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
        let hints = self.cell_hints.enabled().then(|| self.cell_hints.clone());
        Ok(rlm_over(
            &self.config,
            self.lanes.clone(),
            lane,
            spill_dir,
            hints,
            self.images.clone(),
            Some(self.host_of(lane)),
        )
        .with_budget_note(self.budget_note.clone())
        .with_stats(self.stats.clone())
        .with_context_reminders(self.context_reminders.clone()))
    }

    /// `listCommands`: `/mcp`, `/review`, prompt templates and skills (crate::commands).
    pub fn commands(&self) -> Vec<Value> {
        let bundled = self.config.bundled_skills();
        let commands = crate::commands::list_commands_with(
            &self.config.cwd.display().to_string(),
            &self.config.agent_dir.display().to_string(),
            bundled.as_deref(),
            &self.services.resources,
        );
        crate::session_flags::filter_commands(commands, &self.services.session)
    }

    /// The model the session's lanes run on unless told otherwise.
    pub fn model(&self) -> &Model {
        &self.model
    }

    /// The root lane's system prompt, in the settings' current mode.
    pub fn system(&self) -> String {
        self.system.current()
    }

    /// As [`Stack::close`], for a worker whose session a successor worker takes over: running shell jobs keep
    /// running, journaled as running, for the successor to follow (`DurableWork::hand_over`), and subagents are not
    /// cancelled: their tasks stay unfinished in the journal, so the successor reattaches them as after a crash.
    pub fn hand_over(&self) {
        if let Some(work) = self.work.get() {
            work.hand_over();
        }
        // Tool calls run in this worker's MCP connections: they end with it (a successor reads them as interrupted).
        self.apis.tools.close();
        self.mcp.close();
        self.lanes.close();
        self.stats.close();
    }

    /// Stop the session's lanes and subagents, and its durable work (the tick, goal jobs, shell jobs).
    pub fn close(&self) {
        if let Some(work) = self.work.get() {
            work.close();
        }
        self.apis.tools.close();
        self.mcp.close();
        self.agents.close();
        self.delegation.close();
        self.lanes.close();
        // What the closing lanes counted is saved; nothing is counted after.
        self.stats.close();
    }

    /// The session's situational hints (`ULTRON_HINTS`).
    pub fn cell_hints(&self) -> &Arc<kernel_apis::cell_hints::CellHints> {
        &self.cell_hints
    }

    /// The session's MCP gateway (`/mcp`, and the REPL's `mcp` namespace through `tools.*`).
    pub fn mcp(&self) -> &Arc<mcp::gateway::Gateway> {
        &self.mcp
    }

    /// The session's REPL tool calls (`tools.*`).
    pub fn tool_calls(&self) -> &Arc<kernel_apis::ToolCalls> {
        &self.apis.tools
    }

    /// The session's masking of text that reaches the model (see [`session_mask`]).
    pub fn mask(&self) -> crate::async_events::MaskText {
        session_mask(&self.config)
    }
}

/// How a session's subagents run, and where their task journal lives.
pub struct SubagentHost {
    pub store: Box<dyn delegation::task_store::TaskStore>,
    /// Builds the runner from the stack being built (it may hold the stack weakly).
    pub runner: Box<dyn FnOnce(Weak<Stack>) -> delegation::host::ChildRunner>,
    /// The typed agents' task journal, definition registry and usage ledger stores, and the runner of their `rlm`
    /// lanes (none: in memory, and no `rlm` strategy).
    pub agents: Option<AgentStores>,
}

/// Where the typed-agent host keeps its records, and how its `rlm` lanes run.
pub struct AgentStores {
    pub tasks: Box<dyn delegation::task_store::TaskStore>,
    pub definitions: Box<dyn delegation::task_store::TaskStore>,
    pub usage: Box<dyn delegation::task_store::TaskStore>,
    pub lanes: Box<dyn FnOnce(Weak<Stack>) -> delegation::agents::LaneRunner>,
    /// Where frame traces are kept (`ultron.rlm.frames`).
    pub frames: Arc<dyn inference::TraceStore>,
}

/// The typed-agent host of a session: its journal, definitions and usage ledger in `stores` (in memory without),
/// the predict adapter over the session's model, `rlm` lanes as `stores` runs them, root turns charged to roots
/// of their own. Returns the host, its ledger and its task-end channel.
#[allow(clippy::too_many_arguments)]
fn typed_agents(
    config: &Config,
    stores: Option<AgentStores>,
    stack: Weak<Stack>,
    model: &Model,
    api_key: Option<String>,
    goal: Arc<std::sync::OnceLock<Arc<GoalDriver>>>,
    refinements: Option<delegation::agents::RefinementsHook>,
    frames: delegation::agents::FrameRunner,
    stats: &Arc<SessionStats>,
) -> (Agents, UsageLedger, Receiver<delegation::agents::TaskEnd>) {
    use delegation::task_store::MemoryStore;
    let (tasks, definitions, usage_store, lanes) = match stores {
        Some(stores) => (
            stores.tasks,
            stores.definitions,
            stores.usage,
            Some((stores.lanes)(stack.clone())),
        ),
        None => (
            Box::new(MemoryStore::default()) as Box<dyn delegation::task_store::TaskStore>,
            Box::new(MemoryStore::default()) as Box<dyn delegation::task_store::TaskStore>,
            Box::new(MemoryStore::default()) as Box<dyn delegation::task_store::TaskStore>,
            None,
        ),
    };
    // `nativeUsageLimitsFromEnv(process.env, settingsManager.getRootBudgetSettings())`: the session's environment
    // and the `rootBudget` settings.
    let var = config.var.clone();
    let limits = delegation::usage::limits_from_env(&|key| var(key), &config.settings["rootBudget"]);
    // Work refused at a root's limit is counted (`onRefused: () => sessionStats.usageLimitBlock()`).
    let refused = stats.clone();
    let usage = UsageLedger::new(
        usage_store,
        delegation::usage::UsageOptions {
            limits,
            on_refused: Some(Arc::new(move |_: &str| refused.usage_limit_block())),
            ..Default::default()
        },
    )
    .unwrap_or_else(|_| {
        UsageLedger::new(Box::new(MemoryStore::default()), Default::default()).expect("an empty ledger opens")
    });
    let mut options = AgentsOptions::new(tasks);
    options.definition_store = Some(definitions);
    options.usage = Some(usage.clone());
    options.lanes = lanes;
    options.frames = Some(frames);
    options.root_turns = true;
    options.refinements = refinements;
    // The session goal follows its runs' ends; the root's context control attributes each root-level task's
    // result to the cell that returns it (collapse on return).
    let context_stack = stack.clone();
    options.on_task_end = Some(Arc::new(move |task: &Value, cost, _| {
        if let (Some(goal), Some(id)) = (goal.get(), task["id"].as_str()) {
            let _ = goal.task_ended(id, task.get("result"));
        }
        if let Some(stack) = context_stack.upgrade()
            && let Some(context) = stack.context.get()
        {
            context.task_ended(task, cost);
        }
    }));
    let (predict_model, predict_key) = (model.clone(), api_key);
    options.predict = Some(predict_adapter(Arc::new(move |call: PredictCall<'_>| {
        complete_once(&predict_model, predict_key.clone(), &call)
    })));
    // Retained instances pin their lanes' kernels in the session's pool.
    let (pin_stack, unpin_stack) = (stack.clone(), stack.clone());
    options.pin_lane = Some(Arc::new(move |lane: &str, holder: &str| {
        pin_stack.upgrade().is_some_and(|stack| stack.lanes.pin(lane, holder))
    }));
    options.unpin_lane = Some(Arc::new(move |lane: &str, holder: &str| {
        if let Some(stack) = unpin_stack.upgrade() {
            stack.lanes.unpin(lane, holder);
        }
    }));
    // `statusExtras`: the shell jobs and REPL tool calls `lane` may see, for `agents.status` and the TUI graph.
    let extras_stack = stack.clone();
    options.status_extras = Some(Arc::new(move |lane: &str| match extras_stack.upgrade() {
        Some(stack) => crate::agents::status_extras(&stack.host, stack.tool_calls(), lane),
        None => Map::new(),
    }));
    let reuse_stack = stack;
    options.before_lane_reuse = Some(Arc::new(move |lane: &str| {
        let stack = reuse_stack.upgrade().ok_or("The session is closed")?;
        match stack.lanes.kernel(lane) {
            Some(kernel) => kernel.reset_scratch().map(drop).map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }));
    let (agents, ends) = Agents::new(options);
    (agents, usage, ends)
}

/// One model completion without tools (`completeSimple`), for the predict adapter.
fn complete_once(model: &Model, api_key: Option<String>, call: &PredictCall<'_>) -> Result<Completion, String> {
    let context = ai::TranscriptContext {
        messages: vec![
            Message::System(ai::SystemMessage {
                content: ai::SystemContent::Text(call.system.to_string()),
                sections: None,
                tools_added: None,
                tools_removed: None,
                timestamp: crate::lane::now_ms(),
            }),
            Message::User(UserMessage {
                content: UserMessageContent::Text(call.user.clone()),
                timestamp: crate::lane::now_ms(),
            }),
        ],
    };
    let abort = ai::AbortSignal::new();
    let mut options = ai::SimpleStreamOptions::default();
    options.base.api_key = api_key;
    options.base.signal = Some(abort.clone());
    let (signal, done) = (call.signal.clone(), Signal::new());
    let watched = done.clone();
    let watcher = std::thread::spawn(move || {
        while !watched.is_aborted() {
            if signal.wait(Duration::from_millis(200)) {
                abort.abort();
                return;
            }
        }
    });
    let provider = Provider::with_key(None);
    let mut sink = |_: &ai::AssistantMessageEvent| {};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string());
    let message = runtime.map(|runtime| runtime.block_on(provider.stream(model, &context, &options, &mut sink)));
    done.abort("done");
    let _ = watcher.join();
    let message = message?;
    let text = message
        .content
        .iter()
        .filter_map(|c| match c {
            ai::AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    let stop = serde_json::to_value(message.stop_reason)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    Ok(Completion {
        text,
        stop_reason: stop,
        error_message: message.error_message,
    })
}

/// What a subagent runs with: its model, its system prompt (naming its working directory) and thinking level.
pub struct ChildSetup {
    pub model: Model,
    pub system: String,
    pub thinking: String,
}

/// A subagent's lane: in its worktree when it has one (its shell, kernel and prompt name that working directory, as
/// `session-worker.ts` rewrites it), on the session's model unless the spawn named one.
pub fn child_setup(stack: &Stack, run: &ChildRun) -> ChildSetup {
    let model = run
        .model
        .as_deref()
        .and_then(|spec| spec.split_once('/'))
        .and_then(|(provider, id)| stack.config.model(provider, id))
        .unwrap_or_else(|| stack.model.clone());
    let root_cwd = stack.config.cwd.display().to_string();
    let mut system = stack.system.current();
    if let Some(cwd) = run.cwd.as_deref() {
        // A worktree subagent works in its worktree: its shell, kernel and prompt name it, not the checkout.
        let host = Runtime {
            config: stack.config.clone(),
            services: stack.services.clone(),
            diagnostics: Vec::new(),
            system: SystemPrompt::fixed(""),
            sessions_dir: stack.sessions_dir.clone(),
            mode: String::new(),
            hindsight: None,
        }
        .host_in(
            Path::new(cwd),
            &stack.session_id,
            &lane_dir(&run.lane_name),
            Some(&stack.stats),
            None,
        );
        match host {
            Ok(host) => {
                // Its commands run with the worktree's environment (`withShellEnv(..., laneWorkspace(lane).env)`),
                // and its shell jobs are the session's (one `ShellJobs` for every lane).
                let host = host.with_env(run.env.clone()).sharing_jobs(&stack.host);
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
    // A subagent's `view_image` resizes for its own model.
    stack.images.set_model(Some(&run.lane_name), model.clone());
    // Its context reminder: a child conversation compacts at the harness's default threshold.
    stack.context_reminders.set_limit(
        &run.lane_name,
        crate::context_reminder::lane_limit(&model, true, harness::chat::CompactionPolicy::default().reserve_tokens),
    );
    // The model each `rlm.spawn` subagent runs on is counted (`childModel`; a subagent run as a process has no lane
    // to ask).
    stack
        .stats
        .child_model(&run.task_id, &format!("{}/{}", model.provider, model.id));
    ChildSetup {
        model,
        system,
        thinking: run.thinking.clone().unwrap_or_else(|| "off".into()),
    }
}

/// A subagent's agent (its lane per [`child_setup`]), the loop that runs it when it is not run durably.
fn child_agent(stack: &Weak<Stack>, defaults: &ChildDefaults, run: &ChildRun) -> (AgentOptions, Box<dyn StreamFn>) {
    let Some(stack) = stack.upgrade() else {
        // The session is closing: a loop without tools whose provider reports no work.
        let mut options = AgentOptions::new(defaults.model.clone());
        options.system_prompt = Some("The session ended before this subagent started.".into());
        return (options, Box::new(Provider::with_key(defaults.api_key.clone())));
    };
    let setup = child_setup(&stack, run);
    let system = setup.system;
    let mut options = AgentOptions::new(setup.model);
    options.system_prompt = Some(system.clone());
    options.thinking_level = setup.thinking;
    match stack.rlm(&run.lane_name) {
        Ok(tool) => options.tools = vec![Arc::new(tool) as Arc<dyn AgentTool>],
        // The child's loop then runs without its REPL and reports the failure in its reply.
        Err(error) => options.system_prompt = Some(format!("{system}\n\nThe REPL failed to start: {error}")),
    }
    // Its requests carry the session's stream options as the root's do (one harness in TypeScript).
    let provider = Provider::with_key(stack.api_key.clone()).with_live_settings(stack.config.settings_reader(), true);
    (options, Box::new(provider))
}

/// The model and key a subagent falls back to.
struct ChildDefaults {
    model: Model,
    api_key: Option<String>,
}

/// Routes a cell's requests: subagents to the delegation host (as that lane), everything else to the host
/// operations. (The lane API — `rlm.lanes`, `rlm.lane.*`, `rlm.cells`,
/// `rlm.cell.cancel`, `rlm.recovery` — is answered by the lanes before a request gets here.)
struct Router {
    delegation: Arc<dyn HostHandler>,
    host: Arc<Host>,
    /// The root lane's cell progress (crate::turns): shell output is published as the cell's partial output.
    progress: Option<Arc<Mutex<Option<harness::chat::CellProgress>>>>,
    /// The lane the requests come from: its shell jobs are its own.
    lane: String,
    config: Arc<Config>,
    /// `swarm.*`, `board.*`, `notes.*`, `rlm.ask` and persistent children, once the durable runtime is open.
    swarm: Option<Arc<crate::swarm_ops::SwarmOps>>,
    /// The cell these requests come from (none outside a cell): cancelling it stops its shell commands.
    cell: Option<CellShell>,
}

/// Why a cancelled cell's shell request failed: Python raises it, so the cell ends at once with its kernel (and the
/// lane's variables) intact.
pub const CELL_CANCELLED_SHELL: &str = "the cell was cancelled; its shell command was stopped";

/// The shell work of one cell. Cancelling the cell (its wait signal aborts while the kernel still runs it) stops
/// its blocking `bash` at once and cancels the shell jobs it started; a cell that ends normally leaves its jobs
/// running (they outlive cells by design). The kernel aborts a cell's request signal before the lanes abort its
/// wait signal at a normal end, so a wait abort with the request signal live is a cancel.
#[derive(Clone)]
struct CellShell {
    waits: Signal,
    /// Jobs this cell started, and whether the cancel listener is armed.
    jobs: Arc<Mutex<(Vec<String>, bool)>>,
}

impl CellShell {
    fn new(waits: Signal) -> CellShell {
        CellShell {
            waits,
            jobs: Arc::default(),
        }
    }

    /// Whether the cell is being cancelled (as opposed to having ended): `request` is the cell's request signal.
    fn cancelled(&self, request: &Signal) -> bool {
        self.waits.is_aborted() && !request.is_aborted()
    }

    /// Runs one shell request under a signal that aborts with the request or with the cell's cancel; a cancelled
    /// cell's request fails with [`CELL_CANCELLED_SHELL`]. Jobs it starts are recorded.
    fn run(
        &self,
        host: &Arc<Host>,
        lane: &str,
        kind: &str,
        payload: Map<String, Value>,
        request: &Signal,
    ) -> Result<Value, String> {
        if self.cancelled(request) {
            return Err(CELL_CANCELLED_SHELL.into());
        }
        let combined = Signal::new();
        let follow = |source: &Signal| {
            let target = combined.clone();
            source.on_abort(move |reason| target.abort(reason))
        };
        let (from_request, from_cell) = (follow(request), follow(&self.waits));
        if from_request.is_none() || from_cell.is_none() {
            combined.abort("the cell ended");
        }
        let result = host.handle_lane(lane, kind, payload, &combined);
        if let Some(id) = from_request {
            request.forget(id);
        }
        if let Some(id) = from_cell {
            self.waits.forget(id);
        }
        if let Ok(value) = &result {
            let job = match kind {
                "shell.run" => value["id"].as_str(),
                _ => value["job"]["id"].as_str(),
            };
            if let Some(job) = job.filter(|_| kind != "shell.cancel") {
                self.track(host, lane, job, request);
            }
        }
        if self.cancelled(request) {
            return Err(CELL_CANCELLED_SHELL.into());
        }
        result
    }

    /// Records a job this cell started; the first one arms the cancel listener.
    fn track(&self, host: &Arc<Host>, lane: &str, job: &str, request: &Signal) {
        let mut jobs = lock(&self.jobs);
        if !jobs.0.iter().any(|known| known == job) {
            jobs.0.push(job.to_string());
        }
        if jobs.1 {
            return;
        }
        jobs.1 = true;
        drop(jobs);
        let stop = {
            let (host, lane, request, list) = (host.clone(), lane.to_string(), request.clone(), self.jobs.clone());
            move |_reason: String| stop_cell_jobs(&host, &lane, &request, &list)
        };
        if self.waits.on_abort(stop).is_none() {
            // Already aborted: the listener was dropped, so stop now.
            stop_cell_jobs(host, lane, request, &self.jobs);
        }
    }
}

impl HostHandler for Router {
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        // The worker answers this itself, before the host (session-worker.ts `hostHandler`).
        if kind == "rlm.find_models" {
            return Ok(self.config.find_models());
        }
        if let Some(swarm) = &self.swarm
            && let Some(result) = swarm.handle(&self.lane, kind, &payload, signal)
        {
            return result;
        }
        if kind == crate::turns::CELL_OUTPUT {
            if let Some(progress) = &self.progress {
                crate::turns::publish_output(
                    progress,
                    payload.get("text").and_then(Value::as_str).unwrap_or(""),
                    &session_mask(&self.config),
                );
            }
            return Ok(Value::Null);
        }
        if DELEGATION_REQUESTS.contains(&kind) || kind.starts_with("rlm.worktrees.") {
            return self.delegation.handle(kind, payload, signal);
        }
        // runtime.py's `ext` (Pi extension functions): Pi extensions are not supported by ultron-rs.
        if kind == "ext.call" || kind == "ext.list" {
            return Err(EXTENSIONS_UNSUPPORTED.into());
        }
        let shell = matches!(kind, "bash" | "shell.bash" | "shell.run" | "shell.result");
        let result = match &self.cell {
            Some(cell) if shell => cell.run(&self.host, &self.lane, kind, payload, signal),
            _ => self.host.handle_lane(&self.lane, kind, payload, signal),
        };
        if shell
            && let (Some(progress), Ok(value)) = (&self.progress, &result)
            && let Some(output) = value["output"].as_str().filter(|o| !o.is_empty())
            && let Some(progress) = lock(progress).as_ref()
        {
            // Masked, as the cell's result will be: the durable partial output reaches the transcript too.
            let output = session_mask(&self.config)(output);
            progress.output(
                if output.ends_with('\n') {
                    output
                } else {
                    format!("{output}\n")
                }
                .as_str(),
            );
        }
        result
    }
}

/// Cancels the shell jobs a cancelled cell started (`shell.cancel`: the host stops each job's own process group).
/// Nothing when `request` is aborted: the cell ended (or its kernel failed) rather than being cancelled.
fn stop_cell_jobs(host: &Arc<Host>, lane: &str, request: &Signal, jobs: &Mutex<(Vec<String>, bool)>) {
    if request.is_aborted() {
        return;
    }
    let ids = std::mem::take(&mut lock(jobs).0);
    for id in ids {
        let mut payload = Map::new();
        payload.insert("id".into(), Value::String(id));
        let _ = host.handle_lane(lane, "shell.cancel", payload, &Signal::new());
    }
}

/// Every host request of a lane's kernel, observed for the situational hints (what a cell waited on, polled or
/// detached).
struct ObservedByHints {
    inner: Arc<dyn HostHandler>,
    hints: Arc<kernel_apis::cell_hints::CellHints>,
    lane: String,
}

impl HostHandler for ObservedByHints {
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        let started = kernel_apis::util::system_clock()();
        let observed = payload.clone();
        let result = self.inner.handle(kind, payload, signal);
        self.hints
            .observe(&self.lane, kind, &observed, result.as_ref().ok(), started);
        result
    }
}

/// `rlm.infer` / `rlm.map` under a cell: the frames see an abort of the request or of the cell (the cell's host
/// waits end as soon as it is cancelled, before its kernel is interrupted).
struct FramesOfCell {
    inner: Arc<dyn HostHandler>,
    cell: Signal,
}

impl HostHandler for FramesOfCell {
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        if kind != "rlm.infer" && kind != "rlm.map" {
            return self.inner.handle(kind, payload, signal);
        }
        let combined = Signal::new();
        let done = Signal::new();
        let (cell, request, abort, finished) = (self.cell.clone(), signal.clone(), combined.clone(), done.clone());
        let watcher = std::thread::spawn(move || {
            while !finished.wait(Duration::from_millis(20)) {
                for source in [&cell, &request] {
                    if source.is_aborted() {
                        abort.abort(source.reason().unwrap_or_else(|| "the cell was cancelled".into()));
                        return;
                    }
                }
            }
        });
        let result = self.inner.handle(kind, payload, &combined);
        done.abort("finished");
        let _ = watcher.join();
        result
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

/// The summary of a task's end event (`taskEvent` of session-worker.ts): the task's definition, then its verdict
/// tag (`[passed, verified]`, `[unverified]`, ...) when it has one, then its reply (a non-string value as JSON) or,
/// unless it succeeded, its error.
pub fn task_event_summary(task: &Value) -> String {
    let result = &task["result"];
    let detail = if result["status"] == "succeeded" {
        match &result["value"] {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        }
    } else {
        result["error"].as_str().unwrap_or_default().to_string()
    };
    let tag = if result.is_object() {
        delegation::verdict::verdict_tag(result)
    } else {
        None
    };
    let definition = task["definition"].as_str().unwrap_or("undefined");
    match tag {
        Some(tag) => format!("{definition}: {tag} {detail}"),
        None => format!("{definition}: {detail}"),
    }
}

/// One `<runtime_event>` line (`runtimeEventText` of async-events.ts), with the shipped masking rules.
pub fn runtime_event_line(kind: &str, id: &str, status: &str, summary: &str, fetch: &str) -> String {
    runtime_event_line_with(kind, id, status, summary, fetch, &crate::async_events::default_mask())
}

/// [`runtime_event_line`] with the session's masking (`maskCellOutput`).
pub fn runtime_event_line_with(
    kind: &str,
    id: &str,
    status: &str,
    summary: &str,
    fetch: &str,
    mask: &crate::async_events::MaskText,
) -> String {
    format!(
        "<runtime_event kind=\"{}\" id=\"{}\" status=\"{}\" summary=\"{}\" fetch=\"{}\" />",
        attribute(kind),
        attribute(id),
        attribute(status),
        attribute(&bounded(&mask(summary))),
        attribute(fetch)
    )
}

/// `text.slice(0, n)` of a JavaScript string: its first `n` UTF-16 code units (a surrogate pair is not split).
pub fn js_slice(text: &str, n: usize) -> String {
    let mut units = 0;
    let mut out = String::new();
    for c in text.chars() {
        units += c.len_utf16();
        if units > n {
            break;
        }
        out.push(c);
    }
    out
}

/// The session's masking of text that reaches the model: values MCP servers resolved (`registerSecretValues`) and
/// the shipped rules, unless `ULTRON_MASK_SECRETS` turns it off.
pub fn session_mask(config: &Config) -> crate::async_events::MaskText {
    let (masker, flag) = (config.masker.clone(), config.mask_secrets.clone());
    Arc::new(move |text: &str| masker.mask_counted(text, flag.as_deref()).0)
}

/// Completions of work a run started that outlives its cell: subagents (`child_done`) and detached cells
/// (`cell_done`) of the root lane. Between turns the run waits here (in the host, never in a cell) while such work
/// is running, and each completion continues the run as a `<runtime_event>` message. `ULTRON_ASYNC_EVENTS=off`
/// turns this off: nothing waits and nothing is announced.
pub struct RuntimeEvents {
    task_ends: Mutex<Receiver<delegation::agents::TaskEnd>>,
    child_ends: Mutex<Receiver<DetachedEnd>>,
    cell_ends: Mutex<Receiver<CellEnd>>,
    delegation: Delegation,
    lanes: Lanes,
    enabled: bool,
    /// The session's masking of event summaries.
    mask: crate::async_events::MaskText,
}

impl RuntimeEvents {
    fn pending_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        // A typed task's end nobody awaited (`task_done`, as session-worker.ts `onDetachedEnd`).
        while let Ok(end) = lock(&self.task_ends).try_recv() {
            if end.owner_lane != crate::lane::MAIN || end.awaited {
                continue;
            }
            let task = &end.task;
            let status = task["result"]["status"]
                .as_str()
                .or(task["state"].as_str())
                .unwrap_or("ended");
            let id = task["id"].as_str().unwrap_or_default();
            lines.push(runtime_event_line_with(
                end.kind,
                id,
                status,
                &task_event_summary(task),
                &end.fetch,
                &self.mask,
            ));
        }
        while let Ok(end) = lock(&self.child_ends).try_recv() {
            if end.owner_lane != crate::lane::MAIN {
                continue;
            }
            let task = &end.task;
            let result = &task["result"];
            let status = result["status"].as_str().or(task["state"].as_str()).unwrap_or("ended");
            let summary = task_event_summary(task);
            let id = task["id"].as_str().unwrap_or_default();
            lines.push(runtime_event_line_with(
                end.kind, id, status, &summary, &end.fetch, &self.mask,
            ));
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
            lines.push(runtime_event_line_with(
                "cell_done",
                &end.cell,
                status,
                &summary,
                "await rlm.cells()",
                &self.mask,
            ));
        }
        lines
    }

    /// Every completion since the last call, for the event dispatcher (crate::async_events): typed tasks' ends
    /// nobody awaited (`task_done`, with their root), subagents' ends (`child_done`) and detached cells' ends
    /// (`cell_done`), each for the lane that owns the work. A subagent's or cell's root is left for the caller.
    pub fn drain_ends(&self) -> Vec<crate::async_events::RuntimeEvent> {
        let mut events = Vec::new();
        while let Ok(end) = lock(&self.task_ends).try_recv() {
            if end.awaited {
                continue;
            }
            let task = &end.task;
            events.push(crate::async_events::RuntimeEvent {
                kind: end.kind.into(),
                id: task["id"].as_str().unwrap_or_default().into(),
                status: task["result"]["status"]
                    .as_str()
                    .or(task["state"].as_str())
                    .unwrap_or("ended")
                    .into(),
                summary: task_event_summary(task),
                fetch: end.fetch.clone(),
                lane: end.owner_lane.clone(),
                root_id: end.root_id.clone(),
            });
        }
        while let Ok(end) = lock(&self.child_ends).try_recv() {
            let task = &end.task;
            events.push(crate::async_events::RuntimeEvent {
                kind: end.kind.into(),
                id: task["id"].as_str().unwrap_or_default().into(),
                status: task["result"]["status"]
                    .as_str()
                    .or(task["state"].as_str())
                    .unwrap_or("ended")
                    .into(),
                summary: task_event_summary(task),
                fetch: end.fetch.clone(),
                lane: end.owner_lane.clone(),
                root_id: None,
            });
        }
        while let Ok(end) = lock(&self.cell_ends).try_recv() {
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
            events.push(crate::async_events::RuntimeEvent {
                kind: "cell_done".into(),
                id: end.cell.clone(),
                status: status.into(),
                summary,
                fetch: "await rlm.cells()".into(),
                lane: end.lane.clone(),
                root_id: None,
            });
        }
        events
    }

    /// Whether events are on (`ULTRON_ASYNC_EVENTS`).
    pub fn enabled(&self) -> bool {
        self.enabled
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

/// Add `more` (each guard's `fileHooks.stats()` entry) into `totals`: the same guard by position and name, a new one
/// appended.
fn add_guard_stats(totals: &mut Vec<Value>, more: &[Value]) {
    const COUNTERS: [&str; 7] = [
        "checks",
        "blocked",
        "unchecked",
        "afterChecks",
        "afterFindings",
        "totalMs",
        "turnMs",
    ];
    for (index, stats) in more.iter().enumerate() {
        match totals.get_mut(index).filter(|t| t["name"] == stats["name"]) {
            Some(total) => {
                for key in COUNTERS {
                    let sum = total[key].as_u64().unwrap_or(0) + stats[key].as_u64().unwrap_or(0);
                    total[key] = json!(sum);
                }
            }
            None => totals.push(stats.clone()),
        }
    }
}
