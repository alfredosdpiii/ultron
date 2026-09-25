/**
 * Pi-compatible RPC mode on the native Ultron runtime.
 *
 * Speaks the same JSONL protocol as `modes/rpc/rpc-mode.ts`, but every command goes through
 * the server/worker services, so prompts run on the native RLM worker.
 */

import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { type Context, isJsonValue } from "@earendil-works/chord";
import { BACKGROUND_CONTEXT } from "@earendil-works/chord/context";
import type {
	AgentMessage,
	Entry,
	LaneTranscriptSnapshot,
	LaneWatchEvent,
	ThinkingLevel,
} from "@earendil-works/pi-agent-core";
import type { ImageContent } from "@earendil-works/pi-ai";
import { flushRawStdout, takeOverStdout, waitForRawStdoutBackpressure, writeRawStdout } from "../core/output-guard.ts";
import { attachJsonlLineReader, serializeJsonLine } from "../modes/rpc/jsonl.ts";
import { writeNativeSessionHtml } from "../ultron/native-export.ts";
import {
	type ActivatedClientRuntimeServer,
	activateBuiltinClientServices,
	openClientRuntime,
} from "./client-runtime.ts";
import { messageText, queueUpdate, RpcEventTranslator } from "./rpc-events.ts";
import type { ModelSummary } from "./services/models.ts";
import { SessionControl } from "./services/session-control.ts";
import { traceStartup } from "./startup-trace.ts";

export interface NativeRpcOptions {
	readonly sessionDir: string;
	readonly sessionId?: string;
	readonly continue?: boolean;
	readonly forkFromSessionId?: string;
	readonly noSession?: boolean;
	readonly name?: string;
}

type RpcCommand = { id?: string; type: string } & Record<string, unknown>;

class RpcCommandError extends Error {}

/** Run until stdin closes. The caller owns the foreground server lifetime. */
export async function runNativeRpcMode(options: NativeRpcOptions): Promise<void> {
	takeOverStdout();
	const output = (value: object): void => {
		writeRawStdout(serializeJsonLine(value));
	};
	const context = BACKGROUND_CONTEXT;
	traceStartup("rpc.open");
	const runtime = await openClientRuntime({ command: "client" }, { sessionDir: options.sessionDir });
	const cleanups: Array<() => Promise<void> | void> = [() => runtime.dispose()];
	const createdSessions = new Set<string>();
	try {
		if (runtime.servers.length !== 1) {
			throw new Error("RPC mode requires exactly one discovered server");
		}
		const server = await activateBuiltinClientServices(runtime.servers[0]!);
		const controlServices = server.session.open({ services: [SessionControl], assertAccess() {}, onError() {} });
		const control = controlServices.use(SessionControl);
		await controlServices.ready(context);

		traceStartup("rpc.services-ready");
		let sessionId = await selectInitialSession(server, options, context, createdSessions);
		traceStartup("rpc.session-selected");
		await attach(server, sessionId, context);
		traceStartup("rpc.attached");

		const translator = new RpcEventTranslator();
		const eventWaiters = new Set<(event: LaneWatchEvent) => void>();
		let deliveryTail = Promise.resolve();
		const unsubscribe = server.transcript.state.subscribe((value, _context, delivery) => {
			if (delivery.kind !== "update" || value.event === null) return;
			const event = value.event;
			for (const waiter of eventWaiters) waiter(event);
			deliveryTail = deliveryTail.then(async () => {
				for (const wireEvent of translator.translate(event)) output(wireEvent);
				await waitForRawStdoutBackpressure();
			});
		});
		cleanups.unshift(unsubscribe);

		const snapshot = (): LaneTranscriptSnapshot => {
			const value = server.transcript.state.value?.snapshot;
			if (value === null || value === undefined) throw new Error("Transcript has no initialized snapshot");
			return value;
		};
		const waitForEvent = (matches: (event: LaneWatchEvent) => boolean): Promise<LaneWatchEvent> =>
			new Promise((resolveEvent) => {
				const waiter = (event: LaneWatchEvent) => {
					if (!matches(event)) return;
					eventWaiters.delete(waiter);
					resolveEvent(event);
				};
				eventWaiters.add(waiter);
			});
		const switchTo = async (nextSessionId: string): Promise<void> => {
			await server.management.detach(context);
			sessionId = nextSessionId;
			await attach(server, sessionId, context);
		};
		const availableModels = (): ModelSummary[] => server.models.state.value?.catalog.availableModels ?? [];
		const currentModel = (): ModelSummary | undefined => {
			const selected = server.models.state.value?.configuration.model;
			if (!selected) return undefined;
			return availableModels().find(
				(model) => model.provider === selected.provider && model.modelId === selected.modelId,
			);
		};

		const handle = async (command: RpcCommand): Promise<object | undefined> => {
			const id = command.id;
			const ok = (data?: unknown) =>
				data === undefined
					? { id, type: "response", command: command.type, success: true }
					: { id, type: "response", command: command.type, success: true, data };
			switch (command.type) {
				case "prompt": {
					const request = promptRequest(command);
					const running = snapshot().operation?.kind === "run";
					if (running) {
						const behavior = command.streamingBehavior;
						if (behavior !== "steer" && behavior !== "followUp") {
							throw new RpcCommandError(
								"Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message.",
							);
						}
						const queued =
							behavior === "steer"
								? await server.agent.steer(request, context)
								: await server.agent.followUp(request, context);
						if (!queued.accepted) throw new RpcCommandError(queued.error.message);
						return ok();
					}
					// The controller resolves only when the run ends; Pi acknowledges once the run starts.
					const started = waitForEvent((event) => event.type === "run_start");
					const completion = server.agent.prompt(request, context);
					const first = await Promise.race([
						started.then(() => undefined),
						completion.then((response) => response),
					]);
					if (first !== undefined && !first.accepted) throw new RpcCommandError(first.error.message);
					void completion.catch(() => {});
					return ok();
				}
				case "steer":
				case "follow_up": {
					const request = promptRequest(command);
					const queued =
						command.type === "steer"
							? await server.agent.steer(request, context)
							: await server.agent.followUp(request, context);
					if (!queued.accepted) throw new RpcCommandError(queued.error.message);
					return ok();
				}
				case "abort": {
					const operation = snapshot().operation;
					if (operation) await server.agent.requestAbort(operation.id, context);
					return ok();
				}
				case "clear_queue": {
					const queues = snapshot().queues;
					const cleared = queueUpdate(queues);
					for (const item of queues) await server.agent.cancelQueued(item.entryId, context);
					return ok({ steering: cleared.steering, followUp: cleared.followUp });
				}
				case "new_session": {
					const created = await server.management.create({}, context);
					if (options.noSession) createdSessions.add(created.sessionId);
					await switchTo(created.sessionId);
					return ok({ cancelled: false });
				}
				case "get_state": {
					const current = snapshot();
					const settings = await control.getSettings(context);
					return ok({
						model: currentModel() === undefined ? undefined : rpcModel(currentModel()!),
						thinkingLevel: server.models.state.value?.configuration.thinkingLevel ?? "off",
						isStreaming: current.operation?.kind === "run",
						isCompacting: current.operation?.kind === "compaction",
						steeringMode: settings.steeringMode,
						followUpMode: settings.followUpMode,
						sessionId,
						...(settings.name === null ? {} : { sessionName: settings.name }),
						autoCompactionEnabled: settings.autoCompaction,
						messageCount: messages(current.transcript).length,
						pendingMessageCount: current.queues.length,
					});
				}
				case "set_model": {
					const model = availableModels().find(
						(candidate) => candidate.provider === command.provider && candidate.modelId === command.modelId,
					);
					if (!model) throw new RpcCommandError(`Model not found: ${command.provider}/${command.modelId}`);
					await server.models.select({ provider: model.provider, modelId: model.modelId }, context);
					return ok(rpcModel(model));
				}
				case "cycle_model": {
					const models = availableModels();
					if (models.length < 2) return ok(null);
					const current = currentModel();
					const index = current === undefined ? -1 : models.indexOf(current);
					const next = models[(index + 1) % models.length]!;
					await server.models.select({ provider: next.provider, modelId: next.modelId }, context);
					return ok({
						model: rpcModel(next),
						thinkingLevel: server.models.state.value?.configuration.thinkingLevel ?? "off",
						isScoped: false,
					});
				}
				case "get_available_models":
					return ok({ models: availableModels().map(rpcModel) });
				case "set_thinking_level":
					await server.models.selectThinking(command.level as ThinkingLevel, context);
					return ok();
				case "cycle_thinking_level": {
					const levels = await server.models.getThinkingLevels(context);
					if (levels.length < 2) return ok(null);
					await server.models.cycleThinking(context);
					return ok({ level: server.models.state.value?.configuration.thinkingLevel });
				}
				case "get_available_thinking_levels":
					return ok({ levels: await server.models.getThinkingLevels(context) });
				case "set_steering_mode":
					await control.setSteeringMode(queueMode(command.mode), context);
					return ok();
				case "set_follow_up_mode":
					await control.setFollowUpMode(queueMode(command.mode), context);
					return ok();
				case "compact": {
					const customInstructions =
						typeof command.customInstructions === "string" ? command.customInstructions : null;
					const response = await server.agent.compact({ customInstructions }, context);
					if (!response.accepted) throw new RpcCommandError(response.error.message);
					const end = await waitForEvent(
						(event) => event.type === "compaction_end" && event.runId === response.operationId,
					);
					if (end.type !== "compaction_end" || end.status !== "completed") {
						const reason =
							end.type === "compaction_end" && end.status === "failed" ? end.error.message : undefined;
						throw new RpcCommandError(reason ?? "Compaction did not complete");
					}
					const entry = snapshot().transcript.find((candidate) => candidate.id === end.entryId);
					return ok(
						entry?.type === "compaction"
							? { summary: entry.summary, tokensBefore: entry.tokensBefore, entryId: entry.id }
							: { entryId: end.entryId },
					);
				}
				case "set_auto_compaction":
					await control.setAutoCompaction(command.enabled === true, context);
					return ok();
				case "set_auto_retry":
					await control.setAutoRetry(command.enabled === true, context);
					return ok();
				case "abort_retry": {
					const operation = snapshot().operation;
					if (operation?.retry) await server.agent.requestAbort(operation.id, context);
					return ok();
				}
				case "bash": {
					if (typeof command.command !== "string") throw new RpcCommandError("bash requires a command");
					const result = await control.bash(command.command, command.excludeFromContext === true, context);
					return ok({
						output: result.output,
						exitCode: result.exitCode ?? undefined,
						cancelled: result.cancelled,
						truncated: result.truncated,
						...(result.fullOutputPath === null ? {} : { fullOutputPath: result.fullOutputPath }),
					});
				}
				case "abort_bash":
					await control.abortBash(context);
					return ok();
				case "get_session_stats":
					return ok(snapshot().stats);
				case "switch_session": {
					if (typeof command.sessionPath !== "string")
						throw new RpcCommandError("switch_session requires sessionPath");
					await switchTo(await sessionIdFromSelector(command.sessionPath));
					return ok({ cancelled: false });
				}
				case "fork": {
					if (typeof command.entryId !== "string") throw new RpcCommandError("fork requires entryId");
					const entry = snapshot().transcript.find((candidate) => candidate.id === command.entryId);
					if (entry?.type !== "message" || entry.message.role !== "user") {
						throw new RpcCommandError(`Entry is not a user message: ${command.entryId}`);
					}
					const created = await server.management.create({ forkFromSessionId: sessionId }, context);
					await switchTo(created.sessionId);
					// A fork at a user message continues from the state just before that message.
					await navigate(entry.parentId);
					return ok({ text: messageText(entry.message), cancelled: false });
				}
				case "clone": {
					const created = await server.management.create({ forkFromSessionId: sessionId }, context);
					await switchTo(created.sessionId);
					return ok({ cancelled: false });
				}
				case "get_fork_messages":
					return ok({
						messages: snapshot().transcript.flatMap((entry) =>
							entry.type === "message" && entry.message.role === "user"
								? [{ entryId: entry.id, text: messageText(entry.message) }]
								: [],
						),
					});
				case "get_entries": {
					const current = snapshot();
					let entries = current.transcript;
					if (typeof command.since === "string") {
						const index = entries.findIndex((entry) => entry.id === command.since);
						if (index === -1) throw new RpcCommandError(`Entry not found: ${command.since}`);
						entries = entries.slice(index + 1);
					}
					return ok({ entries, leafId: current.tipId });
				}
				case "get_tree": {
					const current = snapshot();
					return ok({ tree: branchTree(current.transcript), leafId: current.tipId });
				}
				case "get_last_assistant_text": {
					const last = [...messages(snapshot().transcript)]
						.reverse()
						.find((message) => message.role === "assistant");
					return ok({ text: last === undefined ? null : messageText(last) });
				}
				case "set_session_name": {
					const name = typeof command.name === "string" ? command.name.trim() : "";
					if (!name) throw new RpcCommandError("Session name cannot be empty");
					await control.setName(name, context);
					return ok();
				}
				case "get_messages":
					return ok({ messages: messages(snapshot().transcript) });
				case "get_commands": {
					const commands = await control.listCommands(context);
					return ok({
						commands: commands.map((entry) => ({
							name: entry.name,
							...(entry.description === null ? {} : { description: entry.description }),
							source: entry.source,
						})),
					});
				}
				case "inspect": {
					// Ultron extension: the read-only inspector behind /agents, /memory why, /skills why, /experiments.
					if (typeof command.request !== "string") throw new RpcCommandError("inspect requires a request type");
					const payload = command.payload === undefined ? {} : command.payload;
					if (!isJsonValue(payload)) throw new RpcCommandError("inspect payload must be JSON");
					return ok(await control.inspect(command.request, payload, context));
				}
				case "export_html": {
					const current = snapshot();
					const summary = server.directory.state.value?.sessions.find((entry) => entry.sessionId === sessionId);
					const path = await writeNativeSessionHtml(
						{
							id: sessionId,
							cwd: process.cwd(),
							createdAt: summary?.createdAt ?? Date.now(),
							entries: current.transcript,
							tipId: current.tipId,
						},
						typeof command.outputPath === "string" ? command.outputPath : undefined,
					);
					return ok({ path });
				}
				default:
					throw new RpcCommandError(`Unknown command: ${command.type}`);
			}

			async function navigate(targetId: string | null): Promise<void> {
				const response = await server.agent.navigate(
					{ targetId, summarize: false, label: null, customInstructions: null },
					context,
				);
				if (!response.accepted) throw new RpcCommandError(response.error.message);
				await waitForEvent((event) => event.type === "navigation_end" && event.runId === response.operationId);
			}
		};

		// Commands run concurrently, as in Pi, so abort can interrupt a long compact or bash.
		const inFlight = new Set<Promise<void>>();
		await new Promise<void>((resolveInput) => {
			const detach = attachJsonlLineReader(process.stdin, (line) => {
				const handled = handleLine(line).finally(() => inFlight.delete(handled));
				inFlight.add(handled);
			});
			const onEnd = () => {
				detach();
				process.stdin.off("end", onEnd);
				resolveInput();
			};
			process.stdin.on("end", onEnd);
		});
		await Promise.allSettled([...inFlight]);
		await deliveryTail;

		async function handleLine(line: string): Promise<void> {
			let command: RpcCommand;
			try {
				command = JSON.parse(line) as RpcCommand;
			} catch (error) {
				output({
					type: "response",
					command: "parse",
					success: false,
					error: `Failed to parse command: ${error instanceof Error ? error.message : String(error)}`,
				});
				return;
			}
			// Native extensions run headless in the worker; there are no UI requests to answer.
			if (command.type === "extension_ui_response") return;
			try {
				const response = await handle(command);
				if (response) output(response);
			} catch (error) {
				output({
					id: command.id,
					type: "response",
					command: command.type,
					success: false,
					error: error instanceof Error ? error.message : String(error),
				});
			}
			await waitForRawStdoutBackpressure();
		}

		for (const created of createdSessions) {
			if (created === sessionId) await server.management.detach(context).catch(() => {});
			await server.management.remove(created, context).catch(() => {});
		}
	} finally {
		for (const cleanup of cleanups) await cleanup();
		await flushRawStdout();
	}
}

async function selectInitialSession(
	server: ActivatedClientRuntimeServer,
	options: NativeRpcOptions,
	context: Context,
	createdSessions: Set<string>,
): Promise<string> {
	const sessions = server.directory.state.value?.sessions ?? [];
	let existing: string | undefined;
	if (options.sessionId !== undefined) {
		existing = sessions.find((session) => session.sessionId === options.sessionId)?.sessionId;
		if (existing === undefined) {
			const created = await server.management.create(
				{ id: options.sessionId, ...(options.name === undefined ? {} : { name: options.name }) },
				context,
			);
			if (options.noSession) createdSessions.add(created.sessionId);
			return created.sessionId;
		}
	} else if (options.continue) {
		existing = [...sessions].sort(
			(left, right) => right.modifiedAt - left.modifiedAt || left.sessionId.localeCompare(right.sessionId),
		)[0]?.sessionId;
	}
	if (existing !== undefined) {
		if (options.name !== undefined) await server.management.rename(existing, options.name, context);
		return existing;
	}
	const created = await server.management.create(
		{
			...(options.forkFromSessionId === undefined ? {} : { forkFromSessionId: options.forkFromSessionId }),
			...(options.name === undefined ? {} : { name: options.name }),
		},
		context,
	);
	if (options.noSession) createdSessions.add(created.sessionId);
	return created.sessionId;
}

async function attach(server: ActivatedClientRuntimeServer, sessionId: string, context: Context): Promise<void> {
	await server.plugins.prepareSession({ sessionId, packagePaths: null }, context);
	await server.management.attach(sessionId, context);
}

/** Accept a native Session ID or a path to a native Session file. */
async function sessionIdFromSelector(selector: string): Promise<string> {
	if (!selector.includes("/") && !selector.includes("\\") && !selector.endsWith(".jsonl")) return selector;
	const header: unknown = JSON.parse((await readFile(resolve(selector), "utf8")).split("\n", 1)[0] ?? "");
	if (typeof header !== "object" || header === null || !("kind" in header) || header.kind !== "header") {
		throw new RpcCommandError(`Session file is not a valid ultron session: ${selector}`);
	}
	if (!("id" in header) || typeof header.id !== "string") {
		throw new RpcCommandError(`Session file has no ID: ${selector}`);
	}
	return header.id;
}

function promptRequest(command: RpcCommand): { message: string; images: ImageContent[] | null } {
	if (typeof command.message !== "string") throw new RpcCommandError(`${command.type} requires a message`);
	const images = Array.isArray(command.images) ? (command.images as ImageContent[]) : null;
	return { message: command.message, images: images && images.length > 0 ? images : null };
}

function queueMode(mode: unknown): "all" | "one-at-a-time" {
	if (mode !== "all" && mode !== "one-at-a-time") throw new RpcCommandError(`Invalid queue mode: ${String(mode)}`);
	return mode;
}

/** Pi clients read `provider` and `id`; native models do not replicate the full provider record. */
function rpcModel(model: ModelSummary): { provider: string; id: string; name: string; reasoning: boolean } {
	return { provider: model.provider, id: model.modelId, name: model.name, reasoning: model.reasoning };
}

function messages(entries: readonly Entry[]): AgentMessage[] {
	return entries.flatMap((entry) => (entry.type === "message" ? [entry.message] : []));
}

/** The replicated transcript holds only the active branch, so the tree is that single path. */
function branchTree(entries: readonly Entry[]): Array<{ entry: Entry; children: unknown[] }> {
	let children: Array<{ entry: Entry; children: unknown[] }> = [];
	for (const entry of [...entries].reverse()) {
		children = [{ entry, children }];
	}
	return children;
}
