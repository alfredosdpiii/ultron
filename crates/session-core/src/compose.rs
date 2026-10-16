//! The composition root of a session's host stack. A cell's host request passes, outermost first:
//!
//! 1. [`kernel_apis::KernelApis`]: the kernel-API modules (`skills.*` with code skills, `memory.*`, `grants.*`,
//!    `hints.*`, and the local services of [`services`]: `refinements.*`, `experiments.*`, `gates.*`), matched by
//!    prefix;
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
use ai::{Message, Model, UserMessage, UserMessageContent};
use delegation::agents::{Agents, AgentsOptions, Completion, PredictCall, predict_adapter};
use delegation::child::agent_runner;
use delegation::host::{ChildRun, ChildRunner, CollectMode, Delegation, DelegationOptions, DetachedEnd, Park};
use delegation::lanes::{CellEnd, Lanes, LanesOptions};
use delegation::usage::UsageLedger;
use extensions::{ExtensionHost, SessionInfo};
use host::Host;
use host::file_hooks::FileHooks;
use host::loki_guard::{LokiGuard, LokiMode, git_root, loki_mode};
use inference::Inference;
use kernel::{ContinuityPolicy, HostHandler, Kernel, KernelOptions, Signal};
use kernel_apis::{
    CodeSkills, CodeSkillsOptions, DurableWork, DurableWorkOptions, FileDocumentStore, GoalDriver, GrantsModule,
    HintsModule, KernelApis, MemoryApi, MemoryBackend, MemoryService, SchedulesModule, SkillSource, SkillsModule,
    TaskGoalJobs,
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

/// Told the session's MCP gateway when its stack opens.
pub type McpListener = Arc<dyn Fn(&Arc<mcp::gateway::Gateway>) + Send + Sync>;

/// Collaborators a caller may supply in place of the defaults (tests, embedders).
#[derive(Clone, Default)]
pub struct Services {
    /// The memory backend behind `memory.*`; none: the Hindsight server and bank this process resolves
    /// ([`hindsight::Endpoint`]), or memory off ("Hindsight is not configured") when that resolves to off.
    pub memory: Option<Arc<dyn MemoryBackend>>,
    /// Answers `prompt` approval decisions (none: they fail closed).
    pub approver: Option<Arc<dyn Approver>>,
    /// Told the session's MCP gateway once its stack opens (the session worker serves `/mcp` with it).
    pub on_mcp: Option<McpListener>,
    /// Where the gateway's notices go when the session starts (configuration errors, an eager server that needs
    /// sign-in): the attached client's UI (`ctx.ui.notify`). None: nowhere, as without a UI in TypeScript.
    pub mcp_notices: Option<mcp::gateway::NoticeSink>,
    /// Pi `--no-extensions`: no extensions are loaded, and no MCP servers (MCP is user configuration like them).
    pub no_extensions: bool,
    /// The client UI extension dialogs and notifications reach (the worker's extension UI bridge); none: headless.
    pub extension_ui: Option<Arc<dyn crate::extension_host::ExtensionUi>>,
    /// What an extension's `ctx.shutdown()` does (`host/shutdown`); none: nothing, as in the TypeScript worker.
    pub on_extension_shutdown: Option<crate::extension_host::ShutdownAction>,
    /// `--system-prompt`, `-e`, `--no-skills`, `--no-context-files` (crate::session_flags).
    pub session: crate::session_flags::SessionFlags,
    /// The `-e` sources' and the settings packages' skills, prompts and extension manifests
    /// (crate::session_resources), resolved by [`Runtime::new`].
    pub resources: Arc<crate::session_resources::SessionResources>,
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
    /// MCP servers are read (not with `--no-extensions`).
    pub mcp_enabled: bool,
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
            frame_env: ["ULTRON_RLM_FRAME_MODEL", "ULTRON_REVIEW_MODEL", "ULTRON_RLM_MAP_TOKENS"]
                .into_iter()
                .filter_map(|key| (env.var)(key).map(|value| (key.to_string(), value)))
                .collect(),
            var: env.var.clone(),
            masker: Arc::default(),
            project_trusted: manager.is_project_trusted(),
            mcp_enabled: true,
            provider_token: {
                let env = env.clone();
                Arc::new(move |provider: &str| crate::lane::api_key(&crate::ModelFlags::default(), &env, provider))
            },
        }
    }

    /// The session's MCP gateway (extensions/mcp): the servers of `mcp.json`, and of `.pi/mcp.json` in a trusted
    /// project, reached from the REPL through the `mcp` tool. Server log messages go to `<agentDir>/mcp.log`.
    pub fn mcp_gateway(&self) -> Arc<mcp::gateway::Gateway> {
        let masker = self.masker.clone();
        mcp::gateway::Gateway::new(mcp::gateway::GatewayOptions {
            agent_dir: self.agent_dir.clone(),
            project_trusted: self.project_trusted,
            servers: (!self.mcp_enabled).then(Vec::new),
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
                ..mcp::connection::ConnectionEnv::default()
            },
            open_url: None,
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
    /// In `cwd` (a worktree subagent's own), Loki checks that tree.
    fn file_hooks_in(&self, cwd: &Path) -> FileHooks {
        // After-cell checks picture the directory between cells (workspace-snapshot.ts).
        let snapshot: host::file_hooks::Snapshotter = std::sync::Arc::new(|dir: &Path| {
            let picture = delegation::workspace::snapshot_workspace(
                dir,
                delegation::workspace::DEFAULT_MAX_SNAPSHOT_ENTRIES,
                true,
            );
            Ok(host::file_hooks::Snapshot {
                root: picture.root,
                entries: picture.entries,
                complete: picture.complete,
            })
        });
        let mut hooks = FileHooks::with_snapshots(cwd, snapshot);
        if let Some(log) = crate::guard_log::loki_log((self.var)("ULTRON_LOKI_LOG").as_deref()) {
            hooks.on_record(log);
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
            hooks.add(Box::new(LokiGuard::new(&root, self.loki).with_post_write(post_write)));
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
        extension_tools: tool
            .into_iter()
            .map(|t| resources::runtime_guide::ExtensionToolSummary {
                name: t.name,
                description: t.description,
            })
            .collect(),
        ..Default::default()
    }
}

/// How long an extension may take to answer `commands/complete` (completions are asked as the user types).
const EXTENSION_COMPLETION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
        extensions: Option<String>,
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
                extensions.as_deref(),
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

/// The process-wide composition: configuration, loaded extensions and injected services.
pub struct Runtime {
    pub config: Arc<Config>,
    pub extensions: Option<Arc<ExtensionHost>>,
    pub services: Services,
    /// Problems found loading extensions (reported once, as warnings).
    pub diagnostics: Vec<String>,
    /// The extensions that did not load (a manifest that is invalid or missing, a JavaScript extension, a process
    /// that failed to start): the TUI's `[Extension issues]`.
    pub extension_errors: Vec<extensions::manifest::Diagnostic>,
    /// What extension processes ask the session (crate::extension_host), bound to the session actor once it opens.
    pub extension_api: Arc<crate::extension_host::SessionHost>,
    /// The root lane's system prompt (subagents use it too).
    pub system: SystemPrompt,
    /// Where session-scoped host files (job logs, inference context) live: the cwd's session directory.
    pub sessions_dir: PathBuf,
    /// `print`, `json`, `rpc` or `interactive`.
    pub mode: String,
    /// The Hindsight server and bank every instance shares (`ULTRON_HINDSIGHT_URL`, the global `hindsightUrl`
    /// setting, else localhost:8888; `ULTRON_HINDSIGHT_BANK`, else "ultron"); `None`: memory is off.
    pub hindsight: Option<hindsight::Endpoint>,
    /// Keeps the extensions' providers in the model registry of this profile (crate::extension_providers).
    pub extension_providers: Option<crate::extension_providers::Installed>,
}

impl Runtime {
    /// Read the settings, start the extensions configured for `cwd` (`mode`: "print", "json" or "rpc") and build
    /// the root system prompt.
    pub fn new(env: &Env, mode: &str, sessions_dir: PathBuf, services: Services) -> Result<Runtime, String> {
        let mut services = services;
        let mut config = Config::from_env(env);
        config.mcp_enabled = !services.no_extensions;
        // The worker's project trust (`workerProjectTrusted`): a trusted project's extensions load too.
        let trusted = crate::extension_commands::worker_project_trusted(&env.cwd, &config.agent_dir, &env.home);
        let manager = config::settings::SettingsManager::create(&env.cwd, &config.agent_dir, trusted);
        // resource-loader.ts `reload`: the settings packages (installed when missing) and the `-e` resources.
        let resolved = crate::session_resources::resolve(
            env,
            config::settings::SettingsManager::create(&env.cwd, &config.agent_dir, trusted),
            &services.session,
        );
        services.resources = Arc::new(resolved);
        let discovery = crate::session_flags::extension_discovery_with(
            env,
            &config.agent_dir,
            &manager,
            &services.session,
            services.no_extensions,
            &services.resources,
        );
        let mut diagnostics: Vec<String> = services.resources.diagnostics.clone();
        diagnostics.extend(
            discovery
                .diagnostics
                .iter()
                .map(|d| format!("{}: {}", d.path.display(), d.message)),
        );
        let extension_api = Arc::new(crate::extension_host::SessionHost::new(
            env.cwd.clone(),
            config.agent_dir.clone(),
            env.home.clone(),
            mode,
            trusted,
            services.extension_ui.clone(),
            services.on_extension_shutdown.clone(),
        ));
        // `--no-extensions`: extensions are not discovered (`-e` ones still load).
        let extensions = (!discovery.manifests.is_empty()).then(|| {
            let session = SessionInfo {
                cwd: env.cwd.display().to_string(),
                mode: mode.into(),
                has_ui: extension_api.ui_capable(),
                flags: Map::new(),
            };
            Arc::new(ExtensionHost::load(discovery.manifests, session, extension_api.clone()))
        });
        let mut extension_errors = discovery.diagnostics.clone();
        if let Some(host) = &extensions {
            diagnostics.extend(
                host.diagnostics
                    .iter()
                    .map(|d| format!("{}: {}", d.path.display(), d.message)),
            );
            extension_errors.extend(host.diagnostics.iter().cloned());
        }
        let hindsight = hindsight::Endpoint::resolve(&|key| (env.var)(key), hindsight::resolve::setting_url(&manager));
        let guide = extensions
            .as_ref()
            .and_then(|host| extensions::kernel_api::guide_section(&host.functions()));
        // The `mcp` gateway lives in the REPL: the runtime guide lists it with its servers (`mcpServerNames`).
        let runtime_guide = mcp_runtime_guide(&config);
        let system = match &services.session.system_prompt {
            // `--system-prompt`: exactly the given text (`currentSystemPrompt`).
            Some(text) => SystemPrompt::fixed(text.clone()),
            None => SystemPrompt::rendered(
                env,
                &config,
                trusted,
                guide,
                runtime_guide,
                &services.session,
                services.resources.clone(),
            )?,
        };
        diagnostics.extend(system.warnings());
        if let Some(host) = &extensions {
            extension_api.bind_extensions(host, system.clone());
        }
        // `registerProvider`: the declared providers join the model registry; one that does not compose is reported.
        let extension_providers = extensions.as_ref().map(|host| {
            let source = crate::extension_providers::from_host(host);
            let models = ::config::paths::AgentPaths::new(&config.agent_dir).models();
            let mut registry = ai::model_registry::ModelRegistry::load(&models, &env.home);
            diagnostics.extend(crate::extension_providers::apply(&mut registry, &source()));
            crate::extension_providers::install(&config.agent_dir, source)
        });
        Ok(Runtime {
            config: Arc::new(config),
            extensions,
            services,
            diagnostics,
            extension_errors,
            extension_api,
            system,
            sessions_dir,
            mode: mode.to_string(),
            hindsight,
            extension_providers,
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
        let config = self.config.clone();
        // Shell jobs are durable: their journal is in the session's module directory, and opening the host recovers
        // the jobs a dead worker left (from their processes).
        let jobs = config.session_dir("rlm-modules", session_id).join("jobs.json");
        let host = Arc::new(
            self.host_in(&config.cwd, session_id, "bash")?
                .with_journal(Arc::new(host::journal::FileJobJournal::new(jobs)))?,
        );
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
        // `--no-skills`: only the `-e` sources' skills are in the catalog.
        let no_skills = self.services.session.no_skills;
        let extra = self.services.resources.clone();
        let loader: kernel_apis::SkillLoader = Arc::new(move || Ok(skill_sources(&skills_config, &extra, no_skills)));
        let code = CodeSkills::new(CodeSkillsOptions::new(&config.skills_dir, &config.runtime_path));
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
        let local = Arc::new(services::LocalServices::worker(Box::new(
            kernel_apis::artifacts::FileKeyedDocuments::new(
                config.session_dir("rlm-modules", session_id).join("local.json"),
            ),
        )));
        let refinements = services::instruction_hook(local.clone());
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
            local: Arc::new(services::LocalServicesModule { services: local }),
            gates: Arc::new(services::ReleaseGateModule::new(modules("release-gates"), None)),
            artifacts: Arc::new(kernel_apis::artifacts::ArtifactsModule::new(artifacts)),
            skills: Arc::new(SkillsModule::new(modules("skills"), loader, clock.clone(), Some(code))),
            memory: Arc::new(MemoryApi { service: memory }),
            grants: Arc::new(GrantsModule::new(modules("grants"), clock, false)),
            hints: Arc::new(HintsModule::new(modules("hints"))),
            // `tools.*`: the `mcp` gateway and the extension processes' functions called from the REPL, detached
            // after `yield_after`.
            tools: Arc::new(kernel_apis::ToolCalls::new(
                (gateway.tool().is_some() || self.extensions.is_some()).then(|| {
                    Arc::new(crate::session_tools::SessionTools {
                        extensions: self.extensions.clone().map(crate::extension_tools::ExtensionTools::new),
                        mcp: Some(gateway.clone()),
                    }) as Arc<dyn kernel_apis::ToolRunner>
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
        let extensions = self.extensions.clone();
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
        ));
        let stack = Arc::new_cyclic(|me: &Weak<Stack>| {
            let runner_stack = me.clone();
            let (store, inner, agent_stores) = match subagents.take() {
                Some(host) => (Some(host.store), (host.runner)(me.clone()), host.agents),
                None => (
                    None,
                    agent_runner(Arc::new(move |run: &ChildRun| child_agent(&runner_stack, &child, run))),
                    None,
                ),
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
                    lock(&stack.workspaces).remove(&lane);
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
            let (delegation, child_ends) = Delegation::new(options);
            // Workflow nodes with worktree=True run as subagents of the workflow's caller (worktrees live there).
            agents.set_worktree_children(Arc::new(delegation.clone()));
            // A spawn naming a model the session cannot run is refused, never run on the root's model (A04).
            delegation.set_model_check(config.model_check());
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
                Ok(pair) => pair,
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
            });
            Stack {
                config,
                mcp: gateway,
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
                lane_steer: Arc::new(std::sync::OnceLock::new()),
            }
        });
        built?;
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

    /// The file hooks of a host in `cwd`: Loki's (see [`Config::file_hooks_in`]), then extensions'
    /// `after_cell_changes`.
    fn file_hooks_in(&self, cwd: &Path) -> FileHooks {
        let mut hooks = self.config.file_hooks_in(cwd);
        if let Some(extensions) = &self.extensions {
            hooks.add(Box::new(crate::extension_file_hooks::ExtensionBeforeWrite::new(
                extensions.clone(),
            )));
            hooks.add(Box::new(crate::extension_file_hooks::ExtensionAfterCell::new(
                extensions.clone(),
            )));
        }
        hooks
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
        // `createLocalBashOperations({ shellPath: settingsManager.getShellPath() })`: the REPL's bash and its jobs.
        let shell_path = config
            .settings
            .as_object()
            .and_then(|settings| config::settings_keys::shell_path(settings, &config.home));
        Ok(Host::new(cwd, jobs_dir, spill_dir, Box::new(self.file_hooks_in(cwd)))
            .with_shell_path(shell_path)
            .sandboxed(
                &config.settings,
                &config.home,
                Some(&tmp_dir),
                self.services.approver.clone(),
            ))
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
    crate::session_resources::skills(&cwd, &agent_dir, extra, no_skills)
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
    local: Arc<services::LocalServicesModule>,
    gates: Arc<services::ReleaseGateModule>,
    artifacts: Arc<kernel_apis::artifacts::ArtifactsModule>,
    skills: Arc<SkillsModule>,
    memory: Arc<MemoryApi>,
    grants: Arc<GrantsModule>,
    hints: Arc<HintsModule>,
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

/// One session's host stack: the sandboxed host, the REPL lanes, the subagent host, the extensions and the
/// kernel-API modules.
pub struct Stack {
    config: Arc<Config>,
    /// The session's MCP servers (closed with the session).
    mcp: Arc<mcp::gateway::Gateway>,
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
    /// Steering into a child lane's running conversation (crate::agent_hub), once the session actor wired it.
    pub lane_steer: Arc<std::sync::OnceLock<crate::agent_hub::LaneSteer>>,
}

impl Stack {
    /// `poolStats()` as the worker answers an inspected `rlm.pool`: the lanes' kernel pool, plus the session's
    /// file-hook guard statistics (`guards`) when it has guards.
    pub fn pool_stats(&self) -> Value {
        let mut stats = self.lanes.pool_stats();
        let guards = self.host.guard_stats();
        if !guards.is_empty() {
            stats["guards"] = Value::Array(guards);
        }
        stats
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
            extensions: self.extensions.clone(),
            host: self.host_of(lane),
            progress: (lane == crate::lane::MAIN).then(|| self.cell_progress.clone()),
            lane: lane.to_string(),
            config: self.config.clone(),
            swarm: self.swarm.get().cloned(),
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
        let text = |v: &Value| v.as_str().map(str::to_string);
        let settings = &self.config.settings;
        let models = inference::ModelSettings {
            frame_model: text(&settings["rlm"]["frameModel"]),
            frame_thinking: text(&settings["rlm"]["frameThinking"]),
            child_model: text(&settings["rlm"]["childModel"]),
            review_model: text(&settings["reviewModel"]),
        };
        let frames = inference::Frames::new(inference, self.config.frame_env.clone(), move || models.clone());
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
            .with(self.apis.grants.clone())
            .with(self.apis.hints.clone())
            .with(self.apis.tools.clone())
            .with(self.apis.local.clone())
            .with(self.apis.gates.clone())
            .with(self.apis.instances.clone())
            .with(self.apis.progress.clone())
            .with(Arc::new(crate::agent_hub::Family::new(
                self.apis.family.clone(),
                self.delegation.clone(),
                self.lane_steer.clone(),
            )) as Arc<dyn kernel_apis::ApiModule>)
            .with(Arc::new(crate::extension_apis::ExtensionCommandsApi::new(
                self.extensions.clone(),
                self.config.cwd.clone(),
                self.config.agent_dir.clone(),
                self.config.home.clone(),
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
            // What a running cell prints reaches the host as it is written (`cell.output`): its partial output.
            ("ULTRON_RLM_STREAM_OUTPUT".into(), "1".into()),
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
        // The whole process tree is capped (ULTRON_RLM_MAX_TREE_MEMORY_MB, default twice the per-process limit) by the
        // host watchdog; ULTRON_RLM_TREE_MEMORY_BACKEND=auto|cgroup opts into a systemd scope where one is available.
        options.tree_memory = Some(kernel::TreeMemoryOptions {
            backend: Some(
                std::env::var("ULTRON_RLM_TREE_MEMORY_BACKEND")
                    .ok()
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
        .with_budget_note(self.budget_note.clone()))
    }

    /// `listCommands`: the extensions' slash commands, `/review`, prompt templates and skills (crate::commands).
    pub fn commands(&self) -> Vec<Value> {
        let config = &self.config;
        let trusted = crate::extension_commands::worker_project_trusted(&config.cwd, &config.agent_dir, &config.home);
        let settings = config::settings::SettingsManager::create(&config.cwd, &config.agent_dir, trusted);
        let extensions: Vec<crate::commands::ExtensionCommand> = self
            .extensions
            .iter()
            .flat_map(|host| host.router().registered_commands())
            .map(|command| crate::commands::ExtensionCommand {
                source_info: crate::extension_commands::source_info(
                    Path::new(&command.extension_path),
                    &settings,
                    &config.cwd,
                    &config.agent_dir,
                    &config.home,
                ),
                name: command.invocation_name,
                description: command.description,
            })
            .collect();
        // The bundled skills sit beside the runtime (packages/coding-agent/skills).
        let bundled = self
            .config
            .runtime_path
            .ancestors()
            .nth(4)
            .map(|root| root.join("skills"));
        let commands = crate::commands::list_commands_with(
            &self.config.cwd.display().to_string(),
            &self.config.agent_dir.display().to_string(),
            &bundled.unwrap_or_default().display().to_string(),
            &extensions,
            &self.services.resources,
        );
        crate::session_flags::filter_commands(commands, &self.services.session)
    }

    /// Runs the extension command `/name args` (`LegacyExtensionCommands.run`): `{notifications: []}` once its
    /// handler returned.
    pub fn run_extension_command(&self, name: &str, args: &str) -> Result<Value, String> {
        let host = self
            .extensions
            .as_ref()
            .filter(|host| {
                host.router()
                    .registered_commands()
                    .iter()
                    .any(|c| c.invocation_name == name)
            })
            .ok_or_else(|| format!("Unknown extension command: /{name}"))?;
        host.run_command(name, args, None).map_err(|e| e.to_string())?;
        Ok(serde_json::json!({"notifications": []}))
    }

    /// The argument completions of the extension command `/name` for `prefix` (`commands/complete`): its items
    /// (`{value, label, description?}`), or `null` when it offers none.
    pub fn complete_extension_command(&self, name: &str, prefix: &str) -> Result<Value, String> {
        let host = self
            .extensions
            .as_ref()
            .ok_or_else(|| format!("Unknown extension command: /{name}"))?;
        let command = host
            .router()
            .registered_commands()
            .into_iter()
            .find(|c| c.invocation_name == name)
            .ok_or_else(|| format!("Unknown extension command: /{name}"))?;
        let process = host
            .processes()
            .iter()
            .find(|p| p.manifest().path.display().to_string() == command.extension_path)
            .ok_or_else(|| format!("Unknown extension command: /{name}"))?;
        process
            .complete_command(&command.name, prefix, EXTENSION_COMPLETION_TIMEOUT)
            .map_err(|e| e.to_string())
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
        // Tool calls run in this worker's extension processes and MCP connections: they end with it (a successor
        // reads them as interrupted).
        self.apis.tools.close();
        self.mcp.close();
        self.lanes.close();
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
    let usage = UsageLedger::new(
        usage_store,
        delegation::usage::UsageOptions {
            limits,
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
            extensions: None,
            services: stack.services.clone(),
            diagnostics: Vec::new(),
            extension_errors: Vec::new(),
            extension_api: Arc::new(crate::extension_host::SessionHost::new(
                PathBuf::new(),
                PathBuf::new(),
                PathBuf::new(),
                "",
                false,
                None,
                None,
            )),
            system: SystemPrompt::fixed(""),
            sessions_dir: stack.sessions_dir.clone(),
            mode: String::new(),
            hindsight: None,
            extension_providers: None,
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
    // A subagent's `view_image` resizes for its own model.
    stack.images.set_model(Some(&run.lane_name), model.clone());
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
    /// The root lane's cell progress (crate::turns): shell output is published as the cell's partial output.
    progress: Option<Arc<Mutex<Option<harness::chat::CellProgress>>>>,
    /// The lane the requests come from: its shell jobs are its own.
    lane: String,
    config: Arc<Config>,
    /// `swarm.*`, `board.*`, `notes.*`, `rlm.ask` and persistent children, once the durable runtime is open.
    swarm: Option<Arc<crate::swarm_ops::SwarmOps>>,
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
                crate::turns::publish_output(progress, payload.get("text").and_then(Value::as_str).unwrap_or(""));
            }
            return Ok(Value::Null);
        }
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
        let shell = matches!(kind, "bash" | "shell.bash" | "shell.run" | "shell.result");
        let result = self.host.handle_lane(&self.lane, kind, payload, signal);
        if shell
            && let (Some(progress), Ok(value)) = (&self.progress, &result)
            && let Some(output) = value["output"].as_str().filter(|o| !o.is_empty())
            && let Some(progress) = lock(progress).as_ref()
        {
            progress.output(
                if output.ends_with('\n') {
                    output.to_string()
                } else {
                    format!("{output}\n")
                }
                .as_str(),
            );
        }
        result
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
    task_ends: Mutex<Receiver<delegation::agents::TaskEnd>>,
    child_ends: Mutex<Receiver<DetachedEnd>>,
    cell_ends: Mutex<Receiver<CellEnd>>,
    delegation: Delegation,
    lanes: Lanes,
    enabled: bool,
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
            lines.push(runtime_event_line(
                end.kind,
                id,
                status,
                &task_event_summary(task),
                &end.fetch,
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
