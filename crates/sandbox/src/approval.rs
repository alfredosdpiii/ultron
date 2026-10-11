//! Approval tiers, ported from oh-my-pi (fa5ff4a) `tools/approval.ts` and mapped onto Ultron's host operations.
//!
//! Ultron's model has one tool, the Python REPL; what omp gates per tool, Ultron gates per host operation a cell
//! reaches: `bash` (blocking bash, shell jobs) is `exec`, `write` (`files.before_write`) is `write`, job inspection is
//! `read`, anything unknown is `exec`. Settings: `approval.mode` (`always-ask` | `write` | `yolo`, default `yolo`),
//! `approval.ops.<op>` (`allow` | `deny` | `prompt`), `approval.bash.patterns` and
//! `approval.bash.allowCompoundCommands`. A `prompt` goes to the UI through [`Approver`]; with no UI it fails closed.

use crate::patterns::{PatternRule, bash_decision, pattern_rules};
use serde_json::{Map, Value, json};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Read,
    Write,
    Exec,
}

impl Tier {
    pub fn name(self) -> &'static str {
        match self {
            Tier::Read => "read",
            Tier::Write => "write",
            Tier::Exec => "exec",
        }
    }
    pub fn parse(value: &Value) -> Option<Tier> {
        match value.as_str()? {
            "read" => Some(Tier::Read),
            "write" => Some(Tier::Write),
            "exec" => Some(Tier::Exec),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    Allow,
    Deny,
    Prompt,
}

impl Policy {
    pub fn name(self) -> &'static str {
        match self {
            Policy::Allow => "allow",
            Policy::Deny => "deny",
            Policy::Prompt => "prompt",
        }
    }
    /// `normalizePolicy`: trimmed, case-insensitive; anything else is no policy.
    pub fn parse(value: &Value) -> Option<Policy> {
        match value.as_str()?.trim().to_lowercase().as_str() {
            "allow" => Some(Policy::Allow),
            "deny" => Some(Policy::Deny),
            "prompt" => Some(Policy::Prompt),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    AlwaysAsk,
    Write,
    #[default]
    Yolo,
}

impl Mode {
    pub fn parse(value: &Value) -> Option<Mode> {
        match value.as_str()? {
            "always-ask" => Some(Mode::AlwaysAsk),
            "write" => Some(Mode::Write),
            "yolo" => Some(Mode::Yolo),
            _ => None,
        }
    }
    fn max_tier(self) -> Tier {
        match self {
            Mode::AlwaysAsk => Tier::Read,
            Mode::Write => Tier::Write,
            Mode::Yolo => Tier::Exec,
        }
    }
}

/// A normalized approval decision (`ToolApprovalDecision` after `normalizeDecision`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub tier: Tier,
    pub override_: bool,
    pub policy: Option<Policy>,
    pub reason: Option<String>,
    pub policy_key: Option<String>,
    /// The bare tier form (`"exec"`) rather than an object.
    bare: bool,
}

impl Decision {
    pub fn tier(tier: Tier) -> Self {
        Decision {
            tier,
            override_: false,
            policy: None,
            reason: None,
            policy_key: None,
            bare: true,
        }
    }
    pub fn object(tier: Tier, override_: bool, policy: Option<Policy>, reason: Option<String>) -> Self {
        Decision {
            tier,
            override_,
            policy,
            reason,
            policy_key: None,
            bare: false,
        }
    }

    /// `normalizeDecision` of a JSON decision: a tier string, an object, or anything else (`exec`).
    pub fn from_json(value: Option<&Value>) -> Self {
        match value {
            Some(value @ Value::String(_)) => match Tier::parse(value) {
                Some(tier) => Decision::tier(tier),
                None => Decision::object(Tier::Exec, false, None, None),
            },
            Some(Value::Object(record)) => {
                let text = |key: &str| {
                    record
                        .get(key)
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                };
                Decision {
                    tier: record.get("tier").and_then(Tier::parse).unwrap_or(Tier::Exec),
                    override_: record.get("override") == Some(&Value::Bool(true)),
                    policy: record.get("policy").and_then(Policy::parse),
                    reason: text("reason"),
                    policy_key: text("policyKey"),
                    bare: false,
                }
            }
            _ => Decision::object(Tier::Exec, false, None, None),
        }
    }

    /// The decision as omp's TypeScript returns it.
    pub fn to_json(&self) -> Value {
        if self.bare {
            return json!(self.tier.name());
        }
        let mut out = Map::new();
        out.insert("tier".into(), json!(self.tier.name()));
        if self.override_ {
            out.insert("override".into(), json!(true));
        }
        if let Some(policy) = self.policy {
            out.insert("policy".into(), json!(policy.name()));
        }
        if let Some(reason) = &self.reason {
            out.insert("reason".into(), json!(reason));
        }
        Value::Object(out)
    }
}

/// `strictestApproval`: the first deny wins, else the highest tier (from `read`).
pub fn strictest(decisions: &[Decision]) -> Decision {
    let mut tier = Tier::Read;
    for decision in decisions {
        if decision.policy == Some(Policy::Deny) && !decision.bare {
            return decision.clone();
        }
        tier = tier.max(decision.tier);
    }
    Decision::tier(tier)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Tool,
    User,
    Mode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub policy: Policy,
    pub tier: Tier,
    pub reason: Option<String>,
    pub override_: bool,
    pub source: Source,
    pub policy_key: Option<String>,
}

impl Resolved {
    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("policy".into(), json!(self.policy.name()));
        out.insert("tier".into(), json!(self.tier.name()));
        out.insert("override".into(), json!(self.override_));
        let source = match self.source {
            Source::Tool => "tool",
            Source::User => "user",
            Source::Mode => "mode",
        };
        out.insert("source".into(), json!(source));
        if let Some(key) = &self.policy_key {
            out.insert("policyKey".into(), json!(key));
        }
        if let Some(reason) = &self.reason {
            out.insert("reason".into(), json!(reason));
        }
        Value::Object(out)
    }
}

/// `resolveApproval`: tool decision, then user `approval.ops.<op>` policy, then the mode's tier limit.
pub fn resolve(
    name: &str,
    legacy_name: Option<&str>,
    decision: &Decision,
    mode: Mode,
    user: &Map<String, Value>,
) -> Resolved {
    let policy_key = decision.policy_key.as_deref().unwrap_or(name);
    let user_policy = user.get(policy_key).and_then(Policy::parse);
    let fallback = if policy_key != name && user_policy.is_none() {
        user.get(name).and_then(Policy::parse)
    } else {
        None
    };
    let effective = user_policy.or(fallback);
    let user_key = if user_policy.is_some() { policy_key } else { name };
    // A renamed op inherits only a restrictive legacy policy.
    let legacy = match legacy_name {
        Some(legacy) if effective.is_none() && legacy != name => user.get(legacy).and_then(Policy::parse),
        _ => None,
    }
    .filter(|policy| *policy != Policy::Allow);
    let combined = effective.or(legacy);
    let combined_key = if effective.is_some() {
        Some(user_key.to_owned())
    } else if legacy.is_some() {
        legacy_name.map(str::to_owned)
    } else {
        None
    };
    let from_tool = |policy: Policy, override_: bool| Resolved {
        policy,
        tier: decision.tier,
        reason: decision.reason.clone(),
        override_,
        source: Source::Tool,
        policy_key: decision.policy_key.clone(),
    };
    let from_user = |policy: Policy, override_: bool| Resolved {
        policy,
        tier: decision.tier,
        reason: None,
        override_,
        source: Source::User,
        policy_key: combined_key.clone(),
    };
    if decision.policy == Some(Policy::Deny) {
        return from_tool(Policy::Deny, decision.override_);
    }
    if combined == Some(Policy::Deny) {
        return from_user(Policy::Deny, decision.override_);
    }
    if mode == Mode::Yolo {
        if let Some(policy) = decision.policy {
            return from_tool(policy, false);
        }
        return match combined {
            Some(policy) => from_user(policy, false),
            None => Resolved {
                policy: Policy::Allow,
                tier: decision.tier,
                reason: None,
                override_: false,
                source: Source::Mode,
                policy_key: None,
            },
        };
    }
    if decision.override_ {
        let policy = if decision.policy == Some(Policy::Allow) {
            Policy::Allow
        } else {
            Policy::Prompt
        };
        return from_tool(policy, true);
    }
    if let Some(policy @ (Policy::Allow | Policy::Prompt)) = decision.policy {
        return from_tool(policy, false);
    }
    if let Some(policy) = combined {
        return from_user(policy, false);
    }
    if decision.tier <= mode.max_tier() {
        return Resolved {
            policy: Policy::Allow,
            tier: decision.tier,
            reason: None,
            override_: false,
            source: Source::Mode,
            policy_key: None,
        };
    }
    Resolved {
        policy: Policy::Prompt,
        tier: decision.tier,
        reason: decision.reason.clone(),
        override_: false,
        source: Source::Mode,
        policy_key: None,
    }
}

/// The operation family and tier of a host request kind.
pub fn op_of(kind: &str) -> (&str, Tier) {
    match kind {
        "bash" | "shell.bash" | "shell.run" => ("bash", Tier::Exec),
        "files.before_write" | "write" | "edit" => ("write", Tier::Write),
        "read" | "shell.get" | "shell.result" | "shell.read" | "shell.list" => (kind, Tier::Read),
        "shell.cancel" => (kind, Tier::Write),
        other => (other, Tier::Exec),
    }
}

/// `truncateForPrompt`: at most `max` UTF-16 units, then a marker naming how many were cut.
pub fn truncate_for_prompt(value: &str, max: usize) -> String {
    let units: Vec<u16> = value.encode_utf16().collect();
    if units.len() <= max {
        return value.to_owned();
    }
    format!(
        "{}[…{}ch elided…]",
        String::from_utf16_lossy(&units[..max]),
        units.len() - max
    )
}

/// What the UI is asked to approve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub op: String,
    pub tier: Tier,
    pub reason: Option<String>,
    /// The prompt body (`formatApprovalPrompt`).
    pub prompt: String,
}

/// The UI side of a `prompt` decision. Blocks until the user answers; true allows the operation.
pub trait Approver: Send + Sync {
    fn approve(&self, request: &ApprovalRequest) -> bool;
}

/// The approval settings of a session plus its UI, if any.
#[derive(Clone, Default)]
pub struct Approvals {
    pub mode: Mode,
    pub ops: Map<String, Value>,
    pub bash_patterns: Vec<PatternRule>,
    pub allow_compound: bool,
    pub approver: Option<Arc<dyn Approver>>,
}

impl std::fmt::Debug for Approvals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Approvals")
            .field("mode", &self.mode)
            .field("ops", &self.ops)
            .field("bash_patterns", &self.bash_patterns)
            .field("allow_compound", &self.allow_compound)
            .field("approver", &self.approver.is_some())
            .finish()
    }
}

impl Approvals {
    /// From the merged settings object: `approval.mode`, `approval.ops`, `approval.bash.patterns`,
    /// `approval.bash.allowCompoundCommands`. Invalid values are ignored.
    pub fn from_settings(settings: &Value) -> Self {
        let approval = settings.get("approval");
        let get = |key: &str| approval.and_then(|a| a.get(key));
        let bash = get("bash");
        Approvals {
            mode: get("mode").and_then(Mode::parse).unwrap_or_default(),
            ops: get("ops").and_then(Value::as_object).cloned().unwrap_or_default(),
            bash_patterns: pattern_rules(bash.and_then(|b| b.get("patterns"))),
            allow_compound: bash.and_then(|b| b.get("allowCompoundCommands")) == Some(&Value::Bool(true)),
            approver: None,
        }
    }

    /// The resolved policy of host request `kind`; `subject` is the command of a bash request.
    pub fn resolve(&self, kind: &str, subject: Option<&str>) -> (String, Resolved) {
        let (op, tier) = op_of(kind);
        let decision = match (op, subject) {
            ("bash", Some(command)) => bash_decision(command, &self.bash_patterns, self.allow_compound),
            _ => Decision::tier(tier),
        };
        (op.to_owned(), resolve(op, None, &decision, self.mode, &self.ops))
    }

    /// Decide host request `kind`: Ok to proceed, Err with the message the cell raises. `subject` is the command
    /// (bash) or path (write) shown in the prompt.
    pub fn check(&self, kind: &str, subject: Option<&str>) -> Result<(), String> {
        let (op, resolved) = self.resolve(kind, subject);
        match resolved.policy {
            Policy::Allow => Ok(()),
            Policy::Deny => Err(deny_message(&op, &resolved)),
            Policy::Prompt => {
                let mut lines = vec![format!("Allow host operation: {op}")];
                if let Some(reason) = &resolved.reason {
                    lines.push(format!("Reason: {reason}"));
                }
                if let Some(subject) = subject {
                    let label = if op == "bash" { "Command" } else { "Path" };
                    lines.push(format!("{label}: {}", truncate_for_prompt(subject, 2000)));
                }
                let request = ApprovalRequest {
                    op: op.clone(),
                    tier: resolved.tier,
                    reason: resolved.reason,
                    prompt: lines.join("\n"),
                };
                match &self.approver {
                    Some(approver) if approver.approve(&request) => Ok(()),
                    Some(_) => Err(format!("Operation \"{op}\" was not approved")),
                    None => Err(format!(
                        "Operation \"{op}\" requires approval{} but no interactive UI is available",
                        request.reason.map(|r| format!(" ({r})")).unwrap_or_default()
                    )),
                }
            }
        }
    }
}

/// `denyError`, worded for host operations and `approval.ops`.
pub fn deny_message(op: &str, resolved: &Resolved) -> String {
    match resolved.source {
        Source::Tool => format!(
            "Operation \"{op}\" is blocked by policy.{}",
            resolved
                .reason
                .as_ref()
                .map(|r| format!("\nReason: {r}"))
                .unwrap_or_default()
        ),
        _ => {
            let key = resolved.policy_key.as_deref().unwrap_or(op);
            format!(
                "Operation \"{key}\" is blocked by user policy.\nTo allow: remove \"approval.ops.{key}: deny\" from settings."
            )
        }
    }
}
