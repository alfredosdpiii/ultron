//! The Radius relay a server generation of this CLI runs (`startServerGeneration`'s `RadiusRelayHost` with
//! `new RadiusRelayAuthResolver(options.relayAuth)`): the gateway from `PI_RADIUS_GATEWAY` (else
//! `https://radius.pi.dev`), no relay in offline mode (`PI_OFFLINE` set), and the stored `radius` credential
//! (`getAuth("radius")`: auth.json's key or OAuth token, refreshed when it has under five minutes left, else
//! `RADIUS_API_KEY`), looked up anew for every connection attempt.

use crate::Env;
use server::radius_relay::{
    AuthInput, DEFAULT_RADIUS_GATEWAY, ENV_RADIUS_GATEWAY, RadiusRelayAuthResolver, RelayConfig, StatusSink,
    StoredToken,
};
use session_core::stored_auth::Stored;
use std::sync::Arc;

/// `getAuthCredential(await runtime.getAuth("radius", { minOAuthValidityMs: 5 * 60_000 }))`.
pub fn stored_radius_token(env: &Env) -> Result<Option<String>, String> {
    if let Some(token) = session_core::stored_auth::oauth_access(env, "radius", Some(5.0 * 60_000.0))? {
        return Ok(Some(token));
    }
    Ok(match session_core::stored_auth::stored_key(env, "radius")? {
        Stored::Owns(key) => key,
        Stored::Nothing => session_core::lane::env_api_key(env, "radius"),
    })
}

/// The relay of a server generation started by this process.
pub fn relay_config(env: &Env) -> RelayConfig {
    relay_config_with(env, None, None)
}

/// `new RadiusRelayAuthResolver(input)` of this environment: an explicit credential (`--auth-token`,
/// `--auth-token-file`) or the stored one, on `PI_RADIUS_GATEWAY` (else `https://radius.pi.dev`); offline mode
/// (`PI_OFFLINE` set) has none.
pub fn auth_resolver(env: &Env, input: Option<AuthInput>) -> RadiusRelayAuthResolver {
    let gateway = (env.var)(ENV_RADIUS_GATEWAY).unwrap_or_else(|| DEFAULT_RADIUS_GATEWAY.into());
    let offline = (env.var)("PI_OFFLINE").is_some();
    let lookup = env.clone();
    let stored: StoredToken = Arc::new(move || stored_radius_token(&lookup));
    RadiusRelayAuthResolver::new(input, &gateway, offline, stored)
}

/// The relay of a server generation started by this process, with the server command's explicit credential and
/// where its statuses go (`relayAuth`, `onRelayStatus`).
pub fn relay_config_with(env: &Env, input: Option<AuthInput>, on_status: Option<StatusSink>) -> RelayConfig {
    RelayConfig {
        auth: Arc::new(auth_resolver(env, input)),
        on_status,
    }
}
