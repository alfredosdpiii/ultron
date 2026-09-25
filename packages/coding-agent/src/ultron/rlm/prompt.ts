/**
 * How the model is told about the RLM REPL: the `rlm` tool description, its line and guidelines in Pi's
 * system prompt, and the runtime guide section. Modelled on Prime Intellect's nano-rlm (its RUNTIME_PROMPT,
 * BASH_SKILL_PROMPT, EDIT_SKILL_PROMPT and DELEGATION_PROMPT), adapted to Ultron's actual API.
 */

/** Native Pi tools that the RLM REPL replaces by default (they stay registered for `--tools` and the opt-out). */
import { CODE_SKILLS_PROMPT } from "../code-skills.ts";
import { CONTEXT_PROMPT } from "../context-control.ts";
import { AGENT_CLASS_PROMPT } from "./agent-class-prompt.ts";

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
	"Run a Python cell in your persistent RLM REPL. Variables, imports and functions persist across calls; top-level `await` works; the value of the last expression is shown along with anything printed, and stays available as `_`.",
	"Pre-imported, nothing to import: `bash`, `edit`, `rlm`, `agents`, `workflows`, `background`, `memory`, `ctx`, `skills`, `agent`/`Agent`, `state`, `jev`, `preview`.",
	"- `out = await bash('''command''')` runs a shell command in the working directory and returns its output as a string, with `[exit code N]` appended on failure (`out.exit_code`, `out.ok`).",
	'- `await edit(path="file.py", old_str=..., new_str=...)` replaces exactly one occurrence and raises ValueError when old_str is absent or appears more than once. Create new files with ordinary Python (`Path(p).write_text(...)`).',
	'- `h = await rlm.spawn(task, name="short-name")` starts a subagent with its own REPL; `await rlm.collect([h.rlm_child_id])` waits for results. `await agents.invoke(definition, input)` runs a typed agent; `await workflows.run(nodes)` runs an agent graph.',
	"- `state` is a dict for data that must survive kernel restarts. Output over about 20 KB is cut in the middle and a large last value is shown by reference (type, size, head, tail): keep data in variables and print what you need.",
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
		"Do all work through the rlm tool: read files with Python or `await bash('''sed -n 1,120p path''')`, search with `await bash('''rg -n pattern path''')`, change files with `await edit(...)`, and create files with Python",
		"Batch related steps into one cell and keep intermediate results in Python variables instead of printing them",
	];
}

const BASH_SKILL = `For shell work, use \`out = await bash('''command here''')\`: always triple-quote the command so shell quotes and multi-line scripts never need escaping (use \`r'''...'''\` when it contains backslashes). It runs in the working directory with the user's shell and returns the combined stdout and stderr as a string, useful for further Python processing: \`out.splitlines()\`, \`json.loads(out)\`, regexes. A failed command has \`[exit code N]\` appended; branch on \`out.ok\` / \`out.exit_code\` rather than only printing the text. \`timeout=\` (seconds) kills a command that runs too long. Run independent commands concurrently with \`await asyncio.gather(bash(a), bash(b))\`. Prefer \`rg -n\`, \`sed -n 'A,Bp'\` and \`head\` over printing whole files. A command runs only while its cell runs: for a long-lived process (a server), start it detached (\`nohup cmd > /tmp/server.log 2>&1 &\`) and read its log in later cells.`;

const BASH_SKILL_WITH_TOOL = `Inside rlm you can also run shell with \`out = await bash('''command''')\`: it returns the output as a string (\`[exit code N]\` appended on failure; \`out.exit_code\`, \`out.ok\`), useful when mixing shell and Python in one cell or avoiding shell quoting.`;

const EDIT_SKILL = `Change existing files with the pre-imported async \`edit\` skill, not with \`str.replace\` + \`write_text\`: \`await edit(path="pkg/file.py", old_str=..., new_str=...)\` replaces exactly one occurrence and raises ValueError when old_str is absent or appears more than once, so a stale or ambiguous hunk cannot be applied silently. Several hunks go in one cell:
\`\`\`python
for old, new in [(OLD_IMPORTS, NEW_IMPORTS), (OLD_CALL, NEW_CALL)]:
    print(await edit(path="src/pkg/module.py", old_str=old, new_str=new))
\`\`\`
Use \`Path(path).write_text(...)\` only for files you create. Read a file before editing it, and copy old_str exactly from what you read.`;

const EDIT_SKILL_WITH_TOOL =
	"The native edit tool and the `edit` skill both replace exactly one occurrence of old text (the skill raises ValueError when old_str is absent or ambiguous); use either.";

const PROJECT_ENV = `The kernel runs the system Python without the project's packages: never import project modules there. Everything that executes project code (tests, repros, builds, imports) goes through \`bash\` with the project's own toolchain and interpreter (for example \`await bash('''.venv/bin/python -m pytest -x -q tests/test_x.py''')\`, \`npm test\`, \`go test ./...\`). Longer scripts and scratch tests go to a file, then run with that toolchain:
\`\`\`python
from pathlib import Path
Path("/tmp/repro/check.py").parent.mkdir(parents=True, exist_ok=True)
Path("/tmp/repro/check.py").write_text(r"""import package
print(package.__version__)
""")
out = await bash('''python3 /tmp/repro/check.py''')
\`\`\``;

const RUNTIME = `## Runtime
You have a persistent Python REPL as your execution environment: the \`rlm\` tool. Each call runs a cell in the same kernel, so variables, imports and functions remain available to later cells. Use Python to program over files, shell commands and agents: read and transform data in Python, keep intermediate results in variables, and print only what the next decision needs. The value of a cell's last expression is shown along with anything printed and stays available as \`_\`. Output over about 20 KB is cut in the middle (the marker says how much was cut), and a large last value (a long string, a big list or dict, a DataFrame) is shown by reference: its type, size, head and tail, while the object itself stays in the kernel. So read large files and outputs into variables and show slices, counts or matches (\`preview(x)\` gives a bounded view of any value) instead of dumping them.

The APIs are pre-imported (nothing to import) and async: use top-level \`await\`. An exception ends the cell and its traceback is shown after anything printed before it; fix the cause rather than repeating the cell, since writes and started agents from the failed cell may already have happened.

Every lane (you, and each subagent) has its own kernel. A supervisor outside the kernel (the Ultron host) owns subagents, tasks and their results: handles in Python variables are references to supervisor-owned work, and losing a variable or restarting the kernel does not cancel it (\`await rlm.list_subagents()\` and \`await agents.tasks()\` recover them). If a cell fails because the kernel was stopped (a memory or CPU limit), the next cell starts a fresh kernel: re-create imports, functions and variables. Idle kernels may be evicted and restored later with plain data variables (numbers, strings, lists, dicts) intact but not functions, modules or objects; the \`state\` dict is the place for data that must survive.`;

const DELEGATION = `## Delegation
\`h = await rlm.spawn(task, name="researcher")\` starts a subagent and returns a handle (\`h.rlm_child_id\`, \`h.name\`) at once; the child gets its own REPL and these same tools. Give it a self-contained brief: the goal, relevant paths and constraints, and exactly what to return (it cannot see your conversation). Children share your filesystem. \`results = await rlm.collect([h.rlm_child_id])\` waits for the listed children (all of yours when called with no argument) and returns \`[{"id": ..., "result": {"status": "succeeded", "value": <the child's final answer>}}]\`; check each status. Start several children before collecting them to run them in parallel, and work on another part yourself meanwhile. \`await rlm.list_subagents()\` lists them; \`await rlm.delete_subagent(id)\` cancels one.
Delegate when a task has independent parts (separate modules, separate questions) or an input too large to read in one context (slice it and give each child its slice); do small, sequential work yourself. Reconcile children's reports with your own evidence before answering.
Typed agents: \`await agents.list()\` shows definitions (for example "rlm-child@1" with input \`{"prompt": ...}\`); \`await agents.invoke(definition, input)\` runs one and returns \`{"status": ..., "value": ...}\`; \`t = await agents.spawn(definition, input)\` starts one in the background and \`await t.result()\` collects it. \`await workflows.run(nodes)\` runs a validated agent graph. \`await background.start(prompt)\` starts a long-running background agent job that outlives the turn (\`background.list()\`, \`background.inspect(id)\`, \`background.result(id)\`, \`background.stop(id)\`).
If you are a subagent, your final reply (with no tool call) is your result for the parent: make it self-contained, with the evidence, paths and uncertainties it needs.`;

const CONTEXT = `## Your context\n${CONTEXT_PROMPT}`;

const CODE_SKILLS = `## Code skills\n${CODE_SKILLS_PROMPT}`;

const AGENT_CLASSES = `## Agents as classes\n${AGENT_CLASS_PROMPT}`;

const MEMORY = `## Memory and other APIs
\`await memory.prepare(query)\` recalls long-term memory relevant to a query; \`await memory.propose(text, evidence)\` retains a durable fact. \`await jev.triage(prompt)\` rates a request. \`preview(value)\` gives a bounded preview of a large object. \`state\` survives kernel restarts. Use \`help(obj)\` to see a signature before guessing.`;

/**
 * The runtime guide for the system prompt when `rlm` is among the active tools. It tells the model how to
 * work in the REPL; when Pi's native bash or edit tools are active too, the skill lines say so.
 */
export function rlmRuntimePrompt(activeTools: readonly string[]): string | undefined {
	if (!activeTools.includes("rlm")) return undefined;
	const nativeBash = activeTools.includes("bash");
	const nativeEdit = activeTools.includes("edit");
	const skills = [
		"## Skills",
		"Pre-imported async skills: `bash`, `edit`. Use `help(bash)` for a signature.",
		nativeBash ? BASH_SKILL_WITH_TOOL : BASH_SKILL,
		nativeEdit ? EDIT_SKILL_WITH_TOOL : EDIT_SKILL,
		PROJECT_ENV,
	];
	return [RUNTIME, skills.join("\n\n"), DELEGATION, CONTEXT, CODE_SKILLS, AGENT_CLASSES, MEMORY].join("\n\n");
}
