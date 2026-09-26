import { resolve } from "node:path";
import {
	combineFacetLoaders,
	createFacetHost,
	defineFacet,
	type FacetHost,
	type FacetLoader,
	type JsonValue,
	type LoadedFacets,
} from "@ultron/chord";
import { awaitWithContext, BACKGROUND_CONTEXT, withAbortSignal } from "@ultron/chord/context";
import {
	CombinedAutocompleteProvider,
	type Component,
	Container,
	isFocusable,
	type SelectItem,
	SelectList,
	setKeybindings,
	Text,
	type TUI,
} from "@ultron/tui";
import type { ClientCommand } from "../cli/experimental/commands/client.ts";
import { getAgentDir } from "../config.ts";
import { KeybindingsManager } from "../core/keybindings.ts";
import { DefaultResourceLoader } from "../core/resource-loader.ts";
import { SettingsManager } from "../core/settings-manager.ts";
import { createChatViewport } from "../modes/interactive/chat-viewport.ts";
import { CustomEditor } from "../modes/interactive/components/custom-editor.ts";
import { ExtensionEditorComponent } from "../modes/interactive/components/extension-editor.ts";
import { ExtensionInputComponent } from "../modes/interactive/components/extension-input.ts";
import { TreeSelectorComponent } from "../modes/interactive/components/tree-selector.ts";
import { UserMessageSelectorComponent } from "../modes/interactive/components/user-message-selector.ts";
import { getEditorTheme, setRegisteredThemes, stopThemeWatcher, theme } from "../modes/interactive/theme/theme.ts";
import { InteractiveThemeController } from "../modes/interactive/theme/theme-controller.ts";
import { createInteractiveTui } from "../modes/interactive/tui-renderer.ts";
import type { RpcExtensionUIRequest } from "../modes/rpc/rpc-types.ts";
import { copyToClipboard } from "../utils/clipboard.ts";
import { ensureTool } from "../utils/tools-manager.ts";
import { type OpenClientRuntimeOptions, openClientRuntime } from "./client-runtime.ts";
import { ExperimentalChatView } from "./client-tui-chat.ts";
import { collectKnownMemories, type JevNoteSource, parseMemoryMessage } from "./jev-annotations.ts";
import {
	JEV_PULSE_MS,
	type JevSnapshot,
	parseJevDecisions,
	parseJevLedger,
	renderJevPanel,
	renderJevPresence,
} from "./jev-visualizer.ts";
import { PiSessionView } from "./pi-session-view.ts";
import { createPresentationFacetLoaders } from "./plugins/bundled.ts";
import { RlmGraphFocus } from "./rlm-focus.ts";
import { buildRlmGraph, graphActive, renderRlmDock, renderRlmFooter } from "./rlm-graph.ts";
import {
	extractRootCell,
	extractTurn,
	isActiveState,
	parseAgentsStatus,
	parseContextState,
	parseFrames,
	parsePool,
	parseProgress,
	parseRetained,
	RlmClock,
	type RlmContextState,
	type RlmFrame,
	type RlmJob,
	type RlmLimits,
	type RlmPool,
	type RlmProgress,
	type RlmSnapshot,
	type RlmStyle,
	type RlmTask,
	type RlmTiming,
	type RlmUsage,
} from "./rlm-visualizer.ts";
import { messageText } from "./rpc-events.ts";
import { AgentController, type AgentOperationResponse, type AgentQueueResponse } from "./services/agent-controller.ts";
import type {
	ServerConnectionState,
	ServerServiceSource,
	SessionAttachmentState,
	SessionServiceSource,
} from "./services/connection.ts";
import { ExtensionUI } from "./services/extension-ui.ts";
import { PresentationPlugins } from "./services/plugins.ts";
import { PresentationUI } from "./services/presentation-ui.ts";
import { SessionControl } from "./services/session-control.ts";
import { SessionDirectory, type SessionForkPath, SessionManagement, type SessionSummary } from "./services/sessions.ts";
import { SlashCommands } from "./services/slash-commands.ts";
import {
	createBuiltInSlashCommandsFacet,
	createSlashCommandsRuntimeFacet,
} from "./services/slash-commands-provider.ts";
import { Transcript, type Transcript as TranscriptService } from "./services/transcript.ts";

export interface RunClientTuiOptions extends OpenClientRuntimeOptions {
	readonly facetLoader?: FacetLoader;
}

export interface ClientTuiServer {
	readonly serverId: string;
	readonly radius: boolean;
	readonly server: ServerServiceSource;
	readonly session: SessionServiceSource;
}

interface SessionFeature {
	readonly serverId: string;
	readonly session: SessionServiceSource;
	readonly transcript: TranscriptService;
}

interface PreparedClientSession {
	readonly server: ClientTuiServer;
	readonly summary: SessionSummary;
	readonly presentationPlugins: JsonValue;
}

/** Pi exits on a second Ctrl-C within this window. */
const CTRL_C_EXIT_WINDOW_MS = 500;

/** Pi's double-Esc window. */
const DOUBLE_ESCAPE_WINDOW_MS = 500;

/** A component shown in the editor slot (Pi's `showSelector`), with the input focus. */
interface ActiveComponent {
	readonly component: Component;
	readonly focus: Component;
	/** Close it as if the user cancelled (a replacing dialog or closing the TUI). */
	cancel(): void;
	dispose?(): void;
}

interface PendingSelection {
	readonly title: string;
	readonly items: readonly SelectItem[];
	readonly selectedValue?: string;
	resolve(value: string | undefined): void;
}

const RLM_POLL_VISIBLE_MS = 1000;
const RLM_POLL_HIDDEN_MS = 5000;
const RLM_MAX_ASSESSED = 8;
/** Frame summaries fetched per poll: enough to fan out a wide `rlm.map` (the worker keeps at most 200). */
const RLM_FRAME_LIMIT = 200;
/** Redraw cadence for spinners and Jev's pulse, only while something animates. */
const ANIMATION_MS = 150;

const rlmStyle: RlmStyle = {
	fg: (color, text) => theme.fg(color, text),
	bold: (text) => theme.bold(text),
};

interface RlmPollState {
	tasks: RlmTask[];
	usage: RlmUsage | null;
	limits: RlmLimits | null;
	pool: RlmPool | null;
	retained: Set<string>;
	progress: Map<string, RlmProgress>;
	timing: Map<string, RlmTiming>;
	context?: RlmContextState | null;
	frames: RlmFrame[];
	jobs: RlmJob[];
	truncatedTasks?: number;
	error?: string;
}

const selectTheme = {
	selectedPrefix: (text: string) => theme.fg("accent", text),
	selectedText: (text: string) => theme.fg("accent", text),
	description: (text: string) => theme.fg("muted", text),
	scrollInfo: (text: string) => theme.fg("dim", text),
	noMatch: (text: string) => theme.fg("warning", text),
};

/** Service-only presentation driven by a replicated main-lane snapshot. */
export class ExperimentalClientTui implements Component {
	readonly #ui: TUI;
	readonly #requestRender: () => void;
	readonly #finish: () => void;
	readonly #documentContainer = new Container();
	readonly #sessionHeading = new Text("", 1, 0);
	readonly #pendingMessagesContainer = new Container();
	readonly #statusContainer = new Container();
	readonly #editorContainer = new Container();
	readonly #footerComponent = new Container();
	readonly #footerText = new Text("", 1, 0);
	readonly #rlmPanel: Component = {
		render: (width) => this.#renderRlm(width, "panel"),
		invalidate() {},
	};
	readonly #rlmFooterLine: Component = {
		render: (width) => this.#renderRlm(width, "footer"),
		invalidate() {},
	};
	readonly #jevPanel: Component = {
		render: (width) => this.#renderJev(width, "panel"),
		invalidate() {},
	};
	readonly #jevFooterLine: Component = {
		render: (width) => this.#renderJev(width, "footer"),
		invalidate() {},
	};
	#rlmVisible = false;
	#jevVisible = false;
	/** The full-screen RLM graph, while open. */
	#rlmFocus: RlmGraphFocus | undefined;
	/** Jev's transcript notes show every recalled memory instead of one line. */
	#jevNotesExpanded = false;
	/** Older workers reject `agents.status {graph: true}`; fall back to the plain listing. */
	#graphListing = true;
	#animationTimer: ReturnType<typeof setInterval> | undefined;
	#jevState: Omit<JevSnapshot, "now"> = { available: null, decisions: [] };
	#rlmTimer: ReturnType<typeof setInterval> | undefined;
	#rlmInFlight = false;
	#rlmQueued = false;
	readonly #rlmClock = new RlmClock();
	#rlmState: RlmPollState = {
		tasks: [],
		usage: null,
		limits: null,
		pool: null,
		retained: new Set(),
		progress: new Map(),
		timing: new Map(),
		frames: [],
		jobs: [],
	};
	#control: SessionControl | undefined;
	readonly #layoutRoot: Component;
	readonly #sharedFacets: LoadedFacets;
	readonly #keybindings = KeybindingsManager.create();
	#presentationFacets: LoadedFacets | undefined;
	#facetHost: FacetHost | undefined;
	#facetReloadTail = Promise.resolve();
	#session: SessionFeature | undefined;
	#slashCommands: SlashCommands | undefined;
	#controller: AgentController | undefined;
	readonly #chatInput: CustomEditor;
	#selectList: SelectList | undefined;
	#selection: PendingSelection | undefined;
	#active: ActiveComponent | undefined;
	#screen: "select" | "component" | "chat" = "chat";
	#lastEscapeTime = 0;
	readonly #settingsManager: SettingsManager;
	#server: ClientTuiServer | undefined;
	#reloadPresentationPlugins: ((data: JsonValue) => Promise<void>) | undefined;
	/** Bumped on a Session switch so the extension UI poll restarts against the new worker. */
	#uiGeneration = 0;
	#selectedServerId: string | undefined;
	#sessionId: string | undefined;
	#status = "Starting Session…";
	#busy = false;
	#lastCtrlCTime = 0;
	#closed = false;
	#closePromise: Promise<void> | undefined;
	#recoveryTransition: Promise<void> = Promise.resolve();
	#laneUnsubscribe: (() => void) | undefined;
	#chatView: ExperimentalChatView | undefined;
	readonly #fdPath: string | null;

	private constructor(
		ui: TUI,
		requestRender: () => void,
		finish: () => void,
		loadedFacets: LoadedFacets,
		fdPath: string | null,
		settingsManager: SettingsManager,
	) {
		this.#fdPath = fdPath;
		this.#settingsManager = settingsManager;
		this.#ui = ui;
		this.#requestRender = requestRender;
		this.#finish = finish;
		this.#sharedFacets = loadedFacets;
		setKeybindings(this.#keybindings);
		this.#chatInput = new CustomEditor(ui, getEditorTheme(), this.#keybindings, { paddingX: 1 });
		this.#chatInput.onSubmit = (message) => void this.#runPrompt(message);
		// Pi's keys: Esc aborts a running turn, Ctrl-C clears the editor and exits when pressed twice within 500 ms,
		// Ctrl-D exits on an empty editor. Exiting leaves a running turn to finish in the worker; only Esc aborts.
		// Idle, a double Esc on an empty editor opens the tree or fork selector (Pi's `doubleEscapeAction`).
		this.#chatInput.onEscape = () => this.#handleEscape();
		this.#chatInput.onCtrlD = finish;
		this.#chatInput.onAction("app.clear", () => this.#handleCtrlC());
		this.#chatInput.onAction("app.model.select", () => void this.#executeSlashCommand("model", ""));
		this.#chatInput.onAction("app.rlm.toggle", () => this.#toggleRlm());
		this.#chatInput.onAction("app.jev.toggle", () => this.#toggleJev());
		this.#chatInput.onAction("app.rlm.focus", () => this.#openRlmFocus());
		this.#chatInput.onAction("app.jev.notes.toggle", () => {
			this.#jevNotesExpanded = !this.#jevNotesExpanded;
			this.#layoutRoot.invalidate();
			this.#requestRender();
		});
		this.#chatInput.onAction("app.session.tree", () => void this.#showTreeSelector());
		this.#chatInput.onAction("app.session.fork", () => void this.#showUserMessageSelector());
		this.#chatInput.onAction("app.message.followUp", () => {
			const text = this.#chatInput.getText().trim();
			if (text.length === 0) return;
			this.#chatInput.setText("");
			void this.#queueFollowUp(text);
		});
		this.#editorContainer.addChild(this.#chatInput);
		this.#footerComponent.addChild(this.#rlmFooterLine);
		this.#footerComponent.addChild(this.#jevFooterLine);
		this.#footerComponent.addChild(this.#footerText);
		this.#layoutRoot = createChatViewport({
			document: this.#documentContainer,
			pendingMessages: this.#pendingMessagesContainer,
			status: this.#statusContainer,
			editor: this.#editorContainer,
			footer: this.#footerComponent,
			scrollbarTrackStyle: (text) => theme.fg("scrollbarTrack", text),
			scrollbarThumbStyle: (text) => theme.fg("scrollbarThumb", text),
		}).root;
		this.#rebuild();
	}

	static async create(options: {
		readonly command: ClientCommand;
		readonly ui: TUI;
		readonly servers: readonly ClientTuiServer[];
		readonly facetLoader?: FacetLoader;
		readonly fdPath?: string | null;
		/** Pi's settings (double-Esc action, tree filter, branch summary prompt, external editor). */
		readonly settingsManager?: SettingsManager;
		requestRender(): void;
		finish(): void;
	}): Promise<ExperimentalClientTui> {
		const prepared = await prepareClientSession(options.command, options.servers);
		const loadedFacets = await combineFacetLoaders(
			options.facetLoader === undefined ? [] : [options.facetLoader],
		).load();
		const component = new ExperimentalClientTui(
			options.ui,
			options.requestRender,
			options.finish,
			loadedFacets,
			options.fdPath ?? null,
			options.settingsManager ?? SettingsManager.inMemory(),
		);
		try {
			await component.#start(prepared);
			await component.#openPreparedSession(prepared);
			return component;
		} catch (error) {
			try {
				await component.close();
			} catch (cleanupError) {
				throw new AggregateError([error, cleanupError], "Experimental TUI startup and cleanup failed");
			}
			throw error;
		}
	}

	get layoutRoot(): Component {
		return this.#layoutRoot;
	}

	render(width: number): string[] {
		return [
			...this.#documentContainer.render(width),
			...this.#pendingMessagesContainer.render(width),
			...this.#statusContainer.render(width),
			...this.#editorContainer.render(width),
			...this.#footerComponent.render(width),
		];
	}

	handleInput(data: string): void {
		if (this.#busy) {
			if (this.#keybindings.matches(data, "app.clear")) this.#handleCtrlC();
			else if (this.#chatInput.getText().length === 0 && this.#keybindings.matches(data, "app.exit")) {
				this.#finish();
			}
			return;
		}
		if (this.#screen === "chat") {
			this.#chatInput.handleInput(data);
			this.#requestRender();
			return;
		}
		if (this.#screen === "component" && this.#active !== undefined) {
			this.#active.focus.handleInput?.(data);
			this.#requestRender();
			return;
		}
		this.#selectList?.handleInput(data);
	}

	invalidate(): void {
		this.#layoutRoot.invalidate();
	}

	dispose(): void {
		void this.close().catch(() => {});
	}

	refreshTheme(): void {
		const snapshot = this.#laneSnapshot();
		if (snapshot !== undefined) this.#chatView?.refreshTheme(snapshot);
		this.#rebuild();
	}

	showError(error: string): void {
		this.#status = `Error: ${error}`;
		this.#rebuild();
	}

	close(): Promise<void> {
		this.#closePromise ??= this.#close();
		return this.#closePromise;
	}

	async #start(prepared: PreparedClientSession): Promise<void> {
		const server = prepared.server;
		this.#server = server;
		let presentationFacets = await combineFacetLoaders(
			createPresentationFacetLoaders(prepared.presentationPlugins),
		).load();
		this.#presentationFacets = presentationFacets;
		let facetHost!: FacetHost;
		const reloadPresentationPlugins = (data: JsonValue): Promise<void> => {
			const operation = this.#facetReloadTail.then(async () => {
				const candidate = await combineFacetLoaders(createPresentationFacetLoaders(data)).load();
				try {
					await facetHost.reload(candidate.facets);
				} catch (error) {
					try {
						await candidate.dispose();
					} catch (cleanupError) {
						throw new AggregateError([error, cleanupError], "TUI plugin reload and cleanup failed");
					}
					throw error;
				}
				const retired = presentationFacets;
				presentationFacets = candidate;
				this.#presentationFacets = candidate;
				await retired.dispose();
			});
			this.#facetReloadTail = operation.catch(() => {});
			return operation;
		};
		this.#reloadPresentationPlugins = reloadPresentationPlugins;
		const presentationBridgeFacet = defineFacet({
			id: "@pi/presentation-bridge",
			setup: (env) => {
				env.provide(PresentationUI, {
					select: (title, items, selectedValue) =>
						this.#select(
							title,
							items.map((item) => ({ ...item })),
							selectedValue,
						),
					showStatus: (status) => {
						this.#status = status;
						this.#rebuild();
					},
				});
				const commands = env.use(SlashCommands);
				const controller = env.use(AgentController);
				const transcript = env.use(Transcript);
				const control = env.use(SessionControl);
				const extensionUI = env.use(ExtensionUI);
				const sessionFeature: SessionFeature = {
					serverId: server.serverId,
					session: server.session,
					transcript,
				};
				env.onActivate(() => {
					if (this.#session !== undefined || this.#slashCommands !== undefined || this.#controller !== undefined) {
						throw new Error("Presentation services are already active");
					}
					this.#session = sessionFeature;
					this.#slashCommands = commands;
					this.#controller = controller;
					this.#control = control ?? undefined;
					if (extensionUI) env.own(this.#serveExtensionUI(extensionUI));
					env.own(() => {
						if (this.#session === sessionFeature) this.#session = undefined;
						if (this.#slashCommands === commands) this.#slashCommands = undefined;
						if (this.#controller === controller) this.#controller = undefined;
						if (this.#control === control) this.#control = undefined;
					});
					env.own(
						commands.replace({
							name: "rlm",
							description: "Toggle the live RLM graph panel; /rlm focus opens it full screen",
							run: (args) => {
								if (args.trim() === "focus") this.#openRlmFocus();
								else this.#toggleRlm();
								return undefined;
							},
						}),
					);
					env.own(
						commands.replace({
							name: "jev",
							description: "Toggle the Jev panel (routing, recall gates, retention decisions)",
							run: () => {
								this.#toggleJev();
								return undefined;
							},
						}),
					);
					// Pi's session tree commands, on the native tree and native fork.
					env.own(
						commands.replace({
							name: "tree",
							description: "Navigate the session tree (switch branches)",
							run: () => {
								void this.#showTreeSelector();
								return undefined;
							},
						}),
					);
					env.own(
						commands.replace({
							name: "fork",
							description: "Create a new fork from a previous user message",
							run: () => {
								void this.#showUserMessageSelector();
								return undefined;
							},
						}),
					);
					env.own(
						commands.replace({
							name: "clone",
							description: "Duplicate the current session at the current position",
							run: () => {
								void this.#cloneSession();
								return undefined;
							},
						}),
					);
					env.own(commands.subscribe(() => this.#updateAutocomplete()));
					if (server.radius) {
						env.own(
							server.server.connection.subscribe((state) => this.#handleConnectionState(server.serverId, state)),
						);
						env.own(
							server.session.attachment.subscribe((state) => this.#handleAttachmentState(sessionFeature, state)),
						);
					}
				});
			},
		});
		facetHost = await createFacetHost({
			facets: [
				createSlashCommandsRuntimeFacet(),
				presentationBridgeFacet,
				createBuiltInSlashCommandsFacet({ reloadPresentationPlugins }),
				...this.#sharedFacets.facets,
				...presentationFacets.facets,
			],
			serviceSources: [server.server, server.session],
		});
		this.#facetHost = facetHost;
	}

	async #openPreparedSession(prepared: PreparedClientSession): Promise<void> {
		const feature = this.#session;
		if (feature === undefined) throw new Error(`No Session service is available for ${prepared.server.serverId}`);
		await feature.session.whenAttached(prepared.summary.sessionId, BACKGROUND_CONTEXT);
		this.#selectedServerId = feature.serverId;
		this.#sessionId = prepared.summary.sessionId;
		this.#updateAutocomplete();
		await this.#openLane(feature);
		this.#screen = "chat";
		this.#status = "";
		this.#scheduleRlmPolling();
		void this.#refreshRlm();
		this.#rebuild();
	}

	async #close(): Promise<void> {
		this.#closed = true;
		if (this.#rlmTimer !== undefined) clearInterval(this.#rlmTimer);
		this.#rlmTimer = undefined;
		if (this.#animationTimer !== undefined) clearInterval(this.#animationTimer);
		this.#animationTimer = undefined;
		this.#completeSelection(undefined);
		this.#active?.cancel();
		const errors: unknown[] = [];
		try {
			await this.#recoveryTransition;
			await this.#closeLane();
			await this.#facetReloadTail;
		} catch (error) {
			errors.push(error);
		}
		if (this.#facetHost !== undefined) {
			try {
				await this.#facetHost.dispose();
			} catch (error) {
				errors.push(error);
			}
			this.#facetHost = undefined;
		}
		const generations = [this.#presentationFacets, this.#sharedFacets].filter(
			(generation): generation is LoadedFacets => generation !== undefined,
		);
		this.#presentationFacets = undefined;
		const results = await Promise.allSettled(generations.map((generation) => generation.dispose()));
		errors.push(...results.flatMap((result) => (result.status === "rejected" ? [result.reason] : [])));
		if (errors.length === 1) throw errors[0];
		if (errors.length > 1) throw new AggregateError(errors, "Failed to dispose experimental TUI facets");
	}

	#rebuild(): void {
		this.#sessionHeading.setText(
			this.#sessionId === undefined || this.#selectedServerId === undefined
				? ""
				: theme.fg("dim", `Server: ${this.#selectedServerId}\nSession: ${this.#sessionId}`),
		);
		this.#statusContainer.clear();
		if (this.#status.length > 0) {
			this.#statusContainer.addChild(new Text(theme.fg("dim", this.#status), 1, 0));
		}
		if (this.#rlmVisible) this.#statusContainer.addChild(this.#rlmPanel);
		if (this.#jevVisible) this.#statusContainer.addChild(this.#jevPanel);
		if (this.#chatView !== undefined) this.#statusContainer.addChild(this.#chatView.status);
		this.#footerText.setText(theme.fg("dim", this.#footer()));
		this.#editorContainer.clear();
		if (this.#screen === "select" && this.#selection !== undefined) {
			this.#chatInput.focused = false;
			const selector = new Container();
			selector.addChild(new Text(theme.bold(this.#selection.title), 1, 1));
			const items = [...this.#selection.items];
			this.#selectList = new SelectList(items, Math.min(Math.max(items.length, 1), 12), selectTheme);
			const selectedIndex = items.findIndex((item) => item.value === this.#selection?.selectedValue);
			if (selectedIndex >= 0) this.#selectList.setSelectedIndex(selectedIndex);
			this.#selectList.onSelect = (item) => this.#completeSelection(item.value);
			this.#selectList.onCancel = () => this.#completeSelection(undefined);
			selector.addChild(this.#selectList);
			this.#editorContainer.addChild(selector);
		} else if (this.#screen === "component" && this.#active !== undefined) {
			this.#chatInput.focused = false;
			this.#selectList = undefined;
			this.#editorContainer.addChild(this.#active.component);
		} else {
			this.#selectList = undefined;
			this.#chatInput.focused = !this.#busy;
			this.#editorContainer.addChild(this.#chatInput);
		}
		this.#layoutRoot.invalidate();
		this.#requestRender();
	}

	#select(title: string, items: readonly SelectItem[], selectedValue?: string): Promise<string | undefined> {
		if (this.#selection !== undefined) throw new Error("A slash command selector is already active");
		// As in Pi, a new dialog replaces an open tree or fork selector.
		this.#active?.cancel();
		return new Promise((resolve) => {
			this.#selection = { title, items, ...(selectedValue === undefined ? {} : { selectedValue }), resolve };
			this.#screen = "select";
			this.#rebuild();
		});
	}

	#completeSelection(value: string | undefined): void {
		const selection = this.#selection;
		if (selection === undefined) return;
		this.#selection = undefined;
		this.#screen = "chat";
		selection.resolve(value);
		if (!this.#closed) this.#rebuild();
	}

	#updateAutocomplete(): void {
		const commands = this.#selectedSlashCommands()?.list() ?? [];
		this.#chatInput.setAutocompleteProvider(
			new CombinedAutocompleteProvider(
				commands.map((command) => ({
					name: command.name,
					description: command.description,
					...(command.argumentHint === undefined ? {} : { argumentHint: command.argumentHint }),
					...(command.getArgumentCompletions === undefined
						? {}
						: {
								getArgumentCompletions: async (prefix: string) => {
									const items = await command.getArgumentCompletions!(prefix);
									return items === null ? null : [...items];
								},
							}),
				})),
				process.cwd(),
				this.#fdPath,
			),
		);
		this.#requestRender();
	}

	#selectedSlashCommands(): SlashCommands | undefined {
		return this.#slashCommands;
	}

	#handleConnectionState(serverId: string, state: ServerConnectionState): void {
		if (this.#closed || this.#selectedServerId !== serverId) return;
		if (state.status === "connected") {
			if (this.#laneUnsubscribe === undefined) {
				this.#busy = true;
				this.#status = "Reattaching Session…";
				this.#rebuild();
			}
			return;
		}
		this.#busy = true;
		this.#status = state.status === "connecting" ? "Reconnecting to Radius…" : "Radius disconnected; retrying…";
		this.#queueRecovery(() => this.#closeLane());
		this.#rebuild();
	}

	#handleAttachmentState(feature: SessionFeature, state: SessionAttachmentState): void {
		if (this.#closed || this.#selectedServerId !== feature.serverId || this.#sessionId === undefined) return;
		if (state.status === "attached" && state.sessionId === this.#sessionId) {
			this.#queueRecovery(async () => {
				if (this.#laneUnsubscribe === undefined) await this.#openLane(feature);
				this.#busy = false;
				this.#status = "";
				this.#rebuild();
			});
			return;
		}
		if (state.status === "attaching" && state.sessionId === this.#sessionId) {
			this.#busy = true;
			this.#status = "Reattaching Session…";
			this.#rebuild();
		}
	}

	#queueRecovery(operation: () => Promise<void>): void {
		this.#recoveryTransition = this.#recoveryTransition
			.then(async () => {
				if (!this.#closed) await operation();
			})
			.catch((error: unknown) => {
				if (this.#closed) return;
				this.#busy = true;
				this.#status = `Reconnect error: ${message(error)}`;
				this.#rebuild();
			});
	}

	async #openLane(feature: SessionFeature): Promise<void> {
		await this.#closeLane();
		const view = new ExperimentalChatView(this.#ui, process.cwd(), { jev: this.#jevNoteSource });
		this.#chatView = view;
		this.#documentContainer.addChild(this.#sessionHeading);
		this.#documentContainer.addChild(view.transcript);
		this.#pendingMessagesContainer.addChild(view.pendingMessages);
		this.#laneUnsubscribe = feature.transcript.state.subscribe((value) => {
			if (value.snapshot === null) return;
			view.apply(value.snapshot);
			// Stamp root-cell start/end times as they happen, and refresh the task tree on tool boundaries.
			extractRootCell(value.snapshot, this.#rlmClock, Date.now());
			const event = value.event;
			if (event !== null && (event.type === "tool_start" || event.type === "tool_end")) void this.#refreshRlm();
			this.#rebuild();
		});
		if (feature.transcript.state.value?.snapshot === null || feature.transcript.state.value?.snapshot === undefined) {
			await this.#closeLane();
			throw new Error("Transcript has no initialized snapshot");
		}
	}

	async #closeLane(): Promise<void> {
		this.#laneUnsubscribe?.();
		this.#laneUnsubscribe = undefined;
		this.#chatView?.dispose();
		this.#chatView = undefined;
		this.#documentContainer.clear();
		this.#pendingMessagesContainer.clear();
		this.#statusContainer.clear();
	}

	async #runPrompt(messageText: string): Promise<void> {
		const prompt = messageText.trim();
		if (prompt.length === 0) return;
		if (prompt.startsWith("/")) {
			const separator = prompt.indexOf(" ");
			const name = prompt.slice(1, separator === -1 ? undefined : separator);
			const args = separator === -1 ? "" : prompt.slice(separator + 1).trim();
			await this.#executeSlashCommand(name, args);
			return;
		}
		this.#chatInput.setText("");
		try {
			await this.#submitPrompt(prompt);
		} catch (error) {
			this.#status = `Error: ${message(error)}`;
			this.#rebuild();
		}
	}

	async #executeSlashCommand(name: string, args: string): Promise<void> {
		const command = this.#selectedSlashCommands()
			?.list()
			.find((candidate) => candidate.name === name);
		this.#chatInput.setText("");
		if (command === undefined) {
			this.#status = `Unknown slash command: /${name}`;
			this.#rebuild();
			return;
		}
		try {
			const result = await command.run(args, BACKGROUND_CONTEXT);
			if (result !== undefined) {
				if ("entryId" in result) this.#reportQueue(result);
				else this.#reportOperation(result);
			}
		} catch (error) {
			this.#status = `Error: ${message(error)}`;
			this.#rebuild();
		}
	}

	async #submitPrompt(prompt: string): Promise<void> {
		const controller = this.#selectedController();
		if (controller === undefined) throw new Error("No Session AgentController service is available");
		const operation = this.#laneSnapshot()?.operation;
		const running = operation !== null && operation !== undefined;
		this.#status = running ? "Queueing steering message…" : "Running turn…";
		this.#rebuild();
		if (running) this.#reportQueue(await controller.steer({ message: prompt, images: null }, BACKGROUND_CONTEXT));
		else this.#reportOperation(await controller.prompt({ message: prompt, images: null }, BACKGROUND_CONTEXT));
	}

	async #queueFollowUp(text: string): Promise<void> {
		const controller = this.#selectedController();
		if (controller === undefined) return;
		try {
			this.#status = "Queueing follow-up…";
			this.#rebuild();
			this.#reportQueue(await controller.followUp({ message: text, images: null }, BACKGROUND_CONTEXT));
		} catch (error) {
			this.#status = `Error: ${message(error)}`;
			this.#rebuild();
		}
	}

	#reportOperation(response: AgentOperationResponse): void {
		this.#status = response.accepted
			? response.error === null
				? ""
				: `Operation failed: ${response.error.message}`
			: `Operation rejected: ${response.error.message}`;
		this.#rebuild();
	}

	#reportQueue(response: AgentQueueResponse): void {
		this.#status = response.accepted ? `Queued ${response.entryId}.` : `Message rejected: ${response.error.message}`;
		this.#rebuild();
	}

	/**
	 * Serve the worker's extension dialogs: select and confirm open the selector, input and editor open Pi's extension
	 * input and editor components, notify shows in the status line, and set_editor_text fills the editor.
	 */
	#serveExtensionUI(extensionUI: ExtensionUI): () => void {
		const abort = new AbortController();
		const context = withAbortSignal(abort.signal, BACKGROUND_CONTEXT);
		let dialogs = Promise.resolve();
		const answer = async (request: RpcExtensionUIRequest): Promise<void> => {
			switch (request.method) {
				case "select": {
					const value = await this.#select(
						request.title,
						request.options.map((option) => ({ value: option, label: option })),
					);
					await extensionUI.respond(request.id, value === undefined ? { cancelled: true } : { value }, context);
					return;
				}
				case "confirm": {
					const value = await this.#select(`${request.title}${request.message ? `: ${request.message}` : ""}`, [
						{ value: "yes", label: "Yes" },
						{ value: "no", label: "No" },
					]);
					await extensionUI.respond(
						request.id,
						value === undefined ? { cancelled: true } : { confirmed: value === "yes" },
						context,
					);
					return;
				}
				case "input":
				case "editor": {
					const value =
						request.method === "input"
							? await this.#showTextInput(request.title, request.placeholder, request.timeout)
							: await this.#showTextEditor(request.title, request.prefill);
					await extensionUI.respond(request.id, value === undefined ? { cancelled: true } : { value }, context);
					return;
				}
				case "notify":
					this.#status = request.message;
					this.#rebuild();
					return;
				case "set_editor_text":
					this.#chatInput.setText(request.text);
					this.#requestRender();
					return;
				default:
					return;
			}
		};
		void (async () => {
			let cursor: number | null = null;
			let generation = this.#uiGeneration;
			while (!abort.signal.aborted) {
				// After a Session switch the worker is a different one: start again from its open dialogs.
				if (generation !== this.#uiGeneration) {
					generation = this.#uiGeneration;
					cursor = null;
				}
				try {
					const polled = extensionUI.poll(cursor, 10_000, context);
					void polled.catch(() => {});
					const result = await awaitWithContext(polled, context);
					if (generation !== this.#uiGeneration) continue;
					cursor = result.cursor;
					for (const item of result.requests) {
						// Dialogs share the one selector, so they are answered in order.
						dialogs = dialogs.then(() => answer(item.request)).catch(() => {});
					}
				} catch {
					if (abort.signal.aborted) return;
					cursor = null;
					await new Promise((resolveDelay) => setTimeout(resolveDelay, 1_000));
				}
			}
		})();
		return () => abort.abort();
	}

	/** Show a component in the editor slot with the input focus (Pi's `showSelector`); returns its close function. */
	#showComponent(active: ActiveComponent): () => void {
		this.#completeSelection(undefined);
		const previous = this.#active;
		this.#active = active;
		// A replaced component closes as cancelled; its close is then a no-op on the screen.
		previous?.cancel();
		this.#screen = "component";
		if (isFocusable(active.focus)) active.focus.focused = true;
		this.#rebuild();
		let closed = false;
		return () => {
			if (closed) return;
			closed = true;
			active.dispose?.();
			if (isFocusable(active.focus)) active.focus.focused = false;
			if (this.#active !== active) return;
			this.#active = undefined;
			this.#screen = "chat";
			if (!this.#closed) this.#rebuild();
		};
	}

	/** Pi's extension text input dialog. */
	#showTextInput(title: string, placeholder?: string, timeout?: number): Promise<string | undefined> {
		return new Promise((resolve) => {
			let close = (): void => {};
			const finish = (value: string | undefined): void => {
				close();
				resolve(value);
			};
			const input = new ExtensionInputComponent(title, placeholder, finish, () => finish(undefined), {
				tui: this.#ui,
				...(timeout === undefined ? {} : { timeout }),
			});
			close = this.#showComponent({
				component: input,
				focus: input,
				cancel: () => finish(undefined),
				dispose: () => input.dispose(),
			});
		});
	}

	/** Pi's extension multi-line editor dialog (Ctrl+G opens the external editor). */
	#showTextEditor(title: string, prefill?: string): Promise<string | undefined> {
		return new Promise((resolve) => {
			let close = (): void => {};
			const finish = (value: string | undefined): void => {
				close();
				resolve(value);
			};
			const editor = new ExtensionEditorComponent(
				this.#ui,
				this.#keybindings,
				title,
				prefill,
				finish,
				() => finish(undefined),
				undefined,
				this.#settingsManager.getExternalEditorCommand(),
			);
			close = this.#showComponent({ component: editor, focus: editor, cancel: () => finish(undefined) });
		});
	}

	/** Pi's Esc: abort a running operation; idle, a double Esc on an empty editor runs `doubleEscapeAction`. */
	#handleEscape(): void {
		const operation = this.#laneSnapshot()?.operation;
		if (operation !== null && operation !== undefined) {
			this.#interrupt();
			return;
		}
		if (this.#chatInput.getText().trim()) return;
		const action = this.#settingsManager.getDoubleEscapeAction();
		if (action === "none") return;
		const now = Date.now();
		if (now - this.#lastEscapeTime < DOUBLE_ESCAPE_WINDOW_MS) {
			this.#lastEscapeTime = 0;
			if (action === "tree") void this.#showTreeSelector();
			else void this.#showUserMessageSelector();
		} else {
			this.#lastEscapeTime = now;
		}
	}

	#showStatus(status: string): void {
		this.#status = status;
		this.#rebuild();
	}

	/** The native Session tree as Pi's session view, read from the worker's storage. */
	async #readSessionView(): Promise<PiSessionView | undefined> {
		const control = this.#control;
		const sessionId = this.#sessionId;
		if (control === undefined || sessionId === undefined) {
			this.#showStatus("No Session is attached");
			return undefined;
		}
		try {
			return new PiSessionView(await control.readTree(BACKGROUND_CONTEXT), sessionId, process.cwd());
		} catch (error) {
			this.#showStatus(`Error: ${message(error)}`);
			return undefined;
		}
	}

	/** Pi's `/tree` selector over every branch of the native Session. */
	async #showTreeSelector(initialSelectedId?: string): Promise<void> {
		const view = await this.#readSessionView();
		if (view === undefined || this.#closed) return;
		const tree = view.tree();
		if (tree.length === 0) {
			this.#showStatus("No entries in session");
			return;
		}
		const control = this.#control;
		let close = (): void => {};
		const selector = new TreeSelectorComponent(
			tree,
			view.leafId,
			this.#ui.terminal.rows,
			(entryId) => {
				close();
				void this.#navigateTree(view, entryId);
			},
			() => {
				close();
				this.#requestRender();
			},
			(entryId, label) => {
				void control?.setLabel(entryId, label ?? null, BACKGROUND_CONTEXT).catch((error: unknown) => {
					this.#showStatus(`Error: ${message(error)}`);
				});
				this.#requestRender();
			},
			initialSelectedId,
			this.#settingsManager.getTreeFilterMode(),
		);
		selector.onCopy = async (text) => {
			if (!text) {
				this.#showStatus("Error: Selected entry has no text to copy");
				return;
			}
			try {
				await copyToClipboard(text);
				this.#showStatus("Copied selected message to clipboard");
			} catch (error) {
				this.#showStatus(`Error: ${message(error)}`);
			}
		};
		close = this.#showComponent({ component: selector, focus: selector, cancel: () => close() });
	}

	/** Pi's tree navigation: optional branch summary, then move the main lane to the selected point. */
	async #navigateTree(view: PiSessionView, entryId: string): Promise<void> {
		if (entryId === view.leafId) {
			this.#showStatus("Already at this point");
			return;
		}
		let wantsSummary = false;
		let customInstructions: string | undefined;
		if (!this.#settingsManager.getBranchSummarySkipPrompt()) {
			while (true) {
				const choice = await this.#select(
					"Summarize branch?",
					["No summary", "Summarize", "Summarize with custom prompt"].map((value) => ({ value, label: value })),
				);
				if (choice === undefined) {
					// Esc returns to the tree with the same selection.
					await this.#showTreeSelector(entryId);
					return;
				}
				wantsSummary = choice !== "No summary";
				if (choice === "Summarize with custom prompt") {
					customInstructions = await this.#showTextEditor("Custom summarization instructions");
					if (customInstructions === undefined) continue;
				}
				break;
			}
		}
		const controller = this.#selectedController();
		if (controller === undefined) return;
		// The user committed to navigating: stop the active turn first, as Pi does.
		if (!(await this.#stopForSessionChange())) return;
		// Pi puts a selected user (or custom) message back in the editor and continues from just before it.
		const entry = view.entries.find((candidate) => candidate.id === entryId);
		let targetId: string | null = entryId;
		let editorText: string | undefined;
		if (entry?.type === "message" && entry.message.role === "user") {
			targetId = entry.parentId;
			editorText = messageText(entry.message);
		} else if (entry?.type === "custom_message") {
			targetId = entry.parentId;
			editorText =
				typeof entry.content === "string"
					? entry.content
					: entry.content.flatMap((part) => (part.type === "text" ? [part.text] : [])).join("");
		}
		this.#showStatus(wantsSummary ? "Summarizing branch… (Esc to cancel)" : "Navigating…");
		try {
			const response = await controller.navigate(
				{ targetId, summarize: wantsSummary, label: null, customInstructions: customInstructions ?? null },
				BACKGROUND_CONTEXT,
			);
			if (response.error !== null) {
				if (response.error.code === "aborted") {
					this.#showStatus("Branch summarization cancelled");
					await this.#showTreeSelector(entryId);
					return;
				}
				this.#showStatus(`Error: ${response.error.message}`);
				return;
			}
			if (editorText && !this.#chatInput.getText().trim()) this.#chatInput.setText(editorText);
			this.#showStatus("Navigated to selected point");
		} catch (error) {
			this.#showStatus(`Error: ${message(error)}`);
		}
	}

	/** Pi's `/fork` selector: user messages from every branch; the fork holds the path to just before the pick. */
	async #showUserMessageSelector(): Promise<void> {
		const view = await this.#readSessionView();
		if (view === undefined || this.#closed) return;
		const messages = view.entries.flatMap((entry) => {
			if (entry.type !== "message" || entry.message.role !== "user") return [];
			const text = messageText(entry.message);
			return text ? [{ id: entry.id, text }] : [];
		});
		if (messages.length === 0) {
			this.#showStatus("No messages to fork from");
			return;
		}
		let close = (): void => {};
		const selector = new UserMessageSelectorComponent(
			messages,
			(entryId) => {
				close();
				const text = messages.find((candidate) => candidate.id === entryId)?.text ?? "";
				void this.#forkSession(entryId, { entryId, position: "before" }, text, "Forked to new session");
			},
			() => {
				close();
				this.#requestRender();
			},
			messages.at(-1)?.id,
		);
		close = this.#showComponent({ component: selector, focus: selector.getMessageList(), cancel: () => close() });
	}

	/** Pi's `/clone`: a new Session with the path to the current position. */
	async #cloneSession(): Promise<void> {
		const view = await this.#readSessionView();
		if (view === undefined) return;
		if (view.leafId === null) {
			this.#showStatus("Nothing to clone yet");
			return;
		}
		await this.#forkSession(view.leafId, { position: "at" }, "", "Cloned to new session");
	}

	async #forkSession(entryId: string, forkPath: SessionForkPath, editorText: string, done: string): Promise<void> {
		const server = this.#server;
		const sourceId = this.#sessionId;
		const control = this.#control;
		if (server === undefined || sourceId === undefined || control === undefined) return;
		if (!(await this.#stopForSessionChange())) return;
		// Pi's session_before_fork: the Session's extensions may cancel, and nothing changes.
		try {
			if ((await control.beforeFork(entryId, forkPath.position, BACKGROUND_CONTEXT)).cancelled) {
				this.#requestRender();
				return;
			}
		} catch (error) {
			this.#showStatus(`Error: ${message(error)}`);
			return;
		}
		const services = server.server.open({
			services: [SessionManagement, PresentationPlugins],
			assertAccess() {},
			onError() {},
		});
		try {
			await services.ready(BACKGROUND_CONTEXT);
			const management = services.use(SessionManagement);
			const plugins = services.use(PresentationPlugins);
			const created = await management.create({ forkFromSessionId: sourceId, forkPath }, BACKGROUND_CONTEXT);
			await control.forked(created.sessionFile ?? null, BACKGROUND_CONTEXT);
			const presentationPlugins = await plugins.prepareSession(
				{ sessionId: created.sessionId, packagePaths: null },
				BACKGROUND_CONTEXT,
			);
			await this.#closeLane();
			this.#uiGeneration += 1;
			await management.attach(created.sessionId, BACKGROUND_CONTEXT);
			await server.session.whenAttached(created.sessionId, BACKGROUND_CONTEXT);
			this.#sessionId = created.sessionId;
			await this.#reloadPresentationPlugins?.(presentationPlugins);
			const feature = this.#session;
			if (feature === undefined) throw new Error("No Session service is available after the fork");
			await this.#openLane(feature);
			this.#chatInput.setText(editorText);
			this.#showStatus(done);
		} catch (error) {
			this.#showStatus(`Error: ${message(error)}`);
		} finally {
			await services.dispose(BACKGROUND_CONTEXT).catch(() => {});
		}
	}

	/** Abort a running turn (returning queued messages to the editor) and wait for the lane to be idle. */
	async #stopForSessionChange(): Promise<boolean> {
		if (this.#laneSnapshot()?.operation == null) return true;
		this.#interrupt();
		const deadline = Date.now() + 10_000;
		while (this.#laneSnapshot()?.operation != null) {
			if (Date.now() > deadline || this.#closed) {
				this.#showStatus("Error: the running turn did not stop");
				return false;
			}
			await new Promise((resolveWait) => setTimeout(resolveWait, 50));
		}
		return true;
	}

	/** Pi's Ctrl-C: the first press clears the editor, a second press within 500 ms exits. */
	#handleCtrlC(): void {
		const now = Date.now();
		if (now - this.#lastCtrlCTime < CTRL_C_EXIT_WINDOW_MS) {
			this.#lastCtrlCTime = 0;
			this.#finish();
			return;
		}
		this.#lastCtrlCTime = now;
		this.#chatInput.setText("");
		this.#requestRender();
	}

	/** Pi's Esc during a turn: queued messages return to the editor, then the turn is aborted. */
	#interrupt(): void {
		const snapshot = this.#laneSnapshot();
		const operation = snapshot?.operation;
		const controller = this.#selectedController();
		if (operation === null || operation === undefined || controller === undefined) return;
		// Only what the user typed returns to the editor; queued host messages (completion events) are not theirs.
		const queued = snapshot!.queues.flatMap((item) =>
			item.type === "message" && item.message.role === "user" ? [item] : [],
		);
		const restored = [
			...queued.filter((item) => item.kind === "steer"),
			...queued.filter((item) => item.kind !== "steer"),
		];
		if (restored.length > 0) {
			const queuedText = restored.map((item) => messageText(item.message)).join("\n\n");
			const combined = [queuedText, this.#chatInput.getText()].filter((text) => text.trim()).join("\n\n");
			this.#chatInput.setText(combined);
			for (const item of restored) void controller.cancelQueued(item.entryId, BACKGROUND_CONTEXT).catch(() => {});
		}
		this.#status = `Aborting ${operation.id}…`;
		this.#rebuild();
		void controller.requestAbort(operation.id, BACKGROUND_CONTEXT).catch((error: unknown) => {
			this.#status = `Error: ${message(error)}`;
			this.#rebuild();
		});
	}

	#selectedController(): AgentController | undefined {
		return this.#controller;
	}

	#laneSnapshot() {
		const snapshot = this.#session?.transcript.state.value?.snapshot;
		return snapshot === null ? undefined : snapshot;
	}

	#toggleRlm(): void {
		this.#rlmVisible = !this.#rlmVisible;
		this.#scheduleRlmPolling();
		void this.#refreshRlm();
		this.#rebuild();
	}

	#toggleJev(): void {
		this.#jevVisible = !this.#jevVisible;
		this.#scheduleRlmPolling();
		void this.#refreshRlm();
		this.#rebuild();
	}

	#scheduleRlmPolling(): void {
		if (this.#rlmTimer !== undefined) clearInterval(this.#rlmTimer);
		this.#rlmTimer = undefined;
		if (this.#closed) return;
		this.#rlmTimer = setInterval(
			() => void this.#refreshRlm(),
			this.#rlmVisible || this.#jevVisible || this.#rlmFocus !== undefined
				? RLM_POLL_VISIBLE_MS
				: RLM_POLL_HIDDEN_MS,
		);
		this.#rlmTimer.unref?.();
	}

	/** Poll read-only inspection requests; never awaited by input handling, and coalesced while in flight. */
	async #refreshRlm(): Promise<void> {
		const control = this.#control;
		if (control === undefined || this.#closed) return;
		if (this.#rlmInFlight) {
			this.#rlmQueued = true;
			return;
		}
		this.#rlmInFlight = true;
		try {
			const graphListing = this.#graphListing;
			const [status, instances, pool, jev, contextState, frames] = await Promise.allSettled([
				control.inspect("agents.status", graphListing ? { graph: true } : {}, BACKGROUND_CONTEXT),
				control.inspect("instances.list", {}, BACKGROUND_CONTEXT),
				control.inspect("rlm.pool", {}, BACKGROUND_CONTEXT),
				control.inspect("jev.decisions", {}, BACKGROUND_CONTEXT),
				control.inspect("ctx.state", {}, BACKGROUND_CONTEXT),
				control.inspect("rlm.frames", { limit: RLM_FRAME_LIMIT }, BACKGROUND_CONTEXT),
			]);
			if (this.#closed) return;
			if (graphListing && status.status === "rejected" && /Unknown payload field/.test(message(status.reason))) {
				this.#graphListing = false;
				this.#rlmQueued = true;
			}
			const next: RlmPollState = { ...this.#rlmState };
			if (status.status === "fulfilled") {
				const parsed = parseAgentsStatus(status.value);
				next.tasks = parsed.tasks;
				next.usage = parsed.usage;
				next.limits = parsed.limits;
				next.jobs = parsed.jobs;
				if (parsed.truncatedTasks === undefined) delete next.truncatedTasks;
				else next.truncatedTasks = parsed.truncatedTasks;
				next.timing = this.#rlmClock.timings(parsed.tasks, parsed.usage, Date.now());
				delete next.error;
			} else {
				next.error = message(status.reason);
			}
			if (instances.status === "fulfilled") next.retained = parseRetained(instances.value);
			next.pool = pool.status === "fulfilled" ? parsePool(pool.value) : null;
			next.context = contextState.status === "fulfilled" ? parseContextState(contextState.value) : null;
			if (frames.status === "fulfilled") next.frames = parseFrames(frames.value);
			// Progress assessments are cheap host reads; only fetch them for a few running tasks while visible.
			const progress = new Map<string, RlmProgress>();
			if (this.#rlmVisible || this.#rlmFocus !== undefined) {
				const running = next.tasks.filter((task) => isActiveState(task.state)).slice(0, RLM_MAX_ASSESSED);
				const assessed = await Promise.allSettled(
					running.map((task) => control.inspect("progress.assess", { task_id: task.id }, BACKGROUND_CONTEXT)),
				);
				assessed.forEach((result, index) => {
					const value = result.status === "fulfilled" ? parseProgress(result.value) : undefined;
					if (value !== undefined) progress.set(running[index]!.id, value);
				});
			}
			next.progress = progress;
			if (this.#closed) return;
			this.#rlmState = next;
			this.#jevState = {
				...(jev.status === "fulfilled"
					? parseJevDecisions(jev.value)
					: { available: this.#jevState.available, decisions: this.#jevState.decisions }),
				...(status.status === "fulfilled" ? parseJevLedger(status.value) : {}),
				...(jev.status === "rejected" && this.#jevState.available !== null ? { error: message(jev.reason) } : {}),
			};
			this.#updateAnimation();
			this.#layoutRoot.invalidate();
			this.#requestRender();
		} finally {
			this.#rlmInFlight = false;
			if (this.#rlmQueued && !this.#closed) {
				this.#rlmQueued = false;
				void this.#refreshRlm();
			}
		}
	}

	#rlmSnapshot(): RlmSnapshot {
		const now = Date.now();
		const state = this.#rlmState;
		const lane = this.#laneSnapshot();
		const { turn, cells } = extractTurn(lane, this.#rlmClock, now);
		return {
			now,
			tasks: state.tasks,
			usage: state.usage,
			limits: state.limits,
			pool: state.pool,
			rootCell: extractRootCell(lane, this.#rlmClock, now),
			retained: state.retained,
			progress: state.progress,
			timing: state.timing,
			context: state.context ?? null,
			frames: state.frames,
			jobs: state.jobs,
			cells,
			turn,
			...(state.truncatedTasks === undefined ? {} : { truncatedTasks: state.truncatedTasks }),
			...(state.error === undefined ? {} : { error: state.error }),
		};
	}

	#keyHint(action: "app.rlm.focus" | "app.rlm.toggle" | "app.jev.notes.toggle"): string | undefined {
		return this.#keybindings.getKeys(action)[0];
	}

	#renderRlm(width: number, mode: "panel" | "footer"): string[] {
		const inner = Math.max(1, width - 2);
		const focusKey = this.#keyHint("app.rlm.focus");
		const options = {
			style: rlmStyle,
			spinnerFrame: Math.floor(Date.now() / 100),
			...(focusKey === undefined ? {} : { focusKey }),
		};
		if (mode === "panel") {
			if (this.#rlmFocus !== undefined) return [];
			return renderRlmDock(this.#rlmSnapshot(), inner, options).map((line) => ` ${line}`);
		}
		if (this.#rlmVisible || this.#rlmFocus !== undefined) return [];
		const line = renderRlmFooter(this.#rlmSnapshot(), inner, options);
		return line === undefined ? [] : [` ${line}`];
	}

	#jevSnapshot(): JevSnapshot {
		const transcript = this.#laneSnapshot()?.transcript ?? [];
		const recalledCounts = new Map<string, number>();
		for (const entry of transcript) {
			const note = parseMemoryMessage((entry as { message?: unknown }).message);
			if (note?.taskId !== undefined) recalledCounts.set(note.taskId, note.items.length);
		}
		return { now: Date.now(), ...this.#jevState, memories: collectKnownMemories(transcript), recalledCounts };
	}

	#renderJev(width: number, mode: "panel" | "footer"): string[] {
		const inner = Math.max(1, width - 2);
		const snapshot = this.#jevSnapshot();
		if (mode === "panel") return renderJevPanel(snapshot, inner, { style: rlmStyle }).map((line) => ` ${line}`);
		if (this.#jevVisible) return [];
		const line = renderJevPresence(snapshot, inner, { style: rlmStyle });
		return line === undefined ? [] : [` ${line}`];
	}

	/** Live data for Jev's transcript notes. */
	readonly #jevNoteSource: JevNoteSource = {
		decisions: () => this.#jevState.decisions,
		thresholds: () => this.#jevState.thresholds,
		expanded: () => this.#jevNotesExpanded,
		expandKey: () => this.#keyHint("app.jev.notes.toggle"),
		style: () => rlmStyle,
	};

	/** The full-screen RLM graph in the editor slot; Esc (or the bound exit key) returns to the chat. */
	#openRlmFocus(): void {
		if (this.#rlmFocus !== undefined) return;
		let close = (): void => {};
		const focus = new RlmGraphFocus({
			snapshot: () => this.#rlmSnapshot(),
			// Full screen above the footer, which keeps Jev's presence and the model line visible.
			height: () =>
				Math.max(10, this.#ui.terminal.rows - this.#footerComponent.render(this.#ui.terminal.columns).length - 1),
			keybindings: this.#keybindings,
			style: rlmStyle,
			onExit: () => close(),
			requestRender: () => this.#requestRender(),
			loadTrace: async (traceId) => {
				const control = this.#control;
				if (control === undefined) throw new Error("no Session control");
				return control.inspect("rlm.frames", { id: traceId }, BACKGROUND_CONTEXT);
			},
		});
		this.#rlmFocus = focus;
		const closeComponent = this.#showComponent({ component: focus, focus, cancel: () => close() });
		close = () => {
			if (this.#rlmFocus === focus) this.#rlmFocus = undefined;
			closeComponent();
			this.#updateAnimation();
			this.#requestRender();
		};
		this.#scheduleRlmPolling();
		void this.#refreshRlm();
		this.#updateAnimation();
	}

	/** Redraw briefly for spinners and Jev's pulse while something animates; idle views cost nothing. */
	#updateAnimation(): void {
		const jevLatest = this.#jevState.decisions.reduce((latest, decision) => Math.max(latest, decision.at), 0);
		const pulsing = Date.now() - jevLatest < JEV_PULSE_MS || (this.#jevState.inFlight ?? 0) > 0;
		const running =
			this.#rlmState.tasks.some((task) => isActiveState(task.state)) ||
			graphActive(buildRlmGraph(this.#rlmSnapshot()));
		const animate = !this.#closed && (pulsing || running);
		if (animate && this.#animationTimer === undefined) {
			this.#animationTimer = setInterval(() => {
				this.#layoutRoot.invalidate();
				this.#requestRender();
				this.#updateAnimation();
			}, ANIMATION_MS);
			this.#animationTimer.unref?.();
		} else if (!animate && this.#animationTimer !== undefined) {
			clearInterval(this.#animationTimer);
			this.#animationTimer = undefined;
		}
	}

	#footer(): string {
		const snapshot = this.#laneSnapshot();
		if (!snapshot) return "/model · /thinking · /compact · /reload · /rlm · /jev";
		return `${snapshot.configuration.model.provider}/${snapshot.configuration.model.modelId} · thinking:${snapshot.configuration.thinkingLevel} · ${snapshot.stats.messageCount} messages · /model · /thinking · /compact · /reload · /rlm · /jev`;
	}
}

async function prepareClientSession(
	command: ClientCommand,
	servers: readonly ClientTuiServer[],
): Promise<PreparedClientSession> {
	const opened = servers.map((server) => ({
		server,
		services: server.server.open({
			services: [SessionDirectory, SessionManagement, PresentationPlugins],
			assertAccess() {},
			onError() {},
		}),
	}));
	try {
		const features = opened.map(({ server, services }) => ({
			server,
			directory: services.use(SessionDirectory),
			management: services.use(SessionManagement),
			plugins: services.use(PresentationPlugins),
		}));
		await Promise.all(opened.map(({ services }) => services.ready(BACKGROUND_CONTEXT)));
		const name = command.name === undefined ? {} : { name: command.name };
		let existing = false;
		let selected:
			| {
					readonly server: ClientTuiServer;
					readonly management: SessionManagement;
					readonly plugins: PresentationPlugins;
					readonly summary: SessionSummary;
			  }
			| undefined;
		if (command.sessionId !== undefined) {
			const matches = features.flatMap((feature) =>
				(feature.directory.state.value?.sessions ?? [])
					.filter((session) => session.sessionId === command.sessionId)
					.map((summary) => ({
						server: feature.server,
						management: feature.management,
						plugins: feature.plugins,
						summary,
					})),
			);
			if (matches.length > 1) throw new Error(`Session ${command.sessionId} is available from more than one server`);
			selected = matches[0];
			existing = selected !== undefined;
			if (selected === undefined) {
				if (command.connect?.transport === "radius") {
					throw new Error(`Remote server does not contain Session ${command.sessionId}`);
				}
				const feature = requireSingleServer(features);
				selected = {
					server: feature.server,
					management: feature.management,
					plugins: feature.plugins,
					summary: await feature.management.create({ id: command.sessionId, ...name }, BACKGROUND_CONTEXT),
				};
			}
		} else if (command.continue === true || command.resume === true) {
			selected = features
				.flatMap((feature) =>
					(feature.directory.state.value?.sessions ?? []).map((summary) => ({
						server: feature.server,
						management: feature.management,
						plugins: feature.plugins,
						summary,
					})),
				)
				.sort(
					(left, right) =>
						right.summary.modifiedAt - left.summary.modifiedAt ||
						left.summary.serverId.localeCompare(right.summary.serverId) ||
						left.summary.sessionId.localeCompare(right.summary.sessionId),
				)[0];
			existing = selected !== undefined;
		}
		if (existing && selected !== undefined && command.name !== undefined) {
			await selected.management.rename(selected.summary.sessionId, command.name, BACKGROUND_CONTEXT);
		}
		if (selected === undefined) {
			const feature = requireSingleServer(features);
			selected = {
				server: feature.server,
				management: feature.management,
				plugins: feature.plugins,
				// Pi's `--fork`: a new Session with the source's whole history.
				summary: await feature.management.create(
					{ ...(command.fork === undefined ? {} : { forkFromSessionId: command.fork }), ...name },
					BACKGROUND_CONTEXT,
				),
			};
		}
		const presentationPlugins = await selected.plugins.prepareSession(
			{
				sessionId: selected.summary.sessionId,
				packagePaths: command.pluginPackages?.map((packagePath) => resolve(packagePath)) ?? null,
			},
			BACKGROUND_CONTEXT,
		);
		await selected.management.attach(selected.summary.sessionId, BACKGROUND_CONTEXT);
		await selected.server.session.whenAttached(selected.summary.sessionId, BACKGROUND_CONTEXT);
		return {
			server: selected.server,
			summary: selected.summary,
			presentationPlugins,
		};
	} finally {
		await Promise.allSettled(opened.map(({ services }) => services.dispose(BACKGROUND_CONTEXT)));
	}
}

export async function runClientTui(command: ClientCommand, options: RunClientTuiOptions = {}): Promise<void> {
	const cwd = process.cwd();
	const agentDir = getAgentDir();
	const settingsManager = SettingsManager.create(cwd, agentDir);
	const resourceLoader = new DefaultResourceLoader({ cwd, agentDir, settingsManager });
	await resourceLoader.reload();
	setRegisteredThemes(resourceLoader.getThemes().themes);
	const fdPath = await ensureTool("fd");
	const runtime = await openClientRuntime(command, options);
	const tui = createInteractiveTui({
		tuiMode: "fullscreen",
		showHardwareCursor: settingsManager.getShowHardwareCursor(),
		logDirectory: agentDir,
	});
	tui.setClearOnShrink(settingsManager.getClearOnShrink());
	let component: ExperimentalClientTui | undefined;
	let tuiStarted = false;
	const themeController = new InteractiveThemeController(tui, {
		getSettingsManager: () => settingsManager,
		showError: (error) => component?.showError(error),
		onChanged: () => component?.refreshTheme(),
	});
	try {
		let finish!: () => void;
		const finished = new Promise<void>((resolve) => {
			finish = () => {
				themeController.disableAutoSync();
				if (tuiStarted) {
					tui.stop();
					tuiStarted = false;
				}
				resolve();
			};
		});
		component = await ExperimentalClientTui.create({
			command,
			ui: tui,
			servers: runtime.servers.map((server) => ({
				serverId: server.route.serverId,
				radius: server.route.transport === "radius",
				server: server.server,
				session: server.session,
			})),
			facetLoader: options.facetLoader,
			fdPath,
			settingsManager,
			requestRender: () => tui.requestRender(),
			finish,
		});
		tui.addChild(component);
		tui.setLayoutRoot(component.layoutRoot);
		tui.setFocus(component);
		tuiStarted = true;
		tui.start();
		await themeController.applyFromSettings();
		await finished;
	} finally {
		themeController.dispose();
		stopThemeWatcher();
		if (tuiStarted) tui.stop();
		await component?.close();
		await runtime.dispose();
	}
}

function requireSingleServer<T>(features: readonly T[]): T {
	if (features.length !== 1) throw new Error("Starting a Session requires exactly one server");
	return features[0]!;
}

function message(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}
