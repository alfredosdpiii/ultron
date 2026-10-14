//! The GitHub accounts autoreview reviews as (`autoreview/accounts.ts`): every account `gh` is logged in to, on
//! every host. A token is read with `gh auth token`, kept in memory, handed to the secret-masking list (a callback
//! here: the host's masking registry), and given to child `gh`/`git` processes only through their environment.
//!
//! Known differences, outside recorded behaviour: object keys of `gh auth status --json` are visited in the
//! file's order (JavaScript visits integer-like host keys first).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::github::Error;
use crate::runner::{Env, RunOptions, Runner, argv};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Account {
    pub login: String,
    pub host: String,
    /// The account `gh` uses by default on this host.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub active: bool,
}

impl Account {
    pub fn new(login: &str, host: &str) -> Self {
        Self {
            login: login.into(),
            host: host.into(),
            active: false,
        }
    }
}

pub fn account_key(account: &Account) -> String {
    format!("{}/{}", account.host, account.login)
}

/// `(key, value)` pairs of a JSON value as `Object.entries` lists them (strings by character, arrays by index).
fn entries(value: &Value) -> Vec<(String, Value)> {
    match value {
        Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v.clone()))
            .collect(),
        Value::String(text) => text
            .chars()
            .enumerate()
            .map(|(i, c)| (i.to_string(), Value::String(c.to_string())))
            .collect(),
        _ => Vec::new(),
    }
}

/// Accounts from `gh auth status --json hosts`, or from its text form on a `gh` that has no `--json`.
pub fn parse_auth_status(stdout: &str, stderr: &str) -> Vec<Account> {
    let mut accounts: Vec<Account> = Vec::new();
    let mut add = |login: &str, host: &str, active: bool| {
        if !login.is_empty() && !host.is_empty() && !accounts.iter().any(|a| a.login == login && a.host == host) {
            accounts.push(Account {
                login: login.into(),
                host: host.into(),
                active,
            });
        }
    };
    // JSON.parse, then `parsed.hosts`: a null document (or a null entry) throws and falls back to the text form.
    let parsed = serde_json::from_str::<Value>(stdout).ok();
    if let Some(parsed) = parsed.filter(|value| !value.is_null()) {
        let mut failed = false;
        let hosts = parsed.get("hosts").filter(|hosts| !hosts.is_null());
        'hosts: for (host, list) in hosts.map(entries).unwrap_or_default() {
            let Value::Array(list) = list else { continue };
            for entry in list {
                if entry.is_null() {
                    failed = true;
                    break 'hosts;
                }
                let state = entry.get("state");
                if let Some(Value::String(login)) = entry.get("login")
                    && (state.is_none() || state == Some(&Value::String("success".into())))
                {
                    add(login, &host, entry.get("active") == Some(&Value::Bool(true)));
                }
            }
        }
        if !failed {
            return accounts;
        }
    }
    // An older gh: "✓ Logged in to github.com account octocat (keyring)".
    let pattern = Regex::new(r"Logged in to (\S+) (?:account|as) (\S+)").expect("static pattern");
    for line in format!("{stdout}\n{stderr}").split('\n') {
        if let Some(found) = pattern.captures(line) {
            add(&found[2], &found[1], false);
        }
    }
    accounts
}

/// The logged-in accounts, restricted to `only` (logins, or `host/login`) when given.
pub fn list_accounts(runner: &dyn Runner, only: Option<&[String]>) -> Result<Vec<Account>, Error> {
    let options = RunOptions::default();
    let mut result = runner.run(&argv(&["gh", "auth", "status", "--json", "hosts"]), &options);
    if result.code == 127 {
        return Err(Error::Other(
            "ultron autoreview needs the GitHub CLI (gh), which is not installed".into(),
        ));
    }
    if result.code != 0 || !crate::js::trim(&result.stdout).starts_with('{') {
        result = runner.run(&argv(&["gh", "auth", "status"]), &options);
    }
    let accounts = parse_auth_status(&result.stdout, &result.stderr);
    let Some(only) = only.filter(|only| !only.is_empty()) else {
        return Ok(accounts);
    };
    let wanted: Vec<String> = only.iter().map(|item| item.to_lowercase()).collect();
    Ok(accounts
        .into_iter()
        .filter(|account| {
            wanted.contains(&account.login.to_lowercase())
                || wanted.contains(&format!("{}/{}", account.host, account.login).to_lowercase())
        })
        .collect())
}

/// The environment that authenticates a child `gh` or `git` as an account.
pub fn auth_env(account: &Account, token: &str) -> Env {
    // gh reads GH_TOKEN for github.com and *.ghe.com, and GH_ENTERPRISE_TOKEN for any other host.
    let hosted = account.host == "github.com" || account.host.ends_with(".ghe.com");
    let mut env = Env::new();
    env.insert(
        if hosted { "GH_TOKEN" } else { "GH_ENTERPRISE_TOKEN" }.into(),
        token.into(),
    );
    env.insert("GH_HOST".into(), account.host.clone());
    env
}

/// Registers a token with the secret-masking list.
pub type SecretSink = Arc<dyn Fn(&str) + Send + Sync>;

/// Tokens by account, read once and held in memory only.
pub struct TokenStore {
    runner: Arc<dyn Runner>,
    tokens: Mutex<HashMap<String, String>>,
    register: Option<SecretSink>,
}

impl TokenStore {
    pub fn new(runner: Arc<dyn Runner>) -> Self {
        Self {
            runner,
            tokens: Mutex::new(HashMap::new()),
            register: None,
        }
    }

    /// Every token read is also handed to `register` (the secret-masking list).
    pub fn with_secret_sink(mut self, register: SecretSink) -> Self {
        self.register = Some(register);
        self
    }

    pub fn env(&self, account: &Account) -> Result<Env, Error> {
        let key = account_key(account);
        let known = self.tokens.lock().unwrap_or_else(|e| e.into_inner()).get(&key).cloned();
        let token = match known {
            Some(token) => token,
            None => {
                let result = self.runner.run(
                    &argv(&[
                        "gh",
                        "auth",
                        "token",
                        "--user",
                        &account.login,
                        "--hostname",
                        &account.host,
                    ]),
                    &RunOptions::default(),
                );
                let token = crate::js::trim(&result.stdout).to_string();
                if result.code != 0 || token.is_empty() {
                    return Err(Error::Other(format!(
                        "gh has no token for {} on {}",
                        account.login, account.host
                    )));
                }
                if let Some(register) = &self.register {
                    register(&token);
                }
                self.tokens
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key, token.clone());
                token
            }
        };
        Ok(auth_env(account, &token))
    }

    /// Forget a token (after an authentication failure, so the next call reads it again).
    pub fn forget(&self, account: &Account) {
        self.tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&account_key(account));
    }
}
