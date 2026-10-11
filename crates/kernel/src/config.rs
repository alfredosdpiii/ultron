//! Kernel configuration: the environment the kernel receives, its resource limits and the output budget.
//! Ports of `kernelEnvironment`, `kernelResourceLimits` (kernel.ts) and `rlmOutputBudget` (output-truncation.ts).

use crate::KernelError;
use std::collections::HashSet;
use std::ffi::OsStr;

/// Names (ASCII case-insensitive) that mark a variable as a credential: kernel.ts `CREDENTIAL_NAME`.
const CREDENTIAL_WORDS: &[&str] = &[
    "APIKEY",
    "API_KEY",
    "ACCESSKEY",
    "ACCESS_KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "PRIVATEKEY",
    "PRIVATE_KEY",
    "AUTH",
    "COOKIE",
    "SESSIONKEY",
    "SESSION_KEY",
    "WEBHOOK",
];

/// Environment for model-written Python: credentials and the session worker's control variables are removed so the
/// kernel gets no ambient authority. `ULTRON_RLM_ENV_ALLOW` lists names (comma-separated) to pass through anyway.
pub fn kernel_environment<K: AsRef<OsStr>, V: AsRef<OsStr>>(source: impl IntoIterator<Item = (K, V)>) -> Vec<(K, V)> {
    let source: Vec<(K, V)> = source.into_iter().collect();
    let allowed: HashSet<String> = source
        .iter()
        .find(|(name, _)| name.as_ref() == "ULTRON_RLM_ENV_ALLOW")
        .map(|(_, value)| {
            value
                .as_ref()
                .to_string_lossy()
                .split(',')
                .map(|name| name.trim_matches(is_js_space).to_string())
                .filter(|name| !name.is_empty())
                .collect()
        })
        .unwrap_or_default();
    source
        .into_iter()
        .filter(|(name, _)| {
            let name = name.as_ref().to_string_lossy();
            let control = name.starts_with("PI_SESSION_WORKER_") || name.starts_with("ULTRON_SESSION_WORKER_");
            let upper = name.to_ascii_uppercase();
            let credential = CREDENTIAL_WORDS.iter().any(|word| upper.contains(word));
            allowed.contains(name.as_ref()) || !(control || credential)
        })
        .collect()
}

/// Per-kernel resource limits; 0 disables one. Resource limits, not isolation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelResourceLimits {
    /// RLIMIT_DATA in MiB for the kernel and, inherited, every process a cell spawns.
    pub max_memory_mb: u64,
    /// CPU time per cell.
    pub max_cpu_seconds: u64,
}

pub const DEFAULT_KERNEL_LIMITS: KernelResourceLimits = KernelResourceLimits {
    max_memory_mb: 4096,
    max_cpu_seconds: 1800,
};

/// Explicit limits that take precedence over the environment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LimitOverrides {
    pub max_memory_mb: Option<u64>,
    pub max_cpu_seconds: Option<u64>,
}

const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// Limits from `ULTRON_RLM_MAX_MEMORY_MB` and `ULTRON_RLM_MAX_CPU_SECONDS` (read through `lookup`), then overrides.
/// A malformed value falls back to the default; a value over 2^53 - 1 is an error, as in TypeScript.
pub fn kernel_resource_limits(
    lookup: impl Fn(&str) -> Option<String>,
    overrides: LimitOverrides,
) -> Result<KernelResourceLimits, KernelError> {
    let value = |name: &str, fallback: u64| -> Result<u64, ()> {
        let Some(raw) = lookup(name) else {
            return Ok(fallback);
        };
        let digits = raw.trim_matches(is_js_space);
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return Ok(fallback);
        }
        match digits.parse::<u64>() {
            Ok(number) if number <= MAX_SAFE_INTEGER => Ok(number),
            _ => Err(()),
        }
    };
    let field = |name: &str, override_value: Option<u64>, env: &str, fallback: u64| {
        let parsed = match override_value {
            Some(number) if number <= MAX_SAFE_INTEGER => Ok(number),
            Some(_) => Err(()),
            None => value(env, fallback),
        };
        parsed.map_err(|()| KernelError::new(format!("RLM {name} must be a non-negative integer")))
    };
    Ok(KernelResourceLimits {
        max_memory_mb: field(
            "maxMemoryMb",
            overrides.max_memory_mb,
            "ULTRON_RLM_MAX_MEMORY_MB",
            DEFAULT_KERNEL_LIMITS.max_memory_mb,
        )?,
        max_cpu_seconds: field(
            "maxCpuSeconds",
            overrides.max_cpu_seconds,
            "ULTRON_RLM_MAX_CPU_SECONDS",
            DEFAULT_KERNEL_LIMITS.max_cpu_seconds,
        )?,
    })
}

const DEFAULT_RLM_OUTPUT_BYTES: u64 = 20_000;
const MAX_RLM_OUTPUT_BYTES: u64 = 128 * 1024;

/// The byte budget for one cell's captured stream: `ULTRON_RLM_OUTPUT_BYTES` (as JavaScript's `Number` reads it),
/// else 20 KB, within 1 KB..128 KiB.
pub fn rlm_output_budget(value: Option<&str>) -> u64 {
    let raw = value.unwrap_or("").trim_matches(is_js_space);
    match js_number(raw) {
        Some(number) if number.fract() == 0.0 && number.is_finite() && number > 0.0 => {
            number.clamp(1024.0, MAX_RLM_OUTPUT_BYTES as f64) as u64
        }
        _ => DEFAULT_RLM_OUTPUT_BYTES,
    }
}

/// JavaScript's whitespace for `trim` and `\s`: Unicode White_Space without U+0085, plus U+FEFF.
pub(crate) fn is_js_space(character: char) -> bool {
    character != '\u{85}' && (character.is_whitespace() || character == '\u{feff}')
}

/// `Number(text)` for already-trimmed, non-empty text; None where JavaScript yields NaN.
fn js_number(text: &str) -> Option<f64> {
    let radix = match text.get(..2) {
        Some("0x" | "0X") => Some(16),
        Some("0o" | "0O") => Some(8),
        Some("0b" | "0B") => Some(2),
        _ => None,
    };
    if let Some(radix) = radix {
        let digits = &text[2..];
        if digits.is_empty() || !digits.chars().all(|digit| digit.is_digit(radix)) {
            return None;
        }
        return Some(digits.chars().fold(0.0, |total, digit| {
            total * f64::from(radix) + f64::from(digit.to_digit(radix).unwrap_or(0))
        }));
    }
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    if unsigned == "Infinity" {
        return Some(if text.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(index) => (&unsigned[..index], Some(&unsigned[index + 1..])),
        None => (unsigned, None),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    let exponent_ok = exponent.is_none_or(|exponent| {
        let exponent = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        !exponent.is_empty() && digits(exponent)
    });
    if (whole.is_empty() && fraction.is_empty()) || !digits(whole) || !digits(fraction) || !exponent_ok {
        return None;
    }
    if text.is_empty() {
        return Some(0.0);
    }
    text.parse::<f64>().ok()
}
