// Generates crates/kernel/fixtures/session.json from the TypeScript RLM kernel supervisor in this checkout
// (packages/coding-agent/src/ultron/rlm/kernel.ts) driving the real runtime.py.
// Records: kernelEnvironment, kernelResourceLimits and rlmOutputBudget over input tables, and one scripted session
// (results, errors, and every protocol line in both directions, captured by fixtures/tap.py). The Rust tests run
// the same session through crates/kernel and require identical results and protocol lines.
// Usage: node crates/kernel/fixtures/generate.mjs
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const rlm = join(root, "packages/coding-agent/src/ultron/rlm");
const { RlmKernel, kernelEnvironment, kernelResourceLimits } = await import(pathToFileURL(join(rlm, "kernel.ts")).href);
const { rlmOutputBudget } = await import(pathToFileURL(join(rlm, "output-truncation.ts")).href);

const outcome = (run) => {
	try {
		return { ok: run() };
	} catch (error) {
		return { error: error.message };
	}
};

const environmentCases = [
	{ PATH: "/bin", HOME: "/home/u", OPENAI_API_KEY: "k", GITHUB_TOKEN: "t", my_secret: "s", PASSWD_FILE: "p" },
	{ AWS_ACCESS_KEY_ID: "a", PRIVATE_KEY_PATH: "p", AUTHOR: "x", COOKIE_JAR: "c", SESSIONKEY: "s", WEBHOOK_URL: "w" },
	{ PI_SESSION_WORKER_FD: "3", ULTRON_SESSION_WORKER_TOKEN: "x", ULTRON_SESSION: "keep", PI_SESSION: "keep" },
	{ ULTRON_RLM_ENV_ALLOW: " GITHUB_TOKEN , PI_SESSION_WORKER_FD,,", GITHUB_TOKEN: "t", PI_SESSION_WORKER_FD: "3", NPM_TOKEN: "n" },
	{ ULTRON_RLM_ENV_ALLOW: "ULTRON_RLM_ENV_ALLOW", api_key: "lower", ApiKey: "mixed", PASSWORD: "", EMPTY: "" },
	{ CREDENTIALS_DIR: "c", ACCESSKEY: "a", "API-KEY": "dash", TOKENIZER: "t", AUTHORITY: "a", PLAIN: "v" },
].map((env) => ({ env, result: kernelEnvironment(env) }));

const limitCases = [
	[{}, {}],
	[{ ULTRON_RLM_MAX_MEMORY_MB: "512", ULTRON_RLM_MAX_CPU_SECONDS: "0" }, {}],
	[{ ULTRON_RLM_MAX_MEMORY_MB: "lots", ULTRON_RLM_MAX_CPU_SECONDS: "-1" }, {}],
	[{ ULTRON_RLM_MAX_MEMORY_MB: "512" }, { maxMemoryMb: 64 }],
	[{ ULTRON_RLM_MAX_MEMORY_MB: " 77 ", ULTRON_RLM_MAX_CPU_SECONDS: "   " }, {}],
	[{ ULTRON_RLM_MAX_MEMORY_MB: "1e3", ULTRON_RLM_MAX_CPU_SECONDS: "0x10" }, {}],
	[{ ULTRON_RLM_MAX_MEMORY_MB: "007", ULTRON_RLM_MAX_CPU_SECONDS: "+5" }, {}],
	[{ ULTRON_RLM_MAX_MEMORY_MB: "99999999999999999999" }, {}],
	[{ ULTRON_RLM_MAX_CPU_SECONDS: "9007199254740991" }, {}],
	[{ ULTRON_RLM_MAX_CPU_SECONDS: "9007199254740992" }, {}],
	[{}, { maxMemoryMb: 0, maxCpuSeconds: 0 }],
	[{ ULTRON_RLM_MAX_MEMORY_MB: "1\n" }, {}],
].map(([env, overrides]) => ({ env, overrides, ...outcome(() => kernelResourceLimits(env, overrides)) }));

const budgetCases = [undefined, "", "  ", "abc", "0", "-5", "1.5", "500", "1024", " 4096 ", "200000", "131072", "1e4", "0x400", ".5e4",
	"5000.", "0b10000000000", "0o4000", "0X400", "Infinity", "-Infinity", "inf", "NaN", "1_000", "-0", "+2048", "1e400", "\ufeff2048\u2028",
	"\u00852048", "2048 x", "0x", "1e", "e5", "+0x10", "00012", "4096.000"].map(
	(value) => ({ value: value ?? null, result: rlmOutputBudget(value === undefined ? {} : { ULTRON_RLM_OUTPUT_BYTES: value }) }),
);

// The scripted session. A `reset` step calls resetScratch(); a `comment` step executes "#" and that many "x"; the rest
// execute their code.
const steps = [
	"answer = 42\nprint('ready')\nanswer",
	"answer + 1",
	"await rlm.host_request('test.echo', {'n': 1, 's': 'é', 'nested': [True, None, 1.5]})",
	"await rlm.host_request('test.fail', {})",
	"raise ValueError('bad')",
	"import os, sys\nos.write(1, b'raw out\\n')\nprint('printed')\nos.write(2, b'raw err\\n')\nprint('to stderr', file=sys.stderr)\nNone",
	"def f(:\n  pass",
	"import asyncio\nawait asyncio.sleep(0.01)\n'awaited'",
	"print('héllo 🎉 \\x1e')\n'ok'",
	"print('x' * 50000)",
	"print('\\udcff lone')\n'\\ud800'",
	"len('\u0001\u001f\u007f\u2028\b\t')",
	{ reset: true },
	"'answer' in globals(), 'state' in globals()",
	"import os\nos._exit(3)",
	"'answer' in globals()",
	"import os\nos._exit(86)",
	"import os\nos._exit(87)",
	"1 + 1",
	"import os, signal\nos.kill(os.getpid(), signal.SIGKILL)",
	{ comment: 1024 * 1024 },
	"6 * 7",
];

/** Lone surrogates become U+FFFD, as the Rust supervisor decodes them (a Rust string cannot hold them). */
const wellFormed = (value) =>
	typeof value === "string"
		? value.toWellFormed()
		: Array.isArray(value)
			? value.map(wellFormed)
			: value && typeof value === "object"
				? Object.fromEntries(Object.entries(value).map(([key, item]) => [key, wellFormed(item)]))
				: value;

function normalizeText(text, cwd) {
	return text.split(rlm).join("<rlm>").split(cwd).join("<cwd>");
}

async function session() {
	const cwd = mkdtempSync(join(tmpdir(), "ultron-kernel-fixture-"));
	const log = join(cwd, "protocol.log");
	const kernel = new RlmKernel(
		{
			cwd,
			runtimePath: join(here, "tap.py"),
			treeMemoryBackend: "off",
			limits: { maxMemoryMb: 4096, maxCpuSeconds: 1800 },
			env: {
				ULTRON_TAP_RUNTIME: join(rlm, "runtime.py"),
				ULTRON_TAP_LOG: log,
				ULTRON_RLM_SPILL_DIR: "",
				ULTRON_RLM_OUTPUT_BYTES: "20000",
			},
		},
		(type, payload) => {
			if (type === "test.fail") throw new Error(`refused ${type}`);
			return { type, payload };
		},
	);
	const results = [];
	try {
		for (const step of steps) {
			const recorded = typeof step === "string" ? { code: step } : step;
			const code = typeof step === "string" ? step : `#${"x".repeat(step.comment ?? 0)}`;
			try {
				const result = await (step.reset ? kernel.resetScratch() : kernel.execute(code));
				results.push({ ...recorded, ok: JSON.parse(normalizeText(JSON.stringify(wellFormed(result)), cwd)) });
			} catch (error) {
				results.push({ ...recorded, error: normalizeText(error.message, cwd) });
			}
		}
	} finally {
		await kernel.shutdown();
	}
	const wire = normalizeWire(readFileSync(log, "utf8"));
	rmSync(cwd, { recursive: true, force: true });
	return { steps: results, wire };
}

/** Protocol lines per direction; host-made UUIDs become req-N in order of first use, the kernel pid becomes 0, and
 * the checkout path becomes <root> so the fixture does not depend on where it was recorded. */
function normalizeWire(text) {
	const ids = new Map();
	const wire = { h2k: [], k2h: [] };
	for (const entry of text.split("\n").filter(Boolean).map((line) => JSON.parse(line))) {
		const line = entry.line
			.replace(/[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}/g, (id) => {
				if (!ids.has(id)) ids.set(id, `req-${ids.size + 1}`);
				return ids.get(id);
			})
			.replace(/"pid":[0-9]+/, '"pid":0')
			.replaceAll(root, "<root>");
		wire[entry.dir].push(line);
	}
	return wire;
}

const fixture = {
	source: "packages/coding-agent/src/ultron/rlm/kernel.ts",
	environment: environmentCases,
	limits: limitCases,
	budget: budgetCases,
	session: await session(),
};
const lines = (items) => items.map((item) => JSON.stringify(item)).join(",\n");
writeFileSync(
	join(here, "session.json"),
	`{"source":${JSON.stringify(fixture.source)},\n"environment":[\n${lines(fixture.environment)}\n],\n"limits":[\n${lines(fixture.limits)}\n],\n"budget":[\n${lines(fixture.budget)}\n],\n"session":{"steps":[\n${lines(fixture.session.steps)}\n],\n"wire":{"h2k":[\n${lines(fixture.session.wire.h2k)}\n],\n"k2h":[\n${lines(fixture.session.wire.k2h)}\n]}}}\n`,
);
