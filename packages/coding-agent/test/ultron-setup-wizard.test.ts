import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createProfileRuntime } from "../src/cli/setup/command.ts";
import {
	type Choice,
	type InputOptions,
	runSetupWizard,
	type SetupDeps,
	SetupQuit,
	type SetupUi,
	type Tone,
} from "../src/cli/setup/wizard.ts";
import { SettingsManager } from "../src/core/settings-manager.ts";
import type { LoginOutcome } from "../src/experimental/client-tui-auth.ts";
import type { AuthSelectorProvider } from "../src/modes/interactive/components/oauth-selector.ts";

/** One scripted answer: the question it expects, and the choice (a label pattern) or text; `esc` presses Esc. */
type Step =
	| { readonly select: RegExp; readonly pick: RegExp | "esc" }
	| { readonly input: RegExp; readonly text: string | "esc" }
	| { readonly quit: true };

class ScriptedUi implements SetupUi {
	readonly transcript: string[] = [];
	readonly asked: string[] = [];
	readonly secrets: string[] = [];
	private readonly steps: Step[];

	constructor(steps: Step[]) {
		this.steps = steps;
	}

	heading(title: string): void {
		this.transcript.push(`# ${title}`);
	}

	note(text: string, _tone?: Tone): void {
		this.transcript.push(text);
	}

	private next(kind: "select" | "input", title: string): Step {
		this.asked.push(title);
		const step = this.steps.shift();
		if (!step) throw new Error(`Unscripted ${kind}: ${title}\n${this.transcript.join("\n")}`);
		if ("quit" in step) throw new SetupQuit();
		const pattern = "select" in step ? step.select : step.input;
		if (!(kind in step) || !pattern.test(title))
			throw new Error(`Expected ${pattern} but was asked ${kind} "${title}"`);
		return step;
	}

	async select<T>(title: string, choices: readonly Choice<T>[]): Promise<T | undefined> {
		const step = this.next("select", title) as { pick: RegExp | "esc" };
		if (step.pick === "esc") return undefined;
		const pick = step.pick;
		const choice = choices.find((candidate) => pick.test(candidate.label));
		if (!choice) throw new Error(`No choice ${pick} in "${title}": ${choices.map((c) => c.label).join(" | ")}`);
		return choice.value;
	}

	async input(title: string, options: InputOptions = {}): Promise<string | undefined> {
		const step = this.next("input", title) as { text: string | "esc" };
		if (step.text === "esc") return undefined;
		if (options.secret) this.secrets.push(step.text);
		const problem = options.validate?.(step.text);
		if (problem) throw new Error(`Invalid scripted answer for "${title}": ${problem}`);
		return step.text;
	}

	async pickLoginProvider(): Promise<AuthSelectorProvider | undefined> {
		throw new Error("not scripted");
	}

	async login(): Promise<LoginOutcome> {
		throw new Error("not scripted");
	}

	busy<T>(_message: string, task: () => Promise<T>): Promise<T> {
		return task();
	}

	get remaining(): number {
		return this.steps.length;
	}
}

const dirs: string[] = [];
afterEach(() => {
	for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

function profile(): { agentDir: string; settings: SettingsManager } {
	const root = mkdtempSync(join(tmpdir(), "ultron-setup-wizard-"));
	dirs.push(root);
	const agentDir = join(root, "agent");
	mkdirSync(agentDir, { recursive: true });
	return { agentDir, settings: SettingsManager.create(root, agentDir, { projectTrusted: false }) };
}

function fakeFetch(routes: Record<string, () => Response>): typeof fetch {
	return (async (url: string | URL | Request) => {
		const key = Object.keys(routes).find((route) => String(url).endsWith(route));
		if (!key) throw new TypeError("fetch failed", { cause: new Error(`connect ECONNREFUSED ${String(url)}`) });
		return routes[key]!();
	}) as typeof fetch;
}

function deps(agentDir: string, settings: SettingsManager, overrides: Partial<SetupDeps> = {}): SetupDeps {
	return {
		agentDir,
		// No provider keys from the machine running the tests.
		env: {},
		nodeVersion: "22.19.0",
		probe: (command) => (command === "python3" ? "Python 3.12.0" : undefined),
		fetch: fakeFetch({
			"/v1/models": () => new Response(JSON.stringify({ data: [{ id: "m1" }, { id: "m2" }] })),
		}),
		settings,
		createRuntime: () => createProfileRuntime(agentDir),
		testModel: async () => ({ ok: true, reply: "ok", ms: 420 }),
		checkJevKey: async () => ({ ok: true }),
		runDocker: async () => ({ ok: true, output: "" }),
		hindsightWaitMs: 50,
		...overrides,
	};
}

const CUSTOM_ENDPOINT: Step[] = [
	{ select: /reach a model/, pick: /Custom OpenAI-compatible/ },
	{ input: /Base URL/, text: "http://127.0.0.1:8317/v1" },
	{ input: /API key/, text: "sk-proxy-secret" },
	{ input: /short name/, text: "proxy" },
	{ select: /Which API/, pick: /Chat Completions/ },
	{ input: /Model ids/, text: "m1, m2" },
	{ select: /Default model/, pick: /^m2$/ },
];

describe("ultron setup wizard", () => {
	it("configures a custom endpoint, the default model, Jev and shows Hindsight's manual install", async () => {
		const { agentDir, settings } = profile();
		const testModel = vi.fn<SetupDeps["testModel"]>(async () => ({ ok: true, reply: "ok", ms: 420 }));
		const checkJevKey = vi.fn<SetupDeps["checkJevKey"]>(async () => ({ ok: true }));
		const ui = new ScriptedUi([
			...CUSTOM_ENDPOINT,
			{ select: /Jev API key/, pick: /Paste a key/ },
			{ input: /Jev API key/, text: "jev-secret-key" },
			{ select: /Check the key/, pick: /^Yes/ },
			{ select: /Set up Hindsight/, pick: /manual install/ },
			{ select: /Check again/, pick: /Continue without/ },
			{ select: /^Loki guardrails$/, pick: /^Keep/ },
		]);
		const result = await runSetupWizard(ui, deps(agentDir, settings, { testModel, checkJevKey }));
		expect(ui.remaining).toBe(0);
		expect(result.completed).toBe(true);
		expect(result.modelReady).toBe(true);

		// models.json in the loader's format, private.
		const modelsPath = join(agentDir, "models.json");
		expect(statSync(modelsPath).mode & 0o777).toBe(0o600);
		expect(JSON.parse(readFileSync(modelsPath, "utf8")).providers.proxy).toEqual({
			baseUrl: "http://127.0.0.1:8317/v1",
			api: "openai-completions",
			apiKey: "sk-proxy-secret",
			models: [{ id: "m1" }, { id: "m2" }],
		});
		// The default model, tested once with the real runtime's model object.
		const saved = JSON.parse(readFileSync(join(agentDir, "settings.json"), "utf8"));
		expect(saved).toMatchObject({ defaultProvider: "proxy", defaultModel: "m2" });
		expect(testModel).toHaveBeenCalledTimes(1);
		expect(testModel.mock.calls[0]?.[1]).toMatchObject({ provider: "proxy", id: "m2" });
		// Jev's key where jev.ts reads it.
		expect(readFileSync(join(agentDir, "jev-api-key"), "utf8")).toBe("jev-secret-key\n");
		expect(statSync(join(agentDir, "jev-api-key")).mode & 0o777).toBe(0o600);
		expect(checkJevKey).toHaveBeenCalledWith("jev-secret-key");

		const text = ui.transcript.join("\n");
		expect(text).toContain("The endpoint lists 2 models: m1, m2");
		expect(text).toContain("✓ Live test passed");
		expect(text).toContain("pip install hindsight-api");
		expect(text).toContain("# Step 5/6: Loki guardrails");
		expect(text).toContain("# Step 6/6: Summary");
		// Secrets were read masked and never shown.
		expect(ui.secrets).toEqual(["sk-proxy-secret", "jev-secret-key"]);
		for (const secret of ui.secrets) expect(text).not.toContain(secret);
		expect(result.lines.map((line) => [line.step, line.status])).toEqual([
			["Provider and model", "configured"],
			["Jev", "configured"],
			["Hindsight", "skipped"],
			["Loki", "unchanged"],
		]);
	});

	it("keeps a working setup when every step is skipped, and asks before replacing a provider", async () => {
		const { agentDir, settings } = profile();
		// First run: configure the endpoint, skip the rest with Esc.
		await runSetupWizard(
			new ScriptedUi([
				...CUSTOM_ENDPOINT,
				{ select: /Jev API key/, pick: "esc" },
				{ select: /Set up Hindsight/, pick: "esc" },
				{ select: /^Loki guardrails$/, pick: "esc" },
			]),
			deps(agentDir, settings),
		);
		const before = readFileSync(join(agentDir, "models.json"), "utf8");
		// Second run: the saved default is offered and kept; a re-entered "proxy" asks before replacing it.
		const ui = new ScriptedUi([
			{ select: /reach a model/, pick: /Custom OpenAI-compatible/ },
			{ input: /Base URL/, text: "http://other:1/v1" },
			{ input: /API key/, text: "" },
			{ input: /short name/, text: "proxy" },
			{ select: /already has a provider named "proxy"/, pick: /Choose another name/ },
			{ input: /short name/, text: "esc" },
			{ select: /reach a model/, pick: /^Keep proxy\/m2$/ },
			{ select: /Jev API key/, pick: /^Skip/ },
			{ select: /Set up Hindsight/, pick: /^Skip$/ },
			{ select: /^Loki guardrails$/, pick: /^Keep/ },
		]);
		const result = await runSetupWizard(ui, deps(agentDir, settings));
		expect(ui.remaining).toBe(0);
		expect(readFileSync(join(agentDir, "models.json"), "utf8")).toBe(before);
		expect(result.lines[0]).toMatchObject({ status: "unchanged", detail: "proxy/m2" });
		expect(existsSync(join(agentDir, "jev-api-key"))).toBe(false);
	});

	it("reports a failed live test with the provider's error and lets the user pick another model", async () => {
		const { agentDir, settings } = profile();
		const testModel = vi
			.fn<SetupDeps["testModel"]>()
			.mockResolvedValueOnce({ ok: false, error: "401 invalid api key", ms: 80 })
			.mockResolvedValueOnce({ ok: true, reply: "ok", ms: 90 });
		const ui = new ScriptedUi([
			...CUSTOM_ENDPOINT,
			{ select: /live test failed/, pick: /Try another model/ },
			{ select: /Default model/, pick: /^m1$/ },
			{ quit: true },
		]);
		const result = await runSetupWizard(ui, deps(agentDir, settings, { testModel }));
		expect(ui.transcript.join("\n")).toContain("✗ Live test failed after 0.1s: 401 invalid api key");
		expect(JSON.parse(readFileSync(join(agentDir, "settings.json"), "utf8"))).toMatchObject({ defaultModel: "m1" });
		// Ctrl+C at the Jev step ends the wizard and keeps what was saved.
		expect(result.completed).toBe(false);
		expect(ui.transcript.join("\n")).toContain("Setup stopped");
	});

	it("installs Hindsight with Docker, passing the key only through the environment", async () => {
		const { agentDir, settings } = profile();
		let healthy = false;
		const runDocker = vi.fn(async (_args: readonly string[], _env: Record<string, string>) => {
			healthy = true;
			return { ok: true, output: "container-id\n" };
		});
		const fetcher = fakeFetch({
			"/health": () => (healthy ? new Response("{}") : new Response("", { status: 503 })),
		});
		const ui = new ScriptedUi([
			{ select: /reach a model/, pick: /Skip/ },
			{ select: /Jev API key/, pick: "esc" },
			{ select: /Set up Hindsight/, pick: /Install it with Docker/ },
			{ select: /LLM provider for Hindsight/, pick: /^Anthropic$/ },
			{ input: /API key for Anthropic/, text: "sk-ant-hindsight" },
			{ input: /Model for Hindsight/, text: "" },
			{ select: /Run it now/, pick: /Run docker/ },
			{ select: /^Loki guardrails$/, pick: /^Keep/ },
		]);
		const result = await runSetupWizard(
			ui,
			deps(agentDir, settings, {
				runDocker,
				fetch: fetcher,
				probe: (command, args) =>
					command === "docker" ? (args[0] === "ps" ? "" : "Docker version 27.0.0") : "Python 3.12.0",
			}),
		);
		expect(ui.remaining).toBe(0);
		const [args, env] = runDocker.mock.calls[0]!;
		expect(args).toContain("HINDSIGHT_API_LLM_PROVIDER=anthropic");
		expect(args).toContain("HINDSIGHT_API_LLM_API_KEY");
		expect(args.join(" ")).not.toContain("sk-ant-hindsight");
		expect(env).toEqual({ HINDSIGHT_API_LLM_API_KEY: "sk-ant-hindsight" });
		expect(ui.transcript.join("\n")).not.toContain("sk-ant-hindsight");
		expect(result.lines[2]).toMatchObject({ step: "Hindsight", status: "configured" });
	});

	it("detects a running Hindsight, and saves a custom URL as a setting", async () => {
		const { agentDir, settings } = profile();
		const fetcher = fakeFetch({ "mem.local:9000/health": () => new Response("{}") });
		const ui = new ScriptedUi([
			{ select: /reach a model/, pick: /Skip/ },
			{ select: /Jev API key/, pick: "esc" },
			{ select: /Set up Hindsight/, pick: /set its URL/ },
			{ input: /Hindsight URL/, text: "http://mem.local:9000/" },
			{ select: /^Loki guardrails$/, pick: /^Keep/ },
		]);
		const result = await runSetupWizard(ui, deps(agentDir, settings, { fetch: fetcher }));
		expect(ui.remaining).toBe(0);
		expect(JSON.parse(readFileSync(join(agentDir, "settings.json"), "utf8")).hindsightUrl).toBe(
			"http://mem.local:9000",
		);
		expect(result.lines[2]).toMatchObject({ status: "configured", detail: "http://mem.local:9000" });

		// Next run: found at the saved URL.
		const again = new ScriptedUi([
			{ select: /reach a model/, pick: /Skip/ },
			{ select: /Jev API key/, pick: "esc" },
			{ select: /^Hindsight$/, pick: /Use it/ },
			{ select: /^Loki guardrails$/, pick: /^Keep/ },
		]);
		const second = await runSetupWizard(again, deps(agentDir, settings, { fetch: fetcher }));
		expect(again.transcript.join("\n")).toContain("✓ Hindsight is running at http://mem.local:9000.");
		expect(second.lines[2]).toMatchObject({ status: "unchanged" });
	});

	it("shows Loki's version and missing analyzers, and saves its toggles as global settings", async () => {
		const { agentDir, settings } = profile();
		const ui = new ScriptedUi([
			{ select: /reach a model/, pick: /Skip/ },
			{ select: /Jev API key/, pick: "esc" },
			{ select: /Set up Hindsight/, pick: /^Skip$/ },
			{ select: /^Loki guardrails$/, pick: /^Turn auto-commit off$/ },
			{ select: /^Loki guardrails$/, pick: /^Advise only/ },
			{ select: /^Loki guardrails$/, pick: /^Keep/ },
		]);
		const result = await runSetupWizard(
			ui,
			deps(agentDir, settings, {
				lokiVersion: "0.1.2 (35aa99c2aeee)",
				env: { ULTRON_LOKI_AUTOINIT: "off" },
				probe: (command) => (command === "python3" || command === "ruff" ? "found" : undefined),
			}),
		);
		expect(ui.remaining).toBe(0);
		const text = ui.transcript.join("\n");
		expect(text).toContain("✓ Bundled Loki 0.1.2 (35aa99c2aeee)");
		expect(text).toContain("✓ ruff (Python)");
		expect(text).toContain("– mypy (Python types): not found. uv tool install mypy");
		expect(text).toContain("– clippy (Rust): not found. rustup component add clippy");
		expect(text).toContain("ULTRON_LOKI_AUTOINIT=off is set");
		expect(JSON.parse(readFileSync(join(agentDir, "settings.json"), "utf8")).loki).toEqual({
			autoCommit: false,
			mode: "advise",
		});
		expect(result.lines.at(-1)).toMatchObject({
			step: "Loki",
			status: "configured",
			detail: "advise-only, auto-install on, auto-commit off",
		});
	});
});
