/**
 * The claude-code provider against a fake `claude` executable on PATH (test/fixtures/fake-claude.mjs): argument
 * construction and isolation, the login check, streaming, usage and cost, --json-schema and its fallback,
 * abort and timeout (process tree stopped), the concurrency cap, warm spares, missing-flag detection and
 * rate-limit errors. No real CLI, credentials or network are involved.
 */
import {
	chmodSync,
	copyFileSync,
	existsSync,
	mkdirSync,
	mkdtempSync,
	readdirSync,
	readFileSync,
	rmSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterAll, afterEach, beforeEach, describe, expect, it } from "vitest";
import {
	classifyFailure,
	claudeCodeCliStats,
	parseHelpFlags,
	REQUIRED_FLAGS,
	renderClaudeCodePrompt,
	resetClaudeCodeCliState,
	USAGE_LIMIT_DIAGNOSTIC,
} from "../src/api/claude-code-cli.ts";
import { createModels } from "../src/models.ts";
import { builtinProviders } from "../src/providers/all.ts";
import { CLAUDE_CODE_API, claudeCodeProvider, isClaudeCodeModel } from "../src/providers/claude-code.ts";
import type { AssistantMessage, AssistantMessageEvent, Context, Message, SimpleStreamOptions } from "../src/types.ts";
import { isRetryableAssistantError } from "../src/utils/retry.ts";

// These tests put a fake `claude` on PATH; the shared test setup hides the real CLI via this variable.
delete process.env.ULTRON_CLAUDE_CODE_BIN;

const fixture = fileURLToPath(new URL("./fixtures/fake-claude.mjs", import.meta.url));
const root = mkdtempSync(join(tmpdir(), "ultron-fake-claude-"));
const binDir = join(root, "bin");
mkdirSync(binDir, { recursive: true });
copyFileSync(fixture, join(binDir, "claude"));
chmodSync(join(binDir, "claude"), 0o755);

let logDir: string;
let counter = 0;

type Call = {
	pid: number;
	at: number;
	phase: "spawn" | "prompt" | "auth";
	argv?: string[];
	cwd?: string;
	env?: Record<string, string | null>;
	message?: { type: string; message: { role: string; content: Array<Record<string, unknown>> } };
};

function calls(): Call[] {
	const file = join(logDir, "calls.jsonl");
	if (!existsSync(file)) return [];
	return readFileSync(file, "utf8")
		.split("\n")
		.filter(Boolean)
		.map((line) => JSON.parse(line) as Call);
}

function promptCalls(): Call[] {
	const all = calls();
	const prompted = new Set(all.filter((call) => call.phase === "prompt").map((call) => call.pid));
	return all.filter((call) => call.phase === "spawn" && prompted.has(call.pid));
}

function promptOf(pid: number): Call["message"] {
	return calls().find((call) => call.phase === "prompt" && call.pid === pid)?.message;
}

function env(extra: Record<string, string> = {}): Record<string, string> {
	return {
		PATH: `${binDir}${delimiter}${process.env.PATH ?? ""}`,
		FAKE_CLAUDE_LOG: logDir,
		ULTRON_CLAUDE_CODE_WARM: "0",
		CLAUDECODE: "1",
		CLAUDE_CODE_SESSION_ID: "parent-session",
		CLAUDE_CONFIG_DIR: join(root, "config-dir"),
		...extra,
	};
}

const models = createModels();
models.setProvider(claudeCodeProvider());
const haiku = models.getModel("claude-code", "haiku")!;

function context(text = "What is 6*7?", systemPrompt = "Answer tersely."): Context {
	return { systemPrompt, messages: [{ role: "user", content: text, timestamp: Date.now() }] };
}

async function run(
	ctx: Context,
	options: SimpleStreamOptions & Record<string, unknown> = {},
	extraEnv: Record<string, string> = {},
): Promise<{ events: AssistantMessageEvent[]; message: AssistantMessage }> {
	const stream = models.streamSimple(haiku, ctx, { ...options, env: env(extraEnv) });
	const events: AssistantMessageEvent[] = [];
	for await (const event of stream) events.push(event);
	return { events, message: await stream.result() };
}

function alive(pid: number): boolean {
	try {
		process.kill(pid, 0);
		return true;
	} catch {
		return false;
	}
}

async function waitFor(check: () => boolean, timeoutMs = 5_000): Promise<void> {
	const until = Date.now() + timeoutMs;
	while (!check()) {
		if (Date.now() > until) throw new Error("timed out waiting");
		await new Promise((resolve) => setTimeout(resolve, 20));
	}
}

beforeEach(() => {
	resetClaudeCodeCliState();
	counter += 1;
	logDir = join(root, `log-${counter}`);
	mkdirSync(logDir, { recursive: true });
});

afterEach(() => resetClaudeCodeCliState());
afterAll(() => rmSync(root, { recursive: true, force: true }));

describe("claude-code provider registration", () => {
	it("exposes Opus 5.5 plus the opus, sonnet and haiku aliases as a built-in, key-less provider", async () => {
		expect(builtinProviders().map((provider) => provider.id)).toContain("claude-code");
		expect(models.getModels("claude-code").map((model) => model.id)).toEqual([
			"claude-opus-5-5",
			"opus",
			"sonnet",
			"haiku",
		]);
		expect(haiku).toMatchObject({ api: CLAUDE_CODE_API, provider: "claude-code", input: ["text", "image"] });
		expect(isClaudeCodeModel(haiku)).toBe(true);
		const withCli = createModels({
			authContext: {
				env: async (name) => env()[name],
				fileExists: async (path) => existsSync(path),
			},
		});
		withCli.setProvider(claudeCodeProvider());
		expect((await withCli.getAvailable("claude-code")).map((model) => model.id)).toEqual([
			"claude-opus-5-5",
			"opus",
			"sonnet",
			"haiku",
		]);
		const withoutCli = createModels({
			authContext: {
				env: async (name) => (name === "PATH" ? join(root, "nothing") : undefined),
				fileExists: async () => false,
			},
		});
		withoutCli.setProvider(claudeCodeProvider());
		expect(await withoutCli.getAvailable("claude-code")).toEqual([]);
	});
});

describe("claude-code requests", () => {
	it("isolates the call: public flags only, replaced system prompt, no tools/MCP/settings, empty cwd, no parent session", async () => {
		const { message } = await run(context());
		expect(message.stopReason).toBe("stop");
		const [call] = promptCalls();
		expect(call.argv).toEqual([
			"--print",
			"--output-format=stream-json",
			"--input-format=stream-json",
			"--verbose",
			"--include-partial-messages",
			"--tools=",
			"--strict-mcp-config",
			'--mcp-config={"mcpServers":{}}',
			"--setting-sources=",
			"--permission-prompts=none",
			"--disable-slash-commands",
			"--no-session-persistence",
			"--model=haiku",
			"--system-prompt=Answer tersely.",
			'--settings={"alwaysThinkingEnabled":false}',
		]);
		expect(call.argv?.some((arg) => arg.startsWith("--append-system-prompt") || arg === "--bare")).toBe(false);
		expect(call.cwd).not.toBe(process.cwd());
		expect(readdirSync(call.cwd!)).toEqual([]);
		expect(call.env).toMatchObject({
			CLAUDECODE: null,
			CLAUDE_CODE_SESSION_ID: null,
			CLAUDE_CONFIG_DIR: join(root, "config-dir"),
			CLAUDE_CODE_MAX_OUTPUT_TOKENS: null,
		});
		expect(promptOf(call.pid)).toEqual({
			type: "user",
			message: { role: "user", content: [{ type: "text", text: "What is 6*7?" }] },
		});
	});

	it("uses a one-line neutral system prompt when the context has none", async () => {
		await run({ messages: [{ role: "user", content: "hi", timestamp: 0 }] });
		expect(promptCalls()[0].argv).toContain(
			"--system-prompt=You are a helpful assistant. Follow the user's instructions exactly.",
		);
	});

	it("streams text deltas and reports usage, the CLI's cost and the resolved model", async () => {
		const { events, message } = await run(context());
		expect(events.map((event) => event.type)).toEqual([
			"start",
			"text_start",
			"text_delta",
			"text_delta",
			"text_delta",
			"text_end",
			"done",
		]);
		expect(message.content).toEqual([{ type: "text", text: "Hello from fake claude" }]);
		expect(message.responseModel).toBe("resolved-haiku");
		expect(message.usage).toMatchObject({
			input: 120,
			output: 7,
			cacheRead: 30,
			cacheWrite: 10,
			totalTokens: 167,
			reasoning: 0,
			cost: { total: 0.0012 },
		});
	});

	it("reports an unknown cost (never zero) when the CLI reports none", async () => {
		const { message } = await run(context(), {}, { FAKE_CLAUDE_SCENARIO: "no_cost" });
		expect(message.stopReason).toBe("stop");
		expect(Number.isNaN(message.usage.cost.total)).toBe(true);
		expect(message.usage.totalTokens).toBe(167);
	});

	it("streams thinking and passes the thinking level as --effort", async () => {
		const { events, message } = await run(context(), { reasoning: "minimal" }, { FAKE_CLAUDE_SCENARIO: "thinking" });
		expect(events.map((event) => event.type)).toContain("thinking_delta");
		expect(message.content[0]).toEqual({ type: "thinking", thinking: "pondering" });
		const argv = promptCalls()[0].argv!;
		expect(argv).toContain("--effort=low");
		expect(argv.some((arg) => arg.startsWith("--settings"))).toBe(false);
	});

	it("emits a whole message when the CLI sends no partial events", async () => {
		const { message } = await run(context(), {}, { FAKE_CLAUDE_SCENARIO: "no_partial" });
		expect(message.content).toEqual([{ type: "text", text: "Hello from fake claude" }]);
	});

	it("renders earlier turns, tool calls and results as a transcript and passes images", async () => {
		const image = { type: "image" as const, data: "aGVsbG8=", mimeType: "image/png" };
		const messages: Message[] = [
			{ role: "user", content: [{ type: "text", text: "Look:" }, image], timestamp: 0 },
			{
				role: "assistant",
				content: [
					{ type: "thinking", thinking: "secret" },
					{ type: "text", text: "Checking." },
					{ type: "toolCall", id: "t1", name: "read", arguments: { path: "a.txt" } },
				],
				api: CLAUDE_CODE_API,
				provider: "claude-code",
				model: "haiku",
				usage: {
					input: 0,
					output: 0,
					cacheRead: 0,
					cacheWrite: 0,
					totalTokens: 0,
					cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
				},
				stopReason: "stop",
				timestamp: 0,
			},
			{
				role: "toolResult",
				toolCallId: "t1",
				toolName: "read",
				content: [{ type: "text", text: "A" }],
				isError: false,
				timestamp: 0,
			},
			{ role: "user", content: "And now?", timestamp: 0 },
		];
		await run({ systemPrompt: "S", messages });
		const content = promptOf(promptCalls()[0].pid)!.message.content;
		expect(content[0]).toEqual({ type: "text", text: "<conversation>\n<user>\nLook:" });
		expect(content[1]).toEqual({
			type: "image",
			source: { type: "base64", media_type: "image/png", data: "aGVsbG8=" },
		});
		const rest = String(content[2].text);
		expect(rest).toContain('[tool call read (id t1): {"path":"a.txt"}]');
		expect(rest).toContain('<tool_result name="read" id="t1">\nA\n</tool_result>');
		expect(rest).toContain("<user>\nAnd now?\n</user>");
		expect(rest).not.toContain("secret");
		expect(rest).toMatch(/Write the assistant's next reply/);
		expect(renderClaudeCodePrompt(messages, false)[0]).toMatchObject({ type: "text" });
		expect(JSON.stringify(renderClaudeCodePrompt(messages, false))).toContain("[image omitted");
	});

	it("fails fast, without starting a process, when the lane declares tools", async () => {
		const ctx: Context = {
			...context(),
			tools: [{ name: "bash", description: "run", parameters: { type: "object", properties: {} } as never }],
		};
		const { message } = await run(ctx);
		expect(message.stopReason).toBe("error");
		expect(message.errorMessage).toMatch(/claude-code\/haiku does not support tool calling/);
		expect(message.errorMessage).toMatch(/ultron claude/);
		expect(message.usage.cost.total).toBe(0);
		expect(calls()).toEqual([]);
	});
});

describe("claude-code login check and feature detection", () => {
	it("refuses a logged-out CLI with a clear message before any prompt is sent", async () => {
		const { message } = await run(context(), {}, { FAKE_CLAUDE_AUTH: "logged_out" });
		expect(message.stopReason).toBe("error");
		expect(message.errorMessage).toMatch(/not logged in.*claude auth login/);
		expect(promptCalls()).toEqual([]);
	});

	it("refuses a third-party CLI setup unless allowed, then warns", async () => {
		const refused = await run(context(), {}, { FAKE_CLAUDE_AUTH: "third_party" });
		expect(refused.message.errorMessage).toMatch(/configured for bedrock.*ULTRON_CLAUDE_CODE_ALLOW_THIRD_PARTY=1/);
		resetClaudeCodeCliState();
		const allowed = await run(
			context(),
			{},
			{ FAKE_CLAUDE_AUTH: "third_party", ULTRON_CLAUDE_CODE_ALLOW_THIRD_PARTY: "1" },
		);
		expect(allowed.message.stopReason).toBe("stop");
		expect(allowed.message.diagnostics?.[0]).toMatchObject({ type: "claude_code_warning" });
	});

	it("warns, without blocking, when the CLI uses an API key", async () => {
		const byStatus = await run(context(), {}, { FAKE_CLAUDE_AUTH: "api_key" });
		expect(byStatus.message.stopReason).toBe("stop");
		expect(JSON.stringify(byStatus.message.diagnostics)).toMatch(/authenticates with api_key/);
		const byEvent = await run(context(), {}, { FAKE_CLAUDE_SCENARIO: "api_key_source" });
		expect(byEvent.message.stopReason).toBe("stop");
		expect(JSON.stringify(byEvent.message.diagnostics)).toMatch(/apiKeySource: ANTHROPIC_API_KEY/);
	});

	it("checks the login once and caches it", async () => {
		await run(context());
		await run(context());
		expect(calls().filter((call) => call.phase === "auth")).toHaveLength(1);
	});

	it("names a required flag the installed CLI does not list", async () => {
		const { message } = await run(context(), {}, { FAKE_CLAUDE_HELP_OMIT: "--include-partial-messages" });
		expect(message.stopReason).toBe("error");
		expect(message.errorMessage).toMatch(/does not support --include-partial-messages/);
		expect(promptCalls()).toEqual([]);
	});

	it("names an optional flag a request needs when the CLI lacks it", async () => {
		const { message } = await run(context(), { reasoning: "high" }, { FAKE_CLAUDE_HELP_OMIT: "--effort" });
		expect(message.errorMessage).toMatch(/needs --effort/);
	});

	it("finds every flag of a help text", () => {
		const flags = parseHelpFlags(
			"  -p, --print   Print\n  --tools <tools...>  x\n  --allowedTools, --allowed-tools <t>",
		);
		expect([...flags]).toEqual(["--print", "--tools", "--allowedtools", "--allowed-tools"]);
		expect(REQUIRED_FLAGS).toContain("--system-prompt");
	});

	it("explains a missing CLI", async () => {
		const stream = models.streamSimple(haiku, context(), {
			env: { PATH: join(root, "nothing"), ULTRON_CLAUDE_CODE_WARM: "0" },
		});
		const message = await stream.result();
		expect(message.errorMessage).toMatch(/`claude`\) was not found on PATH/);
	});
});

describe("claude-code structured output", () => {
	const schema = { type: "object", properties: { ok: { type: "boolean" } }, required: ["ok"] };

	it("passes an object schema as --json-schema and returns the structured result as the text", async () => {
		const { message } = await run(context(), { jsonSchema: schema });
		expect(promptCalls()[0].argv).toContain(`--json-schema=${JSON.stringify(schema)}`);
		expect(message.content).toEqual([{ type: "text", text: '{"ok":true}' }]);
	});

	it("takes the schema from the payload hook (how inference frames pass their contract)", async () => {
		const { message } = await run(context(), {
			onPayload: (payload) => ({ ...(payload as object), json_schema: schema, max_tokens: 900 }),
		});
		const call = promptCalls()[0];
		expect(call.argv).toContain(`--json-schema=${JSON.stringify(schema)}`);
		expect(call.env?.CLAUDE_CODE_MAX_OUTPUT_TOKENS).toBe("900");
		expect(message.content).toEqual([{ type: "text", text: '{"ok":true}' }]);
	});

	it("asks for JSON in the prompt when the schema is not an object", async () => {
		await run(context(), { jsonSchema: { type: "integer" } });
		const call = promptCalls()[0];
		expect(call.argv?.some((arg) => arg.startsWith("--json-schema"))).toBe(false);
		expect(JSON.stringify(promptOf(call.pid))).toContain('satisfies this JSON schema: {\\"type\\":\\"integer\\"}');
	});

	it("falls back to prompt-level JSON when the API rejects the schema", async () => {
		const { message } = await run(context(), { jsonSchema: schema }, { FAKE_CLAUDE_SCENARIO: "schema_reject" });
		const [first, second] = promptCalls();
		expect(first.argv?.some((arg) => arg.startsWith("--json-schema"))).toBe(true);
		expect(second.argv?.some((arg) => arg.startsWith("--json-schema"))).toBe(false);
		expect(JSON.stringify(promptOf(second.pid))).toContain("satisfies this JSON schema");
		expect(message.stopReason).toBe("stop");
		expect(JSON.stringify(message.diagnostics)).toMatch(/--json-schema was rejected/);
	});
});

describe("claude-code failures", () => {
	it("reports an exhausted subscription window as a non-retryable usage-limit error", async () => {
		const { message } = await run(context(), {}, { FAKE_CLAUDE_SCENARIO: "rate_limit" });
		expect(message.stopReason).toBe("error");
		expect(message.errorMessage).toMatch(/^Claude Code usage limit reached \(five_hour window; resets at 2026-/);
		expect(message.diagnostics?.map((item) => item.type)).toContain(USAGE_LIMIT_DIAGNOSTIC);
		expect(isRetryableAssistantError(message)).toBe(false);
	});

	it("reports a transient 429 as a retryable rate-limit error", async () => {
		const { message } = await run(context(), {}, { FAKE_CLAUDE_SCENARIO: "rate_429" });
		expect(message.errorMessage).toMatch(/Claude Code rate limited \(429\)/);
		expect(isRetryableAssistantError(message)).toBe(true);
	});

	it("maps the CLI's output-token overflow to a length stop that keeps the streamed text", async () => {
		const { message } = await run(context(), { maxTokens: 20 }, { FAKE_CLAUDE_SCENARIO: "max_tokens" });
		expect(message.stopReason).toBe("length");
		expect(message.content).toEqual([{ type: "text", text: "Rivers run to the" }]);
		expect(promptCalls()[0].env?.CLAUDE_CODE_MAX_OUTPUT_TOKENS).toBe("20");
	});

	it("surfaces a crash with the CLI's stderr and an unknown cost", async () => {
		const { message } = await run(context(), {}, { FAKE_CLAUDE_SCENARIO: "crash" });
		expect(message.stopReason).toBe("error");
		expect(message.errorMessage).toMatch(/exited \(code 3\) before a result: boom/);
		expect(Number.isNaN(message.usage.cost.total)).toBe(true);
	});

	it("classifies CLI error results", () => {
		expect(classifyFailure({ is_error: true, result: "Not logged in · Please run /login" }, undefined)).toMatchObject(
			{
				kind: "error",
				message: expect.stringMatching(/authentication failed.*claude auth login/),
			},
		);
		expect(classifyFailure({ is_error: true, api_error_status: 529, result: "Overloaded" }, undefined)).toMatchObject(
			{
				message: expect.stringMatching(/overloaded \(529\)/),
			},
		);
	});

	it("aborts: stops the process tree and reports aborted", async () => {
		const controller = new AbortController();
		const stream = models.streamSimple(haiku, context(), {
			signal: controller.signal,
			env: env({ FAKE_CLAUDE_SCENARIO: "slow" }),
		});
		await waitFor(() => calls().some((call) => call.phase === "prompt"));
		const pid = calls().find((call) => call.phase === "prompt")!.pid;
		controller.abort();
		const message = await stream.result();
		expect(message.stopReason).toBe("aborted");
		await waitFor(() => !alive(pid));
	});

	it("times out: stops the process and reports the timeout", async () => {
		const started = Date.now();
		const { message } = await run(context(), { timeoutMs: 400 }, { FAKE_CLAUDE_SCENARIO: "slow" });
		expect(message.stopReason).toBe("error");
		expect(message.errorMessage).toMatch(/timed out after 0s|timed out after 1s/);
		expect(Date.now() - started).toBeLessThan(10_000);
		const pid = calls().find((call) => call.phase === "prompt")!.pid;
		await waitFor(() => !alive(pid));
	});
});

describe("claude-code capacity", () => {
	it("caps concurrent CLI requests (ULTRON_CLAUDE_CODE_CONCURRENCY)", async () => {
		const results = await Promise.all(
			Array.from({ length: 5 }, () =>
				run(context(), {}, { ULTRON_CLAUDE_CODE_CONCURRENCY: "2", FAKE_CLAUDE_DELAY_MS: "300" }),
			),
		);
		expect(results.every(({ message }) => message.stopReason === "stop")).toBe(true);
		const active = readFileSync(join(logDir, "active.log"), "utf8").split("\n").filter(Boolean).map(Number);
		expect(active).toHaveLength(5);
		expect(Math.max(...active)).toBeLessThanOrEqual(2);
		expect(claudeCodeCliStats().inFlight).toBe(0);
	});

	it("defaults to four concurrent requests", async () => {
		await Promise.all(Array.from({ length: 6 }, () => run(context(), {}, { FAKE_CLAUDE_DELAY_MS: "400" })));
		const active = readFileSync(join(logDir, "active.log"), "utf8").split("\n").filter(Boolean).map(Number);
		expect(Math.max(...active)).toBe(4);
	});

	it("serves the next request of the same shape from a warm spare, one process per request", async () => {
		const warm = { ULTRON_CLAUDE_CODE_WARM: "1" };
		await run(context("first"), {}, warm);
		await waitFor(() => claudeCodeCliStats().spares === 1);
		await waitFor(() => calls().filter((call) => call.phase === "spawn").length === 2);
		const spare = calls().filter((call) => call.phase === "spawn")[1];
		await run(context("second"), {}, warm);
		const spawns = calls().filter((call) => call.phase === "spawn");
		const prompts = calls().filter((call) => call.phase === "prompt");
		expect(prompts).toHaveLength(2);
		// Each process got exactly one prompt: context never carries from one request to the next.
		expect(new Set(prompts.map((call) => call.pid)).size).toBe(2);
		// The second request was served by the spare started before it was made.
		expect(prompts[1].pid).toBe(spare.pid);
		expect(spare.argv).toEqual(spawns[0].argv);
		resetClaudeCodeCliState();
		await waitFor(() => spawns.every((call) => !alive(call.pid)));
	});
});
