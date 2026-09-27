/**
 * The quality eval must compare runtimes, not the user's setup: each agent runs with its own agent dir and an empty
 * HOME, so neither loads user-global skills from ~/.agents/skills. A fake OpenAI-compatible provider records each
 * agent's first request; a canary skill in a fake user home must reach it only when isolation is off (the control).
 */
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { ISOLATION, isolatedAgentEnv, rpcSession } from "./eval-quality.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const CANARY = "canary-user-skill-7f3a";

/** A local provider that answers every request with "ok" and keeps each request body. */
async function fakeProvider() {
	const bodies = [];
	const server = createServer(async (request, response) => {
		let raw = "";
		for await (const chunk of request) raw += chunk;
		bodies.push(raw);
		response.writeHead(200, { "content-type": "text/event-stream" });
		const chunk = (delta, finish, usage) =>
			`data: ${JSON.stringify({ id: "fake", object: "chat.completion.chunk", created: 0, model: "fake", choices: [{ index: 0, delta, finish_reason: finish }], ...(usage ? { usage } : {}) })}\n\n`;
		response.write(chunk({ role: "assistant", content: "ok" }, null));
		response.write(chunk({}, "stop", { prompt_tokens: 10, completion_tokens: 1, total_tokens: 11 }));
		response.end("data: [DONE]\n\n");
	});
	await new Promise((resolveListen) => server.listen(0, "127.0.0.1", resolveListen));
	return { bodies, baseUrl: `http://127.0.0.1:${server.address().port}/v1`, close: () => new Promise((done) => server.close(done)) };
}

function modelsJson(baseUrl) {
	return JSON.stringify({
		providers: {
			fake: {
				baseUrl,
				api: "openai-completions",
				apiKey: "fake-key",
				models: [{ id: "fake", name: "fake", reasoning: false, input: ["text"], contextWindow: 128000, maxTokens: 4096 }],
			},
		},
	});
}

/** Runs one agent for one prompt and returns its first request body. */
async function firstRequest({ command, agentDirEnv, isolated, userHome, provider }) {
	const work = mkdtempSync(join(tmpdir(), "ultron-eval-isolation-"));
	try {
		const project = join(work, "project");
		const agentDir = join(work, "agent");
		mkdirSync(project, { recursive: true });
		mkdirSync(agentDir, { recursive: true });
		writeFileSync(join(agentDir, "models.json"), modelsJson(provider.baseUrl));
		const baseEnv = { ...process.env, HOME: userHome };
		const env = {
			...(isolated ? isolatedAgentEnv({ work, agentDirEnv, agentDir, baseEnv }) : { ...baseEnv, [agentDirEnv]: agentDir }),
			ULTRON_SERVER_DIR: mkdtempSync(join(tmpdir(), "u-iso-")),
			ULTRON_HINDSIGHT_URL: "off",
		};
		const before = provider.bodies.length;
		const session = rpcSession({ command, args: ["--mode", "rpc", "--provider", "fake", "--model", "fake", "--no-session"], cwd: project, env });
		try {
			await session.turn("Say ok.", 60_000);
		} finally {
			await session.close().catch(() => {});
		}
		assert.ok(provider.bodies.length > before, "the agent sent no request");
		return provider.bodies[before];
	} finally {
		rmSync(work, { recursive: true, force: true });
	}
}

const agents = [
	{
		name: "ultron (source)",
		command: ["node", "--import", join(root, "packages/coding-agent/src/experimental/source-resolver.ts"), join(root, "packages/coding-agent/src/cli.ts")],
		agentDirEnv: "ULTRON_CODING_AGENT_DIR",
	},
	// Stock Pi is the eval's baseline; checked when it is installed.
	...(spawnSync("pi", ["--version"], { encoding: "utf8" }).status === 0 ? [{ name: "pi", command: ["pi"], agentDirEnv: "PI_CODING_AGENT_DIR" }] : []),
];

test("the result JSON documents home isolation", () => {
	assert.match(ISOLATION.home, /empty HOME/);
});

test("isolated runs point HOME and the XDG directories inside the run's work dir", () => {
	const work = mkdtempSync(join(tmpdir(), "ultron-eval-isolation-env-"));
	try {
		const env = isolatedAgentEnv({ work, agentDirEnv: "PI_CODING_AGENT_DIR", agentDir: join(work, "agent"), baseEnv: { HOME: "/home/someone", PATH: "/bin" } });
		assert.equal(env.HOME, join(work, "home"));
		assert.equal(env.PATH, "/bin");
		assert.equal(env.PI_CODING_AGENT_DIR, join(work, "agent"));
		for (const name of ["XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"]) assert.ok(env[name].startsWith(env.HOME), name);
	} finally {
		rmSync(work, { recursive: true, force: true });
	}
});

for (const agent of agents) {
	test(`${agent.name}: the user's global skills reach the first request only without isolation`, { timeout: 180_000 }, async () => {
		const userHome = mkdtempSync(join(tmpdir(), "ultron-eval-user-home-"));
		const skillDir = join(userHome, ".agents", "skills", CANARY);
		mkdirSync(skillDir, { recursive: true });
		writeFileSync(join(skillDir, "SKILL.md"), `---\nname: ${CANARY}\ndescription: Canary skill from the user's home; must not reach an isolated eval run.\n---\n\nCanary.\n`);
		const provider = await fakeProvider();
		try {
			const control = await firstRequest({ ...agent, isolated: false, userHome, provider });
			assert.ok(control.includes(CANARY), "control: without isolation the user's skill is listed (the check is meaningful)");
			const isolated = await firstRequest({ ...agent, isolated: true, userHome, provider });
			assert.ok(!isolated.includes(CANARY), "isolated: the user's skill must not be listed");
		} finally {
			await provider.close();
			rmSync(userHome, { recursive: true, force: true });
		}
	});
}
