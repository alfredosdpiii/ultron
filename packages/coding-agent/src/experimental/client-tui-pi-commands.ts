/**
 * The rest of Pi's interactive-mode commands for the native TUI: `/settings`, `/login`, `/logout`, `/share`,
 * `/resume`, `/import`, `/trust`, `/scoped-models` and `/debug`. Pi runs them in-process against its session; here
 * the Session lives in a worker, so reads and writes go through `SessionControl`, `Models` and the server's
 * `SessionManagement`, while terminal work (login dialogs, browser, `gh`) stays in the client.
 */

import { spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, join } from "node:path";
import type { Api, Model } from "@ultron/ai";
import type { JsonValue } from "@ultron/chord";
import { BACKGROUND_CONTEXT, withAbortSignal } from "@ultron/chord/context";
import { type Component, hyperlink, Spacer, Text, type TUI, visibleWidth } from "@ultron/tui";
import { APP_NAME, getDebugLogPath, getShareViewerUrl, VERSION } from "../config.ts";
import type { KeybindingsManager } from "../core/keybindings.ts";
import { resolveModelScopeFromModels } from "../core/model-resolver.ts";
import type { SettingsManager } from "../core/settings-manager.ts";
import { BorderedLoader } from "../modes/interactive/components/bordered-loader.ts";
import { ScopedModelsSelectorComponent } from "../modes/interactive/components/scoped-models-selector.ts";
import { TrustSelectorComponent } from "../modes/interactive/components/trust-selector.ts";
import { type TerminalTheme, theme } from "../modes/interactive/theme/theme.ts";
import { parseAuthStatus } from "../ultron/autoreview/accounts.ts";
import { installService, serviceFile, uninstallService } from "../ultron/autoreview/service.ts";
import { selfCommand } from "../ultron/claude/self.ts";
import { handleLogin, handleLogout, type LoginRuntime } from "./client-tui-auth.ts";
import { exportSession, type NativeCommandHost } from "./client-tui-commands.ts";
import { handleImport, showResumeSelector } from "./client-tui-sessions.ts";
import { type ClientSettingsMirror, changeSetting, showSettingsSelector } from "./client-tui-settings.ts";
import type { Models } from "./services/models.ts";
import type { SessionDebugInfo } from "./services/session-control.ts";
import type { SessionManagement } from "./services/sessions.ts";
import type { SlashCommandContribution } from "./services/slash-commands.ts";

/** Pi's theme controller, for `/settings` theme preview and apply. */
export interface ThemeHooks {
	getThemeSelection(): string | undefined;
	getTerminalTheme(): TerminalTheme;
	setThemeSetting(themeSetting: string): Promise<void>;
	preview(themeSettingOrName: string): void;
}

export interface CommandResult {
	readonly code: number | null;
	readonly stdout: string;
	readonly stderr: string;
	/** The command could not start (not installed). */
	readonly error?: Error;
}

/** Run a program; injectable so tests never call the real `gh`. */
export type CommandRunner = (command: string, args: readonly string[], signal?: AbortSignal) => Promise<CommandResult>;

/** Where the client reached its server, for `/debug`. */
export interface ClientServerInfo {
	readonly serverId: string | undefined;
	readonly transport: "unix" | "radius" | undefined;
	readonly socketPath: string | undefined;
}

/** What these commands need from the TUI beyond the basic command host. */
export interface PiCommandHost extends NativeCommandHost {
	readonly ui: TUI;
	readonly keybindings: KeybindingsManager;
	readonly settingsManager: SettingsManager;
	readonly settingsMirror: ClientSettingsMirror;
	/** The client profile directory: `/login` saves to its `auth.json`, `/debug` writes its log there. */
	readonly agentDir: string;
	/** `<agentDir>/auth.json`, where `/login` saves credentials. */
	readonly authPath: string;
	/** Pi's profile directory, read-only, for `/import`. */
	readonly piAgentDir: string;
	readonly theme: ThemeHooks | undefined;
	models(): Models | undefined;
	/** Show a component in the editor slot with the focus; returns its close function. */
	showComponent(component: Component, focus: Component, options?: { cancel?(): void; dispose?(): void }): () => void;
	select(title: string, options: readonly string[]): Promise<string | undefined>;
	requestRender(): void;
	/** Run `operation` with the server's `SessionManagement`. */
	withManagement<T>(operation: (management: SessionManagement) => Promise<T>): Promise<T>;
	/** Switch the TUI to another Session of the same server (as `/fork` and `/new` do). */
	switchSession(sessionId: string): Promise<void>;
	/** Apply a changed presentation setting to the running TUI (Pi's live settings). */
	applySettingLocally(key: string, value: JsonValue): void;
	/** Session-only Ctrl+P scope (Pi's `session.scopedModels`): undefined follows `enabledModels`, [] is all. */
	scopedModels(): readonly string[] | undefined;
	setScopedModels(ids: readonly string[] | undefined): void;
	loginRuntime(): Promise<LoginRuntime>;
	runCommand: CommandRunner;
	serverInfo(): ClientServerInfo;
	/** The TUI's rendered lines at the terminal width, for `/debug`. */
	renderedLines(): { readonly width: number; readonly height: number; readonly lines: readonly string[] };
}

export function piCommands(host: PiCommandHost): SlashCommandContribution[] {
	return [
		{
			name: "settings",
			description: "Open settings menu",
			run: () => void showSettingsSelector(host),
		},
		{
			name: "login",
			description: "Configure provider authentication",
			argumentHint: "<provider>",
			run: (args) => void handleLogin(host, args),
		},
		{
			name: "logout",
			description: "Remove provider authentication",
			run: () => void handleLogout(host),
		},
		{
			name: "share",
			description: "Share session as a secret GitHub gist",
			run: () => void shareSession(host),
		},
		{
			name: "resume",
			description: "Resume a different session",
			run: () => void showResumeSelector(host),
		},
		{
			name: "import",
			description: "Import a Pi session (JSONL) as a native session",
			argumentHint: "[path.jsonl]",
			run: (args) => void handleImport(host, args),
		},
		{
			name: "engineering",
			description: "Engineering mode: put the always-on skills (alwaysSkills) in the system prompt",
			argumentHint: "[true|false]",
			getArgumentCompletions: (prefix) =>
				["true", "false"].filter((value) => value.startsWith(prefix)).map((value) => ({ value, label: value })),
			run: (args) => void setEngineeringMode(host, args),
		},
		{
			name: "autoreview",
			description:
				"Automatic pull-request reviews as your gh account: on (picks the account, starts the service), off, or the status",
			argumentHint: "[on|off]",
			getArgumentCompletions: (prefix) =>
				["on", "off"].filter((value) => value.startsWith(prefix)).map((value) => ({ value, label: value })),
			run: (args) => void setAutoreview(host, args),
		},
		{
			name: "trust",
			description: "Save project trust decision for future sessions",
			run: () => void showTrustSelector(host),
		},
		{
			name: "scoped-models",
			description: "Enable/disable models for Ctrl+P cycling",
			run: () => void showScopedModelsSelector(host),
		},
		{
			name: "debug",
			description: "Write a debug log (TUI, Session worker and server state)",
			run: () => void writeDebugLog(host),
		},
	];
}

/** Where `/autoreview` puts the user service; tests point it at a temporary home. */
export interface AutoreviewToggleOptions {
	readonly platform?: NodeJS.Platform;
	readonly home?: string;
	readonly env?: NodeJS.ProcessEnv;
}

const ALL_ACCOUNTS = "All logged-in accounts";

/**
 * `/autoreview [on|off]`: automatic pull-request reviews. `on` lists the `gh` accounts, asks which one to review as
 * when there are several (saved as `autoreview.accounts`), saves `autoreview.enabled`, writes the user service and
 * starts it (systemd or launchd; elsewhere it says to run `ultron autoreview run`). `off` saves the setting (the
 * loop and the service then refuse to review), stops the service and removes its file. Alone: the status.
 */
export async function setAutoreview(
	host: PiCommandHost,
	args: string,
	options: AutoreviewToggleOptions = {},
): Promise<void> {
	const env = options.env ?? process.env;
	const platform = options.platform ?? process.platform;
	const target = { platform, env, ...(options.home === undefined ? {} : { home: options.home }) };
	const service = (() => {
		try {
			return serviceFile(selfCommand(env), target);
		} catch {
			return undefined;
		}
	})();
	const arg = args.trim().toLowerCase();
	if (arg === "") {
		const saved = host.settingsManager.getAutoreviewSettings();
		const as = saved.accounts?.length ? saved.accounts.join(", ") : "every logged-in gh account";
		const installed = service !== undefined && existsSync(service.path);
		host.showStatus(
			`Autoreview is ${saved.enabled === false ? "off" : "on"}: reviewing as ${as}; service ${installed ? `installed (${service!.path})` : "not installed"}. /autoreview on|off`,
		);
		return;
	}
	if (arg !== "on" && arg !== "off") {
		host.showStatus("Usage: /autoreview on|off");
		return;
	}
	const run = async (line: string): Promise<string | undefined> => {
		const [command, ...rest] = line.split(/\s+/);
		const result = await host.runCommand(command!, rest);
		if (result.error) return result.error.message;
		return result.code === 0 ? undefined : result.stderr.trim() || result.stdout.trim() || `exit ${result.code}`;
	};
	if (arg === "off") {
		if ((await changeSetting(host, "autoreview", { enabled: false }, { quiet: true })) === undefined) return;
		const notes: string[] = [];
		if (service !== undefined && existsSync(service.path)) {
			for (const line of service.disable) {
				const failed = await run(line);
				if (failed) notes.push(`\`${line}\` failed: ${failed}`);
			}
			uninstallService(service);
			notes.unshift(`service removed (${service.path})`);
		}
		host.showStatus(
			`Autoreview off: ${APP_NAME} autoreview run and its service review nothing${notes.length ? `; ${notes.join("; ")}` : ""}`,
		);
		return;
	}
	// on: the accounts gh is logged in to, one of them or all of them.
	const auth = await host.runCommand("gh", ["auth", "status", "--json", "hosts"]);
	if (auth.error) {
		host.showStatus("Autoreview needs the GitHub CLI (gh): install it and run gh auth login, then /autoreview on");
		return;
	}
	const accounts = parseAuthStatus(auth.stdout, auth.stderr);
	if (accounts.length === 0) {
		host.showStatus("No gh account is logged in: run gh auth login, then /autoreview on");
		return;
	}
	let chosen: string[] | null = null;
	if (accounts.length > 1) {
		const labels = accounts.map((account) => `${account.login} (${account.host})`);
		const choice = await host.select("Review pull requests as", [...labels, ALL_ACCOUNTS]);
		if (choice === undefined) {
			host.showStatus("Autoreview left as it was");
			return;
		}
		const index = labels.indexOf(choice);
		if (index >= 0) chosen = [`${accounts[index]!.host}/${accounts[index]!.login}`];
	}
	if ((await changeSetting(host, "autoreview", { enabled: true, accounts: chosen }, { quiet: true })) === undefined)
		return;
	const as = chosen
		? chosen[0]!
		: accounts.length === 1
			? `${accounts[0]!.host}/${accounts[0]!.login}`
			: "every logged-in account";
	if (service === undefined) {
		host.showStatus(
			`Autoreview on, reviewing as ${as}. No user service on ${platform}: run ${APP_NAME} autoreview run under your own supervisor`,
		);
		return;
	}
	installService(service);
	const failures: string[] = [];
	for (const line of service.enable) {
		const failed = await run(line);
		if (failed) failures.push(`\`${line}\` failed: ${failed}`);
	}
	host.showStatus(
		failures.length === 0
			? `Autoreview on, reviewing as ${as}: the service is started (${service.path}); it reviews pull requests that request or mention the account`
			: `Autoreview on, reviewing as ${as}; the service file is written (${service.path}) but could not be started: ${failures.join("; ")}. Start it by hand: ${service.enable.join(" && ")}`,
	);
}

/**
 * `/engineering [true|false]`: engineering mode puts the skills named in `alwaysSkills` in the system prompt, from the
 * next request on (the worker renders the prompt again). Without an argument it reports the mode.
 */
export async function setEngineeringMode(host: PiCommandHost, args: string): Promise<void> {
	const skills = host.settingsManager.getAlwaysSkills();
	const named = skills.length > 0 ? skills.join(", ") : "none: set alwaysSkills in settings.json";
	const arg = args.trim().toLowerCase();
	if (arg === "") {
		host.showStatus(
			`Engineering mode is ${host.settingsManager.getEngineering() ? "on" : "off"} (always-on skills: ${named})`,
		);
		return;
	}
	const enabled = ["true", "on", "1"].includes(arg) ? true : ["false", "off", "0"].includes(arg) ? false : undefined;
	if (enabled === undefined) {
		host.showStatus("Usage: /engineering true|false");
		return;
	}
	if ((await changeSetting(host, "engineering", enabled, { quiet: true })) === undefined) return;
	host.showStatus(
		enabled
			? `Engineering mode on: always-on skills (${named}) are in the system prompt from the next message`
			: "Engineering mode off: always-on skills are listed like other skills",
	);
}

/** Spawn a program and collect its output; a missing program reports `error`. */
export const spawnCommand: CommandRunner = (command, args, signal) =>
	new Promise((resolveResult) => {
		let stdout = "";
		let stderr = "";
		let settled = false;
		const done = (result: CommandResult): void => {
			if (settled) return;
			settled = true;
			resolveResult(result);
		};
		const child = spawn(command, [...args], { stdio: ["ignore", "pipe", "pipe"], signal });
		child.stdout?.on("data", (data: Buffer) => {
			stdout += data.toString();
		});
		child.stderr?.on("data", (data: Buffer) => {
			stderr += data.toString();
		});
		child.on("error", (error) => done({ code: null, stdout, stderr, error }));
		child.on("close", (code) => done({ code, stdout, stderr }));
	});

/**
 * Pi's `/share`: export the Session to HTML (the `/export` path) and upload it as a secret gist with `gh`, then show
 * the viewer and gist URLs. Pi's Radius artifact upload is not offered.
 */
export async function shareSession(host: PiCommandHost): Promise<void> {
	const auth = await host.runCommand("gh", ["auth", "status"]);
	if (auth.error !== undefined) {
		host.showStatus("Error: GitHub CLI (gh) is not installed. Install it from https://cli.github.com/");
		return;
	}
	if (auth.code !== 0) {
		host.showStatus("Error: GitHub CLI is not logged in. Run 'gh auth login' first.");
		return;
	}
	const view = await host.readSessionView();
	const sessionId = host.sessionId();
	if (view === undefined || sessionId === undefined) return;
	const directory = mkdtempSync(join(tmpdir(), `${APP_NAME}-share-`));
	try {
		let file: string;
		try {
			file = exportSession(view, sessionId, process.cwd(), join(directory, "session.html"));
		} catch (error) {
			host.showStatus(`Error: Failed to export session: ${error instanceof Error ? error.message : String(error)}`);
			return;
		}
		const loader = new BorderedLoader(host.ui, theme, "Creating gist...");
		let cancelled = false;
		const close = host.showComponent(loader, loader, {
			cancel: () => {
				cancelled = true;
			},
			dispose: () => loader.dispose(),
		});
		const abort = new AbortController();
		loader.onAbort = () => {
			cancelled = true;
			abort.abort();
			close();
			host.showStatus("Share cancelled");
		};
		const result = await host.runCommand("gh", ["gist", "create", "--public=false", file], abort.signal);
		close();
		if (cancelled) return;
		if (result.code !== 0) {
			host.showStatus(
				`Error: Failed to create gist: ${result.stderr.trim() || result.error?.message || "Unknown error"}`,
			);
			return;
		}
		const gistUrl = result.stdout.trim();
		const gistId = gistUrl.split("/").pop();
		if (!gistId) {
			host.showStatus("Error: Failed to parse gist ID from gh output");
			return;
		}
		const previewUrl = getShareViewerUrl(gistId);
		host.showStatus(`Share URL: ${hyperlink(previewUrl, previewUrl)}\nGist: ${hyperlink(gistUrl, gistUrl)}`);
	} finally {
		rmSync(directory, { recursive: true, force: true });
	}
}

/** Pi's `/trust`, against the worker's profile (`trust.json`) and the Session cwd. */
export async function showTrustSelector(host: PiCommandHost): Promise<void> {
	const control = host.control();
	if (control === undefined) {
		host.showStatus("Error: No Session is attached");
		return;
	}
	let read: Awaited<ReturnType<typeof control.readSettings>>;
	try {
		read = await control.readSettings(BACKGROUND_CONTEXT);
	} catch (error) {
		host.showStatus(`Error: ${error instanceof Error ? error.message : String(error)}`);
		return;
	}
	let close = (): void => {};
	const selector = new TrustSelectorComponent({
		cwd: read.cwd,
		savedDecision: read.savedTrust,
		projectTrusted: read.projectTrusted,
		onSelect: (selection) => {
			close();
			void control
				.setProjectTrust(selection.updates, BACKGROUND_CONTEXT)
				.then(() =>
					host.showStatus(
						`Saved trust decision: ${selection.trusted ? "trusted" : "untrusted"}. Restart ${APP_NAME} for this to take effect.`,
					),
				)
				.catch((error: unknown) =>
					host.showStatus(`Error: ${error instanceof Error ? error.message : String(error)}`),
				);
		},
		onCancel: () => {
			close();
			host.requestRender();
		},
	});
	close = host.showComponent(selector, selector);
}

/** Pi's `/scoped-models`: session scope for Ctrl+P; Ctrl+S saves it to the worker's `enabledModels`. */
export async function showScopedModelsSelector(host: PiCommandHost): Promise<void> {
	const models = host.models();
	if (models === undefined) {
		host.showStatus("Error: No Session is attached");
		return;
	}
	const records = (): Model<Api>[] =>
		(models.state.value?.catalog.availableModels ?? []).flatMap((model) =>
			model.model === undefined ? [] : [model.model],
		);
	let available = records();
	let availableIds = new Set(available.map((model) => `${model.provider}/${model.id}`));
	const configuredEnabledIds = (all: readonly Model<Api>[]): string[] | null => {
		const patterns = host.settingsManager.getEnabledModels();
		if (!patterns?.length) return null;
		const resolved = resolveModelScopeFromModels(patterns, all);
		const ids = resolved.scopedModels.map((scoped) => `${scoped.model.provider}/${scoped.model.id}`);
		for (const diagnostic of resolved.diagnostics) {
			if (diagnostic.code === "no-match" && !ids.includes(diagnostic.pattern)) ids.push(diagnostic.pattern);
		}
		return ids;
	};
	const sessionScope = host.scopedModels();
	let selectionChanged = false;
	const updateSessionModels = (enabledIds: string[] | null): void => {
		const hasAvailable = enabledIds?.some((id) => availableIds.has(id)) ?? false;
		const allEnabled = enabledIds !== null && [...availableIds].every((id) => enabledIds.includes(id));
		host.setScopedModels(enabledIds && hasAvailable && !allEnabled ? [...enabledIds] : []);
		host.requestRender();
	};
	const abort = new AbortController();
	const timeout = setTimeout(() => abort.abort(), 15_000);
	let close = (): void => {};
	let disposed = false;
	const selector = new ScopedModelsSelectorComponent(
		{
			allModels: available,
			enabledModelIds: sessionScope?.length ? [...sessionScope] : configuredEnabledIds(available),
			refreshStatus: "Refreshing model catalogs…",
		},
		{
			onChange: (enabledIds) => {
				selectionChanged = true;
				updateSessionModels(enabledIds);
			},
			onPersist: async (enabledIds) => {
				const allEnabled =
					enabledIds !== null &&
					enabledIds.length === available.length &&
					enabledIds.every((id) => availableIds.has(id));
				const patterns = enabledIds === null || allEnabled ? null : [...enabledIds];
				const applied = await changeSetting(host, "enabledModels", patterns, { quiet: true });
				if (applied !== undefined) host.showStatus("Model selection saved to settings");
			},
			onCancel: () => {
				close();
				host.requestRender();
			},
		},
	);
	close = host.showComponent(selector, selector, {
		dispose: () => {
			disposed = true;
			clearTimeout(timeout);
			abort.abort();
		},
	});
	// Pi refreshes the catalogs while the selector is open; here the worker refreshes and republishes them.
	try {
		await models.refresh(withAbortSignal(abort.signal, BACKGROUND_CONTEXT));
		if (disposed) return;
		available = records();
		availableIds = new Set(available.map((model) => `${model.provider}/${model.id}`));
		if (!selectionChanged && !sessionScope?.length) selector.updateModels(available, configuredEnabledIds(available));
		else selector.updateModels(available);
		const refresh = models.state.value?.refresh;
		if (refresh?.status === "warning") {
			selector.setRefreshStatus(
				`Could not refresh ${Object.keys(refresh.errors).join(", ")}; showing cached models.`,
				"warning",
			);
		} else selector.setRefreshStatus("Model catalogs refreshed.", "success");
	} catch (error) {
		if (disposed) return;
		selector.setRefreshStatus(
			abort.signal.aborted
				? "Model refresh timed out; showing cached models."
				: `Could not refresh model catalogs: ${error instanceof Error ? error.message : String(error)}`,
			"warning",
		);
	} finally {
		clearTimeout(timeout);
		host.requestRender();
	}
}

/**
 * Pi's `/debug`: the rendered TUI lines and the Session's messages, plus the client, server and Session worker
 * state (pids, directories, kernel pool, versions). Written to `<agentDir>/<app>-debug.log`; never credentials.
 */
export async function writeDebugLog(
	host: PiCommandHost,
	path = join(host.agentDir, basename(getDebugLogPath())),
): Promise<void> {
	const { width, height, lines } = host.renderedLines();
	let worker: SessionDebugInfo | { error: string };
	try {
		const control = host.control();
		if (control === undefined) throw new Error("No Session is attached");
		worker = await control.debugInfo(BACKGROUND_CONTEXT);
	} catch (error) {
		worker = { error: error instanceof Error ? error.message : String(error) };
	}
	const server = host.serverInfo();
	const serverDirectory = server.socketPath === undefined ? undefined : dirname(server.socketPath);
	const messages = (host.snapshot()?.transcript ?? []).flatMap((entry) =>
		entry.type === "message" ? [JSON.stringify(entry.message)] : [],
	);
	const summary = [
		`Session: ${host.sessionId() ?? "(none)"}`,
		`Server: ${server.serverId ?? "(none)"} (${server.transport ?? "unknown"})${serverDirectory ? ` in ${serverDirectory}` : ""}`,
		"error" in worker
			? `Worker: ${worker.error}`
			: `Worker: pid ${worker.pid} · ${worker.model ?? "no model"} · kernels ${JSON.stringify(worker.kernelPool)}`,
	];
	const data = [
		`Debug output at ${new Date().toISOString()}`,
		`Terminal: ${width}x${height}`,
		`Total lines: ${lines.length}`,
		"",
		"=== Client ===",
		`${APP_NAME} ${VERSION} · node ${process.version} · ${process.platform}-${process.arch} · pid ${process.pid}`,
		`cwd: ${process.cwd()}`,
		`Session: ${host.sessionId() ?? "(none)"}`,
		`Server id: ${server.serverId ?? "(none)"}`,
		`Server transport: ${server.transport ?? "unknown"}`,
		`Server socket: ${server.socketPath ?? "(n/a)"}`,
		`Server directory: ${serverDirectory ?? "(n/a)"}`,
		"",
		"=== Session worker ===",
		JSON.stringify(worker, null, 2),
		"",
		"=== All rendered lines with visible widths ===",
		...lines.map((line, index) => `[${index}] (w=${visibleWidth(line)}) ${JSON.stringify(line)}`),
		"",
		"=== Agent messages (JSONL) ===",
		...messages,
		"",
	].join("\n");
	try {
		mkdirSync(dirname(path), { recursive: true });
		writeFileSync(path, data, { mode: 0o600 });
	} catch (error) {
		host.showStatus(`Error: Could not write debug log: ${error instanceof Error ? error.message : String(error)}`);
		return;
	}
	const notice: Component[] = [
		new Spacer(1),
		new Text(`${theme.fg("accent", "✓ Debug log written")}\n${theme.fg("muted", path)}`, 1, 0),
		new Text(theme.fg("dim", summary.join("\n")), 1, 0),
	];
	host.notice({
		render: (renderWidth) => notice.flatMap((part) => part.render(renderWidth)),
		invalidate: () => {
			for (const part of notice) part.invalidate();
		},
	});
}
