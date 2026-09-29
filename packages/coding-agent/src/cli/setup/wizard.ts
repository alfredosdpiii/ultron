/**
 * `ultron setup`: a guided first configuration in six steps (environment, provider and model, Jev, Hindsight, Loki
 * guardrails, summary). Every step can be skipped with Esc and the wizard can be run again; nothing is overwritten without
 * asking, and secrets are read masked and never printed. The questions go through `SetupUi`, so the flow is the
 * same in the terminal (`tui.ts`) and in tests.
 */

import { join } from "node:path";
import type { ThinkingLevel } from "@ultron/agent-core";
import { type Api, getSupportedThinkingLevels, type Model } from "@ultron/ai";
import { DEFAULT_THINKING_LEVEL } from "../../core/defaults.ts";
import type { ModelRuntime } from "../../core/model-runtime.ts";
import type { LokiSettings, SettingsManager } from "../../core/settings-manager.ts";
import { type LoginOutcome, type LoginRuntime, loginProviderOptions } from "../../experimental/client-tui-auth.ts";
import type { AuthSelectorProvider } from "../../modes/interactive/components/oauth-selector.ts";
import {
	type CustomEndpointApi,
	checkEnvironment,
	checkLokiAnalyzers,
	customProviderExists,
	effectiveHindsightUrl,
	formatCommand,
	HINDSIGHT_CONTAINER,
	HINDSIGHT_IMAGE,
	HINDSIGHT_LLM_PROVIDERS,
	type HindsightLlmChoice,
	hindsightDockerArgs,
	hindsightManualInstructions,
	hindsightPort,
	isLocalUrl,
	jevKeyPath,
	jevKeySource,
	listEndpointModels,
	mergeCustomEndpoint,
	type ProbeCommand,
	parseModelIds,
	probeHindsight,
	readTextFile,
	saveJevKey,
	validateBaseUrl,
	validateProviderId,
	waitForHindsight,
	writePrivateFile,
} from "./core.ts";

export interface Choice<T> {
	readonly label: string;
	readonly value: T;
	readonly description?: string;
}

export type Tone = "info" | "success" | "warning" | "error" | "dim";

export interface InputOptions {
	/** Read masked: the value is never drawn. */
	readonly secret?: boolean;
	readonly initial?: string;
	readonly description?: string;
	/** An error message for an unacceptable value; the question is asked again. */
	readonly validate?: (value: string) => string | undefined;
}

/** Thrown by a `SetupUi` when the user quits the whole wizard (Ctrl+C). */
export class SetupQuit extends Error {
	constructor() {
		super("Setup stopped");
	}
}

export interface SetupUi {
	/** Start a step. */
	heading(title: string): void;
	note(text: string, tone?: Tone): void;
	/** undefined: Esc (skip or go back). */
	select<T>(title: string, choices: readonly Choice<T>[], options?: { description?: string }): Promise<T | undefined>;
	/** undefined: Esc. */
	input(title: string, options?: InputOptions): Promise<string | undefined>;
	/** Pi's `/login` provider picker for one kind of login. */
	pickLoginProvider(
		runtime: LoginRuntime,
		providers: readonly AuthSelectorProvider[],
		title: string,
	): Promise<AuthSelectorProvider | undefined>;
	/** Pi's login dialog (browser/device flows, masked API-key prompt); saves to the runtime's auth.json. */
	login(runtime: LoginRuntime, option: AuthSelectorProvider): Promise<LoginOutcome>;
	/** Show `message` with a spinner while `task` runs. */
	busy<T>(message: string, task: () => Promise<T>): Promise<T>;
}

export interface ModelTestResult {
	readonly ok: boolean;
	readonly reply?: string;
	readonly error?: string;
	readonly ms: number;
}

export interface SetupDeps {
	readonly agentDir: string;
	readonly env: NodeJS.ProcessEnv;
	readonly nodeVersion: string;
	readonly probe: ProbeCommand;
	readonly fetch: typeof fetch;
	/** Global settings: default model and thinking level, the Hindsight URL. */
	readonly settings: SettingsManager;
	/** A model runtime over `<agentDir>/auth.json` and `models.json`, loaded fresh (after files changed). */
	createRuntime(): Promise<ModelRuntime>;
	/** One tiny live request. */
	testModel(runtime: ModelRuntime, model: Model<Api>): Promise<ModelTestResult>;
	/** One cheap Jev request with this key. */
	checkJevKey(key: string): Promise<{ ok: boolean; error?: string }>;
	/** Run docker with extra environment (for the API key); resolves with its exit status and output. */
	runDocker(args: readonly string[], env: Record<string, string>): Promise<{ ok: boolean; output: string }>;
	/** How long to wait for a new Hindsight container to answer. */
	readonly hindsightWaitMs: number;
	/** The Loki engine version bundled with this installation, when it has one. */
	readonly lokiVersion?: string;
}

export interface SetupSummaryLine {
	readonly step: string;
	readonly status: "configured" | "unchanged" | "skipped" | "failed";
	readonly detail: string;
}

export interface SetupResult {
	readonly completed: boolean;
	readonly lines: readonly SetupSummaryLine[];
	/** A default model is saved and its live test passed (or it was kept as it was). */
	readonly modelReady: boolean;
}

const STEPS = 6;

function stepTitle(index: number, title: string): string {
	return `Step ${index}/${STEPS}: ${title}`;
}

function describeModel(model: Model<Api>): string {
	return `${model.provider}/${model.id}`;
}

async function confirm(ui: SetupUi, title: string, yes = "Yes", no = "No", description?: string): Promise<boolean> {
	const answer = await ui.select(
		title,
		[
			{ label: yes, value: true },
			{ label: no, value: false },
		],
		description === undefined ? undefined : { description },
	);
	return answer === true;
}

/** Run the wizard. Never throws for a user's choice; `SetupQuit` ends it early with what was done so far. */
export async function runSetupWizard(ui: SetupUi, deps: SetupDeps): Promise<SetupResult> {
	const lines: SetupSummaryLine[] = [];
	let modelReady = false;
	let completed = false;
	try {
		environmentStep(ui, deps);
		const model = await providerStep(ui, deps);
		lines.push(model.line);
		modelReady = model.ready;
		lines.push(await jevStep(ui, deps));
		lines.push(await hindsightStep(ui, deps));
		lines.push(await lokiStep(ui, deps));
		completed = true;
	} catch (error) {
		if (!(error instanceof SetupQuit)) throw error;
		ui.note("Setup stopped. Anything saved before this point is kept.", "warning");
	}
	summaryStep(ui, deps, lines, completed);
	return { completed, lines, modelReady };
}

// ---------------------------------------------------------------------------------------------------------------
// 1. Environment
// ---------------------------------------------------------------------------------------------------------------

function environmentStep(ui: SetupUi, deps: SetupDeps): void {
	ui.heading(stepTitle(1, "Environment"));
	for (const check of checkEnvironment(deps.nodeVersion, deps.probe)) {
		const mark = check.ok ? "✓" : check.optional ? "–" : "✗";
		ui.note(`${mark} ${check.label}: ${check.detail}`, check.ok ? "success" : check.optional ? "dim" : "error");
		if (!check.ok && check.fix) ui.note(`  ${check.fix}`, "warning");
	}
}

// ---------------------------------------------------------------------------------------------------------------
// 2. Provider and model
// ---------------------------------------------------------------------------------------------------------------

type ProviderPath = "oauth" | "api_key" | "custom" | "keep" | "skip";

async function providerStep(ui: SetupUi, deps: SetupDeps): Promise<{ line: SetupSummaryLine; ready: boolean }> {
	const step = "Provider and model";
	ui.heading(stepTitle(2, step));
	let runtime = await ui.busy("Loading providers…", () => deps.createRuntime());
	const current = currentDefaultModel(runtime, deps.settings);
	if (current) ui.note(`Current default: ${describeModel(current)}`, "info");
	else if (runtime.getAvailableSnapshot().length > 0)
		ui.note(`${runtime.getAvailableSnapshot().length} models are available, but no default is saved.`, "info");
	else ui.note("No provider is configured yet.", "warning");

	for (;;) {
		const path = await ui.select<ProviderPath>("How should Ultron reach a model?", [
			...(current ? [{ label: `Keep ${describeModel(current)}`, value: "keep" as const }] : []),
			{
				label: "Sign in with a subscription",
				value: "oauth",
				description: "Claude Pro/Max, ChatGPT Plus/Pro, GitHub Copilot, … (Pi's /login)",
			},
			{ label: "Use an API key", value: "api_key", description: "Anthropic, OpenAI, Google, OpenRouter, …" },
			{
				label: "Custom OpenAI-compatible endpoint",
				value: "custom",
				description: "A proxy, Ollama, LM Studio, vLLM, … (written to models.json)",
			},
			{ label: "Skip this step", value: "skip" },
		]);
		if (path === undefined || path === "skip")
			return {
				line: { step, status: "skipped", detail: current ? `kept ${describeModel(current)}` : "no model" },
				ready: false,
			};
		if (path === "keep") {
			const tested = await testAndReport(ui, deps, runtime, current!);
			return {
				line: {
					step,
					status: "unchanged",
					detail: `${describeModel(current!)}${tested ? "" : " (live test failed)"}`,
				},
				ready: tested,
			};
		}

		let providerId: string | undefined;
		if (path === "custom") providerId = await customEndpoint(ui, deps);
		else providerId = await loginProvider(ui, runtime, path, deps.agentDir);
		if (providerId === undefined) continue; // Back to the choice of path.
		runtime = await ui.busy("Reloading providers…", () => deps.createRuntime());

		const outcome = await chooseDefaultModel(ui, deps, runtime, providerId);
		if (outcome === "back") continue;
		return outcome;
	}
}

function currentDefaultModel(runtime: ModelRuntime, settings: SettingsManager): Model<Api> | undefined {
	const provider = settings.getDefaultProvider();
	const id = settings.getDefaultModel();
	if (!provider || !id) return undefined;
	const model = runtime.getModel(provider, id);
	return model && runtime.hasConfiguredAuth(model.provider) ? model : undefined;
}

/** Pi's `/login` for one provider; returns its id once credentials are saved. */
async function loginProvider(
	ui: SetupUi,
	runtime: ModelRuntime,
	authType: "oauth" | "api_key",
	agentDir: string,
): Promise<string | undefined> {
	const options = loginProviderOptions(runtime, authType);
	if (options.length === 0) {
		ui.note(
			authType === "oauth" ? "No subscription providers are available." : "No API-key providers are available.",
			"warning",
		);
		return undefined;
	}
	for (;;) {
		const option = await ui.pickLoginProvider(
			runtime,
			options,
			authType === "oauth" ? "Sign in with a subscription" : "Choose the provider for your API key",
		);
		if (option === undefined) return undefined;
		if (option.status) {
			const replace = await confirm(
				ui,
				`${option.name} is already configured (${option.status.source ?? option.status.type}).`,
				"Use it as it is",
				"Sign in again and replace it",
			);
			if (replace) return option.id;
		}
		if (option.authType === "api_key" && !option.method?.login) {
			ui.note(
				`${option.method?.name ?? option.name} is configured outside Ultron (environment variables or cloud credentials).`,
				"warning",
			);
			continue;
		}
		const outcome = await ui.login(runtime, option);
		if (outcome.ok) {
			const authPath = join(agentDir, "auth.json");
			ui.note(
				option.authType === "oauth"
					? `✓ Signed in to ${option.name}. Saved to ${authPath}.`
					: `✓ Saved the API key for ${option.name} to ${authPath}.`,
				"success",
			);
			return option.id;
		}
		if (outcome.cancelled) continue;
		ui.note(`✗ ${option.name}: ${outcome.error}`, "error");
	}
}

/** Ask for a custom OpenAI-compatible endpoint and write it to models.json; returns its provider id. */
async function customEndpoint(ui: SetupUi, deps: SetupDeps): Promise<string | undefined> {
	const modelsPath = join(deps.agentDir, "models.json");
	ui.note(`The endpoint is saved in ${modelsPath} (readable only by you).`, "dim");
	const baseUrl = await ui.input("Base URL of the endpoint (including /v1)", {
		initial: "http://localhost:11434/v1",
		validate: validateBaseUrl,
	});
	if (baseUrl === undefined) return undefined;
	const apiKey = await ui.input("API key (leave empty if the endpoint needs none)", { secret: true });
	if (apiKey === undefined) return undefined;
	let providerId: string | undefined;
	for (;;) {
		providerId = await ui.input("A short name for this provider", {
			initial: providerId ?? "custom",
			validate: validateProviderId,
		});
		if (providerId === undefined) return undefined;
		if (!customProviderExists(modelsPath, providerId)) break;
		if (
			await confirm(
				ui,
				`models.json already has a provider named "${providerId}".`,
				"Replace it",
				"Choose another name",
			)
		)
			break;
	}
	const api = await ui.select<CustomEndpointApi>("Which API does it speak?", [
		{
			label: "OpenAI Chat Completions",
			value: "openai-completions",
			description: "most proxies, Ollama, vLLM, LM Studio",
		},
		{ label: "OpenAI Responses", value: "openai-responses", description: "/v1/responses" },
	]);
	if (api === undefined) return undefined;
	const listed = await ui.busy(`Asking ${baseUrl} for its models…`, () =>
		listEndpointModels(baseUrl, apiKey, deps.fetch),
	);
	let modelIds: string[] | undefined;
	if (listed && listed.length > 0) {
		const shown = listed.slice(0, 20).join(", ");
		ui.note(
			`The endpoint lists ${listed.length} model${listed.length === 1 ? "" : "s"}: ${shown}${listed.length > 20 ? ", …" : ""}`,
			"info",
		);
	} else {
		ui.note("The endpoint did not list its models; enter the ids yourself.", "dim");
	}
	for (;;) {
		const answer = await ui.input("Model ids to use (comma-separated)", {
			initial: listed?.length === 1 ? listed[0] : "",
			validate: (value) => (parseModelIds(value).length === 0 ? "Enter at least one model id" : undefined),
		});
		if (answer === undefined) return undefined;
		modelIds = parseModelIds(answer);
		const unknown = listed ? modelIds.filter((id) => !listed.includes(id)) : [];
		if (unknown.length === 0) break;
		if (await confirm(ui, `The endpoint does not list ${unknown.join(", ")}.`, "Use them anyway", "Edit the list"))
			break;
	}
	let merged: ReturnType<typeof mergeCustomEndpoint>;
	try {
		merged = mergeCustomEndpoint(readTextFile(modelsPath), {
			providerId,
			baseUrl,
			apiKey: apiKey.trim() || undefined,
			api,
			modelIds,
		});
	} catch (error) {
		ui.note(`✗ ${error instanceof Error ? error.message : String(error)} (${modelsPath} was not changed)`, "error");
		return undefined;
	}
	writePrivateFile(modelsPath, merged.text);
	ui.note(`✓ ${merged.replaced ? "Replaced" : "Added"} provider "${providerId}" in ${modelsPath}.`, "success");
	return providerId;
}

async function chooseDefaultModel(
	ui: SetupUi,
	deps: SetupDeps,
	runtime: ModelRuntime,
	providerId: string,
): Promise<{ line: SetupSummaryLine; ready: boolean } | "back"> {
	const step = "Provider and model";
	const error = runtime.getError();
	if (error) ui.note(error, "error");
	const models = [...(await runtime.getAvailable(providerId))];
	if (models.length === 0) {
		ui.note(`No models of "${providerId}" are available yet; check its credentials.`, "error");
		return "back";
	}
	for (;;) {
		const model = await ui.select<Model<Api>>(
			`Default model (${models.length} from ${runtime.getProvider(providerId)?.name ?? providerId})`,
			models.map((candidate) => ({
				label: candidate.id,
				value: candidate,
				description: candidate.name !== candidate.id ? candidate.name : undefined,
			})),
		);
		if (model === undefined) return "back";
		const thinking = await chooseThinking(ui, deps, model);
		const tested = await testAndReport(ui, deps, runtime, model);
		if (!tested) {
			const next = await ui.select<"retry" | "other" | "save">("The live test failed.", [
				{ label: "Try another model", value: "other" },
				{ label: "Run the test again", value: "retry" },
				{ label: "Save it anyway", value: "save" },
			]);
			if (next === undefined || next === "other") continue;
			if (next === "retry") {
				if (!(await testAndReport(ui, deps, runtime, model))) continue;
			}
		}
		deps.settings.setDefaultModelAndProvider(model.provider, model.id);
		if (thinking !== undefined) deps.settings.setDefaultThinkingLevel(thinking);
		await deps.settings.flush();
		ui.note(`✓ Default model saved: ${describeModel(model)}${thinking ? `, thinking ${thinking}` : ""}.`, "success");
		return {
			line: {
				step,
				status: "configured",
				detail: `${describeModel(model)}${tested ? ", live test passed" : ", live test failed"}`,
			},
			ready: tested,
		};
	}
}

async function chooseThinking(ui: SetupUi, deps: SetupDeps, model: Model<Api>): Promise<ThinkingLevel | undefined> {
	const levels = getSupportedThinkingLevels(model) as ThinkingLevel[];
	if (levels.length <= 1) return undefined;
	const current = deps.settings.getDefaultThinkingLevel() ?? DEFAULT_THINKING_LEVEL;
	const choice = await ui.select<ThinkingLevel | "keep">("Thinking level", [
		{ label: `Keep ${current}`, value: "keep" },
		...levels.filter((level) => level !== current).map((level) => ({ label: level, value: level })),
	]);
	return choice === undefined || choice === "keep" ? undefined : choice;
}

async function testAndReport(ui: SetupUi, deps: SetupDeps, runtime: ModelRuntime, model: Model<Api>): Promise<boolean> {
	const result = await ui.busy(`Testing ${describeModel(model)} with one tiny request…`, () =>
		deps.testModel(runtime, model),
	);
	const seconds = (result.ms / 1000).toFixed(1);
	if (result.ok) {
		const reply = result.reply?.replace(/\s+/g, " ").trim().slice(0, 60);
		ui.note(`✓ Live test passed in ${seconds}s${reply ? `: "${reply}"` : ""}`, "success");
	} else {
		ui.note(`✗ Live test failed after ${seconds}s: ${result.error ?? "unknown error"}`, "error");
	}
	return result.ok;
}

// ---------------------------------------------------------------------------------------------------------------
// 3. Jev
// ---------------------------------------------------------------------------------------------------------------

async function jevStep(ui: SetupUi, deps: SetupDeps): Promise<SetupSummaryLine> {
	const step = "Jev";
	ui.heading(stepTitle(3, "Jev API key"));
	ui.note(
		"Jev gates automatic memory: it decides when a turn should recall memories and which turns are worth keeping.",
		"info",
	);
	const source = jevKeySource(deps.agentDir, deps.env);
	if (source === "env") ui.note("A key is set in TYPESAFE_API_KEY; it takes precedence over a saved key.", "info");
	else if (source === "file") ui.note(`A key is saved in ${jevKeyPath(deps.agentDir)}.`, "info");
	const action = await ui.select<"paste" | "keep" | "skip">("Jev API key", [
		...(source ? [{ label: "Keep the current key", value: "keep" as const }] : []),
		{ label: source === "file" ? "Replace the saved key" : "Paste a key", value: "paste" },
		{ label: "Skip (automatic memory stays off)", value: "skip" },
	]);
	if (action === undefined || action === "skip")
		return {
			step,
			status: "skipped",
			detail: source
				? `kept the key from ${source === "env" ? "TYPESAFE_API_KEY" : "the key file"}`
				: "not configured",
		};
	if (action === "keep")
		return { step, status: "unchanged", detail: source === "env" ? "TYPESAFE_API_KEY" : jevKeyPath(deps.agentDir) };
	const key = await ui.input("Jev API key", {
		secret: true,
		validate: (value) =>
			value.trim() === ""
				? "Paste the key, or press Esc to skip"
				: /\s/.test(value.trim())
					? "The key must not contain spaces"
					: undefined,
	});
	if (key === undefined) return { step, status: "skipped", detail: "not changed" };
	const path = saveJevKey(deps.agentDir, key);
	ui.note(`✓ Saved to ${path} (readable only by you).`, "success");
	if (source === "env") ui.note("TYPESAFE_API_KEY is set in this shell and still wins over the saved key.", "warning");
	if (await confirm(ui, "Check the key now?", "Yes, one small request to Jev", "No")) {
		const check = await ui.busy("Checking the Jev key…", () => deps.checkJevKey(key.trim()));
		if (check.ok) ui.note("✓ Jev accepted the key.", "success");
		else {
			ui.note(
				`✗ Jev did not accept the request: ${check.error ?? "unknown error"}. The key is saved; run setup again to replace it.`,
				"error",
			);
			return { step, status: "failed", detail: `saved to ${path}, check failed` };
		}
	}
	return { step, status: "configured", detail: `saved to ${path}` };
}

// ---------------------------------------------------------------------------------------------------------------
// 4. Hindsight
// ---------------------------------------------------------------------------------------------------------------

type HindsightAction = "docker" | "start" | "url" | "manual" | "off" | "skip" | "keep";

async function hindsightStep(ui: SetupUi, deps: SetupDeps): Promise<SetupSummaryLine> {
	const step = "Hindsight";
	ui.heading(stepTitle(4, "Hindsight memory server"));
	ui.note("Hindsight stores the memories Jev decides to keep and returns them when Jev asks to recall.", "info");
	const effective = effectiveHindsightUrl(deps.env, deps.settings.getHindsightUrl());
	if (effective.source === "env")
		ui.note(
			`ULTRON_HINDSIGHT_URL is set (${deps.env.ULTRON_HINDSIGHT_URL}); it overrides anything saved here.`,
			"warning",
		);
	if (effective.url === undefined) {
		ui.note("Memory is turned off.", "info");
		const turnOn = await confirm(ui, "Turn memory on?", "Yes, set a Hindsight URL", "No, leave it off");
		if (!turnOn) return { step, status: "unchanged", detail: "off" };
		return (await customHindsightUrl(ui, deps)) ?? { step, status: "unchanged", detail: "off" };
	}
	const url = effective.url;
	for (;;) {
		const probe = await ui.busy(`Looking for Hindsight at ${url}…`, () => probeHindsight(url, deps.fetch));
		if (probe.ok) {
			ui.note(`✓ Hindsight is running at ${url}.`, "success");
			const next = await ui.select<"keep" | "url">("Hindsight", [
				{ label: "Use it", value: "keep" },
				{ label: "Use a different URL", value: "url" },
			]);
			if (next === "url") {
				const saved = await customHindsightUrl(ui, deps);
				if (saved) return saved;
				continue;
			}
			return { step, status: "unchanged", detail: `running at ${url}` };
		}
		ui.note(`Hindsight is not reachable at ${url} (${probe.reason}).`, "warning");
		const dockerAvailable = deps.probe("docker", ["--version"]) !== undefined;
		const existing = dockerAvailable
			? deps.probe("docker", ["ps", "-a", "--filter", `name=^${HINDSIGHT_CONTAINER}$`, "--format", "{{.Status}}"])
			: undefined;
		const local = isLocalUrl(url);
		const action = await ui.select<HindsightAction>("Set up Hindsight", [
			...(dockerAvailable && local && existing
				? [
						{
							label: `Start the existing "${HINDSIGHT_CONTAINER}" container`,
							value: "start" as const,
							description: existing,
						},
					]
				: []),
			...(dockerAvailable && local && !existing
				? [{ label: "Install it with Docker", value: "docker" as const, description: HINDSIGHT_IMAGE }]
				: []),
			{ label: "It runs elsewhere: set its URL", value: "url" },
			{ label: "Show manual install instructions", value: "manual" },
			{ label: "Turn memory off", value: "off", description: "saves hindsightUrl: off" },
			{ label: "Skip", value: "skip" },
		]);
		if (action === undefined || action === "skip")
			return { step, status: "skipped", detail: `not reachable at ${url}` };
		if (action === "manual") {
			if (!dockerAvailable) ui.note("Docker was not found on this machine.", "dim");
			for (const line of hindsightManualInstructions(url)) ui.note(line, "dim");
			const again = await confirm(ui, "Check again once it is running?", "Check again", "Continue without it");
			if (again) continue;
			return { step, status: "skipped", detail: "manual install instructions shown" };
		}
		if (action === "off") {
			deps.settings.setHindsightUrl("off");
			await deps.settings.flush();
			ui.note("Memory is turned off. Run `ultron setup` again to turn it on.", "info");
			return { step, status: "configured", detail: "off" };
		}
		if (action === "url") {
			const saved = await customHindsightUrl(ui, deps);
			if (saved) return saved;
			continue;
		}
		const started =
			action === "start" ? await startExistingContainer(ui, deps) : await installWithDocker(ui, deps, url);
		if (started === undefined) continue;
		if (!started) return { step, status: "failed", detail: "the container did not start" };
		const up = await ui.busy(
			`Waiting for Hindsight at ${url} (the first start downloads models; up to ${Math.round(deps.hindsightWaitMs / 60_000)} min)…`,
			() => waitForHindsight(url, { timeoutMs: deps.hindsightWaitMs, fetcher: deps.fetch }),
		);
		if (up) {
			ui.note(`✓ Hindsight is running at ${url}.`, "success");
			return { step, status: "configured", detail: `Docker container "${HINDSIGHT_CONTAINER}" at ${url}` };
		}
		ui.note(
			`Hindsight did not answer within the wait. It may still be starting: \`docker logs -f ${HINDSIGHT_CONTAINER}\`.`,
			"warning",
		);
		return { step, status: "failed", detail: `container "${HINDSIGHT_CONTAINER}" started, not answering yet` };
	}
	// Unreachable: every branch returns or continues.
}

async function customHindsightUrl(ui: SetupUi, deps: SetupDeps): Promise<SetupSummaryLine | undefined> {
	const url = await ui.input("Hindsight URL", {
		initial: deps.settings.getHindsightUrl() ?? "http://localhost:8888",
		validate: validateBaseUrl,
	});
	if (url === undefined) return undefined;
	const trimmed = url.trim().replace(/\/+$/, "");
	const probe = await ui.busy(`Checking ${trimmed}…`, () => probeHindsight(trimmed, deps.fetch));
	if (!probe.ok) {
		ui.note(`Hindsight is not reachable at ${trimmed} (${probe.reason}).`, "warning");
		if (!(await confirm(ui, "Save this URL anyway?", "Save it", "Back"))) return undefined;
	} else ui.note(`✓ Hindsight is running at ${trimmed}.`, "success");
	deps.settings.setHindsightUrl(trimmed);
	await deps.settings.flush();
	ui.note(`✓ Saved hindsightUrl in ${join(deps.agentDir, "settings.json")}.`, "success");
	return { step: "Hindsight", status: "configured", detail: `${trimmed}${probe.ok ? "" : " (not reachable yet)"}` };
}

async function startExistingContainer(ui: SetupUi, deps: SetupDeps): Promise<boolean | undefined> {
	const result = await ui.busy(`docker start ${HINDSIGHT_CONTAINER}`, () =>
		deps.runDocker(["start", HINDSIGHT_CONTAINER], {}),
	);
	if (!result.ok) ui.note(`✗ docker start failed: ${result.output.trim().split("\n").slice(-3).join(" ")}`, "error");
	return result.ok;
}

/** Ask for Hindsight's own LLM, confirm the exact command, then `docker run` it. undefined: backed out. */
async function installWithDocker(ui: SetupUi, deps: SetupDeps, url: string): Promise<boolean | undefined> {
	ui.note(
		"Hindsight needs its own LLM to extract and summarise memories. Its key is passed to the container only.",
		"info",
	);
	const choice = await ui.select<HindsightLlmChoice>(
		"LLM provider for Hindsight",
		HINDSIGHT_LLM_PROVIDERS.map((provider) => ({
			label: provider.label,
			value: provider,
			description: provider.provider,
		})),
	);
	if (choice === undefined) return undefined;
	let baseUrl: string | undefined;
	if (choice.needsBaseUrl) {
		baseUrl = await ui.input("LLM base URL, as seen from inside the container", {
			initial: choice.defaultBaseUrl ?? "",
			description: "Use host.docker.internal for a server on this machine.",
			validate: validateBaseUrl,
		});
		if (baseUrl === undefined) return undefined;
	}
	let apiKey: string | undefined;
	if (choice.needsKey) {
		apiKey = await ui.input(`API key for ${choice.label}`, {
			secret: true,
			validate: (value) => (value.trim() === "" ? "Paste the key, or press Esc to go back" : undefined),
		});
		if (apiKey === undefined) return undefined;
	}
	const model = await ui.input("Model for Hindsight (empty: Hindsight's default for the provider)", { initial: "" });
	if (model === undefined) return undefined;
	const args = hindsightDockerArgs({
		port: hindsightPort(url),
		provider: choice.provider,
		model,
		...(baseUrl ? { baseUrl } : {}),
		hasApiKey: apiKey !== undefined,
	});
	ui.note("This will run (the key is passed through the environment, not the command line):", "info");
	ui.note(`  ${formatCommand("docker", args)}`, "dim");
	ui.note("The image is large (several GB for the full image); the first pull takes a while.", "dim");
	if (!(await confirm(ui, "Run it now?", "Run docker", "Back"))) return undefined;
	const result = await ui.busy("Pulling and starting Hindsight (docker run)…", () =>
		deps.runDocker(args, apiKey === undefined ? {} : { HINDSIGHT_API_LLM_API_KEY: apiKey.trim() }),
	);
	if (!result.ok) {
		ui.note(`✗ docker run failed: ${result.output.trim().split("\n").slice(-3).join(" ")}`, "error");
		return false;
	}
	ui.note(`✓ Started container "${HINDSIGHT_CONTAINER}".`, "success");
	return true;
}

// ---------------------------------------------------------------------------------------------------------------
// 5. Loki guardrails
// ---------------------------------------------------------------------------------------------------------------

type LokiAction = "keep" | "autoInit" | "autoCommit" | "advise" | "enforce" | "off" | "on";

function describeLoki(settings: LokiSettings): string {
	const mode = settings.mode ?? "on";
	if (mode === "off") return "off";
	return [
		mode === "advise" ? "advise-only" : "blocking",
		`auto-install ${settings.autoInit === false ? "off" : "on"}`,
		`auto-commit ${settings.autoCommit === false ? "off" : "on"}`,
	].join(", ");
}

async function lokiStep(ui: SetupUi, deps: SetupDeps): Promise<SetupSummaryLine> {
	const step = "Loki";
	ui.heading(stepTitle(5, "Loki guardrails"));
	ui.note(
		"Loki checks every file the agent writes with deterministic rules and real analyzers: edit() and write() are checked before the file changes, other writes after the cell.",
		"info",
	);
	ui.note(
		deps.lokiVersion === undefined
			? "✗ This installation has no bundled Loki engine; guardrails run only where a repository has its own .loki/."
			: `✓ Bundled Loki ${deps.lokiVersion}`,
		deps.lokiVersion === undefined ? "warning" : "success",
	);
	for (const name of ["ULTRON_LOKI", "ULTRON_LOKI_AUTOINIT", "ULTRON_LOKI_AUTOCOMMIT"])
		if (deps.env[name]?.trim())
			ui.note(`${name}=${deps.env[name]} is set; it overrides the settings saved here.`, "warning");
	const analyzers = checkLokiAnalyzers(deps.probe);
	for (const analyzer of analyzers)
		ui.note(
			`${analyzer.found ? "✓" : "–"} ${analyzer.name} (${analyzer.language})${analyzer.found ? "" : `: not found. ${analyzer.hint}`}`,
			analyzer.found ? "success" : "dim",
		);
	if (analyzers.some((analyzer) => !analyzer.found))
		ui.note("Missing analyzers are reported as NOT CHECKED, never as clean.", "dim");
	const initial = deps.settings.getLokiSettings();
	let current = initial;
	for (;;) {
		ui.note(`Loki: ${describeLoki(current)}.`, "info");
		const mode = current.mode ?? "on";
		const choices: Choice<LokiAction>[] =
			mode === "off"
				? [
						{ label: "Keep Loki off", value: "keep" },
						{ label: "Turn Loki on", value: "on" },
					]
				: [
						{ label: "Keep these settings", value: "keep" },
						{
							label: `Turn auto-install ${current.autoInit === false ? "on" : "off"}`,
							value: "autoInit",
							description: "Create .loki/ (engine and default policy) in a Git repository that lacks it",
						},
						{
							label: `Turn auto-commit ${current.autoCommit === false ? "on" : "off"}`,
							value: "autoCommit",
							description:
								"Commit the .loki/ Ultron created, and only it; on a fork the commit rides along in pull requests",
						},
						mode === "advise"
							? { label: "Block unsafe writes again", value: "enforce" }
							: { label: "Advise only (report, never block)", value: "advise" },
						{ label: "Turn Loki off", value: "off" },
					];
		const action = await ui.select("Loki guardrails", choices);
		if (action === undefined || action === "keep") break;
		if (action === "autoInit") current = { ...current, autoInit: current.autoInit === false };
		else if (action === "autoCommit") current = { ...current, autoCommit: current.autoCommit === false };
		else if (action === "advise") current = { ...current, mode: "advise" };
		else current = { ...current, mode: action === "off" ? "off" : "on" };
	}
	if (describeLoki(current) === describeLoki(initial))
		return { step, status: "unchanged", detail: describeLoki(current) };
	deps.settings.setLokiSettings(current);
	await deps.settings.flush();
	return { step, status: "configured", detail: describeLoki(current) };
}

// ---------------------------------------------------------------------------------------------------------------
// 6. Summary
// ---------------------------------------------------------------------------------------------------------------

function summaryStep(ui: SetupUi, deps: SetupDeps, lines: readonly SetupSummaryLine[], completed: boolean): void {
	ui.heading(stepTitle(6, "Summary"));
	for (const line of lines) {
		const tone: Tone = line.status === "configured" ? "success" : line.status === "failed" ? "error" : "dim";
		ui.note(`${line.step}: ${line.status} — ${line.detail}`, tone);
	}
	if (!completed) ui.note("Not all steps ran.", "warning");
	ui.note(`Configuration lives in ${deps.agentDir} (settings.json, auth.json, models.json, jev-api-key).`, "info");
	ui.note("Run `ultron setup` again at any time to change it; start working with `ultron`.", "info");
}
