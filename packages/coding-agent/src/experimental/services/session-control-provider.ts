import type { AgentHarness, AgentLane, BashExecutionMessage, Session } from "@ultron/agent-core";
import type { Context, JsonValue } from "@ultron/chord";
import { getAgentDir, VERSION } from "../../config.ts";
import { executeBashWithOperations } from "../../core/bash-executor.ts";
import type { ModelRuntime } from "../../core/model-runtime.ts";
import type { SettingsManager } from "../../core/settings-manager.ts";
import { createLocalBashOperations } from "../../core/tools/bash.ts";
import { ProjectTrustStore } from "../../core/trust-manager.ts";
import { nativeSessionEntriesToPi } from "../../ultron/migration.ts";
import { REVIEW_COMMAND } from "../../ultron/review.ts";
import type { CommandSourceInfo, LegacyExtensionCommands } from "./legacy-extensions.ts";
import {
	INSPECTION_REQUESTS,
	type SessionCommandInfo,
	type SessionControl,
	type SessionDebugInfo,
} from "./session-control.ts";
import { applyWorkerSetting, readWorkerSettings, type WorkerSettingsTarget } from "./worker-settings.ts";

/** Pi's session lifecycle events in the worker's extensions. */
export interface ExtensionSessionEvents {
	beforeFork(entryId: string, position: "before" | "at"): Promise<{ cancelled: boolean }>;
	forked(targetSessionFile: string | undefined): void;
}

export function createSessionControl(options: {
	readonly harness: AgentHarness;
	readonly lane: AgentLane;
	readonly session?: Session;
	readonly cwd: string;
	readonly settingsManager?: SettingsManager;
	readonly extensionCommands?: LegacyExtensionCommands;
	/** Pi `SourceInfo` of prompt templates and skills by command name (`name`, `skill:name`). */
	readonly resourceSourceInfo?: () => ReadonlyMap<string, CommandSourceInfo>;
	readonly inspect?: (request: string, payload: Record<string, unknown>, context: Context) => Promise<unknown>;
	readonly extensionSessionEvents?: ExtensionSessionEvents;
	/** The worker's model runtime; `reloadAuth` rebuilds its availability from `auth.json`. */
	readonly modelRuntime?: ModelRuntime;
	/** The worker profile directory (default `getAgentDir()`). */
	readonly agentDir?: string;
	/** Reconfigure the HTTP stack for a changed idle timeout; injectable for tests. */
	readonly configureHttpIdleTimeout?: (timeoutMs: number) => void;
}): SessionControl {
	const { harness, lane } = options;
	const bashAborts = new Set<AbortController>();
	const agentDir = options.agentDir ?? getAgentDir();
	const startedAt = Date.now();
	const settingsTarget = (): WorkerSettingsTarget => {
		if (options.settingsManager === undefined) throw new Error("This Session worker has no settings");
		return {
			harness,
			lane,
			settingsManager: options.settingsManager,
			cwd: options.cwd,
			agentDir,
			...(options.configureHttpIdleTimeout === undefined
				? {}
				: { configureHttpIdleTimeout: options.configureHttpIdleTimeout }),
		};
	};
	return {
		async getSettings(context) {
			const [name, steeringMode, followUpMode, compaction, retry] = await Promise.all([
				harness.getName(context),
				harness.getSteeringMode(context),
				harness.getFollowUpMode(context),
				harness.getCompactionSettings(context),
				harness.getRetryPolicy(context),
			]);
			return {
				name: name ?? null,
				steeringMode,
				followUpMode,
				autoCompaction: compaction.enabled,
				autoRetry: retry.enabled,
			};
		},
		setName: (name, context) => harness.setName(name, context),
		setSteeringMode: (mode, context) => harness.setSteeringMode(mode, context),
		setFollowUpMode: (mode, context) => harness.setFollowUpMode(mode, context),
		async setAutoCompaction(enabled, context) {
			const settings = await harness.getCompactionSettings(context);
			await harness.setCompactionSettings({ ...settings, enabled }, context);
		},
		async setAutoRetry(enabled, context) {
			const policy = await harness.getRetryPolicy(context);
			await harness.setRetryPolicy({ ...policy, enabled }, context);
		},
		async listCommands(context) {
			const [extensionCommands, resources] = await Promise.all([
				options.extensionCommands?.list(context) ?? [],
				harness.getResources(context),
			]);
			const sources = options.resourceSourceInfo?.() ?? new Map<string, CommandSourceInfo>();
			const commands: SessionCommandInfo[] = extensionCommands.map((command) => ({
				name: command.name,
				description: command.description ?? null,
				source: "extension",
				sourceInfo: plainSourceInfo(command.sourceInfo),
			}));
			commands.push({
				name: REVIEW_COMMAND.name,
				description: REVIEW_COMMAND.description,
				source: "prompt",
				sourceInfo: null,
			});
			for (const template of resources.promptTemplates ?? []) {
				commands.push({
					name: template.name,
					description: template.description ?? null,
					source: "prompt",
					sourceInfo: plainSourceInfo(sources.get(template.name)),
				});
			}
			for (const skill of resources.skills ?? []) {
				const name = `skill:${skill.name}`;
				commands.push({
					name,
					description: skill.description ?? null,
					source: "skill",
					sourceInfo: plainSourceInfo(sources.get(name)),
				});
			}
			return commands;
		},
		async bash(command, excludeFromContext, context) {
			const abort = new AbortController();
			bashAborts.add(abort);
			const onContextAbort = () => abort.abort();
			context.abortSignal?.addEventListener("abort", onContextAbort, { once: true });
			try {
				const prefix = options.settingsManager?.getShellCommandPrefix();
				const result = await executeBashWithOperations(
					prefix ? `${prefix}\n${command}` : command,
					options.cwd,
					createLocalBashOperations({ shellPath: options.settingsManager?.getShellPath() }),
					{ signal: abort.signal },
				);
				const message: BashExecutionMessage = {
					role: "bashExecution",
					command,
					output: result.output,
					exitCode: result.exitCode,
					cancelled: result.cancelled,
					truncated: result.truncated,
					...(result.fullOutputPath === undefined ? {} : { fullOutputPath: result.fullOutputPath }),
					timestamp: Date.now(),
					...(excludeFromContext ? { excludeFromContext: true } : {}),
				};
				// During a run the lane queues this write and applies it at the next turn boundary.
				await lane.appendMessage(message, context);
				return {
					output: result.output,
					exitCode: result.exitCode ?? null,
					cancelled: result.cancelled,
					truncated: result.truncated,
					fullOutputPath: result.fullOutputPath ?? null,
				};
			} finally {
				context.abortSignal?.removeEventListener("abort", onContextAbort);
				bashAborts.delete(abort);
			}
		},
		async abortBash() {
			for (const abort of bashAborts) abort.abort();
		},
		async inspect(request, payload, context) {
			if (!INSPECTION_REQUESTS.includes(request)) throw new Error(`Not an inspection request: ${request}`);
			if (!options.inspect) throw new Error("This Session has no Ultron runtime to inspect");
			const body = payload === null ? {} : payload;
			if (typeof body !== "object" || Array.isArray(body)) throw new Error("Inspection payload must be an object");
			return (await options.inspect(request, body, context)) as JsonValue;
		},
		async readTree(context) {
			const session = options.session;
			if (session === undefined) throw new Error("This Session worker does not expose its storage");
			const converted = await nativeSessionEntriesToPi(session, context, lane.name);
			const path = (session.metadata as { path?: unknown }).path;
			return {
				entries: converted.entries as unknown as JsonValue[],
				leafId: converted.leafId,
				labels: Object.fromEntries(converted.labels),
				sessionFile: typeof path === "string" ? path : null,
			};
		},
		beforeFork: async (entryId, position) =>
			(await options.extensionSessionEvents?.beforeFork(entryId, position)) ?? { cancelled: false },
		async forked(targetSessionFile) {
			options.extensionSessionEvents?.forked(targetSessionFile ?? undefined);
		},
		readSettings: (context) => readWorkerSettings(settingsTarget(), context),
		setSetting: (key, value, context) => applyWorkerSetting(settingsTarget(), key, value, context),
		async setProjectTrust(updates) {
			new ProjectTrustStore(agentDir).setMany(updates.map(({ path, decision }) => ({ path, decision })));
		},
		async reloadAuth(providerId) {
			const runtime = options.modelRuntime;
			if (runtime === undefined) throw new Error("This Session worker has no model runtime");
			// Credentials are read from auth.json by revision; the refresh rebuilds which providers are configured.
			await runtime.refresh({ allowNetwork: false, ...(providerId === null ? {} : { providers: [providerId] }) });
			return { availableModels: runtime.getAvailableSnapshot().length };
		},
		async debugInfo(context) {
			const [model, thinkingLevel] = await Promise.all([lane.getModel(context), lane.getThinkingLevel(context)]);
			let kernelPool: JsonValue;
			try {
				kernelPool = options.inspect
					? ((await options.inspect("rlm.pool", {}, context)) as JsonValue)
					: { error: "no RLM runtime" };
			} catch (error) {
				kernelPool = { error: error instanceof Error ? error.message : String(error) };
			}
			const path = (options.session?.metadata as { path?: unknown } | undefined)?.path;
			const info: SessionDebugInfo = {
				pid: process.pid,
				parentPid: process.ppid,
				nodeVersion: process.version,
				version: VERSION,
				platform: `${process.platform}-${process.arch}`,
				cwd: options.cwd,
				agentDir,
				sessionFile: typeof path === "string" ? path : null,
				uptimeMs: Date.now() - startedAt,
				rssBytes: process.memoryUsage().rss,
				model: model === undefined ? null : `${model.provider}/${model.id}`,
				thinkingLevel,
				kernelPool: kernelPool ?? null,
				environment: debugEnvironment(process.env),
			};
			return info;
		},
		async setLabel(entryId, label, context) {
			const session = options.session;
			if (session === undefined) throw new Error("This Session worker does not expose its storage");
			await session.setLabel(entryId, label === null || label === "" ? undefined : label, context);
		},
	};
}

const SECRET_NAME = /KEY|TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL|AUTH|COOKIE|SESSION_KEY/iu;

/** `ULTRON_*` and `PI_*` variables for `/debug`, with values of secret-looking names redacted. */
export function debugEnvironment(env: NodeJS.ProcessEnv): Record<string, string> {
	return Object.fromEntries(
		Object.entries(env)
			.filter(([name, value]) => value !== undefined && /^(ULTRON|PI)_/u.test(name))
			.sort(([left], [right]) => left.localeCompare(right))
			.map(([name, value]) => [name, SECRET_NAME.test(name) ? "<redacted>" : (value as string)]),
	);
}

/** Service results must be plain JSON: drop the optional fields Pi leaves `undefined`. */
function plainSourceInfo(info: CommandSourceInfo | undefined): CommandSourceInfo | null {
	if (info === undefined) return null;
	return {
		path: info.path,
		source: info.source,
		scope: info.scope,
		origin: info.origin,
		...(info.baseDir === undefined ? {} : { baseDir: info.baseDir }),
	};
}
