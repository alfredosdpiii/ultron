//! `ultron auth check|print-api-key|print-bearer-token` (`cli/auth-command.ts`, `cli/auth-check.ts`,
//! `cli/credential-print.ts`): whether a provider has usable credentials, and the credential itself.
//!
//! Credentials come from `auth.json` (an `api_key` or an `oauth` entry) and the provider's environment variables, as
//! `resolveProviderAuth` reads them. Not ported: refreshing an OAuth token that expires within the requested window
//! (a network call; reported as a failed resolution), `--model` resolution, `models.json` and extension providers,
//! and the `claude-code` provider's CLI probe.

use crate::Env;
use crate::args::{Args, parse_args};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Check,
    ApiKey,
    BearerToken,
}

#[derive(Debug, PartialEq)]
pub struct AuthCommand {
    pub kind: Kind,
    pub args: Vec<String>,
    pub json: bool,
    pub credentials: bool,
    pub no_refresh: bool,
    pub min_expiry_ms: Option<f64>,
}

const HELP: &str = "Usage:
  pi auth print-api-key [--provider <provider>] [--model <model>]
  pi auth print-bearer-token [--provider <provider>] [--model <model>] [--min-expiry <duration>]
  pi auth check [--provider <provider>] [--model <model>] [--json] [--credentials] [--no-refresh]

Auth commands require at least one of --provider or --model. Checks refresh expired OAuth credentials by default; --no-refresh prevents this. --credentials emits the credential, or includes it in JSON output.";

/// Providers with an OAuth login (`provider.auth.oauth`).
const OAUTH_PROVIDERS: [&str; 8] = [
    "anthropic",
    "github-copilot",
    "kimi-coding",
    "meta",
    "openai-codex",
    "openrouter",
    "radius",
    "xai",
];
/// Providers without API-key auth.
const NO_API_KEY: [&str; 1] = ["openai-codex"];
const DEFAULT_BEARER_TOKEN_MIN_EXPIRY_MS: f64 = 30.0 * 60_000.0;
const MIN_OAUTH_VALIDITY_MS: f64 = 5.0 * 60_000.0;

fn name(kind: Kind) -> &'static str {
    match kind {
        Kind::Check => "auth check",
        Kind::ApiKey => "auth print-api-key",
        Kind::BearerToken => "auth print-bearer-token",
    }
}

fn usage(kind: Kind) -> &'static str {
    match kind {
        Kind::Check => "ultron auth check --provider <provider> [--json] [--credentials] [--no-refresh]",
        Kind::ApiKey => "ultron auth print-api-key --provider <provider> [--model <model>]",
        Kind::BearerToken => {
            "ultron auth print-bearer-token --provider <provider> [--model <model>] [--min-expiry <duration>]"
        }
    }
}

pub fn is_help(args: &[String]) -> bool {
    args.first().map(String::as_str) == Some("auth")
        && (args.get(1).is_none_or(|a| a == "help") || args.iter().any(|a| a == "--help" || a == "-h"))
}

/// `parseAuthCommand`: None when `args` is not an auth command.
pub fn parse_auth_command(args: &[String]) -> Option<Result<AuthCommand, String>> {
    if args.first().map(String::as_str) != Some("auth") {
        return None;
    }
    let kind = match args.get(1).map(String::as_str) {
        Some("check") => Kind::Check,
        Some("print-api-key") => Kind::ApiKey,
        Some("print-bearer-token") => Kind::BearerToken,
        other => {
            return Some(Err(format!(
                "Unknown auth command \"{}\". Use \"ultron auth print-api-key\", \"ultron auth print-bearer-token\", or \"ultron auth check\".",
                other.unwrap_or("")
            )));
        }
    };
    let mut command = AuthCommand {
        kind,
        args: Vec::new(),
        json: false,
        credentials: false,
        no_refresh: false,
        min_expiry_ms: None,
    };
    let mut index = 2;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--min-expiry" {
            if kind != Kind::BearerToken {
                return Some(Err("--min-expiry is only supported by print-bearer-token".into()));
            }
            index += 1;
            match args.get(index).and_then(|v| parse_duration(v)) {
                Some(ms) => command.min_expiry_ms = Some(ms),
                None => return Some(Err("--min-expiry must use a duration such as 30m or 1h".into())),
            }
        } else if arg == "--json" || arg == "--credentials" || arg == "--no-refresh" {
            if kind != Kind::Check {
                return Some(Err(format!("{arg} is only supported by auth check")));
            }
            match arg.as_str() {
                "--json" => command.json = true,
                "--credentials" => command.credentials = true,
                _ => command.no_refresh = true,
            }
        } else {
            command.args.push(arg.clone());
        }
        index += 1;
    }
    Some(Ok(command))
}

/// `/^(\d+)(ms|s|m|h)$/iu`. Units are compared case-sensitively afterwards, as the TypeScript does: `1H` matches
/// but multiplies by an hour only through the final branch.
fn parse_duration(value: &str) -> Option<f64> {
    let digits = value.bytes().take_while(u8::is_ascii_digit).count();
    let (number, unit) = value.split_at(digits);
    if digits == 0 || !["ms", "s", "m", "h"].contains(&unit.to_ascii_lowercase().as_str()) {
        return None;
    }
    let amount: f64 = number.parse().ok()?;
    Some(
        amount
            * match unit {
                "ms" => 1.0,
                "s" => 1_000.0,
                "m" => 60_000.0,
                _ => 3_600_000.0,
            },
    )
}

/// `validateAuthCommandArgs`.
fn validate(args: &Args, kind: Kind) -> Result<(Option<String>, Option<String>), String> {
    let trimmed = |v: &Option<String>| {
        v.as_deref()
            .map(crate::js::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let (provider, model) = (trimmed(&args.provider), trimmed(&args.model));
    if let Some((option, _)) = args.unknown_flags.first() {
        return Err(format!("Unknown option --{option} for \"{}\".", name(kind)));
    }
    if args.api_key.is_some() || !args.messages.is_empty() || !args.file_args.is_empty() {
        return Err("Auth commands only accept --provider and --model".into());
    }
    if provider.is_none() && model.is_none() {
        return Err(if kind == Kind::Check {
            "Auth checks require --provider <provider> or --model <model>".into()
        } else {
            "Credential printing requires --provider <provider> or --model <model>".into()
        });
    }
    Ok((provider, model))
}

/// A resolution that needs what is not ported (an OAuth refresh, `--model`): TypeScript would try it.
struct Unsupported;

struct Credentials<'a> {
    env: &'a Env,
    storage: config::auth::AuthStorage,
}

impl Credentials<'_> {
    fn known(provider: &str) -> bool {
        ai::models::builtin_providers().any(|p| p == provider) || provider == "claude-code"
    }

    fn read(&mut self, provider: &str) -> Result<Option<Value>, Unsupported> {
        self.storage.read(provider).map_err(|_| Unsupported)
    }

    fn list(&mut self) -> Result<Vec<(String, Value)>, Unsupported> {
        self.storage.list().map_err(|_| Unsupported)
    }

    fn env_key(&self, provider: &str) -> Option<String> {
        let source = ai::env::EnvSource {
            lookup: &*self.env.var,
            exists: &|p: &std::path::Path| p.exists(),
            home: Some(self.env.home.clone()),
        };
        ai::env::env_api_key_in(provider, &source)
    }

    /// `resolveProviderAuth`: the credential and its kind (`api_key` or `oauth`).
    fn resolve(
        &mut self,
        provider: &str,
        min_validity: Option<f64>,
    ) -> Result<Option<(String, &'static str)>, Unsupported> {
        let oauth = OAUTH_PROVIDERS.contains(&provider);
        let api_key = !NO_API_KEY.contains(&provider) && provider != "claude-code";
        match self.read(provider)? {
            Some(stored) => match stored.get("type").and_then(Value::as_str) {
                Some("oauth") if oauth => {
                    let expires = stored.get("expires").and_then(Value::as_f64).unwrap_or(0.0);
                    let minimum = MIN_OAUTH_VALIDITY_MS.max(min_validity.unwrap_or(0.0));
                    if now_ms() + minimum >= expires {
                        return Err(Unsupported);
                    }
                    Ok(stored
                        .get("access")
                        .and_then(Value::as_str)
                        .map(|a| (a.to_string(), "oauth")))
                }
                Some("api_key") if api_key => Ok(stored
                    .get("key")
                    .and_then(Value::as_str)
                    .filter(|k| !k.is_empty())
                    .map(|k| (k.to_string(), "api_key"))),
                _ => Ok(None),
            },
            None if api_key => Ok(self.env_key(provider).map(|k| (k, "api_key"))),
            None => Ok(None),
        }
    }

    /// `checkAuth`: whether the provider is configured, and how.
    fn check(&mut self, provider: &str) -> Result<Option<&'static str>, Unsupported> {
        if let Some(stored) = self.read(provider)?
            && stored.get("type").and_then(Value::as_str) == Some("oauth")
        {
            return Ok(OAUTH_PROVIDERS.contains(&provider).then_some("oauth"));
        }
        Ok(self.resolve(provider, None)?.map(|(_, kind)| kind))
    }
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_millis() as f64)
}

fn check(command: &AuthCommand, parsed: &Args, env: &Env) -> Result<(Value, Option<String>), String> {
    let (provider, model) = validate(parsed, Kind::Check)?;
    let invalid = |p: &str| json!({"status": "invalid", "provider": p, "reason": "invalid_state"});
    if model.is_some() {
        // ponytail: resolveCliModel is not ported; a --model check reports the state as unusable.
        return Ok((invalid(model.as_deref().unwrap_or_default()), None));
    }
    let provider = provider.unwrap_or_default();
    let path = config::paths::AgentPaths::new(env.agent_dir()).auth();
    if command.no_refresh
        && let Ok(content) = std::fs::read_to_string(&path)
        && config::auth::validate_auth_file(&content).is_err()
    {
        return Ok((invalid(&provider), None));
    }
    let mut credentials = Credentials {
        env,
        storage: config::auth::AuthStorage::create(path),
    };
    if credentials.list().is_err() {
        return Ok((invalid(&provider), None));
    }
    if !Credentials::known(&provider) {
        return Ok((
            json!({"status": "not_ready", "provider": provider, "reason": "provider_not_found"}),
            None,
        ));
    }
    let not_configured = json!({"status": "not_ready", "provider": provider, "reason": "credentials_not_configured"});
    let outcome = (|| -> Result<(Value, Option<String>), Unsupported> {
        let Some(auth_type) = credentials.check(&provider)? else {
            return Ok((not_configured.clone(), None));
        };
        if !command.no_refresh && credentials.resolve(&provider, None)?.is_none() {
            return Ok((not_configured.clone(), None));
        }
        let ready = json!({"status": "ready", "provider": provider, "authType": auth_type});
        if !command.credentials {
            return Ok((ready, None));
        }
        let stored = credentials.read(&provider)?;
        let credential = match stored {
            Some(s) if command.no_refresh && s.get("type").and_then(Value::as_str) == Some("oauth") => {
                s.get("access").and_then(Value::as_str).map(str::to_string)
            }
            // Ambient AWS or Google credentials resolve to a marker, not a credential to print.
            _ => credentials
                .resolve(&provider, None)?
                .map(|(c, _)| c)
                .filter(|c| c != "<authenticated>"),
        };
        Ok(match credential {
            Some(c) => (ready, Some(c)),
            None => (
                json!({"status": "not_ready", "provider": provider, "reason": "credential_not_available"}),
                None,
            ),
        })
    })();
    Ok(outcome.unwrap_or_else(|Unsupported| (invalid(&provider), None)))
}

fn print_credential(command: &AuthCommand, parsed: &Args, env: &Env) -> Result<String, String> {
    let kind = command.kind;
    let (provider, model) = validate(parsed, kind)?;
    let failed = |Unsupported| "Failed to resolve credential".to_string();
    let path = config::paths::AgentPaths::new(env.agent_dir()).auth();
    let mut credentials = Credentials {
        env,
        storage: config::auth::AuthStorage::create(path),
    };
    let types: Vec<(String, Value)> = credentials.list().map_err(failed)?;
    let type_of = |id: &str| {
        types
            .iter()
            .find(|(p, _)| p == id)
            .and_then(|(_, t)| t.as_str().map(str::to_string))
    };
    let Some(provider) = provider else {
        // ponytail: resolving --model across providers needs resolveCliModel, which is not ported.
        return Err(format!(
            "Model \"{}\" not found. Use --list-models to see available models.",
            model.unwrap_or_default()
        ));
    };
    if !Credentials::known(&provider) {
        return Err(format!(
            "Unknown provider \"{provider}\". Use --list-models to see available providers."
        ));
    }
    if model.is_some() {
        return Err("Unable to resolve the requested provider/model".into());
    }
    let credential_type = type_of(&provider);
    let mut found = None;
    let skip = match kind {
        Kind::ApiKey => credential_type.as_deref() == Some("oauth"),
        _ => credential_type.as_deref() != Some("oauth"),
    };
    if !skip {
        let min =
            (kind == Kind::BearerToken).then_some(command.min_expiry_ms.unwrap_or(DEFAULT_BEARER_TOKEN_MIN_EXPIRY_MS));
        found = credentials
            .resolve(&provider, min)
            .map_err(failed)?
            .map(|(c, _)| c)
            .filter(|c| c != "<authenticated>");
    }
    if let Some(value) = found {
        return Ok(value);
    }
    if kind == Kind::ApiKey && credential_type.as_deref() == Some("oauth") {
        return Err(format!(
            "Provider \"{provider}\" is configured with OAuth, not an API key"
        ));
    }
    if kind == Kind::BearerToken && credential_type.as_deref() != Some("oauth") {
        return Err(format!(
            "Provider \"{provider}\" is not configured with an OAuth bearer token"
        ));
    }
    Err(format!(
        "No usable {} is configured",
        if kind == Kind::ApiKey {
            "API key"
        } else {
            "OAuth bearer token"
        }
    ))
}

/// Handle `auth ...`; None when `argv` is not an auth command, else the exit code.
pub fn run(argv: &[String], env: &Env, out: &mut dyn std::io::Write, err: &mut dyn std::io::Write) -> Option<i32> {
    if is_help(argv) {
        let _ = writeln!(out, "{HELP}");
        return Some(0);
    }
    let command = match parse_auth_command(argv)? {
        Ok(command) => command,
        Err(message) => {
            let _ = writeln!(err, "Error: {message}");
            return Some(1);
        }
    };
    let parsed = parse_args(&command.args);
    if let Some((option, _)) = parsed.unknown_flags.first() {
        let _ = writeln!(err, "Unknown option --{option} for \"{}\".", name(command.kind));
        let _ = writeln!(err, "Use \"ultron --help\" or \"{}\".", usage(command.kind));
        return Some(1);
    }
    let fail_code = if command.kind == Kind::Check { 2 } else { 1 };
    if !parsed.diagnostics.is_empty() {
        let messages: Vec<&str> = parsed.diagnostics.iter().map(|d| d.message.as_str()).collect();
        let _ = writeln!(err, "Error: {}", messages.join("\n"));
        return Some(fail_code);
    }
    if command.kind != Kind::Check {
        return Some(match print_credential(&command, &parsed, env) {
            Ok(value) => {
                let _ = writeln!(out, "{value}");
                0
            }
            Err(message) => {
                let _ = writeln!(err, "Error: {message}");
                1
            }
        });
    }
    match check(&command, &parsed, env) {
        Ok((mut result, credential)) => {
            let status = result["status"].as_str().unwrap_or_default().to_string();
            let output = if command.json {
                if let Some(c) = &credential {
                    result["credentials"] = Value::String(c.clone());
                }
                session_file::js::stringify(&result)
            } else {
                credential.unwrap_or_else(|| status.clone())
            };
            let _ = writeln!(out, "{output}");
            Some(match status.as_str() {
                "ready" => 0,
                "not_ready" => 1,
                _ => 2,
            })
        }
        Err(message) => {
            let _ = writeln!(err, "Error: {message}");
            Some(2)
        }
    }
}
