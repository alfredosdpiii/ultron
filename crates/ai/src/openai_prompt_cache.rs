//! `api/openai-prompt-cache.ts`: OpenAI prompt cache keys are at most 64 characters (code points).

pub const OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH: usize = 64;

/// The first 64 code points of `key`.
pub fn clamp_openai_prompt_cache_key(key: &str) -> String {
    key.chars().take(OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH).collect()
}
