/**
 * The pure parts of `ultron setup`: environment checks, the models.json entry for a custom endpoint, the Jev key
 * file, the Hindsight Docker command and health probe, and when an interactive start should offer setup.
 * Nothing here draws to the terminal; `wizard.ts` asks the questions and `tui.ts` draws them.
 */

import { spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { DEFAULT_HINDSIGHT_URL } from "../../core/defaults.ts";
import { stripJsonComments } from "../../utils/json.ts";
import { stripBom } from "../../utils/text.ts";

// ---------------------------------------------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------------------------------------------

export const MINIMUM_NODE_VERSION = "22.19.0";

export interface EnvironmentCheck {
	readonly label: string;
	readonly ok: boolean;
	readonly detail: string;
	/** What to do about it, when not ok. */
	readonly fix?: string;
	/** A failed optional check is a warning, not a blocker. */
	readonly optional?: boolean;
}

/** Run `command args` and return its first output line, or undefined when it is missing or fails. */
export type ProbeCommand = (command: string, args: readonly string[]) => string | undefined;

export const probeCommand: ProbeCommand = (command, args) => {
	try {
		const result = spawnSync(command, [...args], {
			encoding: "utf8",
			timeout: 5_000,
			stdio: ["ignore", "pipe", "pipe"],
		});
		if (result.error || result.status !== 0) return undefined;
		const line = `${result.stdout ?? ""}${result.stderr ?? ""}`.trim().split("\n")[0]?.trim();
		return line || "";
	} catch {
		return undefined;
	}
};

function compareVersions(left: string, right: string): number {
	const a = left
		.replace(/^v/, "")
		.split(".")
		.map((part) => Number.parseInt(part, 10) || 0);
	const b = right
		.replace(/^v/, "")
		.split(".")
		.map((part) => Number.parseInt(part, 10) || 0);
	for (let index = 0; index < Math.max(a.length, b.length); index++) {
		const difference = (a[index] ?? 0) - (b[index] ?? 0);
		if (difference !== 0) return Math.sign(difference);
	}
	return 0;
}

/** Node, python3 (the REPL runs on it) and Docker (only for installing Hindsight). */
export function checkEnvironment(nodeVersion: string, probe: ProbeCommand): EnvironmentCheck[] {
	const nodeOk = compareVersions(nodeVersion, MINIMUM_NODE_VERSION) >= 0;
	const python = probe("python3", ["--version"]);
	const docker = probe("docker", ["--version"]);
	return [
		{
			label: "Node.js",
			ok: nodeOk,
			detail: `v${nodeVersion.replace(/^v/, "")}`,
			...(nodeOk ? {} : { fix: `Ultron needs Node.js ${MINIMUM_NODE_VERSION} or newer: https://nodejs.org` }),
		},
		{
			label: "python3",
			ok: python !== undefined,
			detail: python === undefined ? "not found on PATH" : python || "found",
			...(python === undefined
				? {
						fix: "The REPL runs on the system python3. Install Python 3 (e.g. `brew install python`, `sudo apt install python3`) and make sure `python3` is on your PATH.",
					}
				: {}),
		},
		{
			label: "Docker",
			ok: docker !== undefined,
			optional: true,
			detail: docker === undefined ? "not found (only needed to install Hindsight)" : docker || "found",
		},
	];
}

// ---------------------------------------------------------------------------------------------------------------
// Loki guardrails
// ---------------------------------------------------------------------------------------------------------------

export interface LokiAnalyzerCheck {
	readonly name: string;
	/** What Loki uses it for. */
	readonly language: string;
	readonly found: boolean;
	readonly hint: string;
}

/** The analyzers Loki runs when a project needs them; each one missing is reported as NOT CHECKED, never as clean. */
export const LOKI_ANALYZERS: readonly {
	name: string;
	language: string;
	command: string;
	args: string[];
	hint: string;
}[] = [
	{
		name: "ruff",
		language: "Python",
		command: "ruff",
		args: ["--version"],
		hint: "uv tool install ruff (or pipx install ruff)",
	},
	{
		name: "mypy",
		language: "Python types",
		command: "mypy",
		args: ["--version"],
		hint: "uv tool install mypy (or pipx install mypy)",
	},
	{
		name: "tsc",
		language: "TypeScript types",
		command: "tsc",
		args: ["--version"],
		hint: "npm i -D typescript in the project (node_modules/.bin/tsc is used when present)",
	},
	{
		name: "oxlint",
		language: "JavaScript/TypeScript",
		command: "oxlint",
		args: ["--version"],
		hint: "npm i -D oxlint @oxlint/plugins in the project (node_modules/.bin/oxlint is used when present)",
	},
	{
		name: "golangci-lint",
		language: "Go",
		command: "golangci-lint",
		args: ["--version"],
		hint: "https://golangci-lint.run/welcome/install/",
	},
	{
		name: "clippy",
		language: "Rust",
		command: "cargo",
		args: ["clippy", "--version"],
		hint: "rustup component add clippy",
	},
	{
		name: "credo",
		language: "Elixir",
		command: "mix",
		args: ["--version"],
		hint: "install Elixir, then add {:credo, only: [:dev, :test]} to the project's mix.exs deps",
	},
	{
		name: "sobelow",
		language: "Phoenix security",
		command: "mix",
		args: ["--version"],
		hint: "install Elixir, then add {:sobelow, only: [:dev, :test]} to the project's mix.exs deps",
	},
];

/** Which of Loki's analyzers are on PATH (Credo and Sobelow are project dependencies: only Elixir is checked). */
export function checkLokiAnalyzers(probe: ProbeCommand): LokiAnalyzerCheck[] {
	const cache = new Map<string, boolean>();
	return LOKI_ANALYZERS.map((analyzer) => {
		const key = `${analyzer.command} ${analyzer.args.join(" ")}`;
		if (!cache.has(key)) cache.set(key, probe(analyzer.command, analyzer.args) !== undefined);
		return { name: analyzer.name, language: analyzer.language, found: cache.get(key)!, hint: analyzer.hint };
	});
}

/** The bundled Loki version from its VERSION.json next to loki.py, or undefined. */
export function bundledLokiVersion(enginePath: string | undefined): string | undefined {
	if (enginePath === undefined) return undefined;
	try {
		const pin = JSON.parse(readFileSync(join(dirname(enginePath), "VERSION.json"), "utf8")) as {
			version?: unknown;
			source?: { commit?: unknown };
		};
		if (typeof pin.version !== "string") return undefined;
		const commit = typeof pin.source?.commit === "string" ? ` (${pin.source.commit.slice(0, 12)})` : "";
		return `${pin.version}${commit}`;
	} catch {
		return undefined;
	}
}

// ---------------------------------------------------------------------------------------------------------------
// Secret files
// ---------------------------------------------------------------------------------------------------------------

/** Write a file readable only by the user (0600), in a directory only the user can list (0700 when created). */
export function writePrivateFile(path: string, content: string): void {
	mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
	const temporary = `${path}.${process.pid}.tmp`;
	writeFileSync(temporary, content, { encoding: "utf8", mode: 0o600 });
	// The mode above is masked by the umask and ignored for an existing file; set it explicitly.
	chmodSync(temporary, 0o600);
	renameSync(temporary, path);
}

export const JEV_KEY_FILE = "jev-api-key";

export function jevKeyPath(agentDir: string): string {
	return join(agentDir, JEV_KEY_FILE);
}

/** Where Jev's key comes from today: the environment wins over the file, as in `createNativeJevClient`. */
export function jevKeySource(agentDir: string, env: NodeJS.ProcessEnv): "env" | "file" | undefined {
	if (env.TYPESAFE_API_KEY?.trim()) return "env";
	try {
		return readFileSync(jevKeyPath(agentDir), "utf8").trim() ? "file" : undefined;
	} catch {
		return undefined;
	}
}

export function saveJevKey(agentDir: string, key: string): string {
	const trimmed = key.trim();
	if (!trimmed) throw new Error("The Jev API key is empty");
	if (/\s/.test(trimmed)) throw new Error("The Jev API key must not contain spaces or line breaks");
	const path = jevKeyPath(agentDir);
	writePrivateFile(path, `${trimmed}\n`);
	return path;
}

// ---------------------------------------------------------------------------------------------------------------
// Custom OpenAI-compatible endpoint (models.json)
// ---------------------------------------------------------------------------------------------------------------

export type CustomEndpointApi = "openai-completions" | "openai-responses";

export interface CustomEndpoint {
	readonly providerId: string;
	readonly baseUrl: string;
	/** Omitted for endpoints without authentication; a placeholder is written, as models.json expects one. */
	readonly apiKey?: string;
	readonly api: CustomEndpointApi;
	readonly modelIds: readonly string[];
}

const PROVIDER_ID = /^[a-z0-9][a-z0-9._-]*$/;

export function validateProviderId(value: string): string | undefined {
	return PROVIDER_ID.test(value) ? undefined : "Use lowercase letters, digits, '.', '_' or '-' (e.g. my-proxy)";
}

export function validateBaseUrl(value: string): string | undefined {
	if (value.split("://").length > 2) return "That looks like two URLs run together; enter one";
	try {
		const url = new URL(value.trim());
		if (url.protocol !== "http:" && url.protocol !== "https:") return "The URL must start with http:// or https://";
		return undefined;
	} catch {
		return "Not a valid URL (e.g. http://localhost:11434/v1)";
	}
}

export function parseModelIds(value: string): string[] {
	return [
		...new Set(
			value
				.split(/[\s,]+/)
				.map((id) => id.trim())
				.filter(Boolean),
		),
	];
}

/** The key written when the endpoint needs none: models.json makes a provider available only with a key. */
export const NO_KEY_PLACEHOLDER = "none";

export interface ModelsJsonMerge {
	/** The new models.json text. */
	readonly text: string;
	/** A provider of that id existed and is replaced. */
	readonly replaced: boolean;
}

/**
 * models.json with `endpoint` added as a provider (replacing one of the same id); other providers are kept as they
 * are. Throws for a file that is not a JSON object, so a broken file is never overwritten.
 */
export function mergeCustomEndpoint(existing: string | undefined, endpoint: CustomEndpoint): ModelsJsonMerge {
	let config: { providers?: Record<string, unknown>; [key: string]: unknown } = {};
	if (existing !== undefined && existing.trim() !== "") {
		let parsed: unknown;
		try {
			parsed = JSON.parse(stripJsonComments(stripBom(existing)));
		} catch (error) {
			throw new Error(`models.json is not valid JSON: ${error instanceof Error ? error.message : String(error)}`);
		}
		if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed))
			throw new Error("models.json must contain a JSON object");
		config = parsed as typeof config;
	}
	const providers =
		typeof config.providers === "object" && config.providers !== null && !Array.isArray(config.providers)
			? { ...config.providers }
			: {};
	const replaced = Object.hasOwn(providers, endpoint.providerId);
	providers[endpoint.providerId] = {
		baseUrl: endpoint.baseUrl.replace(/\/+$/, ""),
		api: endpoint.api,
		apiKey: endpoint.apiKey?.trim() ? endpoint.apiKey.trim() : NO_KEY_PLACEHOLDER,
		models: endpoint.modelIds.map((id) => ({ id })),
	};
	return { text: `${JSON.stringify({ ...config, providers }, null, 2)}\n`, replaced };
}

export function readTextFile(path: string): string | undefined {
	try {
		return readFileSync(path, "utf8");
	} catch (error) {
		if ((error as NodeJS.ErrnoException).code === "ENOENT") return undefined;
		throw error;
	}
}

export function customProviderExists(modelsJsonPath: string, providerId: string): boolean {
	const text = readTextFile(modelsJsonPath);
	if (text === undefined) return false;
	try {
		const parsed = JSON.parse(stripJsonComments(stripBom(text))) as { providers?: Record<string, unknown> };
		return typeof parsed.providers === "object" && parsed.providers !== null && providerId in parsed.providers;
	} catch {
		return false;
	}
}

/** Model ids an OpenAI-compatible endpoint lists at `GET <baseUrl>/models`; undefined when it does not answer. */
export async function listEndpointModels(
	baseUrl: string,
	apiKey: string | undefined,
	fetcher: typeof fetch = fetch,
	timeoutMs = 5_000,
): Promise<string[] | undefined> {
	try {
		const response = await fetcher(`${baseUrl.replace(/\/+$/, "")}/models`, {
			headers: apiKey?.trim() ? { authorization: `Bearer ${apiKey.trim()}` } : {},
			signal: AbortSignal.timeout(timeoutMs),
			redirect: "error",
		});
		if (!response.ok) return undefined;
		const body = (await response.json()) as { data?: unknown };
		if (!Array.isArray(body.data)) return undefined;
		return body.data
			.map((entry) => (typeof entry === "object" && entry !== null ? (entry as { id?: unknown }).id : undefined))
			.filter((id): id is string => typeof id === "string" && id.length > 0)
			.sort();
	} catch {
		return undefined;
	}
}

// ---------------------------------------------------------------------------------------------------------------
// Hindsight
// ---------------------------------------------------------------------------------------------------------------

/** Source: https://github.com/vectorize-io/hindsight (README "Quick Start", docs/developer/installation.md). */
export const HINDSIGHT_IMAGE = "ghcr.io/vectorize-io/hindsight:latest";
export const HINDSIGHT_CONTAINER = "hindsight";
export const HINDSIGHT_VOLUME = "hindsight-data";
export const HINDSIGHT_DOCS = "https://hindsight.vectorize.io/developer/installation";

export interface HindsightLlmChoice {
	/** `HINDSIGHT_API_LLM_PROVIDER`. */
	readonly provider: string;
	readonly label: string;
	readonly needsKey: boolean;
	readonly needsBaseUrl?: boolean;
	readonly defaultBaseUrl?: string;
}

/** A selection of the providers Hindsight documents for `HINDSIGHT_API_LLM_PROVIDER`. */
export const HINDSIGHT_LLM_PROVIDERS: readonly HindsightLlmChoice[] = [
	{ provider: "openai", label: "OpenAI (Hindsight's default)", needsKey: true },
	{ provider: "anthropic", label: "Anthropic", needsKey: true },
	{ provider: "gemini", label: "Google Gemini", needsKey: true },
	{ provider: "groq", label: "Groq", needsKey: true },
	{ provider: "openrouter", label: "OpenRouter", needsKey: true },
	{ provider: "deepseek", label: "DeepSeek", needsKey: true },
	{
		provider: "ollama",
		label: "Ollama (local)",
		needsKey: false,
		needsBaseUrl: true,
		defaultBaseUrl: "http://host.docker.internal:11434/v1",
	},
	{ provider: "openai", label: "Other OpenAI-compatible endpoint", needsKey: true, needsBaseUrl: true },
];

export interface HindsightDockerOptions {
	readonly port: number;
	readonly provider: string;
	readonly model?: string;
	readonly baseUrl?: string;
	/** Passed in the child's environment, never on the command line. */
	readonly hasApiKey: boolean;
	readonly image?: string;
	readonly name?: string;
	readonly volume?: string;
}

/**
 * `docker run` arguments for the documented single-container install. The API key is named without a value
 * (`-e HINDSIGHT_API_LLM_API_KEY`), so Docker copies it from the environment the command runs with: the key never
 * appears in the process list, the shell history or on screen.
 */
export function hindsightDockerArgs(options: HindsightDockerOptions): string[] {
	const args = [
		"run",
		"-d",
		"--name",
		options.name ?? HINDSIGHT_CONTAINER,
		"--restart",
		"unless-stopped",
		"-p",
		`${options.port}:8888`,
		"-e",
		`HINDSIGHT_API_LLM_PROVIDER=${options.provider}`,
	];
	if (options.hasApiKey) args.push("-e", "HINDSIGHT_API_LLM_API_KEY");
	if (options.model?.trim()) args.push("-e", `HINDSIGHT_API_LLM_MODEL=${options.model.trim()}`);
	if (options.baseUrl?.trim()) args.push("-e", `HINDSIGHT_API_LLM_BASE_URL=${options.baseUrl.trim()}`);
	// host.docker.internal is built in on Docker Desktop; Linux needs the host-gateway mapping.
	if (options.baseUrl?.includes("host.docker.internal")) args.push("--add-host", "host.docker.internal:host-gateway");
	args.push("-v", `${options.volume ?? HINDSIGHT_VOLUME}:/home/hindsight/.pg0`, options.image ?? HINDSIGHT_IMAGE);
	return args;
}

/** A command line for display: arguments with spaces are quoted. It never contains a secret (see above). */
export function formatCommand(command: string, args: readonly string[]): string {
	return [command, ...args]
		.map((part) => (/^[\w@%+=:,./-]+$/.test(part) ? part : `'${part.replace(/'/g, "'\\''")}'`))
		.join(" ");
}

/** The port a Hindsight URL listens on, for `-p <port>:8888`; 8888 when the URL is not local. */
export function hindsightPort(url: string): number {
	try {
		const parsed = new URL(url);
		if (parsed.port) return Number(parsed.port);
		return parsed.protocol === "https:" ? 443 : 80;
	} catch {
		return 8888;
	}
}

export function isLocalUrl(url: string): boolean {
	try {
		return ["localhost", "127.0.0.1", "::1", "[::1]"].includes(new URL(url).hostname);
	} catch {
		return false;
	}
}

/** The Hindsight URL Ultron uses: ULTRON_HINDSIGHT_URL, then the `hindsightUrl` setting, then localhost:8888. */
export function effectiveHindsightUrl(
	env: NodeJS.ProcessEnv,
	setting: string | undefined,
): { url: string | undefined; source: "env" | "setting" | "default" } {
	const fromEnv = env.ULTRON_HINDSIGHT_URL?.trim();
	const value = fromEnv || setting?.trim();
	const source = fromEnv ? "env" : setting?.trim() ? "setting" : "default";
	if (!value) return { url: DEFAULT_HINDSIGHT_URL, source };
	return { url: ["off", "none", "0", "false"].includes(value.toLowerCase()) ? undefined : value, source };
}

export type HindsightProbe = { readonly ok: true } | { readonly ok: false; readonly reason: string };

/** GET `<url>/health` (Hindsight's readiness endpoint). */
export async function probeHindsight(
	url: string,
	fetcher: typeof fetch = fetch,
	timeoutMs = 2_000,
): Promise<HindsightProbe> {
	try {
		const response = await fetcher(`${url.replace(/\/+$/, "")}/health`, {
			signal: AbortSignal.timeout(timeoutMs),
			redirect: "error",
		});
		if (response.ok) return { ok: true };
		return { ok: false, reason: `HTTP ${response.status}` };
	} catch (error) {
		return { ok: false, reason: describeFetchError(error) };
	}
}

/** "connection refused" rather than fetch's bare "fetch failed" (the cause may be an AggregateError without text). */
export function describeFetchError(error: unknown): string {
	const cause = error instanceof Error ? error.cause : undefined;
	if (cause instanceof Error) {
		const code = (cause as NodeJS.ErrnoException).code;
		const inner = cause instanceof AggregateError ? cause.errors.find((entry) => entry instanceof Error) : undefined;
		const innerCode = (inner as NodeJS.ErrnoException | undefined)?.code;
		const known = code ?? innerCode;
		if (known === "ECONNREFUSED") return "connection refused";
		if (known === "ENOTFOUND" || known === "EAI_AGAIN") return "host not found";
		if (cause.message) return cause.message;
		if (inner instanceof Error && inner.message) return inner.message;
		if (known) return known;
	}
	if (error instanceof Error && error.name === "TimeoutError") return "timed out";
	return error instanceof Error ? error.message : String(error);
}

/** Poll until Hindsight answers or `timeoutMs` passes (the first start downloads models and can take minutes). */
export async function waitForHindsight(
	url: string,
	options: {
		timeoutMs: number;
		intervalMs?: number;
		fetcher?: typeof fetch;
		signal?: AbortSignal;
		onAttempt?: (elapsedMs: number) => void;
	},
): Promise<boolean> {
	const started = Date.now();
	while (Date.now() - started < options.timeoutMs) {
		if (options.signal?.aborted) return false;
		if ((await probeHindsight(url, options.fetcher)).ok) return true;
		options.onAttempt?.(Date.now() - started);
		await new Promise((resolve) => setTimeout(resolve, options.intervalMs ?? 3_000));
	}
	return false;
}

/** Manual install steps, from Hindsight's README, for when Docker is missing or the user prefers to run it. */
export function hindsightManualInstructions(url: string): string[] {
	const port = hindsightPort(url);
	return [
		"Docker (recommended; the full image is several GB):",
		"  export OPENAI_API_KEY=sk-...",
		`  docker run -d --name ${HINDSIGHT_CONTAINER} --restart unless-stopped -p ${port}:8888 \\`,
		"    -e HINDSIGHT_API_LLM_API_KEY=$OPENAI_API_KEY \\",
		`    -v ${HINDSIGHT_VOLUME}:/home/hindsight/.pg0 ${HINDSIGHT_IMAGE}`,
		"",
		"Without Docker (Python 3.11 or newer):",
		"  pip install hindsight-api",
		"  export HINDSIGHT_API_LLM_PROVIDER=openai   # or anthropic, gemini, groq, ollama, ...",
		"  export HINDSIGHT_API_LLM_API_KEY=sk-...",
		`  hindsight-api${port === 8888 ? "" : ` --port ${port}`}`,
		"",
		`Other providers and options: ${HINDSIGHT_DOCS}`,
		"Then run `ultron setup` again to check the connection.",
	];
}

// ---------------------------------------------------------------------------------------------------------------
// First run
// ---------------------------------------------------------------------------------------------------------------

export interface SetupOfferInput {
	readonly stdinIsTTY: boolean;
	readonly stdoutIsTTY: boolean;
	/** The start runs the TUI: no `-p`, `--mode`, prompt, piped stdin or `@file`. */
	readonly interactive: boolean;
	/** A model or credential was named on the command line, or extensions were loaded with `-e`. */
	readonly explicitModel: boolean;
	readonly env: NodeJS.ProcessEnv;
	/** The user chose "Don't ask again". */
	readonly dismissed: boolean;
	/** Checked last, and only when everything else allows the offer (it loads the model catalog). */
	readonly hasUsableModel: () => Promise<boolean>;
}

/** Whether an interactive start should offer `ultron setup`. Never prompts without a terminal on both ends. */
export async function shouldOfferSetup(input: SetupOfferInput): Promise<boolean> {
	if (!input.stdinIsTTY || !input.stdoutIsTTY || !input.interactive) return false;
	if (input.explicitModel || input.dismissed) return false;
	const skip = input.env.ULTRON_SKIP_SETUP?.trim().toLowerCase();
	if (skip && !["0", "false", "no"].includes(skip)) return false;
	if (input.env.CI) return false;
	return !(await input.hasUsableModel());
}

export function fileExists(path: string): boolean {
	return existsSync(path);
}
