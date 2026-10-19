//! The runtime guide: the system prompt section that tells the model how to work in the RLM REPL.
//! Port of packages/coding-agent/src/ultron/rlm/prompt.ts (`rlmRuntimePrompt`, `extensionToolsPrompt`,
//! `rlmToolGuidelines`) and the prompt texts it imports.

/// Native Pi tools that the REPL replaces by default.
pub const NATIVE_FILE_TOOLS: [&str; 4] = ["read", "edit", "write", "bash"];

/// At most this many extension tools are listed by name in the guide.
pub const EXTENSION_TOOLS_LISTED: usize = 12;

/// How detached work's completions reach the root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventDelivery {
    Wake,
    NextCall,
    Off,
}

/// An extension tool as the guide lists it.
#[derive(Clone, Debug)]
pub struct ExtensionToolSummary {
    pub name: String,
    pub description: String,
}

/// Options of `rlmRuntimePrompt`. `hints` is explicit: the TS default reads `ULTRON_HINTS`.
#[derive(Clone, Debug)]
pub struct RuntimeGuideOptions {
    pub delivery: EventDelivery,
    pub external_root: bool,
    pub hints: bool,
    pub extension_tools: Vec<ExtensionToolSummary>,
    pub mcp_servers: Vec<String>,
    pub native_extension_tools: Vec<String>,
    /// The host serves the Rust session runtime's APIs that TS lacks: the durable many-agent APIs (`keep=True`
    /// children, `rlm.ask`, `swarm`, `board`, `notes`), typed memory records with file pointers, `rlm.images` and `schedule`. The
    /// TS host and the Claude root do not. Off keeps the guide equal to TS.
    pub durable_apis: bool,
}

impl Default for RuntimeGuideOptions {
    fn default() -> Self {
        Self {
            delivery: EventDelivery::Wake,
            external_root: false,
            hints: false,
            extension_tools: Vec::new(),
            mcp_servers: Vec::new(),
            native_extension_tools: Vec::new(),
            durable_apis: false,
        }
    }
}

const BASH_SKILL: &str = r#"- `out = await bash('''cmd''')` (always triple-quoted, even for one line; the text reaches bash as written, backslashes included) returns stdout and stderr as a str, with `[exit code N]` appended on failure (`out.ok`, `out.exit_code`). Prefer `rg -n`, `sed -n 'A,Bp'`, `head` over whole files. A command still running after 30 s continues as a job (`out.job`; its end arrives as an event); `yield_after=0` starts a suite or build you know takes minutes as a job at once. `help(bash)`."#;
const BASH_SKILL_WITH_TOOL: &str = r#"- Inside rlm you can also run shell with `out = await bash('''command''')`: it returns the output as a string (`[exit code N]` appended on failure; `out.ok`; a command still running after 30 s continues as a job, `out.job`), useful when mixing shell and Python in one cell."#;
const READ_SKILL: &str = r#"- `await read(path)` returns a file's text (a ContextHandle over 256 KiB); `await view_image(path)` shows you an image."#;
const EDIT_SKILL: &str = r#"- `await edit(path=..., old_str=..., new_str=...)` replaces exactly one occurrence and raises ValueError when old_str is absent or ambiguous: read first, copy old_str exactly, several hunks in one cell. Create files with `await write(path, text)`."#;
const EDIT_SKILL_WITH_TOOL: &str = r#"- The native edit tool and the `edit` skill both replace exactly one occurrence of old text (the skill raises ValueError when old_str is absent or ambiguous); use either."#;
const PROJECT_ENV: &str = r#"- The kernel is the system Python without the project's packages: run all project code (tests, repros, builds, imports) through `bash` with the project's own interpreter and toolchain (`.venv/bin/python -m pytest -q`, `npm test`). Work on data files in the cell itself, not in a `python - <<EOF` heredoc through `bash`, and write the requested output in the cell that computes it."#;
const RUNTIME: &str = r#"## Runtime
Each rlm call runs a cell in your lane's persistent kernel: program over files, shell and agents, keep data in variables and print only what the next step needs. Output over ~20 KB is cut in the middle (the marker names a file with all of it) and a large value is shown by reference; `preview(x)` gives a bounded view. APIs are pre-imported and async; check `help(obj)` before guessing.
A turn ends when you reply without calling rlm; that reply is your answer. Never end a turn with a promise ("I'll check next"): do the work now. An exception ends the cell after what it printed: fix the cause, do not rerun: its writes and started agents may already have happened. Subagents, tasks and jobs survive a kernel restart, and so does `state` (a dict); re-create imports and functions."#;
const HINTS_LINE: &str = r#"Act on a trailing `[hint:<tag>]` line (`await hints.mute(tag)` once understood)."#;
const ASYNC_EVENTS: &str = r#"Nothing needs polling: a job, tool call, subagent or task that ends while you are not waiting sends a `<runtime_event kind=... id=... status=... fetch=...>` message, which starts a new turn if yours ended. Start long work and do the rest meanwhile; never sleep, poll or loop waiting, and if only that result is left, await it (`await job.result()`). Run independent operations together with `asyncio.gather`."#;
const ASYNC_EVENTS_NEXT_CALL: &str = r#"Nothing needs polling: a job, tool call, subagent or task that ends while you are not waiting on it is reported as a `<runtime_event kind=... id=... status=... fetch=...>` line at the top of your next rlm result (after you reply, with the user's next message). Nothing wakes you after you reply, so before ending your turn await what the answer still needs (`await rlm.collect(hs)`, `await job.result()`; waiting costs no model turns). Start long work and do the rest meanwhile; never sleep, poll or loop checking. Run independent operations together with `asyncio.gather`."#;
const ASYNC_EVENTS_OFF: &str = r#"Completions are not announced in this session (ULTRON_ASYNC_EVENTS=off): when you need a result, wait for it with `await job.result()`, `await rlm.collect(...)` or `await agents.result(id)`. Run independent operations together with `asyncio.gather`."#;
const SEARCH_FIRST: &str = r#"## Search before delegating
For many files or a large input, narrow with code first: 1. search the concept and its synonyms (`re`, `h.search`, `rg -il`) and count hits; 2. print one compact line per candidate (id and matching sentence); 3. read the deciding passages of unclear candidates and judge them yourself. Use `rlm.map` only for candidates a line or two cannot settle, or text over ~100 KB, one passage per frame. Do not spawn subagents to read or classify documents: each re-sends this prompt and its transcript every turn."#;
const MEMORY: &str = r#"- `memory.prepare(query)` recalls long-term memory, `memory.propose(text, evidence)` keeps a fact; neither runs on its own."#;
const INFERENCE_PROMPT: &str = r#"Large inputs stay out of your context as handles; examples: `help(rlm.load)`, `help(rlm.map)`.
- `h = await rlm.load(path_or_text)`: `h.search(regex)`, `h.count(regex)`, `h.lines(a, b)`, `h.chunks(n)`; views print their text.
- `await rlm.infer(task, context=[views], contract=T, budget=Budget(calls, tokens, depth))`: one private sub-model call (no transcript, no tools), contract-validated and re-asked; running out returns a falsy `Incomplete`.
- `await rlm.map(task, items, contract=T)`: one frame per item, in order, under one shared budget (default 500,000 tokens; `budget=Budget(tokens=...)` for more).
- Every frame is a model request: filter with code first, give each frame only the passage it must judge, and compute exact numbers in Python."#;
const CONTEXT_PROMPT: &str = r#"- `ctx` edits your view of your own context (`ctx.history()`, `ctx.forget`, `ctx.summarize`, `ctx.pin`, `ctx.note`); a seen task result collapses to one line (`await agents.result(id)`). A `[context: ...]` line on a result means your context nears its limit: free space with `ctx` then. `help(ctx)`."#;
const CODE_SKILLS_PROMPT: &str = r#"- Save a working procedure that will recur as a tested code skill (`await skills.propose_code(...)`); reuse one with `from code_skills import <name>`. `help(skills)`."#;
const AGENT_CLASS_PROMPT: &str = r#"- `@agent` classes: docstring-only async methods are typed model calls, annotated fields durable state. `help(agent)`."#;
const EXT_INTRO: &str = r#"Tools from Pi extensions are pre-imported async skills in the REPL, not separate tools. `r = await tools.call("name", {"key": value})` or `await tools.<name>(key=value)` runs one in the host with its real context and returns a ToolResult: the text as a string (kept whole up to 256 KiB, so keep it in a variable and print only what you need), `.details` (structured, or None) and `.json()` to parse a JSON reply; a failing tool raises ToolError. `await tools.list()` and `await tools.describe(name)` give names, descriptions and JSON-schema parameters. "#;
const MCP_TAIL: &str = r#"are reached through the `mcp` namespace: `await mcp.servers()`, `await mcp.tools("server")` (tool names), `await mcp.describe("tool")` (its parameters), `await mcp.search("query")`, and `r = await mcp.call("tool", key=value, ...)`, whose keyword arguments are the MCP tool's arguments (or `await mcp.<server>.<tool>(...)`, with `-` in the server name written as `_`). Gateway errors (unknown tool, server not connected, auth required) raise McpError."#;
const SLOW_HEAD: &str = r#"Slow calls never hold you: a plain call still running after 30 s keeps running in the host (`r.running` True, `r.call` its ToolCall) and its completion arrives as a `<runtime_event kind="tool_done">`"#;
const SLOW_BODY: &str = r#" whose fetch returns the result (`await tools.result(id)`). `yield_after=0` returns a ToolCall handle at once (`await call.result()` waits, `await call.cancel()` stops it); `yield_after=None` waits however long it takes. Run independent calls together in one cell with `asyncio.gather` and keep ids and results in variables. For a start-run/wait-run pair (such as Exa's `exa_agent_create_run` and `exa_agent_wait_run`), start every run in one cell, then leave the waits running as calls instead of blocking the turn:
```python
queries = ["first question", "second question"]
runs = await asyncio.gather(*(mcp.call("exa-agent_exa_agent_create_run", query=q, effort="medium") for q in queries))
run_ids = [r.json()["id"] for r in runs]  # the id field as the reply names it: print(runs[0]) once if unsure
waits = await asyncio.gather(*(mcp.call("exa-agent_exa_agent_wait_run", run_id=i, yield_after=0) for i in run_ids))
```
"#;
const DELEGATION_HEAD: &str = r#"## Delegation
`h = await rlm.spawn(brief, name=...)` starts a subagent (own REPL, your tools and files, not your chat). Spawn only for independent multi-step work, with a self-contained brief (goal, paths, constraints, what to return), several at once; `depth=N` lets a child delegate too (≤3 levels). Children editing files: `worktree=True` each (coupled work in one brief), then `await rlm.merge(hs)`. Then do only work no child owns; never check on children's files, logs or progress: results come to you. With nothing of your own left, "#;
const DELEGATION_TAIL: &str = r#". Trust only verdicts whose `check.outcome` is "verified"; re-check the rest. `help(rlm.spawn)`.
If you are a subagent, do the brief yourself (spawn only if given depth), then `await rlm.finish(status, summary, evidence=[...], changed_files=[...])` and reply briefly with paths and uncertainties.
Typed agents, graphs, jobs: `help(agents)`, `help(workflows)`, `help(background)`."#;

const TYPED_MEMORY: &str = r#" A fact tied to files can be typed: `memory.propose(text, evidence, type="pitfall", situation="...", path_globs=["src/**"])`; a `[memory] <id> <type>: <situation>` line on a result points at one matching a file you changed (`await memory.get(id)`)."#;
const IMAGES_PROMPT: &str = r#"- `await rlm.images.generate(prompt, size="16:9")` makes an image and saves it to a file: `{"paths": [...], "model": ...}` (`help(rlm.images)`)."#;
const SCHEDULE_PROMPT: &str = r#"- `await schedule.every("1h", prompt)` or `schedule.at(when, prompt)` sends this session `prompt` later as a new turn (`help(schedule)`)."#;
const SWARM_PROMPT: &str = r#"Persistent agents: `h = await rlm.spawn(brief, name=..., keep=True)` stays alive after its answer (its context, files and kernel survive restarts); `await rlm.ask(h, question)` asks it a follow-up, so your context grows only by the question and the answer: ask the child that already read something instead of re-reading it. `await swarm.route(question)` ranks the agents holding matching context; `swarm.send(name, text)` and `swarm.inbox()` message agents; `board` (tasks with exclusive claims) and `notes` are the session's shared durable records. Contracts: `rlm.spawn(brief, name=..., contract={"checks": [{"run": "cargo test -q"}, {"changed_files_within": ["src/**"]}]}, hop={"retry": 1})` makes the host run the checks itself when the child ends, and its result's `check["contract"]["verdict"]` is "verified" or "contract_failed" (`help(rlm.spawn)`). `help(swarm)`."#;

/// Guideline bullets for the `rlm` tool, depending on whether native file tools are active too.
pub fn rlm_tool_guidelines(active_tools: &[&str]) -> Vec<String> {
    let native = NATIVE_FILE_TOOLS.iter().any(|name| active_tools.contains(name));
    vec![if native {
        "Use the rlm tool for multi-step or data-heavy work: load and process data in Python, keep it in variables across calls, and print only what you need"
    } else {
        "Do all work through the rlm tool; batch related steps into one cell and keep intermediate results in variables"
    }
    .to_string()]
}

/// JS `text.replace(/\s+/g, " ").trim()`.
pub(crate) fn collapse_whitespace(text: &str) -> String {
    text.split(crate::js_space)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Truncate to `max` characters with an ellipsis, like the TS `slice(0, max - 1) + "…"`.
fn one_line(text: &str) -> String {
    let flat = collapse_whitespace(text);
    if flat.chars().count() > 120 {
        format!("{}…", flat.chars().take(119).collect::<String>())
    } else {
        flat
    }
}

/// How to call extension tools (and MCP servers through `mcp`) from Python; `None` without REPL tools.
pub fn extension_tools_prompt(
    tools: &[ExtensionToolSummary],
    mcp_servers: &[String],
    native: &[String],
    delivery: EventDelivery,
) -> Option<String> {
    let repl: Vec<&ExtensionToolSummary> = tools.iter().filter(|tool| !native.contains(&tool.name)).collect();
    if repl.is_empty() {
        return None;
    }
    let mut listed: Vec<String> = repl
        .iter()
        .take(EXTENSION_TOOLS_LISTED)
        .map(|tool| {
            let line = one_line(&tool.description);
            let line = if line.is_empty() {
                "(no description)".to_string()
            } else {
                line
            };
            format!("- {}: {line}", tool.name)
        })
        .collect();
    if repl.len() > EXTENSION_TOOLS_LISTED {
        listed.push(format!(
            "- … {} more: `await tools.list()`",
            repl.len() - EXTENSION_TOOLS_LISTED
        ));
    }
    let mut parts = vec![
        "## Extension tools".to_string(),
        format!("{EXT_INTRO}Available:\n{}", listed.join("\n")),
    ];
    if repl.iter().any(|tool| tool.name == "mcp") {
        let servers: Vec<&str> = mcp_servers.iter().take(12).map(String::as_str).collect();
        let named = if servers.is_empty() {
            String::new()
        } else {
            format!(" ({})", servers.join(", "))
        };
        parts.push(format!("MCP servers{named} {MCP_TAIL}"));
    }
    let next_call = delivery == EventDelivery::NextCall;
    parts.push(format!(
        "{SLOW_HEAD}{}{SLOW_BODY}{}",
        if next_call { " (at the top of your next rlm result)" } else { "" },
        if next_call {
            "Then do other work: each wait's completion arrives as a runtime event with a later rlm result; await the ones your answer needs before replying."
        } else {
            "Then do other work or end your turn: each wait's completion arrives as a runtime event."
        }
    ));
    Some(parts.join("\n\n"))
}

fn delegation_prompt(delivery: EventDelivery) -> String {
    let wait = if delivery == EventDelivery::Wake {
        "`await rlm.collect(hs)` (free), or end your turn: each end arrives as a `child_done` event"
    } else {
        "`await rlm.collect(hs)` (free)"
    };
    format!("{DELEGATION_HEAD}{wait}{DELEGATION_TAIL}")
}

/// The runtime guide for the system prompt when `rlm` is among the active tools.
pub fn rlm_runtime_prompt(active_tools: &[&str], options: &RuntimeGuideOptions) -> Option<String> {
    if !active_tools.contains(&"rlm") {
        return None;
    }
    let delivery = options.delivery;
    let skills = [
        "## Skills",
        if active_tools.contains(&"bash") {
            BASH_SKILL_WITH_TOOL
        } else {
            BASH_SKILL
        },
        READ_SKILL,
        if active_tools.contains(&"edit") {
            EDIT_SKILL_WITH_TOOL
        } else {
            EDIT_SKILL
        },
        PROJECT_ENV,
    ];
    let extension_tools = extension_tools_prompt(
        &options.extension_tools,
        &options.mcp_servers,
        &options.native_extension_tools,
        delivery,
    );
    let events = match delivery {
        EventDelivery::Off => ASYNC_EVENTS_OFF,
        EventDelivery::NextCall => ASYNC_EVENTS_NEXT_CALL,
        EventDelivery::Wake => ASYNC_EVENTS,
    };
    let runtime = if options.hints {
        RUNTIME.replacen("before guessing.", &format!("before guessing. {HINTS_LINE}"), 1)
    } else {
        RUNTIME.to_string()
    };
    let context = if options.external_root {
        format!("## Other APIs\n{CODE_SKILLS_PROMPT}\n{AGENT_CLASS_PROMPT}")
    } else {
        format!("## Other APIs\n{CONTEXT_PROMPT}\n{CODE_SKILLS_PROMPT}\n{AGENT_CLASS_PROMPT}")
    };
    let mut parts = vec![
        format!("{runtime}\n{events}"),
        skills.join("\n"),
        SEARCH_FIRST.to_string(),
        format!("## Bounded inference\n{INFERENCE_PROMPT}"),
    ];
    parts.extend(extension_tools);
    if options.durable_apis {
        parts.push(format!("{}\n{SWARM_PROMPT}", delegation_prompt(delivery)));
    } else {
        parts.push(delegation_prompt(delivery));
    }
    if options.durable_apis {
        parts.push(format!(
            "{context}\n{MEMORY}{TYPED_MEMORY}\n{IMAGES_PROMPT}\n{SCHEDULE_PROMPT}"
        ));
    } else {
        parts.push(format!("{context}\n{MEMORY}"));
    }
    Some(parts.join("\n\n"))
}
