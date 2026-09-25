#!/usr/bin/env node
/**
 * Per-task overhead profile: stock Pi vs Ultron against a local fake OpenAI-compatible provider,
 * so model latency is zero and every millisecond measured is host work.
 *
 *   node scripts/profile-overhead.mjs [--runs 5] [--variants pi,source,ultron] [--scenario bash,rlm,text]
 *
 * Variants: `pi` (installed stock Pi), `source` (this checkout's source CLI via the source resolver),
 * `ultron` (the linked `ultron` build), `bundle` (this checkout's dist/bundle, after `npm run build`). Each run uses a fresh isolated agent dir holding only a
 * models.json that points at the fake provider, and runs RPC mode with --no-session like
 * scripts/eval-quality.mjs. Reports medians of: time until RPC ready, time from prompt to the first
 * provider request, total prompt wall time, time between tool call and the follow-up request
 * (tool execution + per-turn host work), shutdown time, and request body composition.
 */
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");

function arg(name, fallback) {
	const index = process.argv.indexOf(`--${name}`);
	return index === -1 ? fallback : process.argv[index + 1];
}

const SCENARIOS = {
	/** One bash tool call, then an answer. */
	bash: {
		prompts: ["Run echo hi with bash, then say done."],
		reply: (turn) => (turn === 0 ? { tool: "bash", args: { command: "echo hi" } } : { text: "done" }),
	},
	/** Two prompts that each run Python in the rlm tool: first is a cold kernel, second warm. */
	rlm: {
		prompts: ["Compute 1+1 in Python.", "Compute 2+2 in Python."],
		reply: (turn, request) => {
			const last = request.messages.at(-1);
			return last?.role === "tool" ? { text: "ok" } : { tool: "rlm", args: { code: `print(${turn}+1)` } };
		},
		only: ["source", "ultron", "bundle"],
	},
	/** One long streamed reasoning + text answer: per-delta relay overhead. */
	stream: {
		prompts: ["Write a long answer."],
		reply: () => ({ chunks: Number(process.env.PROFILE_STREAM_CHUNKS ?? 4000) }),
	},
	/** Same, paced at 1 delta per 2 ms like a real model stream (8 s of model time). */
	paced: {
		prompts: ["Write a long answer."],
		reply: () => ({ chunks: 4000, intervalMs: 2 }),
	},
	/** A small edit task shaped like the quality eval's: read, edit, run, answer; 1 s model latency per request. */
	edit: {
		prompts: ["Fix the bug in calc.py."],
		files: { "calc.py": "def average(values):\n    return sum(values) / (len(values) - 1)\n" },
		reply: (turn) =>
			[
				{ tool: "read", args: { path: "calc.py" }, delayMs: 1000 },
				{
					tool: "edit",
					args: { path: "calc.py", edits: [{ oldText: "(len(values) - 1)", newText: "len(values)" }] },
					delayMs: 1000,
				},
				{ tool: "bash", args: { command: "python3 -c 'import calc; print(calc.average([2,4,6]))'" }, delayMs: 1000 },
				{ text: "Fixed.", delayMs: 1000 },
			][Math.min(turn, 3)],
	},
	/** The first attempt fails like the eval proxy's transient 500s, then succeeds: retry wall time. */
	retry: {
		prompts: ["Say hi."],
		reply: (_turn, _body, attempt) =>
			attempt === 1
				? {
						status: 500,
						body: {
							message: "empty_stream: upstream stream closed before first payload",
							type: "server_error",
							code: "internal_server_error",
						},
					}
				: { text: "hi" },
	},
	/** Two plain-text prompts: pure per-turn host overhead. */
	text: {
		prompts: ["Say hi.", "Say bye."],
		reply: () => ({ text: "hi" }),
	},
};

function startProvider(script) {
	const requests = [];
	let calls = 0;
	const server = createServer(async (request, response) => {
		const received = performance.now();
		const chunks = [];
		for await (const chunk of request) chunks.push(chunk);
		const raw = Buffer.concat(chunks).toString("utf8");
		const body = JSON.parse(raw);
		const system = body.messages.filter((message) => message.role === "system" || message.role === "developer");
		const systemText = system
			.map((message) =>
				typeof message.content === "string" ? message.content : message.content.map((part) => part.text).join(""),
			)
			.join("");
		requests.push({
			at: received,
			bytes: raw.length,
			systemChars: systemText.length,
			tools: body.tools?.length ?? 0,
			toolBytes: JSON.stringify(body.tools ?? []).length,
			toolNames: (body.tools ?? []).map((tool) => tool.function?.name),
			messages: body.messages.length,
			system: systemText,
			raw,
			params: Object.fromEntries(Object.entries(body).filter(([key]) => key !== "messages" && key !== "tools")),
			headers: request.headers,
		});
		const turn = body.messages.filter((message) => message.role === "assistant").length;
		const reply = script(turn, body, requests.length);
		if (reply.status) {
			response.writeHead(reply.status, { "content-type": "application/json" }).end(JSON.stringify(reply.body));
			return;
		}
		if (reply.delayMs) await new Promise((done) => setTimeout(done, reply.delayMs));
		const id = `p-${++calls}`;
		const chunk = (delta, finish, usage) =>
			`data: ${JSON.stringify({ id, object: "chat.completion.chunk", created: 0, model: "fake", choices: [{ index: 0, delta, finish_reason: finish }], ...(usage ? { usage } : {}) })}\n\n`;
		const usage = { prompt_tokens: Math.ceil(raw.length / 4), completion_tokens: 4, total_tokens: 0 };
		response.writeHead(200, { "content-type": "text/event-stream" });
		if (reply.chunks) {
			// A long streamed answer: reasoning deltas then text deltas, like a reasoning model.
			for (let index = 0; index < reply.chunks; index += 1) {
				const delta = index < reply.chunks / 2 ? { reasoning_content: "think " } : { content: "word " };
				response.write(chunk(index === 0 ? { role: "assistant", ...delta } : delta, null));
				if (reply.intervalMs) await new Promise((done) => setTimeout(done, reply.intervalMs));
			}
			response.write(chunk({}, "stop", usage));
		} else if ("text" in reply) {
			response.write(chunk({ role: "assistant", content: reply.text }, null));
			response.write(chunk({}, "stop", usage));
		} else {
			response.write(
				chunk(
					{
						role: "assistant",
						tool_calls: [
							{
								index: 0,
								id: `call_${calls}`,
								type: "function",
								function: { name: reply.tool, arguments: JSON.stringify(reply.args) },
							},
						],
					},
					null,
				),
			);
			response.write(chunk({}, "tool_calls", usage));
		}
		response.end("data: [DONE]\n\n");
	});
	return new Promise((resolveStart) =>
		server.listen(0, "127.0.0.1", () =>
			resolveStart({
				requests,
				baseUrl: `http://127.0.0.1:${server.address().port}/v1`,
				stop: () => {
					server.closeAllConnections();
					return new Promise((done) => server.close(done));
				},
			}),
		),
	);
}

function modelsJson(baseUrl) {
	return JSON.stringify({
		providers: {
			fake: {
				baseUrl,
				api: "openai-completions",
				apiKey: "fake-key",
				// Same compat and model shape as the eval's cliproxyapi/gpt-6-sol entry.
				compat: {
					supportsDeveloperRole: false,
					supportsMultipleSystemMessages: true,
					supportsReasoningEffort: true,
					supportsUsageInStreaming: true,
				},
				models: [
					{
						id: "fake",
						name: "fake",
						reasoning: true,
						input: ["text", "image"],
						contextWindow: 272000,
						maxTokens: 128000,
					},
				],
			},
		},
	});
}

function variantCommand(variant, agentDir, work) {
	const base = { ...process.env };
	for (const key of Object.keys(base)) if (/_API_KEY$/.test(key)) delete base[key];
	const args = ["--mode", "rpc", "--provider", "fake", "--model", "fake"];
	if (!process.env.PROFILE_KEEP_SESSION) args.push("--no-session");
	if (variant === "pi") return { command: "pi", args, env: { ...base, PI_CODING_AGENT_DIR: agentDir } };
	const env = {
		...base,
		ULTRON_CODING_AGENT_DIR: agentDir,
		ULTRON_SERVER_DIR: mkdtempSync(join("/tmp", "u-prof-")),
		ULTRON_HINDSIGHT_URL: "off",
		ULTRON_STARTUP_TRACE: join(work, "startup-trace.jsonl"),
	};
	if (variant === "ultron") return { command: "ultron", args, env };
	// This checkout's bundled build (`npm run build`), the shape `ultron` ships as.
	if (variant === "bundle")
		return { command: process.execPath, args: [join(root, "packages/coding-agent/dist/bundle/cli.js"), ...args], env };
	// Another checkout's bundled build, e.g. a `git archive` of the base commit: --variants bundle:/path/to/checkout
	if (variant.startsWith("bundle:"))
		return {
			command: process.execPath,
			args: [join(variant.slice("bundle:".length), "packages/coding-agent/dist/bundle/cli.js"), ...args],
			env,
		};
	return {
		command: process.execPath,
		args: [
			"--import",
			join(root, "packages/coding-agent/src/experimental/source-resolver.ts"),
			join(root, "packages/coding-agent/src/cli.ts"),
			...args,
		],
		env,
		cleanup: () => rmSync(env.ULTRON_SERVER_DIR, { recursive: true, force: true }),
		work,
	};
}

async function runOnce(variant, scenario) {
	const provider = await startProvider(scenario.reply);
	const work = mkdtempSync(join(tmpdir(), "ultron-prof-"));
	const agentDir = join(work, "agent");
	const project = join(work, "project");
	mkdirSync(agentDir, { recursive: true });
	mkdirSync(project, { recursive: true });
	writeFileSync(join(agentDir, "models.json"), modelsJson(provider.baseUrl));
	for (const [path, content] of Object.entries(scenario.files ?? {})) writeFileSync(join(project, path), content);
	// PROFILE_SESSION_HISTORY=<count>x<MiB>: earlier sessions in the profile, like a long-used install.
	const history = /^(\d+)x(\d+)$/.exec(process.env.PROFILE_SESSION_HISTORY ?? "");
	if (history) {
		const directory = join(agentDir, "experimental", "sessions", "--old-project--");
		mkdirSync(directory, { recursive: true });
		const body = `${JSON.stringify([{ kind: "value", op: "set", seq: 1, namespace: "x", key: "y", value: "z".repeat(1000) }])}\n`.repeat(
			Math.ceil((Number(history[2]) * 1024 * 1024) / 1100),
		);
		for (let index = 0; index < Number(history[1]); index += 1) {
			const header = { v: 4, kind: "header", id: `old-${index}`, storageVersion: 1, createdAt: index, cwd: "/old" };
			writeFileSync(join(directory, `old-${index}.jsonl`), `${JSON.stringify(header)}\n${body}`);
		}
	}
	const { command, args, env, cleanup } = variantCommand(variant, agentDir, work);
	const t0 = performance.now();
	const child = spawn(command, args, { cwd: project, env, stdio: ["pipe", "pipe", "pipe"] });
	let stderr = "";
	child.stderr.on("data", (chunk) => {
		stderr = (stderr + chunk).slice(-4000);
	});
	const exited = new Promise((done) => child.on("exit", done));
	let buffer = "";
	let nextId = 0;
	const pending = new Map();
	const listeners = new Set();
	child.stdout.on("data", (chunk) => {
		buffer += chunk;
		let newline = buffer.indexOf("\n");
		while (newline !== -1) {
			const line = buffer.slice(0, newline);
			buffer = buffer.slice(newline + 1);
			newline = buffer.indexOf("\n");
			let message;
			try {
				message = JSON.parse(line);
			} catch {
				continue;
			}
			if (message.type === "response" && pending.has(message.id)) {
				pending.get(message.id)(message);
				pending.delete(message.id);
			} else for (const listener of listeners) listener(message);
		}
	});
	const send = (payload) =>
		new Promise((done, fail) => {
			const id = `r${++nextId}`;
			pending.set(id, done);
			child.stdin.write(`${JSON.stringify({ ...payload, id })}\n`);
			void exited.then(() => fail(new Error(`exited: ${stderr}`)));
		});
	const result = { variant, prompts: [] };
	try {
		await send({ type: "get_state" });
		result.readyMs = performance.now() - t0;
		result.spawnedAt = performance.timeOrigin + t0;
		for (const prompt of scenario.prompts) {
			const before = provider.requests.length;
			const started = performance.now();
			const settled = new Promise((done) => {
				const listener = (event) => {
					if (event.type === "agent_end" || event.type === "agent_settled") {
						listeners.delete(listener);
						done(performance.now());
					}
				};
				listeners.add(listener);
			});
			const toolTimes = [];
			const toolListener = (event) => {
				if (event.type === "tool_execution_start") toolTimes.push({ start: performance.now() });
				if (event.type === "tool_execution_end" && toolTimes.at(-1)) toolTimes.at(-1).end = performance.now();
			};
			listeners.add(toolListener);
			await send({ type: "prompt", message: prompt });
			const ended = await settled;
			listeners.delete(toolListener);
			for (let attempt = 0; attempt < 50; attempt += 1) {
				const state = await send({ type: "get_state" });
				if (!state.data?.isStreaming) break;
				await new Promise((done) => setTimeout(done, 20));
			}
			const idle = performance.now();
			const requests = provider.requests.slice(before);
			result.prompts.push({
				firstRequestMs: requests[0] ? requests[0].at - started : null,
				endMs: ended - started,
				idleMs: idle - started,
				// From the end of the tool call to the follow-up request: per-turn host work.
				afterToolMs: requests[1] && toolTimes[0]?.end ? requests[1].at - toolTimes[0].end : null,
				toolMs: toolTimes[0]?.end ? toolTimes[0].end - toolTimes[0].start : null,
				requests: requests.map(({ system: _system, toolNames: _names, at: _at, raw: _raw, headers: _headers, ...rest }) => rest),
			});
		}
		result.firstRequest = provider.requests[0];
		result.requestCount = provider.requests.length;
		result.raws = provider.requests.map((request) => request.raw);
	} catch (error) {
		result.error = `${error.message}\n${stderr}`.slice(0, 2000);
	} finally {
		const closing = performance.now();
		child.stdin.end();
		const timer = setTimeout(() => child.kill("SIGKILL"), 15_000);
		await exited;
		clearTimeout(timer);
		result.closeMs = performance.now() - closing;
		result.totalMs = performance.now() - t0;
		await provider.stop();
		cleanup?.();
		result.trace = readTrace(join(work, "startup-trace.jsonl"), result.spawnedAt);
		if (process.env.PROFILE_KEEP_SESSION) result.work = work;
		else rmSync(work, { recursive: true, force: true });
	}
	return result;
}

/** Startup milestones relative to spawn: `<label>` at its time, `<pid>.boot` at each process's time origin. */
function readTrace(path, spawnedAt) {
	if (!spawnedAt || !existsSync(path)) return undefined;
	const marks = {};
	const seen = new Set();
	for (const line of readFileSync(path, "utf8").split("\n")) {
		if (!line) continue;
		const entry = JSON.parse(line);
		const role = entry.label.split(".")[0];
		if (!seen.has(entry.pid)) {
			seen.add(entry.pid);
			marks[`${role}.boot`] ??= entry.origin - spawnedAt;
		}
		marks[entry.label] ??= entry.at - spawnedAt;
	}
	return marks;
}

function median(values) {
	const sorted = values.filter((value) => typeof value === "number").sort((a, b) => a - b);
	if (sorted.length === 0) return null;
	const middle = Math.floor(sorted.length / 2);
	return Math.round(sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2);
}

async function main() {
	const runs = Number(arg("runs", "5"));
	const variants = arg("variants", "pi,source").split(",");
	const scenarios = arg("scenario", "bash,text,rlm").split(",");
	const report = {};
	for (const name of scenarios) {
		const scenario = SCENARIOS[name];
		for (const variant of variants) {
			if (scenario.only && !scenario.only.includes(variant.split(":")[0])) continue;
			const results = [];
			for (let run = 0; run < runs; run += 1) results.push(await runOnce(variant, scenario));
			const failed = results.filter((result) => result.error);
			if (failed.length) console.error(`${variant}/${name}: ${failed.length} failed\n${failed[0].error}`);
			const ok = results.filter((result) => !result.error);
			const first = ok[0]?.firstRequest;
			const summary = {
				readyMs: median(ok.map((result) => result.readyMs)),
				requestCount: median(ok.map((result) => result.requestCount)),
				closeMs: median(ok.map((result) => result.closeMs)),
				totalMs: median(ok.map((result) => result.totalMs)),
				prompts: scenario.prompts.map((_, index) => ({
					firstRequestMs: median(ok.map((result) => result.prompts[index]?.firstRequestMs)),
					toolMs: median(ok.map((result) => result.prompts[index]?.toolMs)),
					afterToolMs: median(ok.map((result) => result.prompts[index]?.afterToolMs)),
					endMs: median(ok.map((result) => result.prompts[index]?.endMs)),
					idleMs: median(ok.map((result) => result.prompts[index]?.idleMs)),
				})),
				startup: ok[0]?.trace
					? Object.fromEntries(
							Object.keys(ok[0].trace).map((label) => [label, median(ok.map((result) => result.trace?.[label]))]),
						)
					: undefined,
				firstRequest: first && {
					bytes: first.bytes,
					systemChars: first.systemChars,
					tools: first.tools,
					toolBytes: first.toolBytes,
					toolNames: first.toolNames,
					params: first.params,
				},
			};
			if (process.env.PROFILE_KEEP_SESSION) summary.work = ok.map((result) => result.work);
			report[`${name}/${variant}`] = summary;
			console.log(`${name}/${variant}: ${JSON.stringify(summary)}`);
			if (arg("dump-system", "") && first) writeFileSync(join(arg("dump-system", ""), `${name}-${variant}.txt`), first.system);
			if (arg("dump-system", "") && ok[0])
				writeFileSync(join(arg("dump-system", ""), `${name}-${variant}.requests.jsonl`), `${ok[0].raws.join("\n")}\n`);
			if (arg("dump-system", "") && first)
				writeFileSync(join(arg("dump-system", ""), `${name}-${variant}.headers.json`), JSON.stringify(first.headers, null, 2));
		}
	}
	const out = arg("out", "");
	if (out) writeFileSync(out, `${JSON.stringify(report, null, 2)}\n`);
}

await main();
