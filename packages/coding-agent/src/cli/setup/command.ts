/**
 * `ultron setup`, and the offer to run it when an interactive start finds no usable model.
 */

import { spawn } from "node:child_process";
import { join } from "node:path";
import type { Api, Model } from "@ultron/ai";
import { isClaudeCodeModel } from "@ultron/ai/providers/claude-code";
import type { Terminal } from "@ultron/tui";
import chalk from "chalk";
import { APP_NAME, getAgentDir, getBundledLokiPath } from "../../config.ts";
import { ModelRuntime } from "../../core/model-runtime.ts";
import { SettingsManager } from "../../core/settings-manager.ts";
import { NativeJevClient } from "../../ultron/jev.ts";
import type { Args } from "../args.ts";
import { bundledLokiVersion, probeCommand, shouldOfferSetup } from "./core.ts";
import type { ModelTestResult, SetupDeps, SetupResult } from "./wizard.ts";

const MODEL_TEST_TIMEOUT_MS = 90_000;
const HINDSIGHT_WAIT_MS = 5 * 60_000;

/** Pi's model runtime over the profile's `auth.json` and `models.json`, without network catalog refreshes. */
export async function createProfileRuntime(agentDir: string): Promise<ModelRuntime> {
	const runtime = await ModelRuntime.create({
		refreshOnCreate: false,
		allowModelNetwork: false,
		authPath: join(agentDir, "auth.json"),
		modelsPath: join(agentDir, "models.json"),
	});
	await runtime.refresh({ allowNetwork: false });
	return runtime;
}

/** One short request, as small as the provider allows. */
export async function testModel(runtime: ModelRuntime, model: Model<Api>): Promise<ModelTestResult> {
	const started = Date.now();
	try {
		const message = await runtime.completeSimple(
			model,
			{
				messages: [{ role: "user", content: "Reply with the single word: ok", timestamp: Date.now() }],
			},
			{ signal: AbortSignal.timeout(MODEL_TEST_TIMEOUT_MS) },
		);
		const ms = Date.now() - started;
		if (message.stopReason === "error" || message.stopReason === "aborted")
			return { ok: false, error: message.errorMessage ?? `request ${message.stopReason}`, ms };
		const reply = message.content
			.filter((part) => part.type === "text")
			.map((part) => part.text)
			.join("");
		return { ok: true, reply, ms };
	} catch (error) {
		return { ok: false, error: error instanceof Error ? error.message : String(error), ms: Date.now() - started };
	}
}

export function createSetupDeps(
	agentDir: string,
	settings: SettingsManager,
	env: NodeJS.ProcessEnv = process.env,
): SetupDeps {
	return {
		agentDir,
		env,
		nodeVersion: process.versions.node,
		probe: probeCommand,
		fetch: globalThis.fetch,
		settings,
		createRuntime: () => createProfileRuntime(agentDir),
		testModel,
		checkJevKey: async (key) => {
			try {
				await new NativeJevClient({ apiKey: key, baseUrl: env.TYPESAFE_BASE_URL, timeoutMs: 15_000 }).memoryRecall(
					"ultron setup: checking the key",
				);
				return { ok: true };
			} catch (error) {
				return { ok: false, error: error instanceof Error ? error.message : String(error) };
			}
		},
		runDocker: (args, extraEnv) =>
			new Promise((resolve) => {
				let output = "";
				const child = spawn("docker", [...args], {
					env: { ...process.env, ...extraEnv },
					stdio: ["ignore", "pipe", "pipe"],
				});
				child.stdout.on("data", (chunk: Buffer) => {
					output += chunk.toString("utf8");
				});
				child.stderr.on("data", (chunk: Buffer) => {
					output += chunk.toString("utf8");
				});
				child.on("error", (error) => resolve({ ok: false, output: error.message }));
				child.on("close", (code) => resolve({ ok: code === 0, output }));
			}),
		hindsightWaitMs: HINDSIGHT_WAIT_MS,
		...lokiVersionOption(),
	};
}

function lokiVersionOption(): { lokiVersion?: string } {
	const version = bundledLokiVersion(getBundledLokiPath());
	return version === undefined ? {} : { lokiVersion: version };
}

/** Run the wizard on a terminal (the process terminal, or a virtual one in tests). */
export async function runSetupOnTerminal(
	settings: SettingsManager,
	deps: SetupDeps,
	terminal?: Terminal,
): Promise<SetupResult> {
	const [{ createStartupTui }, { TuiSetupUi }, { runSetupWizard }] = await Promise.all([
		import("../startup-ui.ts"),
		import("./tui.ts"),
		import("./wizard.ts"),
	]);
	const ui = await createStartupTui(settings, terminal);
	const setupUi = new TuiSetupUi(ui);
	ui.start();
	try {
		return await runSetupWizard(setupUi, deps);
	} finally {
		setupUi.dispose();
		// Let the last frame reach the terminal: the transcript stays in the scrollback as the record of what changed.
		await new Promise((resolve) => setTimeout(resolve, 30));
		ui.stop();
	}
}

function printSetupHelp(): void {
	console.log(`Usage:
  ${APP_NAME} setup

A guided setup: checks Node.js and python3, sets up a provider (subscription login, API key or a custom
OpenAI-compatible endpoint) and the default model with a live test, saves a Jev API key and finds or installs
the Hindsight memory server. Every step can be skipped; run it again at any time.

Files are written to ${getAgentDir()} (secrets with mode 0600).`);
}

/** `ultron setup`; returns false when `args` is not the setup command. */
export async function runSetupCommand(args: readonly string[]): Promise<boolean> {
	if (args[0] !== "setup") return false;
	if (args.includes("--help") || args.includes("-h")) {
		printSetupHelp();
		return true;
	}
	if (args.length > 1) {
		console.error(chalk.red(`Unknown option for "${APP_NAME} setup": ${args[1]}`));
		process.exitCode = 1;
		return true;
	}
	if (!process.stdin.isTTY || !process.stdout.isTTY) {
		console.error(
			chalk.red(`${APP_NAME} setup is interactive and needs a terminal.`) +
				`\nWithout one, configure ${getAgentDir()} directly: provider keys in environment variables (e.g. ANTHROPIC_API_KEY),` +
				` custom endpoints in models.json, the Jev key in jev-api-key, and ULTRON_HINDSIGHT_URL.`,
		);
		process.exitCode = 1;
		return true;
	}
	const agentDir = getAgentDir();
	const settings = SettingsManager.create(process.cwd(), agentDir, { projectTrusted: false });
	const result = await runSetupOnTerminal(settings, createSetupDeps(agentDir, settings));
	process.stdout.write(
		result.completed
			? `\nSetup finished. Start with: ${APP_NAME}\n`
			: `\nSetup stopped. Run "${APP_NAME} setup" to continue.\n`,
	);
	return true;
}

/**
 * What the session worker would start on, by its own rules (`findInitialModel`): the saved default when its provider
 * has credentials, else any available model. Claude Code CLI models are listed whenever a `claude` executable is on
 * PATH, but the worker never falls back to them (they cannot drive a root lane on their own), so they only count
 * under `ultron --claude` (ULTRON_ROOT=claude).
 */
async function startModelState(
	agentDir: string,
	settings: SettingsManager,
	env: NodeJS.ProcessEnv,
): Promise<"usable" | "none" | "default-without-credentials" | "unknown-default"> {
	const runtime = await createProfileRuntime(agentDir);
	const provider = settings.getDefaultProvider();
	const id = settings.getDefaultModel();
	const saved = provider && id ? runtime.getModel(provider, id) : undefined;
	if (saved && runtime.hasConfiguredAuth(saved.provider)) return "usable";
	const claudeRoot = env.ULTRON_ROOT?.trim().toLowerCase() === "claude";
	if (runtime.getAvailableSnapshot().some((model) => claudeRoot || !isClaudeCodeModel(model))) return "usable";
	if (!provider || !id) return "none";
	// A default this profile does not know may come from an extension's provider, which only the worker loads.
	return saved ? "default-without-credentials" : "unknown-default";
}

/**
 * A model the worker could start with: a saved default (taken on trust, so a normal start does not load the model
 * catalog), or any model the worker would fall back to.
 */
export async function hasUsableModel(
	agentDir: string,
	settings: SettingsManager,
	env: NodeJS.ProcessEnv = process.env,
): Promise<boolean> {
	if (settings.getDefaultProvider() && settings.getDefaultModel()) return true;
	try {
		return (await startModelState(agentDir, settings, env)) === "usable";
	} catch {
		// A broken models.json or auth.json is reported by the normal start; do not stand in its way.
		return true;
	}
}

/** Whether this parsed command starts the TUI with no model named on the command line. */
function interactiveStartWithoutModel(parsed: Args): { interactive: boolean; explicitModel: boolean } {
	return {
		interactive:
			parsed.mode === undefined &&
			!parsed.print &&
			parsed.messages.length === 0 &&
			parsed.fileArgs.length === 0 &&
			!parsed.help &&
			!parsed.version &&
			parsed.listModels === undefined &&
			!parsed.export,
		explicitModel:
			parsed.model !== undefined ||
			parsed.provider !== undefined ||
			parsed.apiKey !== undefined ||
			(parsed.extensions?.length ?? 0) > 0 ||
			(parsed.models?.length ?? 0) > 0,
	};
}

/**
 * On an interactive start with no usable model, offer `ultron setup` (once per start; "Don't ask again" is saved).
 * Print, JSON and RPC runs, and any start without a terminal on both ends, are never prompted. Returns false when
 * the start should end here: the wizard was quit, or still left no model to start with.
 */
export async function offerSetupOnFirstRun(parsed: Args): Promise<boolean> {
	const agentDir = getAgentDir();
	const settings = SettingsManager.create(process.cwd(), agentDir, { projectTrusted: false });
	const offer = await shouldOfferSetup({
		stdinIsTTY: process.stdin.isTTY === true,
		stdoutIsTTY: process.stdout.isTTY === true,
		...interactiveStartWithoutModel(parsed),
		env: process.env,
		dismissed: settings.getSkipSetupPrompt(),
		hasUsableModel: () => hasUsableModel(agentDir, settings),
	});
	if (!offer) return true;
	const { showStartupSelector } = await import("../startup-ui.ts");
	const choice = await showStartupSelector(
		settings,
		`No model is set up for ${APP_NAME} yet. Run the guided setup now?`,
		[
			{ label: "Run setup (recommended)", value: "run" as const },
			{ label: "Not now", value: "later" as const },
			{ label: "Don't ask again", value: "never" as const },
		],
	);
	if (choice === "never") {
		settings.setSkipSetupPrompt(true);
		await settings.flush();
		process.stdout.write(`You can run "${APP_NAME} setup" at any time.\n`);
		return true;
	}
	if (choice !== "run") return true;
	const result = await runSetupOnTerminal(settings, createSetupDeps(agentDir, settings));
	if (result.completed && (await hasUsableModel(agentDir, settings))) return true;
	process.stdout.write(`\nNo model is set up yet. Run "${APP_NAME} setup" to continue.\n`);
	return false;
}

/**
 * A hint for a failed start that had no model to start with: nothing configured, or a saved default whose provider
 * has no credentials any more (after `/logout`, or a default saved although its live test failed).
 */
export async function noModelHint(
	agentDir: string = getAgentDir(),
	env: NodeJS.ProcessEnv = process.env,
): Promise<string | undefined> {
	const settings = SettingsManager.create(process.cwd(), agentDir, { projectTrusted: false });
	const state = await startModelState(agentDir, settings, env);
	if (state === "none")
		return `No model is configured. Run "${APP_NAME} setup", or set a provider key such as ANTHROPIC_API_KEY or OPENAI_API_KEY.`;
	if (state === "default-without-credentials")
		return `The default model ${settings.getDefaultProvider()}/${settings.getDefaultModel()} has no credentials, and no other model is set up. Run "${APP_NAME} setup" to sign in again or choose another model.`;
	return undefined;
}
