/**
 * How the model is told about the RLM REPL: the `rlm` tool description, its line and guidelines in Pi's
 * system prompt, and the runtime guide section. Modelled on Prime Intellect's nano-rlm (its RUNTIME_PROMPT,
 * BASH_SKILL_PROMPT, EDIT_SKILL_PROMPT and DELEGATION_PROMPT), adapted to Ultron's actual API.
 */

/** Native Pi tools that the RLM REPL replaces by default (they stay registered for `--tools` and the opt-out). */
import { CODE_SKILLS_PROMPT } from "../code-skills.ts";
import { CONTEXT_PROMPT } from "../context-control.ts";
import { AGENT_CLASS_PROMPT } from "./agent-class-prompt.ts";
import { INFERENCE_PROMPT } from "./inference.ts";

export const NATIVE_FILE_TOOLS = ["read", "edit", "write", "bash"] as const;

export type RlmToolMode = "rlm" | "native";

/** `ULTRON_TOOLS=native` restores Pi's read, edit, write and bash next to `rlm`; anything else is the REPL-only default. */
export function rlmToolMode(env: NodeJS.ProcessEnv = process.env): RlmToolMode {
	return env.ULTRON_TOOLS?.trim().toLowerCase() === "native" ? "native" : "rlm";
}

/**
 * The root and child lanes' built-in tools. By default the REPL is the only one (nano-rlm's
 * `DEFAULT_TOOLS = ("ipython",)`): shell and edits are Python skills inside it. Extension tools are added by the
 * worker on top, and Pi's `--tools`/`--exclude-tools` still apply when given.
 */
export function defaultBuiltinToolNames(env: NodeJS.ProcessEnv = process.env): string[] {
	return rlmToolMode(env) === "native" ? [...NATIVE_FILE_TOOLS, "rlm"] : ["rlm"];
}

/**
 * The `rlm` tool's description: what a cell is and which APIs are pre-imported. How to use them is the runtime
 * guide's job (system prompt), and each API's detail is in its docstring (`help(obj)`), so nothing is said twice.
 */
export const RLM_TOOL_DESCRIPTION = [
	"Run a Python cell in your persistent REPL: variables, imports and functions persist across calls, top-level `await` works, and the last expression's value is shown with anything printed (kept as `_`).",
	"Pre-imported: `bash`, `read`, `edit`, `view_image`, `rlm`, `agents`, `workflows`, `background`, `tools`, `mcp`, `ctx`, `skills`, `memory`, `hints`, `agent`, `Budget`, `state`, `jev`, `preview`, `asyncio`; `help(obj)` shows any API's docs.",
].join("\n");

export const RLM_TOOL_SNIPPET = "Python REPL for files, shell (`bash`), edits (`edit`), data and subagents";

/** Rules for Pi's guideline list: how to use the REPL, depending on whether Pi's native file tools are active too. */
export function rlmToolGuidelines(activeTools: readonly string[]): string[] {
	const nativeFileTools = NATIVE_FILE_TOOLS.some((name) => activeTools.includes(name));
	if (nativeFileTools) {
		return [
			"Use the rlm tool for multi-step or data-heavy work: load and process data in Python, keep it in variables across calls, and print only what you need",
		];
	}
	return [
		"Do all work through the rlm tool; batch related steps into one cell and keep intermediate results in variables",
	];
}

const BASH_SKILL = `- \`out = await bash('''cmd''')\` (always triple-quoted, even for one line; the text reaches bash as written, backslashes included) returns stdout and stderr as a str, with \`[exit code N]\` appended on failure (\`out.ok\`, \`out.exit_code\`). Prefer \`rg -n\`, \`sed -n 'A,Bp'\`, \`head\` over whole files. A command still running after 30 s continues as a job (\`out.job\`; its end arrives as an event); \`yield_after=0\` starts a suite or build you know takes minutes as a job at once. \`help(bash)\`.`;

const BASH_SKILL_WITH_TOOL = `- Inside rlm you can also run shell with \`out = await bash('''command''')\`: it returns the output as a string (\`[exit code N]\` appended on failure; \`out.ok\`; a command still running after 30 s continues as a job, \`out.job\`), useful when mixing shell and Python in one cell.`;

const READ_SKILL = `- \`await read(path)\` returns a file's text (a ContextHandle over 256 KiB); \`await view_image(path)\` shows you an image.`;

const EDIT_SKILL = `- \`await edit(path=..., old_str=..., new_str=...)\` replaces exactly one occurrence and raises ValueError when old_str is absent or ambiguous: read first, copy old_str exactly, several hunks in one cell. Create files with \`Path(p).write_text(...)\`.`;

const EDIT_SKILL_WITH_TOOL =
	"- The native edit tool and the `edit` skill both replace exactly one occurrence of old text (the skill raises ValueError when old_str is absent or ambiguous); use either.";

const PROJECT_ENV = `- The kernel is the system Python without the project's packages: run all project code (tests, repros, builds, imports) through \`bash\` with the project's own interpreter and toolchain (\`.venv/bin/python -m pytest -q\`, \`npm test\`). Work on data files in the cell itself, not in a \`python - <<EOF\` heredoc through \`bash\`: parse, count and join there, and write the requested output in the cell that computes it.`;

const RUNTIME = `## Runtime
Each rlm call runs a cell in your lane's persistent kernel: program over files, shell and agents, keep data in variables and print only what the next step needs. Output over about 20 KB is cut in the middle and a large value is shown by reference; \`preview(x)\` gives a bounded view. APIs are pre-imported and async; check \`help(obj)\` before guessing. Act on a trailing \`[hint:<tag>]\` line (\`await hints.mute(tag)\` once understood).
A turn ends when you reply without calling rlm; that reply is your answer. Never end a turn with a promise ("I'll check next"): do the work now. An exception ends the cell after what it printed: fix the cause instead of rerunning, since its writes and started agents may already have happened. Subagents, tasks and jobs survive a kernel restart, and so does \`state\` (a dict); re-create imports and functions.`;

/** How completion events reach the model (ULTRON_ASYNC_EVENTS, on by default). */
const ASYNC_EVENTS = `Nothing needs polling: a job, tool call, subagent or task that ends while you are not waiting sends a \`<runtime_event kind=... id=... status=... fetch=...>\` message, which starts a new turn if yours ended. Start long work and do the rest meanwhile; never sleep, poll or loop waiting, and if only that result is left, await it (\`await job.result()\`). Run independent operations together with \`asyncio.gather\`.`;

const ASYNC_EVENTS_OFF =
	"Completions are not announced in this session (ULTRON_ASYNC_EVENTS=off): when you need a result, wait for it with `await job.result()`, `await rlm.collect(...)` or `await agents.result(id)`. Run independent operations together with `asyncio.gather`.";

/** The cost rule that matters most on large inputs: narrow with code, read candidates, delegate last. */
const SEARCH_FIRST = `## Search before delegating
For many files or a large input, narrow with code first: 1. search the concept and its synonyms (\`re\`, \`h.search\`, \`rg -il\`) and count hits; 2. print one compact line per candidate (id and matching sentence); 3. read the deciding passages of unclear candidates and judge them yourself. Use \`rlm.map\` only for candidates a line or two cannot settle, or text still over about 100 KB, one passage per frame. Do not spawn subagents to read or classify documents: each re-sends this prompt and its transcript every turn. Example: \`help(rlm)\`.`;

const BOUNDED_INFERENCE = `## Bounded inference\n${INFERENCE_PROMPT}`;

/** Delegation, with how to wait for children depending on whether their ends are announced (ULTRON_ASYNC_EVENTS). */
function delegationPrompt(asyncEvents: boolean): string {
	const wait = asyncEvents
		? "`await rlm.collect(hs)` (free), or end your turn: each end arrives as a `child_done` event"
		: "`await rlm.collect(hs)` (free)";
	return `## Delegation
\`h = await rlm.spawn(brief, name=...)\` starts a subagent (own REPL, your tools and files, not your chat). Spawn only for independent multi-step work, with a self-contained brief (goal, paths, constraints, what to return), several at once; \`depth=N\` lets a child delegate too (≤3 levels) if its part splits again. Then do only your own work that no child owns; never check on children's files, logs or progress: results come to you. With nothing of your own left, ${wait}. Trust only verdicts whose \`check.outcome\` is "verified"; re-check the rest. \`help(rlm.spawn)\`.
If you are a subagent, do the brief yourself (spawn only if given depth), then \`await rlm.finish(status, summary, evidence=[...], changed_files=[...])\` and reply briefly with paths and uncertainties.
Typed agents, graphs, background jobs: \`help(agents)\`, \`help(workflows)\`, \`help(background)\`.`;
}

const CONTEXT = `## Other APIs\n${CONTEXT_PROMPT}\n${CODE_SKILLS_PROMPT}\n${AGENT_CLASS_PROMPT}`;

const MEMORY =
	"- `memory.prepare(query)` recalls long-term memory, `memory.propose(text, evidence)` keeps a fact; `jev.triage(prompt)` rates a request.";

/** An extension tool as the runtime guide lists it. */
export interface ExtensionToolSummary {
	readonly name: string;
	readonly description: string;
}

/** At most this many extension tools are listed by name in the guide; `tools.list()` shows the rest. */
export const EXTENSION_TOOLS_LISTED = 12;

/**
 * How to call extension tools (and MCP servers through the pi-mcp-adapter's `mcp` gateway) from Python. Bounded:
 * a dozen tools, one line each, and the server names.
 */
export function extensionToolsPrompt(
	tools: readonly ExtensionToolSummary[],
	options: { mcpServers?: readonly string[]; native?: readonly string[] } = {},
): string | undefined {
	const repl = tools.filter((tool) => !options.native?.includes(tool.name));
	if (repl.length === 0) return undefined;
	const line = (text: string) => {
		const flat = text.replace(/\s+/g, " ").trim();
		return flat.length > 120 ? `${flat.slice(0, 119)}…` : flat;
	};
	const listed = repl
		.slice(0, EXTENSION_TOOLS_LISTED)
		.map((tool) => `- ${tool.name}: ${line(tool.description) || "(no description)"}`);
	if (repl.length > EXTENSION_TOOLS_LISTED)
		listed.push(`- … ${repl.length - EXTENSION_TOOLS_LISTED} more: \`await tools.list()\``);
	const hasMcp = repl.some((tool) => tool.name === "mcp");
	const servers = (options.mcpServers ?? []).slice(0, 12);
	const parts = [
		"## Extension tools",
		`Tools from Pi extensions are pre-imported async skills in the REPL, not separate tools. \`r = await tools.call("name", {"key": value})\` or \`await tools.<name>(key=value)\` runs one in the host with its real context and returns a ToolResult: the text as a string (kept whole up to 256 KiB, so keep it in a variable and print only what you need), \`.details\` (structured, or None) and \`.json()\` to parse a JSON reply; a failing tool raises ToolError. \`await tools.list()\` and \`await tools.describe(name)\` give names, descriptions and JSON-schema parameters. Available:\n${listed.join("\n")}`,
	];
	if (hasMcp)
		parts.push(
			`MCP servers${servers.length > 0 ? ` (${servers.join(", ")})` : ""} are reached through the \`mcp\` namespace: \`await mcp.servers()\`, \`await mcp.tools("server")\` (tool names), \`await mcp.describe("tool")\` (its parameters), \`await mcp.search("query")\`, and \`r = await mcp.call("tool", key=value, ...)\`, whose keyword arguments are the MCP tool's arguments (or \`await mcp.<server>.<tool>(...)\`, with \`-\` in the server name written as \`_\`). Gateway errors (unknown tool, server not connected, auth required) raise McpError.`,
		);
	parts.push(
		`Slow calls never hold you: a plain call still running after 30 s keeps running in the host (\`r.running\` True, \`r.call\` its ToolCall) and its completion arrives as a \`<runtime_event kind="tool_done">\` whose fetch returns the result (\`await tools.result(id)\`). \`yield_after=0\` returns a ToolCall handle at once (\`await call.result()\` waits, \`await call.cancel()\` stops it); \`yield_after=None\` waits however long it takes. Run independent calls together in one cell with \`asyncio.gather\` and keep ids and results in variables. For a start-run/wait-run pair (such as Exa's \`exa_agent_create_run\` and \`exa_agent_wait_run\`), start every run in one cell, then leave the waits running as calls instead of blocking the turn:
\`\`\`python
queries = ["first question", "second question"]
runs = await asyncio.gather(*(mcp.call("exa-agent_exa_agent_create_run", query=q, effort="medium") for q in queries))
run_ids = [r.json()["id"] for r in runs]  # the id field as the reply names it: print(runs[0]) once if unsure
waits = await asyncio.gather(*(mcp.call("exa-agent_exa_agent_wait_run", run_id=i, yield_after=0) for i in run_ids))
\`\`\`
Then do other work or end your turn: each wait's completion arrives as a runtime event.`,
	);
	return parts.join("\n\n");
}

/**
 * The runtime guide for the system prompt when `rlm` is among the active tools. It tells the model how to
 * work in the REPL; when Pi's native bash or edit tools are active too, the skill lines say so.
 */
export function rlmRuntimePrompt(
	activeTools: readonly string[],
	options: {
		asyncEvents?: boolean;
		/** Extension tools callable from the REPL, and MCP servers behind the `mcp` gateway. */
		extensionTools?: readonly ExtensionToolSummary[];
		mcpServers?: readonly string[];
		/** Extension tools that are native model tools as well (they are still callable from Python). */
		nativeExtensionTools?: readonly string[];
	} = {},
): string | undefined {
	if (!activeTools.includes("rlm")) return undefined;
	const nativeBash = activeTools.includes("bash");
	const nativeEdit = activeTools.includes("edit");
	const skills = [
		"## Skills",
		nativeBash ? BASH_SKILL_WITH_TOOL : BASH_SKILL,
		READ_SKILL,
		nativeEdit ? EDIT_SKILL_WITH_TOOL : EDIT_SKILL,
		PROJECT_ENV,
	];
	const extensionTools = extensionToolsPrompt(options.extensionTools ?? [], {
		...(options.mcpServers === undefined ? {} : { mcpServers: options.mcpServers }),
		...(options.nativeExtensionTools === undefined ? {} : { native: options.nativeExtensionTools }),
	});
	return [
		`${RUNTIME}\n${options.asyncEvents === false ? ASYNC_EVENTS_OFF : ASYNC_EVENTS}`,
		skills.join("\n"),
		SEARCH_FIRST,
		BOUNDED_INFERENCE,
		...(extensionTools === undefined ? [] : [extensionTools]),
		delegationPrompt(options.asyncEvents !== false),
		`${CONTEXT}\n${MEMORY}`,
	].join("\n\n");
}
