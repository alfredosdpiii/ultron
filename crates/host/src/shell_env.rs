//! The command environment, ported from utils/shell.ts `getShellEnv`: the PATH variable (found
//! case-insensitively, else `PATH`) gets the agent's bin dir prepended unless it already lists it exactly.

/// `env` (in order) with the agent bin dir on PATH; a missing PATH is appended at the end.
pub fn shell_env(env: &[(String, String)], bin_dir: &str) -> Vec<(String, String)> {
    let key = env
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("path"))
        .map_or("PATH", |(key, _)| key.as_str())
        .to_owned();
    let current = env
        .iter()
        .find(|(k, _)| *k == key)
        .map_or("", |(_, value)| value.as_str());
    let path = if current.split(':').any(|entry| entry == bin_dir) {
        current.to_owned()
    } else if current.is_empty() {
        bin_dir.to_owned()
    } else {
        format!("{bin_dir}:{current}")
    };
    let mut out: Vec<(String, String)> = env.to_vec();
    match out.iter_mut().find(|(k, _)| *k == key) {
        Some(entry) => entry.1 = path,
        None => out.push((key, path)),
    }
    out
}
