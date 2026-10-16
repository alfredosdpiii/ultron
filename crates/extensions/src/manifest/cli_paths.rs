//! Pi's `-e`/`--extension` paths (`additionalExtensionPaths` of `DefaultResourceLoader`): extensions named on the
//! command line load before the discovered ones, and still load with `--no-extensions`. Each path is read like a
//! settings entry (a manifest file, a directory holding `extension.json`, or a directory of such directories);
//! a path that does not exist is reported as TypeScript reports it. Package sources (`npm:`, `git:`, URLs) are not
//! installed from the command line here: they are reported instead.

use super::{Collector, Diagnostic, Discovery};
use std::path::Path;

/// `isLocalPath`: not a package source or remote URL (`npm:`, `git:`, `github:`, `http:`, `https:`, `ssh:`).
pub fn is_local_path(value: &str) -> bool {
    let trimmed = value.trim();
    !["npm:", "git:", "github:", "http:", "https:", "ssh:"]
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

/// The manifests of the `-e` paths (absolute, or relative to `cwd`; `~` expands to `home`), in order.
pub fn discover_cli_paths(paths: &[String], cwd: &Path, home: &Path) -> Discovery {
    let mut collector = Collector {
        out: Discovery::default(),
    };
    for path in paths {
        if is_local_path(path) {
            for manifest in collector.settings_entry(path, cwd, home) {
                collector.load(&manifest, true);
            }
        } else {
            collector.out.diagnostics.push(Diagnostic {
                path: cwd.to_path_buf(),
                message: format!(
                    "Extension package \"{path}\" is not installed from the command line; install it with \"ultron install {path}\""
                ),
            });
        }
    }
    collector.out
}

/// The `-e` manifests first, then `discovered` (unless `--no-extensions` left it out), each manifest once at its
/// first place (`mergePaths`).
pub fn merge_discoveries(cli: Discovery, discovered: Option<Discovery>) -> Discovery {
    let mut out = cli;
    if let Some(discovered) = discovered {
        for manifest in discovered.manifests {
            if !out.manifests.iter().any(|m| m.path == manifest.path) {
                out.manifests.push(manifest);
            }
        }
        out.diagnostics.extend(discovered.diagnostics);
    }
    out
}
