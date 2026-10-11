//! `parseArgs` from packages/coding-agent/src/cli/args.ts.

use serde_json::{Map, Value, json};

const VALID_THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticKind {
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    pub message: String,
}

/// A value of an unknown (possibly extension) flag: `--flag` alone is `True`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlagValue {
    True,
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListModels {
    All,
    Search(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Args {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<Vec<String>>,
    pub thinking: Option<String>,
    pub continue_: bool,
    pub resume: bool,
    /// `-r <name|id>`: the argument after `--resume`/`-r` when it is not a flag (a Rust addition; TypeScript's
    /// `--resume` takes no value).
    pub resume_selector: Option<String>,
    pub help: bool,
    pub version: bool,
    pub mode: Option<String>,
    pub name: Option<String>,
    pub no_session: bool,
    pub session: Option<String>,
    pub session_id: Option<String>,
    pub fork: Option<String>,
    pub session_dir: Option<String>,
    pub models: Option<Vec<String>>,
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Option<Vec<String>>,
    pub no_tools: bool,
    pub no_builtin_tools: bool,
    pub extensions: Option<Vec<String>>,
    pub no_extensions: bool,
    pub print: bool,
    pub export: Option<String>,
    pub no_skills: bool,
    pub skills: Option<Vec<String>>,
    pub prompt_templates: Option<Vec<String>>,
    pub no_prompt_templates: bool,
    pub themes: Option<Vec<String>>,
    pub use_theme: Option<String>,
    pub no_themes: bool,
    pub no_context_files: bool,
    pub list_models: Option<ListModels>,
    pub offline: bool,
    pub claude: bool,
    pub tui_mode: Option<String>,
    pub verbose: bool,
    pub project_trust_override: Option<bool>,
    pub messages: Vec<String>,
    pub file_args: Vec<String>,
    /// Insertion-ordered; a repeated flag keeps its first position and its last value, like a JS `Map`.
    pub unknown_flags: Vec<(String, FlagValue)>,
    pub diagnostics: Vec<Diagnostic>,
}

/// `normalizeSessionName`: the trimmed name, or `None` when blank.
pub fn normalize_session_name(value: &str) -> Option<String> {
    let name = js_trim(value);
    (!name.is_empty()).then(|| name.to_string())
}

/// `String.prototype.trim`: JS whitespace and line terminators, which `char::is_whitespace` covers plus U+FEFF.
fn js_trim(value: &str) -> &str {
    value.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

fn split_list(value: &str, drop_empty: bool) -> Vec<String> {
    value
        .split(',')
        .map(|s| js_trim(s).to_string())
        .filter(|s| !drop_empty || !s.is_empty())
        .collect()
}

fn error(result: &mut Args, message: String) {
    result.diagnostics.push(Diagnostic {
        kind: DiagnosticKind::Error,
        message,
    });
}

fn set_flag(flags: &mut Vec<(String, FlagValue)>, name: &str, value: FlagValue) {
    match flags.iter_mut().find(|(k, _)| k == name) {
        Some(entry) => entry.1 = value,
        None => flags.push((name.to_string(), value)),
    }
}

pub fn parse_args(args: &[String]) -> Args {
    let mut r = Args::default();
    let mut i = 0;
    // `args[i + 1]` when present.
    let next = |i: usize| args.get(i + 1).map(String::as_str);
    while i < args.len() {
        let arg = args[i].as_str();
        let has_next = i + 1 < args.len();
        match arg {
            "--" => {
                for positional in &args[i + 1..] {
                    match positional.strip_prefix('@') {
                        Some(file) => r.file_args.push(file.to_string()),
                        None => r.messages.push(positional.clone()),
                    }
                }
                break;
            }
            "--help" | "-h" => r.help = true,
            "--version" | "-v" => r.version = true,
            "--mode" => match next(i) {
                None => error(&mut r, "--mode requires text, json, or rpc".into()),
                Some(mode) if mode.starts_with('-') => error(&mut r, "--mode requires text, json, or rpc".into()),
                Some(mode) => {
                    i += 1;
                    if matches!(mode, "text" | "json" | "rpc") {
                        r.mode = Some(mode.to_string());
                    } else {
                        error(
                            &mut r,
                            format!("Invalid mode \"{mode}\". Valid values: text, json, rpc"),
                        );
                    }
                }
            },
            "--continue" | "-c" => r.continue_ = true,
            "--resume" | "-r" => {
                r.resume = true;
                if let Some(n) = next(i)
                    && !n.starts_with('-')
                    && !n.starts_with('@')
                {
                    r.resume_selector = Some(n.to_string());
                    i += 1;
                }
            }
            "--provider" if has_next => r.provider = take(args, &mut i),
            "--model" if has_next => r.model = take(args, &mut i),
            "--api-key" if has_next => r.api_key = take(args, &mut i),
            "--system-prompt" if has_next => r.system_prompt = take(args, &mut i),
            "--append-system-prompt" if has_next => push(&mut r.append_system_prompt, args, &mut i),
            "--name" | "-n" => {
                if has_next {
                    r.name = take(args, &mut i);
                } else {
                    error(&mut r, "--name requires a value".into());
                }
            }
            "--no-session" => r.no_session = true,
            "--session" if has_next => r.session = take(args, &mut i),
            "--session-id" if has_next => r.session_id = take(args, &mut i),
            "--fork" if has_next => r.fork = take(args, &mut i),
            "--session-dir" if has_next => r.session_dir = take(args, &mut i),
            "--models" if has_next => r.models = take(args, &mut i).map(|v| split_list(&v, false)),
            "--no-tools" | "-nt" => r.no_tools = true,
            "--no-builtin-tools" | "-nbt" => r.no_builtin_tools = true,
            "--tools" | "-t" if has_next => r.tools = take(args, &mut i).map(|v| split_list(&v, true)),
            "--exclude-tools" | "-xt" if has_next => r.exclude_tools = take(args, &mut i).map(|v| split_list(&v, true)),
            "--thinking" if has_next => {
                let level = take(args, &mut i).unwrap_or_default();
                if VALID_THINKING_LEVELS.contains(&level.as_str()) {
                    r.thinking = Some(level);
                } else {
                    r.diagnostics.push(Diagnostic {
                        kind: DiagnosticKind::Warning,
                        message: format!(
                            "Invalid thinking level \"{level}\". Valid values: {}",
                            VALID_THINKING_LEVELS.join(", ")
                        ),
                    });
                }
            }
            "--print" | "-p" => {
                r.print = true;
                if let Some(n) = next(i)
                    && !n.starts_with('@')
                    && (!n.starts_with('-') || n.starts_with("---"))
                {
                    r.messages.push(n.to_string());
                    i += 1;
                }
            }
            "--export" if has_next => r.export = take(args, &mut i),
            "--extension" | "-e" if has_next => push(&mut r.extensions, args, &mut i),
            "--no-extensions" | "-ne" => r.no_extensions = true,
            "--skill" if has_next => push(&mut r.skills, args, &mut i),
            "--prompt-template" if has_next => push(&mut r.prompt_templates, args, &mut i),
            "--theme" if has_next => push(&mut r.themes, args, &mut i),
            "--use-theme" => match next(i) {
                Some(name) if !name.starts_with('-') => {
                    r.use_theme = Some(name.to_string());
                    i += 1;
                }
                _ => error(&mut r, "--use-theme requires a theme name".into()),
            },
            "--no-skills" | "-ns" => r.no_skills = true,
            "--no-prompt-templates" | "-np" => r.no_prompt_templates = true,
            "--no-themes" => r.no_themes = true,
            "--no-context-files" | "-nc" => r.no_context_files = true,
            "--list-models" => match next(i) {
                Some(p) if !p.starts_with('-') && !p.starts_with('@') => {
                    r.list_models = Some(ListModels::Search(p.to_string()));
                    i += 1;
                }
                _ => r.list_models = Some(ListModels::All),
            },
            "--tui-mode" => match next(i) {
                Some(mode @ ("regular" | "fullscreen")) => {
                    r.tui_mode = Some(mode.to_string());
                    i += 1;
                }
                None => error(&mut r, "--tui-mode requires regular or fullscreen".into()),
                Some(mode) if mode.starts_with('-') => {
                    error(&mut r, "--tui-mode requires regular or fullscreen".into())
                }
                Some(mode) => {
                    i += 1;
                    error(
                        &mut r,
                        format!("Invalid TUI mode \"{mode}\". Valid values: regular, fullscreen"),
                    );
                }
            },
            "--verbose" => r.verbose = true,
            "--approve" | "-a" => r.project_trust_override = Some(true),
            "--no-approve" | "-na" => r.project_trust_override = Some(false),
            "--offline" => r.offline = true,
            "--claude" => r.claude = true,
            _ => {
                if let Some(file) = arg.strip_prefix('@') {
                    r.file_args.push(file.to_string());
                } else if let Some(flag) = arg.strip_prefix("--") {
                    if let Some((name, value)) = flag.split_once('=') {
                        set_flag(&mut r.unknown_flags, name, FlagValue::Text(value.to_string()));
                    } else {
                        match next(i) {
                            Some(n) if !n.starts_with('-') && !n.starts_with('@') => {
                                set_flag(&mut r.unknown_flags, flag, FlagValue::Text(n.to_string()));
                                i += 1;
                            }
                            _ => set_flag(&mut r.unknown_flags, flag, FlagValue::True),
                        }
                    }
                } else if arg.starts_with('-') {
                    error(&mut r, format!("Unknown option: {arg}"));
                } else {
                    r.messages.push(arg.to_string());
                }
            }
        }
        i += 1;
    }
    r
}

fn take(args: &[String], i: &mut usize) -> Option<String> {
    *i += 1;
    args.get(*i).cloned()
}

fn push(list: &mut Option<Vec<String>>, args: &[String], i: &mut usize) {
    if let Some(value) = take(args, i) {
        list.get_or_insert_with(Vec::new).push(value);
    }
}

impl Args {
    /// The TypeScript `Args` object as `JSON.stringify` shows it (absent fields omitted), with `unknownFlags` as
    /// its entry list; used by the differential tests.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        let mut opt = |k: &str, v: Option<Value>| {
            if let Some(v) = v {
                m.insert(k.to_string(), v);
            }
        };
        let s = |v: &Option<String>| v.as_ref().map(|v| json!(v));
        let l = |v: &Option<Vec<String>>| v.as_ref().map(|v| json!(v));
        let b = |v: bool| v.then_some(json!(true));
        opt("provider", s(&self.provider));
        opt("model", s(&self.model));
        opt("apiKey", s(&self.api_key));
        opt("systemPrompt", s(&self.system_prompt));
        opt("appendSystemPrompt", l(&self.append_system_prompt));
        opt("thinking", s(&self.thinking));
        opt("continue", b(self.continue_));
        opt("resume", b(self.resume));
        opt("resumeSelector", s(&self.resume_selector));
        opt("help", b(self.help));
        opt("version", b(self.version));
        opt("mode", s(&self.mode));
        opt("name", s(&self.name));
        opt("noSession", b(self.no_session));
        opt("session", s(&self.session));
        opt("sessionId", s(&self.session_id));
        opt("fork", s(&self.fork));
        opt("sessionDir", s(&self.session_dir));
        opt("models", l(&self.models));
        opt("tools", l(&self.tools));
        opt("excludeTools", l(&self.exclude_tools));
        opt("noTools", b(self.no_tools));
        opt("noBuiltinTools", b(self.no_builtin_tools));
        opt("extensions", l(&self.extensions));
        opt("noExtensions", b(self.no_extensions));
        opt("print", b(self.print));
        opt("export", s(&self.export));
        opt("noSkills", b(self.no_skills));
        opt("skills", l(&self.skills));
        opt("promptTemplates", l(&self.prompt_templates));
        opt("noPromptTemplates", b(self.no_prompt_templates));
        opt("themes", l(&self.themes));
        opt("useTheme", s(&self.use_theme));
        opt("noThemes", b(self.no_themes));
        opt("noContextFiles", b(self.no_context_files));
        opt(
            "listModels",
            self.list_models.as_ref().map(|l| match l {
                ListModels::All => json!(true),
                ListModels::Search(p) => json!(p),
            }),
        );
        opt("offline", b(self.offline));
        opt("claude", b(self.claude));
        opt("tuiMode", s(&self.tui_mode));
        opt("verbose", b(self.verbose));
        opt("projectTrustOverride", self.project_trust_override.map(|v| json!(v)));
        m.insert("messages".into(), json!(self.messages));
        m.insert("fileArgs".into(), json!(self.file_args));
        let flags: Vec<Value> = self
            .unknown_flags
            .iter()
            .map(|(k, v)| match v {
                FlagValue::True => json!([k, true]),
                FlagValue::Text(t) => json!([k, t]),
            })
            .collect();
        m.insert("unknownFlags".into(), Value::Array(flags));
        let diagnostics: Vec<Value> = self
            .diagnostics
            .iter()
            .map(|d| {
                let kind = if d.kind == DiagnosticKind::Error {
                    "error"
                } else {
                    "warning"
                };
                json!({"type": kind, "message": d.message})
            })
            .collect();
        m.insert("diagnostics".into(), Value::Array(diagnostics));
        Value::Object(m)
    }
}
