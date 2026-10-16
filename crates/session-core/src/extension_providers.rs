//! Providers the session's extensions declare (`capabilities.providers`, TypeScript `pi.registerProvider`) reach the
//! model registry. TypeScript applies `pendingProviderRegistrations` to the session's `ModelRuntime` once the
//! extensions load (`agent-session-services.ts`), and later registrations at once; the Rust registry
//! ([`crate::lane::registry`]) is rebuilt from disk on every call, so the loaded extensions are an injected source it
//! reads each time: the current registrations, applied over models.json in load order with `registerProvider`'s
//! validation and merging (`ai::model_registry` extension layer).
//!
//! The source is installed for the agent directory the session reads its models from, for as long as the session's
//! extensions run ([`install`] returns the guard the runtime keeps). Every lookup of that profile in this process
//! sees it: `/model`, the scoped-model cycle, `--list-models`, model selection and the requests themselves
//! (base URL, api, key and headers).

use ai::model_registry::ModelRegistry;
use extensions::ExtensionHost;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

/// One `registerProvider(name, config)` and the extension that made it.
#[derive(Clone, Debug, PartialEq)]
pub struct Registration {
    pub name: String,
    pub config: Map<String, Value>,
    pub extension_path: String,
}

/// The current registrations, in load order.
pub type Source = Arc<dyn Fn() -> Vec<Registration> + Send + Sync>;

static NEXT: AtomicU64 = AtomicU64::new(0);
static SOURCES: Mutex<Vec<(u64, PathBuf, Source)>> = Mutex::new(Vec::new());

fn sources() -> std::sync::MutexGuard<'static, Vec<(u64, PathBuf, Source)>> {
    SOURCES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Keeps a source installed; dropping it removes the source.
pub struct Installed(u64);

impl Drop for Installed {
    fn drop(&mut self) {
        sources().retain(|(id, _, _)| *id != self.0);
    }
}

/// Install `source` for the profile at `agent_dir` until the guard drops.
pub fn install(agent_dir: &Path, source: Source) -> Installed {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    sources().push((id, agent_dir.to_path_buf(), source));
    Installed(id)
}

/// The registrations of a loaded extension host, read live (a `registrations/changed` replaces an extension's), as
/// long as the host exists.
pub fn from_host(host: &Arc<ExtensionHost>) -> Source {
    let host: Weak<ExtensionHost> = Arc::downgrade(host);
    Arc::new(move || {
        host.upgrade()
            .map(|host| {
                host.providers()
                    .into_iter()
                    .map(|(spec, extension_path)| Registration {
                        name: spec.name,
                        config: spec.config,
                        extension_path,
                    })
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// Every installed source's registrations for the profile at `agent_dir`, in installation order.
pub fn registrations(agent_dir: &Path) -> Vec<Registration> {
    let installed: Vec<Source> = sources()
        .iter()
        .filter(|(_, dir, _)| dir == agent_dir)
        .map(|(_, _, source)| source.clone())
        .collect();
    installed.iter().flat_map(|source| source()).collect()
}

/// Apply `registrations` to `registry` in order; a registration that does not compose is skipped and reported as
/// `Extension "<path>" error: <message>` (`agent-session-services.ts`).
pub fn apply(registry: &mut ModelRegistry, registrations: &[Registration]) -> Vec<String> {
    registrations
        .iter()
        .filter_map(|r| {
            registry
                .register_extension_provider(&r.name, &r.config)
                .err()
                .map(|error| format!("Extension \"{}\" error: {error}", r.extension_path))
        })
        .collect()
}

/// The extensions a command without a session loads for their providers (`ultron --list-models`, which TypeScript
/// runs after `createAgentSessionServices` loaded the extensions): discovered as a session discovers them, started
/// headless in print mode, their providers installed until [`Loaded`] drops (which also stops the processes).
pub struct Loaded {
    host: Arc<ExtensionHost>,
    _installed: Installed,
}

impl Drop for Loaded {
    fn drop(&mut self) {
        self.host.shutdown();
    }
}

/// Load the extensions configured for `env` (None when there are none) and install their providers.
pub fn load_headless(
    env: &crate::RunEnv,
    flags: &crate::session_flags::SessionFlags,
    no_extensions: bool,
) -> Option<Loaded> {
    let agent_dir = env.agent_dir();
    let trusted = crate::extension_commands::worker_project_trusted(&env.cwd, &agent_dir, &env.home);
    let settings = config::settings::SettingsManager::create(&env.cwd, &agent_dir, trusted);
    let discovery = crate::session_flags::extension_discovery(env, &agent_dir, &settings, flags, no_extensions);
    if discovery.manifests.is_empty() {
        return None;
    }
    let session = extensions::SessionInfo {
        cwd: env.cwd.display().to_string(),
        mode: "print".into(),
        has_ui: false,
        flags: Map::new(),
    };
    let host = Arc::new(ExtensionHost::load(
        discovery.manifests,
        session,
        Arc::new(extensions::NoHost),
    ));
    let installed = install(&agent_dir, from_host(&host));
    Some(Loaded {
        host,
        _installed: installed,
    })
}
