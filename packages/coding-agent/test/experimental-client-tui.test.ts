import { createHash } from "node:crypto";
import { resolve } from "node:path";
import {
	type AgentLane,
	type LaneSnapshot,
	type LaneTranscriptSnapshot,
	type LaneWatchEvent,
	reduceLaneSnapshot,
} from "@ultron/agent-core";
import {
	createRemoteServiceBinding,
	type JsonValue,
	type MutableReplicatedState,
	RemoteServiceProvider,
	type RemoteServiceTransport,
	replicatedState,
} from "@ultron/chord";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import {
	FACET_BUNDLE_ARTIFACT_FORMAT,
	FACET_BUNDLE_ARTIFACT_FORMAT_VERSION,
	type FacetBundleArtifact,
} from "@ultron/chord/node";
import { ProcessTerminal, TuiMainScreen } from "@ultron/tui";
import { beforeAll, describe, expect, test, vi } from "vitest";
import type { ExtensionUIContext } from "../src/core/extensions/types.ts";
import { type ClientTuiServer, ExperimentalClientTui } from "../src/experimental/client-tui.ts";
import { createPresentationFacetData } from "../src/experimental/plugins/bundled.ts";
import { AgentController } from "../src/experimental/services/agent-controller.ts";
import { createAgentController } from "../src/experimental/services/agent-controller-provider.ts";
import type {
	ServerConnectionState,
	ServerServiceSource,
	SessionAttachmentState,
	SessionServiceSource,
} from "../src/experimental/services/connection.ts";
import { ExtensionUI } from "../src/experimental/services/extension-ui.ts";
import { ExtensionUIBridge } from "../src/experimental/services/extension-ui-provider.ts";
import { LegacyExtensionCommands } from "../src/experimental/services/legacy-extensions.ts";
import { Models, type ModelsState } from "../src/experimental/services/models.ts";
import { PresentationPlugins, SessionPlugins } from "../src/experimental/services/plugins.ts";
import { SessionControl } from "../src/experimental/services/session-control.ts";
import {
	SessionDirectory,
	type SessionDirectoryState,
	SessionManagement,
	type SessionSummary,
} from "../src/experimental/services/sessions.ts";
import { Transcript, type TranscriptState } from "../src/experimental/services/transcript.ts";
import { initTheme } from "../src/modes/interactive/theme/theme.ts";

const serverId = "00000000-0000-4000-8000-000000000001";

function session(sessionId: string, createdAt: number): SessionSummary {
	return { serverId, sessionId, createdAt, modifiedAt: createdAt };
}

function createLoopbackServiceTransport(provider: RemoteServiceProvider): RemoteServiceTransport {
	return {
		invoke: (call, context) => provider.invoke(call, context),
		subscribe: async (serviceId, mode, listener) => {
			const subscription = provider.subscribe(serviceId, mode, (update) => listener(update, BACKGROUND_CONTEXT));
			return {
				snapshot: subscription.snapshot,
				activate: () => subscription.activate(),
				close: () => subscription.close(),
			};
		},
	};
}

function publishReplacement<T extends object>(state: MutableReplicatedState<T>, value: T): void {
	state.replace(BACKGROUND_CONTEXT, value);
}

const agentsStatusFixture: JsonValue = {
	definitions: [],
	tasks: [
		{ id: "ultron-task-aaaa1111", definition: "planner@1", state: "running" },
		{
			id: "ultron-task-bbbb2222",
			definition: "rlm-child@1",
			state: "completed",
			parentId: "ultron-task-aaaa1111",
			result: { status: "succeeded", value: "child answer", verification: "unverified" },
		},
		{
			id: "ultron-task-cccc3333",
			definition: "reviewer@1",
			state: "failed",
			parentId: "ultron-task-aaaa1111",
			result: { status: "failed", error: "deadline exceeded", verification: "unverified" },
		},
	],
	usage: { admittedTasks: 3, remainingWallMs: 600_000, usage: { cost: null }, reservations: [] },
	limits: { maxAdmittedTasks: 24, maxWallMs: 1_800_000 },
	controls: {},
};

function plain(lines: readonly string[]): string {
	return lines.join("\n").replace(/\u001b\[[0-9;]*m/g, "");
}

function inspectFixture(request: string): JsonValue {
	if (request === "agents.status") return agentsStatusFixture;
	if (request === "instances.list") return [{ task_id: "ultron-task-aaaa1111", state: "open", invocations: [] }];
	if (request === "rlm.pool") return { live: 2, maxLive: 16, lanes: [], evictions: 0 };
	if (request === "progress.assess") return { classification: "progressing", receipts: ["r1"] };
	if (request === "jev.decisions") {
		return {
			available: { jev: true, hindsight: false },
			capacity: 200,
			decisions: [
				{
					id: "jev-1",
					at: Date.now() - 2000,
					kind: "triage",
					status: "ok",
					durationMs: 40,
					inputSha256: "abcdef012345",
					inputChars: 5,
					route: "powerful",
					routeConfidence: 0.8,
					complexity: 1,
					urgency: "normal",
					category: "debugging",
				},
			],
		};
	}
	return null;
}

function laneSnapshot(): LaneSnapshot {
	return {
		lane: "main",
		transcript: [],
		tipId: null,
		configuration: {
			model: { provider: "test", modelId: "one" },
			thinkingLevel: "off",
			activeToolNames: [],
		},
		stats: {
			messageCount: 0,
			usage: {
				input: 0,
				output: 0,
				cacheRead: 0,
				cacheWrite: 0,
				totalTokens: 0,
				cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
			},
		},
		operation: null,
		queues: [],
		faulted: false,
	};
}

describe("experimental client TUI", () => {
	beforeAll(() => initTheme("dark"));

	test.each([
		["new", { command: "client" as const }, "two", 1],
		["continued", { command: "client" as const, continue: true }, "one", 0],
		["plugin-selected", { command: "client" as const, pluginPackages: ["./example-plugin"] }, "two", 1],
	] as const)(
		"opens a %s Session directly and exercises the full lifecycle only for a new Session",
		async (kind, command, sessionId, creates) => {
			const directoryState = replicatedState<SessionDirectoryState>({ revision: 1, sessions: [session("one", 1)] });
			const attachment = replicatedState<SessionAttachmentState>({ status: "detached" });
			const connectionState = replicatedState<ServerConnectionState>({ status: "connected", since: "now" });
			const modelsState = replicatedState<ModelsState>({
				catalog: {
					revision: 1,
					availableModels: [
						{ provider: "test", modelId: "one", name: "Model One", reasoning: false },
						{ provider: "test", modelId: "two", name: "Model Two", reasoning: true },
					],
				},
				configuration: { model: { provider: "test", modelId: "one" }, thinkingLevel: "off" },
				refresh: { status: "idle" },
			});
			const create = vi.fn(async () => {
				const created = session("two", 2);
				directoryState.change(BACKGROUND_CONTEXT, (draft) => {
					draft.revision = 2;
					draft.sessions.push(created);
				});
				return created;
			});
			const select = vi.fn(async (model: { provider: string; modelId: string }) => {
				modelsState.change(BACKGROUND_CONTEXT, (draft) => {
					draft.configuration.model = model;
				});
			});
			const selectThinking = vi.fn(async (thinkingLevel: "off" | "high") => {
				modelsState.change(BACKGROUND_CONTEXT, (draft) => {
					draft.configuration.thinkingLevel = thinkingLevel;
				});
			});
			const transcriptState = replicatedState<TranscriptState>({
				snapshot: laneSnapshot() as LaneTranscriptSnapshot,
				event: null,
			});
			const emitTranscriptEvent = (event: LaneWatchEvent): void => {
				transcriptState.change(BACKGROUND_CONTEXT, (draft) => {
					if (reduceLaneSnapshot(draft.snapshot as unknown as LaneSnapshot, event) === "rebase") {
						throw new Error("Test transcript event unexpectedly requires a rebase");
					}
					draft.event = event;
				});
			};
			let finishPrompt!: () => void;
			const promptFinished = new Promise<void>((resolve) => {
				finishPrompt = resolve;
			});
			const prompt = vi.fn(async () => {
				emitTranscriptEvent({
					type: "run_start",
					lane: "main",
					runId: "run-1",
					startedAt: 1,
				});
				await promptFinished;
				emitTranscriptEvent({
					type: "entry_added",
					lane: "main",
					entry: {
						id: "entry-user",
						parentId: null,
						seq: 1,
						timestamp: 1,
						type: "message",
						message: { role: "user", content: [{ type: "text", text: "hello" }], timestamp: 1 },
					},
				});
				emitTranscriptEvent({
					type: "entry_added",
					lane: "main",
					entry: {
						id: "entry-assistant",
						parentId: "entry-user",
						seq: 2,
						timestamp: 2,
						type: "message",
						message: {
							role: "assistant",
							content: [{ type: "text", text: "remote answer" }],
							provider: "test",
							model: "one",
							api: "test",
							usage: transcriptState.value.snapshot!.stats.usage,
							stopReason: "stop",
							timestamp: 2,
						},
					},
				});
				emitTranscriptEvent({
					type: "run_end",
					lane: "main",
					runId: "run-1",
					status: "completed",
					fromTipId: null,
					tipId: "entry-assistant",
					endedAt: 2,
				});
				return {
					ok: true as const,
					value: {
						operationId: "run-1",
						kind: "run" as const,
						status: "completed" as const,
						fromTipId: null,
						tipId: "entry-assistant",
						startedAt: 1,
						endedAt: 2,
					},
				};
			});

			const reloadSource =
				'"use strict";\nconst { defineFacet, defineService } = require("@ultron/chord");\nconst Models = defineService("pi.models");\nmodule.exports = { __esModule: true, default: defineFacet({ id: "test-tui-facet", setup(env) { env.use(Models); } }) };\n';
			const reloadArtifact: FacetBundleArtifact = {
				format: FACET_BUNDLE_ARTIFACT_FORMAT,
				formatVersion: FACET_BUNDLE_ARTIFACT_FORMAT_VERSION,
				plugin: { id: "test-tui-plugin" },
				entryName: "tui",
				entry: {
					file: "tui.cjs",
					integrity: `sha256-${createHash("sha256").update(reloadSource).digest("base64")}`,
					externalImports: ["@ultron/chord"],
				},
				source: reloadSource,
			};
			const reloadData = createPresentationFacetData([reloadArtifact]);
			const prepareSessionPlugins = vi.fn(async () => reloadData);
			const reloadPresentationPlugins = vi.fn(async () => reloadData);
			const reloadSessionPlugins = vi.fn(async () => {});
			const inspect = vi.fn(async (request: string) => inspectFixture(request));
			const serverProvider = new RemoteServiceProvider([SessionDirectory, SessionManagement, PresentationPlugins]);
			serverProvider.provide(SessionDirectory, { state: directoryState });
			serverProvider.provide(PresentationPlugins, {
				prepareSession: prepareSessionPlugins,
				reload: reloadPresentationPlugins,
			});
			serverProvider.provide(SessionManagement, {
				create,
				async remove() {},
				async rename() {},
				async attach(sessionId) {
					publishReplacement(attachment, { status: "attaching", sessionId });
				},
				async detach() {
					publishReplacement(attachment, { status: "detached" });
				},
			});
			const sessionProvider = new RemoteServiceProvider([
				Models,
				AgentController,
				SessionPlugins,
				Transcript,
				LegacyExtensionCommands,
				SessionControl,
				ExtensionUI,
			]);
			const extensionUIBridge = new ExtensionUIBridge();
			sessionProvider.provide(ExtensionUI, extensionUIBridge.service);
			sessionProvider.provide(SessionPlugins, { reload: reloadSessionPlugins });
			sessionProvider.provide(SessionControl, {
				getSettings: async () => ({
					name: null,
					steeringMode: "all",
					followUpMode: "all",
					autoCompaction: true,
					autoRetry: true,
				}),
				setName: async () => {},
				setSteeringMode: async () => {},
				setFollowUpMode: async () => {},
				setAutoCompaction: async () => {},
				setAutoRetry: async () => {},
				listCommands: async () => [],
				bash: async () => ({ output: "", exitCode: 0, cancelled: false, truncated: false, fullOutputPath: null }),
				abortBash: async () => {},
				inspect,
				readTree: async () => ({ entries: [], leafId: null, labels: {}, sessionFile: null }),
			});
			sessionProvider.provide(LegacyExtensionCommands, {
				list: async () => [],
				run: async () => ({ notifications: [] }),
			});
			sessionProvider.provide(Models, {
				state: modelsState,
				async cycleThinking() {},
				async getThinkingLevels() {
					return ["off", "high"];
				},
				async refresh() {},
				select,
				selectThinking,
			});
			sessionProvider.provide(AgentController, createAgentController({ prompt } as unknown as AgentLane));
			sessionProvider.provide(Transcript, { state: transcriptState });

			const serverNamespace = createRemoteServiceBinding({
				services: [SessionDirectory, SessionManagement, PresentationPlugins],
				transport: createLoopbackServiceTransport(serverProvider),
				bound: false,
			});
			const serverNamespaceReady = serverNamespace.ready.bind(serverNamespace);
			const serverServices: ServerServiceSource = Object.assign(serverNamespace, {
				acceptsUnavailableServices: false,
				connection: connectionState,
				async catalogue() {
					return serverProvider.catalogue;
				},
				open() {
					return {
						use: serverNamespace.use.bind(serverNamespace),
						observe: serverNamespace.observe.bind(serverNamespace),
						async ready() {
							await serverNamespace.rebind(true, BACKGROUND_CONTEXT);
							await serverNamespaceReady(BACKGROUND_CONTEXT);
						},
						async dispose() {},
					};
				},
				async ready() {
					await serverNamespace.rebind(true, BACKGROUND_CONTEXT);
					await serverNamespaceReady(BACKGROUND_CONTEXT);
				},
			});
			const sessionNamespace = createRemoteServiceBinding({
				services: [
					Models,
					AgentController,
					SessionPlugins,
					Transcript,
					LegacyExtensionCommands,
					SessionControl,
					ExtensionUI,
				],
				transport: createLoopbackServiceTransport(sessionProvider),
				bound: false,
			});
			const sessionServices: SessionServiceSource = Object.assign(sessionNamespace, {
				acceptsUnavailableServices: true,
				attachment,
				async catalogue() {
					return [];
				},
				open() {
					return {
						use: sessionNamespace.use.bind(sessionNamespace),
						observe: sessionNamespace.observe.bind(sessionNamespace),
						ready: sessionNamespace.ready.bind(sessionNamespace),
						async dispose() {},
					};
				},
				async whenAttached(sessionId: string) {
					await sessionNamespace.rebind(true, BACKGROUND_CONTEXT);
					await sessionNamespace.ready(BACKGROUND_CONTEXT);
					publishReplacement(attachment, { status: "attached", sessionId });
				},
				async whenDetached() {
					await sessionNamespace.rebind(false, BACKGROUND_CONTEXT);
					publishReplacement(attachment, { status: "detached" });
				},
			});
			const server: ClientTuiServer = {
				serverId,
				radius: true,
				server: serverServices,
				session: sessionServices,
			};
			let finished = false;
			const requestRender = vi.fn();
			const ui = new TuiMainScreen(new ProcessTerminal());
			const component = await ExperimentalClientTui.create({
				command,
				ui,
				servers: [server],
				requestRender,
				finish() {
					finished = true;
				},
			});
			try {
				expect(create).toHaveBeenCalledTimes(creates);
				expect(prepareSessionPlugins).toHaveBeenCalledWith(
					{
						sessionId,
						packagePaths:
							"pluginPackages" in command
								? command.pluginPackages.map((packagePath) => resolve(packagePath))
								: null,
					},
					expect.anything(),
				);
				expect(attachment.value).toEqual({ status: "attached", sessionId });
				expect(select).not.toHaveBeenCalled();
				expect(component.render(80).join("\n")).toContain(`Server: ${serverId}`);
				expect(component.render(80).join("\n")).toContain(`Session: ${sessionId}`);
				expect(component.render(80).join("\n")).toContain("test/one");
				expect(component.render(80).join("\n")).not.toContain("Experimental Sessions");
				expect(component.render(80).join("\n")).not.toContain("Experimental Models");

				// Startup selection is the only behavior specific to continue and plugin-selected Sessions.
				if (kind !== "new") return;

				component.handleInput("hello");
				component.handleInput("\r");
				await vi.waitFor(() => expect(prompt).toHaveBeenCalledWith("hello", undefined, BACKGROUND_CONTEXT));
				await vi.waitFor(() => expect(component.render(80).join("\n")).toContain("Working..."));
				finishPrompt();
				await vi.waitFor(() => expect(component.render(80).join("\n")).toContain("remote answer"));
				expect(component.render(80).join("\n")).toContain("hello");
				expect(component.render(80).join("\n")).not.toContain("Working...");
				expect(component.render(80).join("\n")).not.toContain("Operation run-1 completed");

				// The compact RLM summary shows in the footer while the panel is hidden and a task runs.
				await vi.waitFor(() =>
					expect(plain(component.render(80))).toContain("RLM ▸ 1 running · 1 done · 1 failed · 3/24 tasks"),
				);
				component.handleInput("/rlm");
				component.handleInput("\u001b");
				component.handleInput("\r");
				await vi.waitFor(() => {
					const panel = plain(component.render(80));
					expect(panel).toContain("planner@1 aaaa1111 running");
					expect(panel).toContain("◆ retained");
					expect(panel).toContain("progressing·1r");
					expect(panel).toContain("└─ ✓ rlm-child@1 bbbb2222 completed");
					expect(panel).toContain("✗ reviewer@1 cccc3333 failed deadline exceeded");
					expect(panel).toContain("kernels 2/16 live");
				});
				expect(plain(component.render(80))).not.toContain("RLM ▸");
				expect(inspect).toHaveBeenCalledWith("agents.status", {}, expect.anything());
				// ctrl+r hides the panel again; the footer summary returns.
				component.handleInput("\u0012");
				await vi.waitFor(() => {
					const hidden = plain(component.render(80));
					expect(hidden).not.toContain("planner@1");
					expect(hidden).toContain("RLM ▸");
				});

				// Jev: collapsed line while hidden, full decision panel via /jev, alt+j hides it again.
				await vi.waitFor(() => expect(plain(component.render(80))).toContain("Jev ▸ 1 triage"));
				component.handleInput("/jev");
				component.handleInput("\u001b");
				component.handleInput("\r");
				await vi.waitFor(() => {
					const panel = plain(component.render(100));
					expect(panel).toContain("jev ✓ configured · hindsight ✗ off");
					expect(panel).toContain("◇ triage → powerful 80% · debugging");
				});
				expect(plain(component.render(80))).not.toContain("Jev ▸");
				component.handleInput("\u001bj");
				await vi.waitFor(() => expect(plain(component.render(80))).not.toContain("jev ✓ configured"));

				component.handleInput("/reload");
				component.handleInput("\u001b");
				component.handleInput("\r");
				await vi.waitFor(() => {
					expect(reloadPresentationPlugins).toHaveBeenCalledOnce();
					expect(reloadSessionPlugins).toHaveBeenCalledOnce();
				});
				await vi.waitFor(() => expect(component.render(80).join("\n")).toContain("Reloaded plugins."));

				publishReplacement(attachment, { status: "detached" });
				publishReplacement(connectionState, {
					status: "disconnected",
					since: "later",
					reason: "network lost",
					retryAt: null,
				});
				await vi.waitFor(() => expect(component.render(80).join("\n")).toContain("retrying"));
				// Pi's Ctrl-C: the first press only clears the editor; a second press within 500 ms exits.
				component.handleInput("\u0003");
				expect(finished).toBe(false);
				component.handleInput("\u0003");
				expect(finished).toBe(true);
				finished = false;
				component.handleInput("\u0004");
				expect(finished).toBe(true);
				finished = false;
				publishReplacement(connectionState, { status: "connecting", attempt: 1 });
				publishReplacement(connectionState, { status: "connected", since: "reconnected" });
				publishReplacement(attachment, { status: "attached", sessionId });
				await vi.waitFor(() => expect(component.render(80).join("\n")).not.toContain("Reattaching"));

				component.handleInput("/model");
				component.handleInput("\u001b");
				component.handleInput("\r");
				await vi.waitFor(() => expect(component.render(80).join("\n")).toContain("Select model:"));
				component.handleInput("\u001b[B");
				component.handleInput("\r");
				await vi.waitFor(() =>
					expect(select).toHaveBeenCalledWith({ provider: "test", modelId: "two" }, expect.anything()),
				);
				expect(modelsState.value.configuration.model).toEqual({ provider: "test", modelId: "two" });
				await vi.waitFor(() => expect(component.render(80).join("\n")).not.toContain("Select model:"));

				component.handleInput("/thinking");
				component.handleInput("\u001b");
				component.handleInput("\r");
				await vi.waitFor(() => expect(component.render(80).join("\n")).toContain("Select thinking level:"));
				component.handleInput("\u001b[B");
				component.handleInput("\r");
				await vi.waitFor(() => expect(selectThinking).toHaveBeenCalledWith("high", expect.anything()));
				expect(modelsState.value.configuration.thinkingLevel).toBe("high");

				// Extension dialogs from the worker open the TUI selector and the answer flows back.
				const extensionContext = extensionUIBridge.createContext({} as ExtensionUIContext);
				await vi.waitFor(() => expect(extensionUIBridge.serving).toBe(true));
				const picked = extensionContext.select("Pick one", ["alpha", "beta"]);
				await vi.waitFor(() => expect(component.render(80).join("\n")).toContain("Pick one"));
				component.handleInput("\u001b[B");
				component.handleInput("\r");
				expect(await picked).toBe("beta");
				const confirmed = extensionContext.confirm("Proceed", "really?");
				await vi.waitFor(() => expect(component.render(80).join("\n")).toContain("Proceed: really?"));
				component.handleInput("\r");
				expect(await confirmed).toBe(true);
				extensionContext.notify("extension says hi", "info");
				await vi.waitFor(() => expect(component.render(80).join("\n")).toContain("extension says hi"));

				// Ctrl-C clears a draft without exiting; Ctrl-D does nothing on a non-empty editor; two quick
				// Ctrl-C presses exit.
				component.handleInput("draft");
				component.handleInput("\u0004");
				expect(finished).toBe(false);
				expect(component.render(80).join("\n")).toContain("draft");
				component.handleInput("\u0003");
				expect(finished).toBe(false);
				expect(component.render(80).join("\n")).not.toContain("draft");
				await new Promise((resolveWait) => setTimeout(resolveWait, 600));
				component.handleInput("\u0003");
				expect(finished).toBe(false);
				component.handleInput("\u0003");
				expect(finished).toBe(true);

				await component.close();
				const rendersAfterClose = requestRender.mock.calls.length;
				directoryState.change(BACKGROUND_CONTEXT, (draft) => {
					draft.revision = 3;
					draft.sessions = [];
				});
				publishReplacement(attachment, { status: "detached" });
				modelsState.change(BACKGROUND_CONTEXT, (draft) => {
					draft.refresh = { status: "refreshing" };
				});
				expect(requestRender).toHaveBeenCalledTimes(rendersAfterClose);
			} finally {
				await component.close();
				await Promise.all([
					serverNamespace.dispose(BACKGROUND_CONTEXT),
					sessionNamespace.dispose(BACKGROUND_CONTEXT),
				]);
				serverProvider.dispose();
				sessionProvider.dispose();
			}
		},
	);
});
