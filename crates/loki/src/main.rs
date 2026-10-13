//! `loki` command line: loki.py's argparse interface over the Rust engine.

use loki::proc::Deadline;
use loki::run;
use loki::{CheckError, config, hook, injected_context, py, resolve};
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod shell_cli;

const COMMANDS: [&str; 13] = [
    "verify-installation",
    "explain",
    "context",
    "elixir",
    "apply",
    "shell",
    "shell-writes",
    "protect",
    "hook",
    "scan",
    "slop",
    "daemon",
    "init",
];
const TOP_USAGE: &str = "usage: loki [-h] [--version] [--root ROOT]\n            {verify-installation,explain,context,elixir,apply,shell,shell-writes,protect,hook,scan,slop,daemon,init} ...";

fn usage(command: &str) -> &'static str {
    match command {
        "protect" => {
            "usage: loki protect [-h] [--root ROOT] (--file FILE |\n                    --harness {claude,codex,factory}) [--record]\n                    [--format {text,json}] [--python PYTHON]\n                    [--preview {pi,omp,ultron}]"
        }
        "hook" => {
            "usage: loki hook [-h] [--root ROOT] (--file FILE |\n                 --harness {claude,codex,factory}) [--record]\n                 [--format {text,json}] [--python PYTHON]"
        }
        "scan" => {
            "usage: loki scan [-h] [--online] [--strict] [--base BASE] [--ruff-new]\n                 [--python PYTHON]"
        }
        "slop" => "usage: loki slop [-h] [--base BASE] [--json] [--top TOP]",
        "init" => {
            "usage: loki init [-h] [--force] [--dir DIR] [--managed-dir MANAGED_DIR]\n                 [--shell-guard] [--minimal]"
        }
        _ => {
            "usage: loki context [-h] [--root ROOT]\n                    [--event {SessionStart,UserPromptSubmit}]\n                    [--host {ultron}]"
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Flag,
    Value,
    Int,
    Choice(&'static [&'static str]),
}

fn options(command: &str) -> Vec<(&'static str, Kind)> {
    const HARNESS: &[&str] = &["claude", "codex", "factory"];
    const FORMAT: &[&str] = &["text", "json"];
    match command {
        "protect" | "hook" => {
            let mut list = vec![
                ("--root", Kind::Value),
                ("--file", Kind::Value),
                ("--harness", Kind::Choice(HARNESS)),
                ("--record", Kind::Flag),
                ("--format", Kind::Choice(FORMAT)),
                ("--python", Kind::Value),
            ];
            if command == "protect" {
                list.push(("--preview", Kind::Choice(&["pi", "omp", "ultron"])));
            }
            list
        }
        "scan" => vec![
            ("--online", Kind::Flag),
            ("--strict", Kind::Flag),
            ("--base", Kind::Value),
            ("--ruff-new", Kind::Flag),
            ("--python", Kind::Value),
        ],
        "slop" => vec![("--base", Kind::Value), ("--json", Kind::Flag), ("--top", Kind::Int)],
        "init" => vec![
            ("--force", Kind::Flag),
            ("--dir", Kind::Value),
            ("--managed-dir", Kind::Value),
            ("--shell-guard", Kind::Flag),
            ("--minimal", Kind::Flag),
        ],
        _ => vec![
            ("--root", Kind::Value),
            ("--event", Kind::Choice(&["SessionStart", "UserPromptSubmit"])),
            ("--host", Kind::Choice(&["ultron"])),
        ],
    }
}

/// Parsed values: last value per option, all values for `--file`.
#[derive(Default)]
struct Args {
    root: Option<String>,
    command: String,
    values: Vec<(&'static str, String)>,
}

impl Args {
    fn get(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
    }

    fn has(&self, name: &str) -> bool {
        self.values.iter().any(|(n, _)| *n == name)
    }

    fn all(&self, name: &str) -> Vec<String> {
        self.values
            .iter()
            .filter(|(n, _)| *n == name)
            .map(|(_, v)| v.clone())
            .collect()
    }
}

enum ParseError {
    Top(String),
    Sub(String, String),
    Exit(i32),
}

/// argparse treats a token as an option unless it looks like a negative number.
fn optional_like(token: &str) -> bool {
    if !token.starts_with('-') || token == "-" {
        return false;
    }
    let rest = &token[1..];
    let number = !rest.is_empty()
        && (rest.chars().all(|c| c.is_ascii_digit())
            || rest.split_once('.').is_some_and(|(a, b)| {
                a.chars().all(|c| c.is_ascii_digit()) && !b.is_empty() && b.chars().all(|c| c.is_ascii_digit())
            }));
    !number
}

/// `int(text)` as argparse's `type=int` accepts it.
fn python_int(text: &str) -> Option<i64> {
    let trimmed = py::strip(text);
    let (sign, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') || digits.contains("__") {
        return None;
    }
    let clean: String = digits.chars().filter(|c| *c != '_').collect();
    if !clean.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    clean.parse::<i64>().ok().map(|n| sign * n)
}

fn parse(argv: &[String]) -> Result<Args, ParseError> {
    let mut args = Args::default();
    let mut items = argv.iter().peekable();
    let top = |m: String| ParseError::Top(m);
    while let Some(token) = items.next() {
        let (flag, inline) = match token.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (token.clone(), None),
        };
        if flag.starts_with("--") && flag.len() > 2 {
            let matches: Vec<&str> = ["--version", "--root"]
                .into_iter()
                .filter(|o| o.starts_with(flag.as_str()))
                .collect();
            let chosen = if matches.contains(&flag.as_str()) {
                Some(flag.as_str())
            } else if matches.len() == 1 {
                Some(matches[0])
            } else {
                None
            };
            match chosen {
                Some("--version") => {
                    println!("loki {}", loki::VERSION);
                    return Err(ParseError::Exit(0));
                }
                Some(_) => {
                    let value = match inline {
                        Some(v) => v,
                        None => match items.peek() {
                            Some(v) if !optional_like(v) => items.next().cloned().unwrap_or_default(),
                            _ => return Err(top("argument --root: expected one argument".into())),
                        },
                    };
                    args.root = Some(value);
                    continue;
                }
                None => return Err(top(format!("unrecognized arguments: {}", argv.join(" ")))),
            }
        }
        if !COMMANDS.contains(&token.as_str()) {
            let listed = COMMANDS.iter().map(|c| format!("'{c}'")).collect::<Vec<_>>().join(", ");
            return Err(top(format!(
                "argument command: invalid choice: '{token}' (choose from {listed})"
            )));
        }
        args.command = token.clone();
        break;
    }
    if args.command.is_empty() {
        return Err(top("the following arguments are required: command".into()));
    }
    let command = args.command.clone();
    if !["protect", "hook", "scan", "slop", "init", "context"].contains(&command.as_str()) {
        return Ok(args);
    }
    let sub = |m: String| ParseError::Sub(command.clone(), m);
    let specs = options(&command);
    let mut extras: Vec<String> = Vec::new();
    let mut exclusive: Option<&'static str> = None;
    let rest: Vec<String> = items.cloned().collect();
    let mut index = 0;
    while index < rest.len() {
        let token = &rest[index];
        index += 1;
        if token == "--" {
            extras.extend(rest[index..].iter().cloned());
            break;
        }
        if !optional_like(token) || !token.starts_with("--") {
            extras.push(token.clone());
            continue;
        }
        let (flag, inline) = match token.split_once('=') {
            Some((f, v)) => (f.to_string(), Some(v.to_string())),
            None => (token.clone(), None),
        };
        let exact = specs.iter().find(|(n, _)| *n == flag);
        let candidates: Vec<&(&str, Kind)> = specs.iter().filter(|(n, _)| n.starts_with(flag.as_str())).collect();
        let spec = match (exact, candidates.len()) {
            (Some(spec), _) => *spec,
            (None, 1) => *candidates[0],
            (None, 0) => {
                extras.push(token.clone());
                continue;
            }
            (None, _) => {
                let names = candidates.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ");
                return Err(sub(format!("ambiguous option: {flag} could match {names}")));
            }
        };
        let (name, kind) = spec;
        if name == "--file" || name == "--harness" {
            match exclusive {
                Some(other) if other != name => {
                    return Err(sub(format!("argument {name}: not allowed with argument {other}")));
                }
                _ => exclusive = Some(name),
            }
        }
        if kind == Kind::Flag {
            if inline.is_some() {
                return Err(sub(format!(
                    "argument {name}: ignored explicit argument '{}'",
                    inline.unwrap_or_default()
                )));
            }
            args.values.push((name, String::new()));
            continue;
        }
        let value = match inline {
            Some(v) => v,
            None if index < rest.len() && !optional_like(&rest[index]) => {
                index += 1;
                rest[index - 1].clone()
            }
            None => return Err(sub(format!("argument {name}: expected one argument"))),
        };
        match kind {
            Kind::Choice(choices) if !choices.contains(&value.as_str()) => {
                let listed = choices.iter().map(|c| format!("'{c}'")).collect::<Vec<_>>().join(", ");
                return Err(sub(format!(
                    "argument {name}: invalid choice: '{value}' (choose from {listed})"
                )));
            }
            Kind::Int if python_int(&value).is_none() => {
                return Err(sub(format!("argument {name}: invalid int value: '{value}'")));
            }
            _ => {}
        }
        if name == "--root" {
            args.root = Some(value);
        } else {
            args.values.push((name, value));
        }
    }
    if matches!(command.as_str(), "protect" | "hook") && exclusive.is_none() {
        return Err(sub("one of the arguments --file --harness is required".into()));
    }
    if !extras.is_empty() {
        return Err(top(format!("unrecognized arguments: {}", extras.join(" "))));
    }
    Ok(args)
}

fn root_of(args: &Args) -> PathBuf {
    resolve(Path::new(args.root.as_deref().unwrap_or(".")))
}

fn context(args: &Args) -> ExitCode {
    let root = root_of(args);
    let host = args.get("--host");
    let loaded = config::load_config(&root).map_err(|e| match e {
        config::ConfigError::Invalid(m) | config::ConfigError::Guard(_, m) => m,
    });
    match loaded.and_then(|c| injected_context(&c, host)) {
        Ok(text) => {
            let mut output = json!({"hookSpecificOutput": {
                "hookEventName": args.get("--event").unwrap_or("SessionStart"),
                "additionalContext": text,
            }});
            if host.is_some()
                && let Some(map) = output.as_object_mut()
            {
                map.insert(
                    "loki".into(),
                    json!({"version": loki::VERSION, "formats": ["text", "json"]}),
                );
            }
            println!("{}", py::dumps(&output));
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("loki: context unavailable: {message}");
            ExitCode::from(2)
        }
    }
}

fn read_stdin_json(what: &str) -> Result<Value, CheckError> {
    let mut raw = Vec::new();
    std::io::stdin()
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut raw)
        .map_err(|e| CheckError::os(&e, None))?;
    let text = loki::proc::decode(&raw)
        .map_err(CheckError::Value)?
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    if text.chars().count() > 4 * 1024 * 1024 {
        return Err(CheckError::Value(format!("{what} input exceeds 4 MiB")));
    }
    serde_json::from_str(&text).map_err(|e| CheckError::Value(loki::pyjson::error_text(&text, &e)))
}

/// The protect/hook body: what blocks and what is only reported.
fn check(args: &Args, root: &Path, config: &config::Config) -> Result<(Vec<String>, Vec<String>), CheckError> {
    let deadline = Deadline::after(20.0);
    let mut warnings: Vec<String> = Vec::new();
    let harness = args.get("--harness");
    if args.get("--format") == Some("json") && harness.is_some() {
        return Err(CheckError::Value("--format json requires --file".into()));
    }
    let (paths, payload) = match harness {
        Some(harness) => {
            let payload = read_stdin_json("hook")?;
            (loki::harness_paths(&payload, harness)?, Some(payload))
        }
        None => (run::file_paths(&args.all("--file"))?, None),
    };
    if args.command == "protect" {
        let messages = run::protect_violations(
            root,
            config,
            &paths,
            payload.as_ref(),
            harness.unwrap_or(""),
            deadline,
            &mut warnings,
        )?;
        return Ok((messages, warnings));
    }
    let mut messages = run::protected(root, config, &paths)?;
    if messages.is_empty() {
        let targets: Vec<PathBuf> = paths.iter().map(|p| loki::abspath(root, p)).collect();
        messages.extend(hook::write_guardrail_violations(&targets, root, config, deadline)?);
        let typescript = targets.iter().any(|t| {
            let name = t
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            name.rfind('.')
                .filter(|&i| i > 0)
                .is_some_and(|i| loki::typescript::TYPESCRIPT_SUFFIXES.contains(&&name[i..]))
        });
        if messages.is_empty() && typescript {
            messages.extend(loki::typescript::typescript_findings(
                root,
                config,
                deadline,
                &mut warnings,
            )?);
        }
        if messages.is_empty() {
            let notes = match loki::slop::written_slop(root, &targets, deadline) {
                Ok(notes) => notes,
                Err(error) if matches!(error, CheckError::Value(_)) || error.is_os() => {
                    vec![format!("NOT CHECKED slop: {}", error.message())]
                }
                Err(other) => return Err(other),
            };
            if config::slop_block(config).map_err(CheckError::Value)? {
                for note in notes {
                    if note.contains("NOT CHECKED") {
                        warnings.push(note)
                    } else {
                        messages.push(note)
                    }
                }
            } else {
                warnings.extend(notes);
            }
            let strict = std::env::var("LOKI_STRICT").as_deref() == Ok("1");
            let mut batch = hook::Batch::default();
            for target in &targets {
                messages.extend(hook::check_file(
                    target,
                    root,
                    config,
                    strict,
                    deadline,
                    &mut warnings,
                    &targets,
                    &mut batch,
                    args.get("--python"),
                )?);
            }
        }
    }
    Ok((messages, warnings))
}

fn guard(args: &Args) -> ExitCode {
    let json = args.get("--format") == Some("json");
    let root = root_of(args);
    let outcome = match run::load(&root) {
        Err(error) => run::fail(&args.command, &error, json),
        Ok(_) if args.has("--record") => run::fail(&args.command, &CheckError::not_ported("--record"), json),
        Ok(config) if args.command == "protect" && args.get("--harness").is_none() => {
            let files = args.all("--file");
            run::protect_with(&root, &config, &files, args.get("--preview"), json, || {
                read_stdin_json("preview")
            })
        }
        Ok(config) => match check(args, &root, &config) {
            Ok((messages, warnings)) => run::report(
                &args.command,
                &messages,
                warnings,
                json,
                args.get("--harness").is_some(),
            ),
            Err(error) => run::fail(&args.command, &error, json),
        },
    };
    eprint!("{}", outcome.stderr);
    print!("{}", outcome.stdout);
    ExitCode::from(outcome.status)
}

fn exit(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// `scan` and `slop`: the policy loads first, then the command runs.
fn audit(args: &Args) -> ExitCode {
    let root = root_of(args);
    let config = match config::load_config(&root) {
        Ok(config) => config,
        Err(config::ConfigError::Invalid(m)) => {
            eprintln!("loki: invalid hook input: {m}");
            return ExitCode::from(1);
        }
        Err(config::ConfigError::Guard(kind, m)) => {
            eprintln!("loki: guard error: {kind}: {m}");
            return ExitCode::from(1);
        }
    };
    let report = |error: CheckError| {
        match error {
            CheckError::Value(m) => eprintln!("loki: {m}"),
            other if other.is_os() => eprintln!("loki: {}", other.message()),
            other => eprintln!("loki: {}", other.describe()),
        }
        ExitCode::from(1)
    };
    if args.command == "slop" {
        let top = args.get("--top").and_then(python_int).unwrap_or(10);
        return match loki::slop::slop_command(&root, &config, args.get("--base"), args.has("--json"), top) {
            Ok(code) => exit(code),
            Err(error) => report(error),
        };
    }
    if args.has("--ruff-new") {
        return match hook::ruff_new_violations(&root, args.get("--base"), None, Deadline::none()) {
            Ok(findings) => {
                if !findings.is_empty() {
                    println!("{}", findings.join("\n"));
                }
                exit(i32::from(!findings.is_empty()))
            }
            Err(hook::RuffError::MissingConfig(m)) => report(CheckError::Value(m)),
            Err(hook::RuffError::Other(error)) => report(error),
        };
    }
    let strict = args.has("--strict") || std::env::var("LOKI_STRICT").as_deref() == Ok("1");
    match loki::scan::scan(
        &root,
        &config,
        args.has("--online"),
        strict,
        args.get("--base"),
        args.get("--python"),
    ) {
        Ok(code) => exit(code),
        Err(error) => report(error),
    }
}

fn init(args: &Args) -> ExitCode {
    let result = if args.has("--minimal") {
        if args.has("--shell-guard") || args.get("--managed-dir").is_some() {
            Err(CheckError::Value(
                "--minimal installs only .loki/; it takes neither --shell-guard nor --managed-dir".into(),
            ))
        } else {
            loki::scan::install_minimal(args.get("--dir").unwrap_or("."), args.has("--force"))
        }
    } else {
        Err(CheckError::not_ported(
            "init without --minimal (agent hooks, linter configuration and CI templates)",
        ))
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(CheckError::Value(m)) => {
            eprintln!("loki: {m}");
            ExitCode::from(1)
        }
        Err(other) => {
            eprintln!(
                "loki: {}",
                if other.is_os() {
                    other.message().to_string()
                } else {
                    other.describe()
                }
            );
            ExitCode::from(1)
        }
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = shell_cli::dispatch(&argv) {
        return code;
    }
    let args = match parse(&argv) {
        Ok(args) => args,
        Err(ParseError::Exit(code)) => return exit(code),
        Err(ParseError::Top(message)) => {
            eprintln!("{TOP_USAGE}\nloki: error: {message}");
            return ExitCode::from(2);
        }
        Err(ParseError::Sub(command, message)) => {
            eprintln!("{}\nloki {command}: error: {message}", usage(&command));
            return ExitCode::from(2);
        }
    };
    match args.command.as_str() {
        "context" => context(&args),
        "protect" | "hook" => guard(&args),
        "scan" | "slop" => audit(&args),
        "init" => init(&args),
        other => {
            eprintln!(
                "loki: {}",
                CheckError::not_ported(&format!("the {other} command")).describe()
            );
            ExitCode::from(2)
        }
    }
}
