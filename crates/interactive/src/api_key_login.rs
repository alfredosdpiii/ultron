//! `/login` with an API key for the providers whose key prompt is not Pi's single generic one (`apiKey.login` of
//! packages/ai providers amazon-bedrock.ts, google-vertex.ts and cloudflare-auth.ts): a choice of method first, or
//! several fields. The flow runs in the client as the TypeScript one does (`runProviderLogin`): each step is a
//! prompt or a notice for the login dialog, and the dialog's answers drive it to the credential `ModelRuntime.login`
//! saves to `auth.json`. It is plain data, so the TUI drives it without waiting on anything.

use serde_json::{Map, Value, json};

use crate::lane::{LoginEvent, LoginPrompt};

/// The providers this flow signs in to.
pub const MULTI_FIELD_PROVIDERS: [&str; 4] = [
    "amazon-bedrock",
    "cloudflare-ai-gateway",
    "cloudflare-workers-ai",
    "google-vertex",
];

/// What the flow does next.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// Show a notice in the dialog (`interaction.notify`).
    Event(LoginEvent),
    /// Ask (`interaction.prompt`); [`ApiKeyLogin::answer`] continues with the answer.
    Prompt(LoginPrompt),
    /// The credential to save (`{type: "api_key", key?, env?}`).
    Done(Value),
    /// The flow failed with this error.
    Failed(String),
}

/// One provider's login in progress: the answers so far.
#[derive(Clone, Debug)]
pub struct ApiKeyLogin {
    provider: String,
    answers: Vec<String>,
}

fn text(message: &str) -> Step {
    Step::Prompt(LoginPrompt::Text {
        message: message.into(),
        placeholder: None,
        secret: false,
    })
}

fn secret(message: &str) -> Step {
    Step::Prompt(LoginPrompt::Text {
        message: message.into(),
        placeholder: None,
        secret: true,
    })
}

fn select(message: &str, options: &[(&str, &str)]) -> Step {
    Step::Prompt(LoginPrompt::Select {
        message: message.into(),
        options: options
            .iter()
            .map(|(id, label)| (id.to_string(), label.to_string()))
            .collect(),
    })
}

fn info(message: &str, label: &str, url: &str) -> Step {
    Step::Event(LoginEvent::Info {
        message: message.into(),
        links: vec![(Some(label.into()), url.into())],
    })
}

fn credential(key: Option<&str>, env: &[(&str, &str)]) -> Step {
    let mut map = Map::new();
    map.insert("type".into(), json!("api_key"));
    if let Some(key) = key {
        map.insert("key".into(), json!(key));
    }
    if !env.is_empty() {
        let env: Map<String, Value> = env.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
        map.insert("env".into(), Value::Object(env));
    }
    Step::Done(Value::Object(map))
}

impl ApiKeyLogin {
    /// Start `provider`'s login: the flow and its first steps (up to the first prompt); `None` for a provider this
    /// flow does not sign in to.
    pub fn start(provider: &str) -> Option<(ApiKeyLogin, Vec<Step>)> {
        if !MULTI_FIELD_PROVIDERS.contains(&provider) {
            return None;
        }
        let login = ApiKeyLogin {
            provider: provider.to_string(),
            answers: Vec::new(),
        };
        let steps = login.steps();
        Some((login, steps))
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// The pending prompt's answer: the steps that follow it.
    pub fn answer(&mut self, answer: String) -> Vec<Step> {
        self.answers.push(answer);
        self.steps()
    }

    /// The steps after the answers so far: notices up to and including the next prompt, or the outcome.
    fn steps(&self) -> Vec<Step> {
        let a: Vec<&str> = self.answers.iter().map(String::as_str).collect();
        match self.provider.as_str() {
            "amazon-bedrock" => bedrock(&a),
            "google-vertex" => vertex(&a),
            "cloudflare-workers-ai" => cloudflare(&a, false),
            _ => cloudflare(&a, true),
        }
    }
}

fn bedrock(a: &[&str]) -> Vec<Step> {
    let method = || {
        select(
            "Select Amazon Bedrock authentication method:",
            &[
                ("bearer-token", "Bearer token"),
                ("aws-profile", "AWS profile"),
                ("credential-chain", "Existing AWS credential chain"),
            ],
        )
    };
    let chain = || {
        info(
            "Amazon Bedrock supports AWS profiles, IAM credentials, and role-based credentials.",
            "AWS credential provider chain",
            "https://docs.aws.amazon.com/sdkref/latest/guide/standardized-credentials.html",
        )
    };
    match a {
        [] => vec![method()],
        ["bearer-token"] => vec![secret("Enter Amazon Bedrock bearer token")],
        ["bearer-token", key] => vec![credential(Some(key), &[])],
        ["aws-profile"] => vec![chain(), text("Enter AWS profile name")],
        ["aws-profile", profile] => vec![credential(None, &[("AWS_PROFILE", profile)])],
        ["credential-chain"] => vec![chain(), text("Configure AWS credentials, then press Enter to continue")],
        ["credential-chain", _] => vec![credential(None, &[])],
        [other, ..] => vec![Step::Failed(format!("Unknown Amazon Bedrock auth method: {other}"))],
    }
}

fn vertex(a: &[&str]) -> Vec<Step> {
    let notice = |method: &str| {
        info(
            if method == "adc" {
                "Run `gcloud auth application-default login`, then provide the project and location."
            } else {
                "Provide a service account credentials file, project, and location."
            },
            "Application Default Credentials",
            "https://cloud.google.com/docs/authentication/provide-credentials-adc",
        )
    };
    match a {
        [] => vec![select(
            "Select Google Vertex AI authentication method:",
            &[
                ("api-key", "Google Cloud API key"),
                ("adc", "Application Default Credentials"),
                ("service-account", "Service account credentials file"),
            ],
        )],
        ["api-key"] => vec![secret("Enter Google Cloud API key")],
        ["api-key", key] => vec![credential(Some(key), &[])],
        [method @ ("adc" | "service-account")] => vec![notice(method), text("Enter Google Cloud project ID")],
        ["adc" | "service-account", _] => vec![text("Enter Google Cloud location")],
        ["adc", project, location] => vec![credential(
            None,
            &[("GOOGLE_CLOUD_PROJECT", project), ("GOOGLE_CLOUD_LOCATION", location)],
        )],
        ["service-account", _, _] => vec![text("Enter service account credentials file path")],
        ["service-account", project, location, path] => {
            let mut env = vec![("GOOGLE_CLOUD_PROJECT", *project), ("GOOGLE_CLOUD_LOCATION", *location)];
            if !path.is_empty() {
                env.push(("GOOGLE_APPLICATION_CREDENTIALS", path));
            }
            vec![credential(None, &env)]
        }
        [other, ..] => vec![Step::Failed(format!("Unknown Google Vertex AI auth method: {other}"))],
    }
}

fn cloudflare(a: &[&str], gateway: bool) -> Vec<Step> {
    match a {
        [] => vec![secret("Enter Cloudflare API key")],
        [_] => vec![text("Enter Cloudflare account ID")],
        [key, account] if !gateway => vec![credential(Some(key), &[("CLOUDFLARE_ACCOUNT_ID", account)])],
        [_, _] => vec![text("Enter Cloudflare AI Gateway ID")],
        [key, account, gateway_id, ..] => vec![credential(
            Some(key),
            &[
                ("CLOUDFLARE_ACCOUNT_ID", account),
                ("CLOUDFLARE_GATEWAY_ID", gateway_id),
            ],
        )],
    }
}
