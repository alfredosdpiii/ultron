import { resolve } from "node:path";
import type { Context } from "@earendil-works/chord";
import {
	awaitWithContext,
	BACKGROUND_CONTEXT,
	withAbortSignal,
	withoutAbortSignal,
} from "@earendil-works/chord/context";
import type { LaneWatchEvent } from "@earendil-works/pi-agent-core";
import type { AssistantMessage } from "@earendil-works/pi-ai";
import type { ClientCommand } from "../cli/experimental/commands/client.ts";
import {
	type ActivatedClientRuntimeServer,
	activateBuiltinClientServices,
	openClientRuntime,
} from "./client-runtime.ts";
import type { AgentOperationResponse } from "./services/agent-controller.ts";
import type { SessionAddress } from "./services/sessions.ts";

export type ClientResult =
	| {
			readonly kind: "list";
			readonly sessions: readonly SessionAddress[];
	  }
	| { readonly kind: "attached"; readonly serverId: string; readonly sessionId: string }
	| { readonly kind: "prompted"; readonly serverId: string; readonly sessionId: string; readonly text: string };

export interface RunClientOptions {
	/** Directory searched when --connect is omitted. Defaults to ULTRON_SERVER_DIR or ~/.ultron/server. */
	readonly directory?: string;
	/** Session storage directory used when a server is automatically activated. */
	readonly sessionDir?: string;
	/** Create a child session by forking this session before prompting. */
	readonly forkFromSessionId?: string;
	/** Do not persist a newly created session. */
	readonly noSession?: boolean;
	/** Cancels service calls, terminal-event waiting, and client shutdown waiting. */
	readonly signal?: AbortSignal;
	/**
	 * Aborting this signal explicitly aborts the prompted turn (Pi's print mode stops its turn on SIGINT/SIGTERM).
	 * Unlike `signal`, which only stops waiting and leaves the turn running in the worker, the client keeps waiting
	 * until the aborted turn ends.
	 */
	readonly interrupt?: AbortSignal;
	/** Receives snapshot-ordered main-lane events while a prompt is active. */
	readonly onEvent?: (event: LaneWatchEvent) => void | Promise<void>;
}

/** Discover servers, then list Sessions, attach to one, or create one for a prompt. */
export async function runClient(command: ClientCommand, options: RunClientOptions = {}): Promise<ClientResult> {
	const context =
		options.signal === undefined ? BACKGROUND_CONTEXT : withAbortSignal(options.signal, BACKGROUND_CONTEXT);
	const runtime = await awaitOperation(
		openClientRuntime(command, { directory: options.directory, sessionDir: options.sessionDir }),
		context,
	);
	let createdSession:
		| {
				readonly server: ActivatedClientRuntimeServer;
				readonly sessionId: string;
		  }
		| undefined;
	let result: ClientResult | undefined;
	let failed = false;
	let operationError: unknown;
	try {
		try {
			const discovered = await awaitOperation(
				Promise.all(runtime.servers.map((server) => activateBuiltinClientServices(server))),
				context,
			);
			result = await runClientOperation(command, options, context, discovered, (session) => {
				createdSession = session;
			});
		} catch (error: unknown) {
			failed = true;
			operationError = error;
		}

		const cleanupErrors: unknown[] = [];
		if (createdSession !== undefined && options.noSession === true) {
			try {
				await awaitOperation(
					createdSession.server.management.remove(createdSession.sessionId, withoutAbortSignal(context)),
					withoutAbortSignal(context),
				);
			} catch (error: unknown) {
				cleanupErrors.push(error);
			}
		}

		if (failed || options.signal?.aborted === true || cleanupErrors.length > 0) {
			for (const server of runtime.servers) {
				try {
					server.client.disconnect("Client operation finished");
				} catch (error: unknown) {
					cleanupErrors.push(error);
				}
			}
		}
		try {
			await awaitOperation(runtime.dispose(), context);
		} catch (error: unknown) {
			if (options.signal?.aborted !== true) cleanupErrors.push(error);
		}

		if (failed) {
			if (cleanupErrors.length > 0) {
				throw new AggregateError([operationError, ...cleanupErrors], "Client operation and cleanup failed");
			}
			throw operationError;
		}
		if (cleanupErrors.length > 0) {
			if (cleanupErrors.length === 1) throw cleanupErrors[0];
			throw new AggregateError(cleanupErrors, "Client cleanup failed");
		}
		return result!;
	} finally {
		if (failed) await runtime.dispose().catch(() => {});
	}
}

async function runClientOperation(
	command: ClientCommand,
	options: RunClientOptions,
	context: Context,
	discovered: readonly ActivatedClientRuntimeServer[],
	onCreated: (session: { readonly server: ActivatedClientRuntimeServer; readonly sessionId: string }) => void,
): Promise<ClientResult> {
	let sessionId = command.sessionId;
	if (sessionId === undefined && command.prompt === undefined) {
		return {
			kind: "list",
			sessions: discovered
				.flatMap(({ route, directory }) =>
					directory.state.value!.sessions.map(({ sessionId }) => ({ serverId: route.serverId, sessionId })),
				)
				.sort(
					(left, right) =>
						left.serverId.localeCompare(right.serverId) || left.sessionId.localeCompare(right.sessionId),
				),
		};
	}

	let match: ActivatedClientRuntimeServer;
	if (sessionId === undefined && (command.continue === true || command.resume === true)) {
		const candidates = discovered
			.flatMap((candidate) => candidate.directory.state.value!.sessions.map((summary) => ({ candidate, summary })))
			.sort(
				(left, right) =>
					right.summary.modifiedAt - left.summary.modifiedAt ||
					left.summary.serverId.localeCompare(right.summary.serverId) ||
					left.summary.sessionId.localeCompare(right.summary.sessionId),
			);
		const selected = candidates[0];
		if (selected !== undefined) {
			match = selected.candidate;
			sessionId = selected.summary.sessionId;
		}
	}
	if (sessionId === undefined) {
		if (discovered.length !== 1) {
			throw new Error("Client prompt requires exactly one discovered server to create a Session");
		}
		match = discovered[0]!;
		const created = await awaitOperation(
			match.management.create(
				{
					...(options.forkFromSessionId === undefined ? {} : { forkFromSessionId: options.forkFromSessionId }),
					...(command.name === undefined ? {} : { name: command.name }),
				},
				context,
			),
			context,
		);
		sessionId = created.sessionId;
		onCreated({ server: match, sessionId });
	} else {
		const selectedSessionId = sessionId;
		const matches = discovered.filter((candidate) =>
			candidate.directory.state.value!.sessions.some(({ sessionId }) => sessionId === selectedSessionId),
		);
		if (matches.length > 1) {
			throw new Error(`Session ${selectedSessionId} is available from more than one server`);
		}
		const existing = matches[0];
		if (existing) {
			if (options.forkFromSessionId !== undefined) {
				throw new Error(`Cannot fork into existing Session ${selectedSessionId}`);
			}
			match = existing;
			if (command.name !== undefined) {
				await awaitOperation(match.management.rename(selectedSessionId, command.name, context), context);
			}
		} else {
			if (command.connect?.transport === "radius" || command.prompt === undefined || discovered.length !== 1) {
				throw new Error(`No discovered server contains session ${selectedSessionId}`);
			}
			match = discovered[0]!;
			await awaitOperation(
				match.management.create(
					{
						id: selectedSessionId,
						...(options.forkFromSessionId === undefined ? {} : { forkFromSessionId: options.forkFromSessionId }),
						...(command.name === undefined ? {} : { name: command.name }),
					},
					context,
				),
				context,
			);
			onCreated({ server: match, sessionId: selectedSessionId });
		}
	}

	await awaitOperation(
		match.plugins.prepareSession(
			{
				sessionId,
				packagePaths: command.pluginPackages?.map((packagePath) => resolve(packagePath)) ?? null,
			},
			context,
		),
		context,
	);
	await awaitOperation(match.management.attach(sessionId, context), context);
	if (command.prompt === undefined) {
		return { kind: "attached", serverId: match.route.serverId, sessionId };
	}

	const agent = match.agent;
	const completedText = new Map<string, string>();
	const operationBoundaries = new Set<string>();
	const boundaryWaiters = new Map<string, () => void>();
	// An interrupt aborts the turn this prompt started, as soon as its operation id is known.
	let runningOperationId: string | undefined;
	let abortRequested: string | undefined;
	const abortIfInterrupted = (): void => {
		if (options.interrupt?.aborted !== true || runningOperationId === undefined) return;
		if (abortRequested === runningOperationId) return;
		abortRequested = runningOperationId;
		void agent.requestAbort(runningOperationId, withoutAbortSignal(context)).catch(() => {});
	};
	let deliveryTail = Promise.resolve();
	const unsubscribe = match.transcript.state.subscribe((value, _context, delivery) => {
		if (delivery.kind !== "update" || value.event === null) return;
		const event = value.event;
		deliveryTail = deliveryTail.then(async () => {
			if (event.type === "message_end" && event.runId !== undefined && event.message.role === "assistant") {
				completedText.set(event.runId, messageText(event.message));
			}
			await options.onEvent?.(event);
		});
		if (event.type === "run_start") {
			runningOperationId = event.runId;
			abortIfInterrupted();
		}
		if (event.type === "run_end" || event.type === "run_suspend") {
			if (runningOperationId === event.runId) runningOperationId = undefined;
			operationBoundaries.add(event.runId);
			boundaryWaiters.get(event.runId)?.();
			boundaryWaiters.delete(event.runId);
		}
	});
	options.interrupt?.addEventListener("abort", abortIfInterrupted, { once: true });
	if (match.transcript.state.value?.snapshot === null || match.transcript.state.value?.snapshot === undefined) {
		unsubscribe();
		throw new Error("Transcript has no initialized snapshot");
	}

	let response: AgentOperationResponse | undefined;
	let promptFailed = false;
	let promptError: unknown;
	try {
		response = await awaitOperation(
			agent.prompt({ message: command.prompt, images: command.images ?? null }, context),
			context,
		);
		if (response.accepted) {
			const operationId = response.operationId;
			if (!operationBoundaries.has(operationId)) {
				await awaitOperation(
					new Promise<void>((resolveBoundary) => boundaryWaiters.set(operationId, resolveBoundary)),
					context,
				);
			}
		}
	} catch (error: unknown) {
		promptFailed = true;
		promptError = error;
	} finally {
		options.interrupt?.removeEventListener("abort", abortIfInterrupted);
		unsubscribe();
		try {
			await awaitOperation(deliveryTail, context);
		} catch (error: unknown) {
			if (!promptFailed) {
				promptFailed = true;
				promptError = error;
			}
		}
	}
	if (promptFailed) throw promptError;
	if (response === undefined) throw new Error("Agent prompt returned no response");
	if (!response.accepted) throw new Error(response.error.message);
	if (response.error !== null) throw new Error(response.error.message);
	return {
		kind: "prompted",
		serverId: match.route.serverId,
		sessionId,
		text: completedText.get(response.operationId) ?? "",
	};
}

function awaitOperation<T>(promise: Promise<T>, context: Context): Promise<T> {
	// Keep a cancelled operation's eventual rejection observed while the caller stops waiting.
	void promise.catch(() => {});
	return awaitWithContext(promise, context);
}

function messageText(message: AssistantMessage): string {
	return message.content
		.filter((content) => content.type === "text")
		.map((content) => content.text)
		.join("");
}
