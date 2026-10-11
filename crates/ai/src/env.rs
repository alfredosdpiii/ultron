//! API-key resolution from environment variables (`env-api-keys.ts`, `utils/provider-env.ts`).

use std::collections::HashMap;
use std::path::PathBuf;

/// Provider-scoped environment overrides; they take precedence over the process environment.
pub type ProviderEnv = HashMap<String, String>;

pub const ANTHROPIC_AUTH_TOKEN_ENV: &str = "ANTHROPIC_AUTH_TOKEN";
pub const ANTHROPIC_OAUTH_TOKEN_ENV: &str = "ANTHROPIC_OAUTH_TOKEN";
pub const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// A non-empty value from `env`, else the process environment (`getProviderEnvValue`).
pub fn provider_env_value(name: &str, env: Option<&ProviderEnv>) -> Option<String> {
    env.and_then(|e| e.get(name))
        .filter(|v| !v.is_empty())
        .cloned()
        .or_else(|| std::env::var(name).ok().filter(|v| !v.is_empty()))
}

fn api_key_env_vars(provider: &str) -> Option<&'static [&'static str]> {
    Some(match provider {
        "github-copilot" => &["COPILOT_GITHUB_TOKEN"],
        "anthropic" => &[
            ANTHROPIC_AUTH_TOKEN_ENV,
            ANTHROPIC_OAUTH_TOKEN_ENV,
            ANTHROPIC_API_KEY_ENV,
        ],
        "ant-ling" => &["ANT_LING_API_KEY"],
        "qwen-token-plan" | "qwen-token-plan-individual" => &["QWEN_TOKEN_PLAN_API_KEY"],
        "qwen-token-plan-cn" => &["QWEN_TOKEN_PLAN_CN_API_KEY"],
        "openai" => &["OPENAI_API_KEY"],
        "azure-openai-responses" => &["AZURE_OPENAI_API_KEY"],
        "nvidia" => &["NVIDIA_API_KEY"],
        "deepseek" => &["DEEPSEEK_API_KEY"],
        "google" => &["GEMINI_API_KEY"],
        "google-vertex" => &["GOOGLE_CLOUD_API_KEY"],
        "groq" => &["GROQ_API_KEY"],
        "cerebras" => &["CEREBRAS_API_KEY"],
        "xai" => &["XAI_API_KEY"],
        "radius" => &["RADIUS_API_KEY"],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "vercel-ai-gateway" => &["AI_GATEWAY_API_KEY"],
        "zai" => &["ZAI_API_KEY"],
        "zai-coding-cn" => &["ZAI_CODING_CN_API_KEY"],
        "mistral" => &["MISTRAL_API_KEY"],
        "minimax" => &["MINIMAX_API_KEY"],
        "minimax-cn" => &["MINIMAX_CN_API_KEY"],
        "moonshotai" | "moonshotai-cn" => &["MOONSHOT_API_KEY"],
        "huggingface" => &["HF_TOKEN"],
        "fireworks" => &["FIREWORKS_API_KEY"],
        "together" => &["TOGETHER_API_KEY"],
        "baseten" => &["BASETEN_API_KEY"],
        "opencode" | "opencode-go" => &["OPENCODE_API_KEY"],
        "kimi-coding" => &["KIMI_API_KEY"],
        "meta" => &["META_API_KEY"],
        "cloudflare-workers-ai" | "cloudflare-ai-gateway" => &["CLOUDFLARE_API_KEY"],
        "xiaomi" => &["XIAOMI_API_KEY"],
        "xiaomi-token-plan-cn" => &["XIAOMI_TOKEN_PLAN_CN_API_KEY"],
        "xiaomi-token-plan-ams" => &["XIAOMI_TOKEN_PLAN_AMS_API_KEY"],
        "xiaomi-token-plan-sgp" => &["XIAOMI_TOKEN_PLAN_SGP_API_KEY"],
        _ => return None,
    })
}

/// Environment lookups and the file-existence check, injectable so tests never read the real environment or home.
pub struct EnvSource<'a> {
    pub lookup: &'a dyn Fn(&str) -> Option<String>,
    pub exists: &'a dyn Fn(&std::path::Path) -> bool,
    pub home: Option<PathBuf>,
}

/// Configured variables that can supply an API key for `provider` (not ambient AWS/Google credentials).
pub fn find_env_keys_in(provider: &str, source: &EnvSource<'_>) -> Option<Vec<&'static str>> {
    let found: Vec<&'static str> = api_key_env_vars(provider)?
        .iter()
        .copied()
        .filter(|name| (source.lookup)(name).is_some())
        .collect();
    (!found.is_empty()).then_some(found)
}

/// The API key for `provider` from known variables; `<authenticated>` for configured Vertex ADC or AWS credentials.
/// `ANTHROPIC_AUTH_TOKEN` is reported by [`find_env_keys_in`] but never returned (it is sent as a bearer token).
pub fn env_api_key_in(provider: &str, source: &EnvSource<'_>) -> Option<String> {
    if let Some(keys) = find_env_keys_in(provider, source) {
        let key = if provider == "anthropic" {
            keys.iter().find(|k| **k != ANTHROPIC_AUTH_TOKEN_ENV).copied()
        } else {
            keys.first().copied()
        };
        if let Some(key) = key {
            return (source.lookup)(key);
        }
    }
    let has = |name: &str| (source.lookup)(name).is_some();
    if provider == "google-vertex" {
        let credentials = match (source.lookup)("GOOGLE_APPLICATION_CREDENTIALS") {
            Some(path) => (source.exists)(std::path::Path::new(&path)),
            None => source
                .home
                .as_ref()
                .is_some_and(|home| (source.exists)(&home.join(".config/gcloud/application_default_credentials.json"))),
        };
        if credentials && (has("GOOGLE_CLOUD_PROJECT") || has("GCLOUD_PROJECT")) && has("GOOGLE_CLOUD_LOCATION") {
            return Some("<authenticated>".into());
        }
    }
    if provider == "amazon-bedrock"
        && (has("AWS_PROFILE")
            || (has("AWS_ACCESS_KEY_ID") && has("AWS_SECRET_ACCESS_KEY"))
            || has("AWS_BEARER_TOKEN_BEDROCK")
            || has("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI")
            || has("AWS_CONTAINER_CREDENTIALS_FULL_URI")
            || has("AWS_WEB_IDENTITY_TOKEN_FILE"))
    {
        return Some("<authenticated>".into());
    }
    None
}

fn with_process_env<T>(env: Option<&ProviderEnv>, f: impl FnOnce(&EnvSource<'_>) -> T) -> T {
    let lookup = |name: &str| provider_env_value(name, env);
    let exists = |path: &std::path::Path| path.exists();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    f(&EnvSource {
        lookup: &lookup,
        exists: &exists,
        home,
    })
}

pub fn find_env_keys(provider: &str, env: Option<&ProviderEnv>) -> Option<Vec<&'static str>> {
    with_process_env(env, |source| find_env_keys_in(provider, source))
}

pub fn env_api_key(provider: &str, env: Option<&ProviderEnv>) -> Option<String> {
    with_process_env(env, |source| env_api_key_in(provider, source))
}
