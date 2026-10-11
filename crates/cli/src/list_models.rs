//! `listModels` from packages/coding-agent/src/cli/list-models.ts over the built-in catalog.

use ai::Model;
use std::cmp::Ordering;
use std::path::Path;

/// Built-in models whose provider has an API key in the environment or an entry in auth.json.
pub fn available_models(env: &crate::Env) -> Vec<&'static Model> {
    let auth = config::paths::AgentPaths::new(env.agent_dir()).auth();
    let source = ai::env::EnvSource {
        lookup: &*env.var,
        exists: &|p: &Path| p.exists(),
        home: Some(env.home.clone()),
    };
    ai::models::builtin_providers()
        .filter(|p| {
            ai::env::env_api_key_in(p, &source).is_some() || config::auth::read_stored_credential(&auth, p).is_some()
        })
        .flat_map(ai::models::builtin_models)
        .collect()
}

/// `formatTokenCount`: 200000 -> "200K", 1000000 -> "1M", 131072 -> "131.1K" (`toFixed(1)` rounds ties up).
pub fn format_token_count(count: u64) -> String {
    for (unit, suffix) in [(1_000_000, "M"), (1_000, "K")] {
        if count >= unit {
            if count.is_multiple_of(unit) {
                return format!("{}{suffix}", count / unit);
            }
            let tenths = (count * 10 + unit / 2) / unit;
            return format!("{}.{}{suffix}", tenths / 10, tenths % 10);
        }
    }
    count.to_string()
}

/// ICU root collation as `String.prototype.localeCompare` applies it to catalog ids: punctuation sorts before digits,
/// digits before letters, letters case-insensitively, with lowercase first as the tie-break.
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    const PUNCT: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
    let key = |c: char| -> (u8, u32) {
        if let Some(i) = PUNCT.find(c) {
            (0, i as u32)
        } else if c.is_ascii_digit() {
            (1, c as u32)
        } else {
            (2, c.to_ascii_lowercase() as u32)
        }
    };
    let primary = a.chars().map(key).cmp(b.chars().map(key));
    primary.then_with(|| {
        let tertiary = |c: char| u8::from(c.is_ascii_uppercase());
        a.chars().map(tertiary).cmp(b.chars().map(tertiary))
    })
}

/// The `--list-models` output for `models`, filtered by `search`.
pub fn render(models: &[&'static Model], search: Option<&str>, docs: &Path) -> String {
    if models.is_empty() {
        return format!(
            "No models available. Use /login to log into a provider via OAuth or API key. See:\n  {}\n  {}\n",
            docs.join("providers.md").display(),
            docs.join("models.md").display()
        );
    }
    let mut filtered: Vec<&Model> = match search {
        Some(q) if !q.is_empty() => tui::fuzzy::fuzzy_filter(models, q, |m| format!("{} {}", m.provider, m.id)),
        _ => models.to_vec(),
    };
    if filtered.is_empty() {
        return format!("No models matching \"{}\"\n", search.unwrap_or_default());
    }
    filtered.sort_by(|a, b| locale_compare(&a.provider, &b.provider).then_with(|| locale_compare(&a.id, &b.id)));
    let yes_no = |b: bool| if b { "yes" } else { "no" };
    let mut rows = vec![["provider", "model", "context", "max-out", "thinking", "images"].map(String::from)];
    rows.extend(filtered.iter().map(|m| {
        [
            m.provider.clone(),
            m.id.clone(),
            format_token_count(m.context_window),
            format_token_count(m.max_tokens),
            yes_no(m.reasoning).into(),
            yes_no(m.input.iter().any(|i| i == "image")).into(),
        ]
    }));
    let widths: Vec<usize> = (0..6)
        .map(|c| rows.iter().map(|r| r[c].encode_utf16().count()).max().unwrap_or(0))
        .collect();
    let mut text = String::new();
    for row in &rows {
        let cells: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, w)| format!("{cell}{}", " ".repeat(w - cell.encode_utf16().count())))
            .collect();
        text.push_str(&cells.join("  "));
        text.push('\n');
    }
    text
}
