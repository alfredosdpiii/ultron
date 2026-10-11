//! Cloudflare endpoints (`api/cloudflare.ts`, `providers/cloudflare-stream.ts`, the HTTP side of
//! `api/cloudflare-ai-binding.ts`): base URLs with `{CLOUDFLARE_ACCOUNT_ID}`/`{CLOUDFLARE_GATEWAY_ID}` placeholders that
//! materialize from the resolved provider env before dispatch. Requests then go through the API implementation of the
//! model (OpenAI completions/responses, Anthropic) unchanged.

use crate::env::ProviderEnv;
use crate::types::Model;

/// Workers AI direct endpoint.
pub const CLOUDFLARE_WORKERS_AI_BASE_URL: &str =
    "https://api.cloudflare.com/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai/v1";
/// AI Gateway Unified API.
pub const CLOUDFLARE_AI_GATEWAY_COMPAT_BASE_URL: &str =
    "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/compat";
/// AI Gateway OpenAI passthrough.
pub const CLOUDFLARE_AI_GATEWAY_OPENAI_BASE_URL: &str =
    "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/openai";
/// AI Gateway Anthropic passthrough.
pub const CLOUDFLARE_AI_GATEWAY_ANTHROPIC_BASE_URL: &str =
    "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/anthropic";
/// Placeholder `cf-aig-authorization` bearer for requests routed through a Workers AI binding, which are
/// pre-authenticated (`CLOUDFLARE_GATEWAY_BINDING_AUTH_SENTINEL`).
pub const CLOUDFLARE_GATEWAY_BINDING_AUTH_SENTINEL: &str = "cloudflare-gateway-binding";

/// `resolveCloudflareModel`: replace the placeholders present in `env`; a model without an env, or whose URL does not
/// change, is returned as is.
pub fn resolve_cloudflare_model(model: &Model, env: Option<&ProviderEnv>) -> Model {
    let Some(env) = env else {
        return model.clone();
    };
    let mut base_url = model.base_url.clone();
    for name in ["CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_GATEWAY_ID"] {
        if let Some(value) = env.get(name) {
            base_url = base_url.replace(&format!("{{{name}}}"), value);
        }
    }
    Model {
        base_url,
        ..model.clone()
    }
}

// ------------------------------------------------------------------------------------------------------- auth

use crate::auth::{
    ApiKeyAuth, ApiKeyCredential, AuthContext, AuthInteraction, AuthPrompt, AuthResult, CredentialEnv, ModelAuth,
};
use crate::http::BoxFuture;

/// Which Cloudflare endpoint the auth is for (`providers/cloudflare-auth.ts`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloudflareAuthKind {
    WorkersAi,
    AiGateway,
}

/// `cloudflareWorkersAIAuth` / `cloudflareAIGatewayAuth`: the API key plus account (and gateway) id, merged per
/// field from the stored credential and the environment.
pub struct CloudflareAuth(pub CloudflareAuthKind);

fn resolve_value(name: &str, ctx: &dyn AuthContext, credential: Option<&ApiKeyCredential>) -> Option<String> {
    let from_credential = credential.and_then(|c| {
        if name == "CLOUDFLARE_API_KEY" {
            c.key.clone()
        } else {
            c.env.as_ref().and_then(|env| env.get(name).cloned())
        }
    });
    if credential.is_some() && from_credential.is_some() {
        return from_credential;
    }
    ctx.env(name)
}

impl ApiKeyAuth for CloudflareAuth {
    fn name(&self) -> &str {
        "Cloudflare API key"
    }

    fn login<'a>(
        &'a self,
        interaction: &'a dyn AuthInteraction,
    ) -> Option<BoxFuture<'a, Result<ApiKeyCredential, String>>> {
        Some(Box::pin(async move {
            let ask = |kind: &str, message: &str| match kind {
                "secret" => AuthPrompt::Secret {
                    message: message.into(),
                    placeholder: None,
                },
                _ => AuthPrompt::Text {
                    message: message.into(),
                    placeholder: None,
                },
            };
            let key = interaction
                .prompt(ask("secret", "Enter Cloudflare API key"), None)
                .await?;
            let account = interaction
                .prompt(ask("text", "Enter Cloudflare account ID"), None)
                .await?;
            let mut env = CredentialEnv::new();
            env.insert("CLOUDFLARE_ACCOUNT_ID".into(), account);
            if self.0 == CloudflareAuthKind::AiGateway {
                let gateway = interaction
                    .prompt(ask("text", "Enter Cloudflare AI Gateway ID"), None)
                    .await?;
                env.insert("CLOUDFLARE_GATEWAY_ID".into(), gateway);
            }
            Ok(ApiKeyCredential {
                key: Some(key),
                env: Some(env),
            })
        }))
    }

    fn resolve<'a>(
        &'a self,
        ctx: &'a dyn AuthContext,
        credential: Option<&'a ApiKeyCredential>,
    ) -> BoxFuture<'a, Result<Option<AuthResult>, String>> {
        Box::pin(async move {
            let api_key = resolve_value("CLOUDFLARE_API_KEY", ctx, credential).filter(|v| !v.is_empty());
            let account = resolve_value("CLOUDFLARE_ACCOUNT_ID", ctx, credential).filter(|v| !v.is_empty());
            let gateway = match self.0 {
                CloudflareAuthKind::AiGateway => {
                    resolve_value("CLOUDFLARE_GATEWAY_ID", ctx, credential).filter(|v| !v.is_empty())
                }
                CloudflareAuthKind::WorkersAi => None,
            };
            let (Some(api_key), Some(account)) = (api_key, account) else {
                return Ok(None);
            };
            if self.0 == CloudflareAuthKind::AiGateway && gateway.is_none() {
                return Ok(None);
            }
            let mut env = CredentialEnv::new();
            env.insert("CLOUDFLARE_ACCOUNT_ID".into(), account);
            if let Some(gateway) = gateway {
                env.insert("CLOUDFLARE_GATEWAY_ID".into(), gateway);
            }
            let auth = match self.0 {
                CloudflareAuthKind::WorkersAi => ModelAuth {
                    api_key: Some(api_key),
                    ..Default::default()
                },
                CloudflareAuthKind::AiGateway => ModelAuth {
                    headers: Some(
                        [
                            ("cf-aig-authorization".to_string(), Some(format!("Bearer {api_key}"))),
                            ("Authorization".to_string(), None),
                            ("x-api-key".to_string(), None),
                        ]
                        .into_iter()
                        .collect(),
                    ),
                    ..Default::default()
                },
            };
            Ok(Some(AuthResult {
                auth,
                env: Some(env),
                source: Some(if credential.is_some() {
                    "stored credential".into()
                } else {
                    "CLOUDFLARE_API_KEY".into()
                }),
            }))
        })
    }
}
