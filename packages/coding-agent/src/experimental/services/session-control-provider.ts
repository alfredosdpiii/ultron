import type { AgentHarness, AgentLane, BashExecutionMessage, Session } from "@ultron/agent-core";
import type { Context, JsonValue } from "@ultron/chord";
import { executeBashWithOperations } from "../../core/bash-executor.ts";
import type { SettingsManager } from "../../core/settings-manager.ts";
import { createLocalBashOperations } from "../../core/tools/bash.ts";
import { nativeSessionEntriesToPi } from "../../ultron/migration.ts";
import type { CommandSourceInfo, LegacyExtensionCommands } from "./legacy-extensions.ts";
import { INSPECTION_REQUESTS, type SessionCommandInfo, type SessionControl } from "./session-control.ts";

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
}): SessionControl {
	const { harness, lane } = options;
	const bashAborts = new Set<AbortController>();
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
	};
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
