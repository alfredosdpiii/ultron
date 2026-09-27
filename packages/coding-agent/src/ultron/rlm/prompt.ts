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

export const RLM_TOOL_DESCRIPTION = [
	"Run a Python cell in your persistent RLM REPL. Variables, imports and functions persist across calls; top-level `await` works; the last expression's value is shown with anything printed (and kept as `_`). Output over about 20 KB is cut in the middle.",
	"Pre-imported: `bash`, `edit`, `read`, `tools`, `mcp`, `rlm`, `agents`, `workflows`, `background`, `memory`, `ctx`, `skills`, `hints`, `agent`/`Agent`, `Budget`, `state`, `jev`, `preview`, `asyncio`; `help(obj)` shows any API's docs.",
	"- `out = await bash('''command''')` returns the shell output as a string (`[exit code N]` appended on failure; `out.ok`); a command still running after 30 s continues as a job whose completion arrives as a `<runtime_event>`.",
	'- `await edit(path="file.py", old_str=..., new_str=...)` replaces exactly one occurrence and raises ValueError when old_str is absent or ambiguous; create files with `Path(p).write_text(...)`.',
	"- `await read(path)` returns a file's text (a handle over 256 KiB). `rlm.load`/`rlm.infer`/`rlm.map` keep large inputs out of your context; `rlm.spawn` starts a subagent; `tools.call`/`mcp.call` run extension tools.",
].join("\n");

export const RLM_TOOL_SNIPPET =
	"Run Python in your persistent REPL: read and search files, run shell commands (`await bash(...)`), edit code (`await edit(...)`), process data, and delegate to subagents";

/** Rules for Pi's guideline list: how to use the REPL, depending on whether Pi's native file tools are active too. */
export function rlmToolGuidelines(activeTools: readonly string[]): string[] {
	const nativeFileTools = NATIVE_FILE_TOOLS.some((name) => activeTools.includes(name));
	if (nativeFileTools) {
		return [
			"Use the rlm tool for multi-step or data-heavy work: load and process data in Python, keep it in variables across calls, and print only what you need",
		];
	}
	return [
		"Do all work through the rlm tool: read files with `await read(path)` or `await bash('''sed -n 1,120p path''')`, search with `await bash('''rg -n pattern path''')`, change files with `await edit(...)`, and create files with Python",
		"Batch related steps into one cell and keep intermediate results in Python variables instead of printing them",
	];
}

const BASH_SKILL = `- \`out = await bash('''command''')\`: always triple-quote the command (\`r'''...'''\` when it has backslashes). It runs in the working directory and returns combined stdout and stderr as a string for further Python (\`out.splitlines()\`, \`json.loads(out)\`); a failed command has \`[exit code N]\` appended, so branch on \`out.ok\` / \`out.exit_code\`. \`timeout=\` kills a slow command. Prefer \`rg -n\`, \`sed -n 'A,Bp'\` and \`head\` over printing whole files; run independent commands with \`asyncio.gather\`.
  A command still running after 30 s returns with \`out.running\` and keeps running as a job (\`out.job\`) whose completion arrives as a \`<runtime_event>\`; \`yield_after=0\` starts a known-long suite, build or install as a job at once, \`yield_after=None\` blocks. \`await job.result()\` waits, \`await job.cancel()\` stops; \`help(ShellJob)\` has the rest. A server that must outlive everything is started detached (\`nohup cmd > /tmp/server.log 2>&1 &\`).`;

const BASH_SKILL_WITH_TOOL = `- Inside rlm you can also run shell with \`out = await bash('''command''')\`: it returns the output as a string (\`[exit code N]\` appended on failure; \`out.ok\`; a command still running after 30 s continues as a job, \`out.job\`), useful when mixing shell and Python in one cell.`;

const READ_SKILL = `- \`text = await read(path)\` returns a file's text; a file over 256 KiB comes back as a ContextHandle (see Bounded inference), so a huge file never lands in your context by accident.`;

const VIEW_IMAGE_SKILL = `- \`await view_image(path_or_bytes, detail=None)\` attaches an image (path, bytes, PIL image or matplotlib figure) to this cell's result for you to see (at most 8 per cell; \`detail="low"\` for a preview).`;

const EDIT_SKILL = `- \`await edit(path="pkg/file.py", old_str=..., new_str=...)\` changes an existing file: it replaces exactly one occurrence and raises ValueError when old_str is absent or appears more than once. Read the file first and copy old_str exactly; put several hunks in one cell (a loop over (old, new) pairs). Use \`Path(path).write_text(...)\` only for files you create.`;

const EDIT_SKILL_WITH_TOOL =
	"- The native edit tool and the `edit` skill both replace exactly one occurrence of old text (the skill raises ValueError when old_str is absent or ambiguous); use either.";

const PROJECT_ENV = `- The kernel runs the system Python without the project's packages: never import project modules there. Everything that executes project code (tests, repros, builds, imports) goes through \`bash\` with the project's own toolchain and interpreter (\`await bash('''.venv/bin/python -m pytest -x -q tests/test_x.py''')\`, \`npm test\`, \`go test ./...\`); write longer scripts to a file first, then run them that way.`;

const RUNTIME = `## Runtime
The \`rlm\` tool is a persistent Python REPL: each call runs a cell in the same kernel, so variables, imports and functions stay available. Program over files, shell commands and agents in Python: keep data in variables and print only what the next decision needs. The last expression's value is shown and kept as \`_\`; output over about 20 KB is cut in the middle, and a large value is shown by reference (type, size, head, tail) while the object stays in the kernel; \`preview(x)\` gives a bounded view of anything. The APIs are pre-imported and async (top-level \`await\`); \`help(obj)\` shows an API's full docs, so check it before guessing.
A result may end with one \`[hint:<tag>] ...\` line about how the cell used the runtime: act on it, and \`await hints.mute("<tag>")\` once understood.
A turn ends when you reply without calling rlm, and that reply is your answer. Never end a turn with a promise ("I'll check next"): do the work now. An exception ends the cell after anything printed; fix the cause instead of repeating the cell, since its writes and started agents may already have happened. Every lane has its own kernel, and supervisor-owned work (subagents, tasks, jobs) survives a lost variable or restart (\`await rlm.list_subagents()\`, \`await agents.tasks()\`). After a kernel restart re-create imports and functions; an evicted kernel keeps plain data variables; \`state\` (a dict) survives both.`;

/** How completion events reach the model (ULTRON_ASYNC_EVENTS, on by default). */
const ASYNC_EVENTS = `Nothing needs polling. When a detached job, tool call, subagent or task finishes while you are not waiting on it, a \`<runtime_event kind="job_done|tool_done|child_done|task_done" id=... status=... summary=... fetch=...>\` message arrives, and starts a new turn if you had ended yours. So start long work, do the rest meanwhile, and act on the event; never sleep, poll or loop waiting. If your answer needs a result and nothing else is left, wait for it in a cell (\`await job.result()\`). Batch independent operations into one cell with \`await asyncio.gather(...)\`.`;

const ASYNC_EVENTS_OFF =
	"Completions are not announced in this session (ULTRON_ASYNC_EVENTS=off): when you need a result, wait for it with `await job.result()`, `await rlm.collect(...)` or `await agents.result(id)`. Batch independent operations into one cell with `await asyncio.gather(...)` instead of one cell each.";

/** The cost rule that matters most on large inputs: narrow with code, read candidates, delegate last. */
const SEARCH_FIRST = `## Search before delegating
For many files or a large input, narrow with code before reading or delegating. 1. Search for the concept and its synonyms in Python (\`re\` over the texts, \`h.search\`/\`h.count\`, \`await bash('''rg -il ...''')\`) and count the hits. 2. Print one compact line per candidate (its id and the matching sentence). 3. Read the deciding passages of the ambiguous candidates (the root cause or summary lines) and judge them yourself. Use \`rlm.map\` only for candidates a line or two cannot settle, or when the narrowed text is still too large to read (over about 100 KB), and give each frame just that passage. Do not spawn subagents to read or classify documents: each one re-sends this whole prompt and its growing transcript on every turn, often 10 to 50 times the cost of reading the same candidates yourself.
\`\`\`python
import re
from pathlib import Path
docs = {p.stem: p.read_text() for p in Path("reports").glob("*.md")}
topic, event = re.compile(r"certific|\\bTLS\\b|\\bSSL\\b|x\\.?509", re.I), re.compile(r"expir|lapsed|notAfter|validity", re.I)
hits = {k: [l.strip() for l in t.splitlines() if topic.search(l) and event.search(l)] for k, t in docs.items()}
for k, lines in hits.items():
    if lines: print(k, " | ".join(l[:150] for l in lines[:3]))
\`\`\`
Then print the root-cause lines of the unclear candidates, decide, and check synonyms you may have missed on the rest.`;

const BOUNDED_INFERENCE = `## Bounded inference\n${INFERENCE_PROMPT}`;

const DELEGATION = `## Delegation
\`h = await rlm.spawn(task, name="researcher")\` starts a subagent with its own REPL and these tools and returns a handle (\`h.rlm_child_id\`) at once; \`await rlm.collect([h.rlm_child_id])\` waits and returns \`[{"id": ..., "result": {"status": "succeeded", "value": <its final answer>}}]\` (check each status). Spawn for independent multi-step work (separate modules or questions), not for reading or classifying files: narrow with code and use \`rlm.map\` for that. Give a self-contained brief (goal, paths, constraints, what to return); the child cannot see your conversation but shares your filesystem. Start several before collecting and keep working: each completion arrives as a \`child_done\` event. \`rlm.list_subagents()\` lists them, \`rlm.delete_subagent(id)\` cancels one. Reconcile their reports with your own evidence.
If you are a subagent, do the brief yourself (spawn only when it asks you to delegate; subagents nest at most two levels); your final reply without a tool call is your result: self-contained, with the evidence, paths and uncertainties it needs.
Typed agents (\`agents.list()\`, \`agents.invoke(definition, input)\`, \`agents.spawn(...)\`), agent graphs (\`workflows.run(nodes)\`) and background jobs that outlive the turn (\`background.start(prompt)\`): see \`help(agents)\`, \`help(workflows)\`, \`help(background)\`.`;

const CONTEXT = `## Other APIs\n${CONTEXT_PROMPT}\n${CODE_SKILLS_PROMPT}\n${AGENT_CLASS_PROMPT}`;

const MEMORY =
	"- `await memory.prepare(query)` recalls long-term memory; `await memory.propose(text, evidence)` keeps a durable fact; `await jev.triage(prompt)` rates a request.";

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
		VIEW_IMAGE_SKILL,
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
		DELEGATION,
		`${CONTEXT}\n${MEMORY}`,
	].join("\n\n");
}
