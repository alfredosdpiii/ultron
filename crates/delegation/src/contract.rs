//! Delegation briefs and their contracts (rewriteplan "Workflow design items", from Sakana MASS): a brief carries the
//! child's `role`, its `instruction`, a `contract` and its `hop` (how many delegations below the root it runs). A
//! contract lists boolean checks the host verifies on the child's result before anyone sees it as a success; the
//! check outcomes are evidence in the result's `check` (`check.contract`), beside its verdict.
//!
//! Checks address the result object by a dotted path (`value.passed`, `verdict.status`, `value.items.0`):
//!
//! - `{"check": "equals", "path", "value"}`: the field equals the JSON value;
//! - `{"check": "present", "path"}`: the field exists and is not null;
//! - `{"check": "nonempty", "path"}`: a nonempty string, array or object;
//! - `{"check": "contains", "path", "value"}`: a string field contains the text, or an array field an equal item.
//!
//! The model writes the brief (a kernel API payload); the host stamps the hop, so a child cannot claim to be
//! shallower than it is.

use crate::schema::json_equal;
use serde_json::{Map, Value, json};

/// Most checks one contract may list.
pub const MAX_CHECKS: usize = 32;
const MAX_TEXT: usize = 4000;

/// One boolean check on a child's result.
#[derive(Clone, Debug, PartialEq)]
pub enum Check {
    Equals { path: String, value: Value },
    Present { path: String },
    Nonempty { path: String },
    Contains { path: String, value: Value },
}

/// What a child is asked to be and do, and what its result must satisfy.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Brief {
    pub role: Option<String>,
    pub instruction: Option<String>,
    pub contract: Vec<Check>,
    /// Delegations between the root and this child (the root's own children are hop 1); stamped by the host.
    pub hop: u32,
}

fn only(map: &Map<String, Value>, allowed: &[&str], what: &str) -> Result<(), String> {
    match map.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(format!("Unknown {what} field: {key}")),
        None => Ok(()),
    }
}

fn text(map: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if !text.trim().is_empty() && text.chars().count() <= MAX_TEXT => {
            Ok(Some(text.clone()))
        }
        Some(_) => Err(format!(
            "brief.{key} must be a nonempty string of at most {MAX_TEXT} characters"
        )),
    }
}

fn path_of(map: &Map<String, Value>, index: usize) -> Result<String, String> {
    match map.get("path").and_then(Value::as_str) {
        Some(path) if !path.is_empty() && path.split('.').all(|segment| !segment.is_empty()) => Ok(path.into()),
        _ => Err(format!(
            "brief.contract[{index}].path must be a dotted path such as \"value.passed\""
        )),
    }
}

impl Check {
    /// One check as written; `index` names it in errors.
    pub fn parse(value: &Value, index: usize) -> Result<Check, String> {
        let map = value
            .as_object()
            .ok_or_else(|| format!("brief.contract[{index}] must be an object"))?;
        let kind = map.get("check").and_then(Value::as_str).unwrap_or_default();
        let operand = || {
            map.get("value")
                .cloned()
                .ok_or_else(|| format!("brief.contract[{index}].value is required for {kind}"))
        };
        match kind {
            "equals" => {
                only(map, &["check", "path", "value"], "contract check")?;
                Ok(Check::Equals {
                    path: path_of(map, index)?,
                    value: operand()?,
                })
            }
            "contains" => {
                only(map, &["check", "path", "value"], "contract check")?;
                Ok(Check::Contains {
                    path: path_of(map, index)?,
                    value: operand()?,
                })
            }
            "present" => {
                only(map, &["check", "path"], "contract check")?;
                Ok(Check::Present {
                    path: path_of(map, index)?,
                })
            }
            "nonempty" => {
                only(map, &["check", "path"], "contract check")?;
                Ok(Check::Nonempty {
                    path: path_of(map, index)?,
                })
            }
            _ => Err(format!(
                "brief.contract[{index}].check must be one of equals, present, nonempty, contains"
            )),
        }
    }

    fn path(&self) -> &str {
        match self {
            Check::Equals { path, .. }
            | Check::Present { path }
            | Check::Nonempty { path }
            | Check::Contains { path, .. } => path,
        }
    }

    /// The check as written (its evidence entry, before the outcome).
    pub fn describe(&self) -> Value {
        match self {
            Check::Equals { path, value } => json!({"check": "equals", "path": path, "value": value}),
            Check::Present { path } => json!({"check": "present", "path": path}),
            Check::Nonempty { path } => json!({"check": "nonempty", "path": path}),
            Check::Contains { path, value } => json!({"check": "contains", "path": path, "value": value}),
        }
    }

    /// Whether `result` satisfies the check.
    pub fn holds(&self, result: &Value) -> bool {
        let field = lookup(result, self.path());
        match self {
            Check::Equals { value, .. } => field.is_some_and(|field| json_equal(field, value)),
            Check::Present { .. } => field.is_some_and(|field| !field.is_null()),
            Check::Nonempty { .. } => match field {
                Some(Value::String(text)) => !text.is_empty(),
                Some(Value::Array(items)) => !items.is_empty(),
                Some(Value::Object(map)) => !map.is_empty(),
                _ => false,
            },
            Check::Contains { value, .. } => match (field, value) {
                (Some(Value::String(text)), Value::String(part)) => text.contains(part.as_str()),
                (Some(Value::Array(items)), item) => items.iter().any(|candidate| json_equal(candidate, item)),
                _ => false,
            },
        }
    }
}

/// The field at a dotted path; array segments are indexes.
fn lookup<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(value, |current, segment| match current {
        Value::Object(map) => map.get(segment),
        Value::Array(items) => segment.parse::<usize>().ok().and_then(|index| items.get(index)),
        _ => None,
    })
}

impl Brief {
    /// A brief from a request payload (`role`, `instruction`, `contract`, `hop`). `hop` is the host's: a payload
    /// that names another one is refused (`None`: validation only, before the hop is known).
    pub fn parse(value: &Value, hop: Option<u32>) -> Result<Brief, String> {
        let map = value.as_object().ok_or("brief must be an object")?;
        only(map, &["role", "instruction", "contract", "hop"], "brief")?;
        if let (Some(claimed), Some(hop)) = (map.get("hop"), hop)
            && !claimed.is_null()
            && claimed.as_u64() != Some(u64::from(hop))
        {
            return Err(format!("brief.hop is {hop} here; it is set by the host"));
        }
        let contract = match map.get("contract") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) if items.len() <= MAX_CHECKS => items
                .iter()
                .enumerate()
                .map(|(index, item)| Check::parse(item, index))
                .collect::<Result<_, _>>()?,
            Some(_) => {
                return Err(format!(
                    "brief.contract must be an array of at most {MAX_CHECKS} checks"
                ));
            }
        };
        Ok(Brief {
            role: text(map, "role")?,
            instruction: text(map, "instruction")?,
            contract,
            hop: hop.unwrap_or(0),
        })
    }

    /// The brief as recorded: what was asked, with its hop.
    pub fn to_value(&self) -> Value {
        json!({
            "role": self.role,
            "instruction": self.instruction,
            "contract": self.contract.iter().map(Check::describe).collect::<Vec<_>>(),
            "hop": self.hop,
        })
    }

    /// The child's prompt: its role and instruction ahead of the task text.
    pub fn prompt(&self, task: &str) -> String {
        let mut lines = Vec::new();
        if let Some(role) = &self.role {
            lines.push(format!("Role: {role}"));
        }
        if let Some(instruction) = &self.instruction {
            lines.push(format!("Instruction: {instruction}"));
        }
        if !self.contract.is_empty() {
            let checks: Vec<String> = self.contract.iter().map(|check| check.describe().to_string()).collect();
            lines.push(format!(
                "Contract (the host checks your result against these): {}",
                checks.join("; ")
            ));
        }
        if lines.is_empty() {
            return task.to_string();
        }
        format!("{}\n\n{task}", lines.join("\n"))
    }

    /// Verify the contract on a terminal result. A succeeded result gains the evidence under its `check`
    /// (`check.contract`: the brief's role, instruction and hop, `passed`, and each check with `ok`; the task
    /// journal's result shape is the TypeScript one, whose `check` is free-form) and, when a check fails, becomes a
    /// failure naming the first failing check. Any other result is returned as it is: nothing was claimed.
    pub fn verify(&self, result: Value) -> Value {
        if result["status"] != "succeeded" {
            return result;
        }
        let mut result = result;
        let checks: Vec<Value> = self
            .contract
            .iter()
            .map(|check| {
                let mut entry = check.describe();
                entry["ok"] = json!(check.holds(&result));
                entry
            })
            .collect();
        let failed = checks.iter().find(|check| check["ok"] == false).cloned();
        let evidence = json!({
            "role": self.role, "instruction": self.instruction, "hop": self.hop,
            "passed": failed.is_none(), "checks": checks,
        });
        if !result["check"].is_object() {
            result["check"] = json!({});
        }
        result["check"]["contract"] = evidence;
        if let Some(check) = failed {
            if let Some(map) = result.as_object_mut() {
                map.shift_remove("value");
            }
            result["status"] = json!("failed");
            let mut described = check;
            if let Some(map) = described.as_object_mut() {
                map.shift_remove("ok");
            }
            result["error"] = json!(format!("Contract not met: {described}"));
        }
        result
    }
}
