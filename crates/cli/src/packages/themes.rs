//! The themes the client TUI registers at startup (client-tui.ts `runClientTui`: a `DefaultResourceLoader` over the
//! settings, `reload()`, `setRegisteredThemes(resourceLoader.getThemes().themes)`): every enabled theme path the
//! package manager resolves (packages' `pi.themes` or `themes/` directories with their filters, settings `themes`
//! entries, `<agentDir>/themes`), a directory giving its `.json` files, loaded in order (`loadThemes` without the
//! defaults), the first theme of a name kept (`dedupeThemes`). `/settings` then offers them beside the built-in and
//! custom themes (`getAvailableThemesWithPaths`), and the theme setting can name them.

use super::manager::PackageManager;
use config::settings::SettingsManager;
use std::path::Path;
use tui::theme::{ColorMode, Theme, load_theme_from_path};

/// A theme file that did not load (`warning`) or a name already taken (`collision`): (type, path).
pub type Diagnostic = (String, String);

/// The registered themes and the problems loading them.
#[derive(Default)]
pub struct LoadedThemes {
    pub themes: Vec<Theme>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Resolve the enabled theme paths of `env`'s profile (global settings, as the client TUI's settings manager reads
/// them) and load them.
pub fn load(env: &crate::Env, mode: ColorMode) -> LoadedThemes {
    let settings = SettingsManager::create(&env.cwd, &env.agent_dir(), false);
    let Ok(resolved) = PackageManager::new(env, settings).resolve() else {
        return LoadedThemes::default();
    };
    let paths: Vec<String> =
        resolved
            .themes
            .into_iter()
            .filter(|r| r.enabled)
            .map(|r| r.path)
            .fold(Vec::new(), |mut paths, path| {
                // `mergePaths`: each path once.
                if !paths.contains(&path) {
                    paths.push(path);
                }
                paths
            });
    load_paths(&paths, mode)
}

/// `loadThemes(paths, false)` then `dedupeThemes`.
pub fn load_paths(paths: &[String], mode: ColorMode) -> LoadedThemes {
    let mut loaded = LoadedThemes::default();
    let load_file = |path: &Path, loaded: &mut LoadedThemes| match load_theme_from_path(path, mode) {
        Ok(theme) => loaded.themes.push(theme),
        Err(_) => loaded.diagnostics.push(("warning".into(), path.display().to_string())),
    };
    for path in paths.iter().map(Path::new) {
        if !path.exists() {
            loaded.diagnostics.push(("warning".into(), path.display().to_string()));
        } else if path.is_dir() {
            // `loadThemesFromDir`: its `.json` files (symlinks followed), in name order.
            let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(path)
                .map(|dir| dir.flatten().map(|e| e.path()).collect())
                .unwrap_or_default();
            files.sort();
            for file in files {
                if file.is_file() && file.extension().is_some_and(|e| e == "json") {
                    load_file(&file, &mut loaded);
                }
            }
        } else if path.is_file() && path.extension().is_some_and(|e| e == "json") {
            load_file(path, &mut loaded);
        } else {
            loaded.diagnostics.push(("warning".into(), path.display().to_string()));
        }
    }
    let mut kept: Vec<Theme> = Vec::new();
    for theme in loaded.themes {
        let name = theme.name.clone().unwrap_or_else(|| "unnamed".into());
        if kept
            .iter()
            .any(|k| k.name.clone().unwrap_or_else(|| "unnamed".into()) == name)
        {
            loaded
                .diagnostics
                .push(("collision".into(), theme.source_path.clone().unwrap_or_default()));
        } else {
            kept.push(theme);
        }
    }
    LoadedThemes {
        themes: kept,
        diagnostics: loaded.diagnostics,
    }
}
