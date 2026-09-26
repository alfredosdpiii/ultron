import type {
	AgentHarness as AgentHarnessInstance,
	AgentHarnessTool,
	AgentLane,
	AgentMessage,
	AgentToolResult,
	AgentToolUpdateCallback,
	JsonlSessionMetadata,
	Session,
} from "@ultron/agent-core";
import type { Api, Model } from "@ultron/ai";
import type { JsonValue } from "@ultron/chord";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { ExtensionRunner } from "../core/extensions/runner.ts";
import type {
	ExtensionMode,
	ExtensionUIContext,
	SessionShutdownEvent,
	SessionStartEvent,
	ToolDefinition,
	ToolInfo,
} from "../core/extensions/types.ts";
import { ModelRegistry } from "../core/model-registry.ts";
import type { ModelRuntime } from "../core/model-runtime.ts";
import type { ResourceLoader } from "../core/resource-loader.ts";
import { SessionManager } from "../core/session-manager.ts";
import { theme } from "../modes/interactive/theme/theme.ts";
import { type ExtensionToolInfo, prepareToolArguments } from "../ultron/rlm/extension-tools.ts";
import type { ExtensionUIBridge } from "./services/extension-ui-provider.ts";
import type { LegacyExtensionCommandInfo, LegacyExtensionCommandResult } from "./services/legacy-extensions.ts";

export interface LegacyExtensionAdapterOptions {
	readonly session: Session<JsonlSessionMetadata>;
	readonly lane: AgentLane;
	readonly harness: AgentHarnessInstance;
	readonly modelRuntime: ModelRuntime;
	readonly resourceLoader: ResourceLoader;
	readonly cwd: string;
	readonly model: Model<Api>;
	readonly systemPrompt: string;
	readonly onShutdown?: () => void;
	/** Routes extension dialogs and notifications to an attached presentation; headless without it. */
	readonly ui?: ExtensionUIBridge;
	/** Pi's `ctx.mode` for the client the worker was started for. Defaults to `"tui"`. */
	readonly mode?: ExtensionMode;
	/**
	 * Filters the active tool list an extension sets with `pi.setActiveTools` (extension tools that live in the REPL
	 * stay out of the model's tool list).
	 */
	readonly filterActiveTools?: (names: readonly string[]) => string[];
}

/** Why the worker's Session started, as Pi's `session_start` reports it. */
export type LegacySessionStart = Pick<SessionStartEvent, "reason" | "previousSessionFile">;

/**
 * Compatibility bridge for ordinary Pi extensions inside the native worker.
 * It deliberately exposes the established ExtensionAPI rather than inventing a
 * second extension API. Session-changing commands remain unavailable because
 * native sessions are owned by the worker/server control plane.
 */
export class LegacyExtensionAdapter {
	readonly #runner: ExtensionRunner;
	readonly #session: Session<JsonlSessionMetadata>;
	readonly #lane: AgentLane;
	readonly #harness: AgentHarnessInstance;
	readonly #cwd: string;
	readonly #model: Model<Api>;
	readonly #systemPrompt: string;
	readonly #onShutdown?: () => void;
	readonly #sessionManager: SessionManager;
	readonly #filterActiveTools: (names: readonly string[]) => string[];
	#activeTools: string[] = [];
	#shutdown: Omit<SessionShutdownEvent, "type"> = { reason: "quit" };
	#bound = false;

	constructor(options: LegacyExtensionAdapterOptions) {
		this.#session = options.session;
		this.#lane = options.lane;
		this.#harness = options.harness;
		this.#cwd = options.cwd;
		this.#model = options.model;
		this.#systemPrompt = options.systemPrompt;
		this.#onShutdown = options.onShutdown;
		this.#filterActiveTools = options.filterActiveTools ?? ((names) => [...names]);
		this.#sessionManager = createSessionManagerFacade(options.session, options.cwd);
		const extensions = options.resourceLoader.getExtensions();
		const ui = options.ui;
		this.#runner = new WorkerExtensionRunner(
			extensions.extensions,
			extensions.runtime,
			this.#cwd,
			this.#sessionManager,
			new ModelRegistry(options.modelRuntime),
			// As in Pi, `ctx.hasUI` is true only for an interactive client (the TUI or an RPC client): one that serves
			// the extension UI by polling, or that the worker was started for and that has not arrived yet.
			// Print and JSON runs never poll, so there it is false.
			ui === undefined ? () => true : () => ui.serving,
		);
		const headless = createHeadlessExtensionUI();
		this.#runner.setUIContext(
			options.ui === undefined ? headless : options.ui.createContext(headless),
			options.mode ?? "tui",
		);
		this.#runner.bindCommandContext();
	}

	get runner(): ExtensionRunner {
		return this.#runner;
	}

	get tools(): AgentHarnessTool<{ env: never }>[] {
		return this.#runner
			.getAllRegisteredTools()
			.map(({ definition }) => this.#wrapTool(definition))
			.filter((tool) => tool.name !== "rlm" && tool.name !== "ipython");
	}

	/** Extension tools callable from the REPL (every registered tool except the REPL itself), read live. */
	get replTools(): ExtensionToolInfo[] {
		return this.#runner
			.getAllRegisteredTools()
			.filter(({ definition }) => definition.name !== "rlm" && definition.name !== "ipython")
			.map(({ definition }) => ({
				name: definition.name,
				...(definition.label === undefined ? {} : { label: definition.label }),
				description: definition.description,
				parameters: definition.parameters,
			}));
	}

	/**
	 * Run extension tool `name` for a REPL call, as the harness would for a model call: arguments prepared and
	 * validated against the tool's schema, the extension context with the UI bridge, and the cell's abort signal.
	 */
	async executeTool(
		name: string,
		toolCallId: string,
		params: Record<string, unknown>,
		signal: AbortSignal,
		onUpdate: (partial: AgentToolResult<unknown>) => void,
	): Promise<AgentToolResult<unknown>> {
		const registered = this.#runner.getAllRegisteredTools().find(({ definition }) => definition.name === name);
		if (!registered) throw new Error(`Unknown extension tool "${name}"`);
		const definition = registered.definition;
		const args = prepareToolArguments(
			{
				name: definition.name,
				description: definition.description,
				parameters: definition.parameters,
				...(definition.prepareArguments === undefined
					? {}
					: { prepareArguments: definition.prepareArguments as (args: unknown) => unknown }),
			},
			params,
		);
		signal.throwIfAborted();
		return (await definition.execute(
			toolCallId,
			args,
			signal,
			onUpdate as AgentToolUpdateCallback<unknown>,
			this.#runner.createContext(),
		)) as AgentToolResult<unknown>;
	}

	get toolInfos(): ToolInfo[] {
		return this.#runner.getAllRegisteredTools().map(({ definition, sourceInfo }) => ({
			name: definition.name,
			description: definition.description,
			parameters: definition.parameters,
			...(definition.promptGuidelines === undefined ? {} : { promptGuidelines: definition.promptGuidelines }),
			sourceInfo,
		}));
	}

	get commands(): readonly LegacyExtensionCommandInfo[] {
		return this.#runner.getRegisteredCommands().map((command) => ({
			name: command.invocationName,
			description: command.description,
			// Plain JSON for the service contract: Pi leaves optional fields undefined.
			sourceInfo: JSON.parse(JSON.stringify(command.sourceInfo)),
		}));
	}

	async runCommand(name: string, args: string): Promise<LegacyExtensionCommandResult> {
		const command = this.#runner.getCommand(name);
		if (!command) throw new Error(`Unknown extension command: /${name}`);
		await command.handler(args, this.#runner.createCommandContext());
		return { notifications: [] };
	}

	bind(start: LegacySessionStart = { reason: "startup" }): void {
		if (this.#bound) return;
		this.#bound = true;
		this.#runner.bindCore(
			{
				sendMessage: () => {},
				sendUserMessage: (content) => {
					void this.#lane.followUp(
						contentToText(content as unknown as AgentMessage),
						undefined,
						BACKGROUND_CONTEXT,
					);
				},
				appendEntry: (customType, data) => {
					void this.#lane.appendCustomEntry(
						customType,
						isJsonValue(data) ? data : String(data),
						BACKGROUND_CONTEXT,
					);
				},
				setSessionName: (name) => {
					void this.#session.setName(name, BACKGROUND_CONTEXT);
				},
				getSessionName: () => undefined,
				setLabel: (entryId, label) => {
					void this.#session.setLabel(entryId, label, BACKGROUND_CONTEXT);
				},
				getActiveTools: () => [...this.#activeTools],
				getAllTools: () => this.toolInfos,
				setActiveTools: (names) => {
					this.#activeTools = this.#filterActiveTools(names);
					void this.#lane.setActiveTools(this.#activeTools, BACKGROUND_CONTEXT);
				},
				refreshTools: () => {},
				getCommands: () => [],
				setModel: async (model) => {
					await this.#lane.setModel({ provider: model.provider, modelId: model.id }, BACKGROUND_CONTEXT);
					return true;
				},
				getThinkingLevel: () => "medium",
				setThinkingLevel: (level) => {
					void this.#lane.setThinkingLevel(level, BACKGROUND_CONTEXT);
				},
			},
			{
				getModel: () => this.#model,
				getScopedModels: () => [],
				isIdle: () => true,
				isProjectTrusted: () => true,
				getSignal: () => undefined,
				abort: () => {
					void this.#lane.abort(BACKGROUND_CONTEXT);
				},
				hasPendingMessages: () => false,
				shutdown: () => this.#onShutdown?.(),
				getContextUsage: () => undefined,
				compact: (options) => {
					void this.#lane.compact(options, BACKGROUND_CONTEXT);
				},
				getSystemPrompt: () => this.#systemPrompt,
				getSystemPromptOptions: () => ({ cwd: this.#cwd }),
			},
		);
		void this.#runner.emit({
			type: "session_start",
			reason: start.reason,
			...(start.previousSessionFile === undefined ? {} : { previousSessionFile: start.previousSessionFile }),
		});
		this.installHooks();
	}

	/**
	 * Pi's `session_before_fork`: extensions may cancel a fork of this Session (`entryId`, `position` as in Pi).
	 * As in Pi, it is emitted only when an extension handles it.
	 */
	async beforeFork(entryId: string, position: "before" | "at"): Promise<{ cancelled: boolean }> {
		if (!this.#runner.hasHandlers("session_before_fork")) return { cancelled: false };
		const result = await this.#runner.emit({ type: "session_before_fork", entryId, position });
		return { cancelled: result?.cancel === true };
	}

	/**
	 * The client moved on to a fork of this Session. As Pi's runtime is replaced on a fork, the worker's extensions
	 * see `session_shutdown` with reason "fork" when the worker retires, unless the Session is used again first.
	 */
	forked(targetSessionFile: string | undefined): void {
		this.#shutdown = {
			reason: "fork",
			...(targetSessionFile === undefined ? {} : { targetSessionFile }),
		};
	}

	async close(): Promise<void> {
		await this.#runner.emit({ type: "session_shutdown", ...this.#shutdown });
		this.#runner.invalidate("Native extension worker is closed");
	}

	private installHooks(): void {
		this.#harness.hooks.on("before_run", async (event) => {
			// The Session is in use again: a later retirement is an ordinary quit.
			this.#shutdown = { reason: "quit" };
			const prompt = event.prompt.map(contentToText).join("\n");
			const result = await this.#runner.emitBeforeAgentStart(prompt, undefined, {
				cwd: this.#cwd,
				selectedTools: this.#activeTools,
				contextFiles: [],
				skills: [],
			});
			return result.messages.length === 0 ? undefined : { messages: result.messages.map(toAgentMessage) };
		});
		this.#harness.hooks.on("transform_context", async (event) => ({
			messages: await this.#runner.emitContext(event.messages),
		}));
		this.#harness.events.on("run_end", async () => {
			await this.#runner.emit({ type: "agent_end", messages: [] });
		});
	}

	#wrapTool(definition: ToolDefinition): AgentHarnessTool<{ env: never }> {
		return {
			name: definition.name,
			label: definition.label,
			description: definition.description,
			parameters: definition.parameters,
			...(definition.constrainedSampling === undefined
				? {}
				: { constrainedSampling: definition.constrainedSampling }),
			...(definition.prepareArguments === undefined ? {} : { prepareArguments: definition.prepareArguments }),
			execute: async (toolCallId, params, onUpdate, _toolContext, _invocation, context) => {
				const extensionContext = this.#runner.createContext();
				return definition.execute(
					toolCallId,
					params,
					context.abortSignal,
					onUpdate as AgentToolUpdateCallback<unknown>,
					extensionContext,
				) as Promise<AgentToolResult<unknown>>;
			},
		};
	}
}

/** Pi's runner whose `hasUI` follows whether an interactive client is attached to the worker. */
class WorkerExtensionRunner extends ExtensionRunner {
	readonly #interactive: () => boolean;

	constructor(
		extensions: ConstructorParameters<typeof ExtensionRunner>[0],
		runtime: ConstructorParameters<typeof ExtensionRunner>[1],
		cwd: string,
		sessionManager: SessionManager,
		modelRegistry: ModelRegistry,
		interactive: () => boolean,
	) {
		super(extensions, runtime, cwd, sessionManager, modelRegistry);
		this.#interactive = interactive;
	}

	override hasUI(): boolean {
		return super.hasUI() && this.#interactive();
	}
}

function createSessionManagerFacade(session: Session<JsonlSessionMetadata>, cwd: string): SessionManager {
	const facade = SessionManager.inMemory(cwd);
	const target = facade as unknown as Record<string, unknown>;
	target.getSessionFile = () => session.metadata.path;
	target.getSessionDir = () => session.metadata.path;
	target.getSessionId = () => session.metadata.id;
	target.getCwd = () => cwd;
	target.getBranch = () => [];
	target.getLeafId = () => undefined;
	return facade;
}

function createHeadlessExtensionUI(): ExtensionUIContext {
	return {
		select: async () => undefined,
		confirm: async () => false,
		input: async () => undefined,
		notify: () => {},
		onTerminalInput: () => () => {},
		setStatus: () => {},
		setWorkingMessage: () => {},
		setWorkingVisible: () => {},
		setWorkingIndicator: () => {},
		setHiddenThinkingLabel: () => {},
		setWidget: () => {},
		setFooter: () => {},
		setHeader: () => {},
		setTitle: () => {},
		custom: async () => undefined as never,
		pasteToEditor: () => {},
		setEditorText: () => {},
		getEditorText: () => "",
		editor: async () => undefined,
		addAutocompleteProvider: () => {},
		setEditorComponent: () => {},
		getEditorComponent: () => undefined,
		getAllThemes: () => [],
		getTheme: () => undefined,
		setTheme: () => ({ success: false, error: "Native worker UI adapter does not switch themes" }),
		getToolsExpanded: () => false,
		setToolsExpanded: () => {},
		get theme() {
			return theme;
		},
	};
}

function contentToText(value: AgentMessage | { role?: string; content?: unknown } | string): string {
	if (typeof value === "string") return value;
	if (typeof value !== "object" || value === null || !("content" in value)) return "";
	const content = value.content;
	if (typeof content === "string") return content;
	if (!Array.isArray(content)) return "";
	return content
		.filter(
			(part: unknown): part is { type: "text"; text: string } =>
				typeof part === "object" &&
				part !== null &&
				"type" in part &&
				part.type === "text" &&
				"text" in part &&
				typeof part.text === "string",
		)
		.map((part) => part.text)
		.join("\n");
}

function toAgentMessage(message: { customType: string; content: unknown; display: boolean }): AgentMessage {
	return {
		role: "user",
		content: [{ type: "text", text: contentToText(message.content as string) }],
		timestamp: Date.now(),
	};
}

function isJsonValue(value: unknown): value is JsonValue {
	if (value === null || typeof value === "string" || typeof value === "boolean" || typeof value === "number")
		return true;
	if (Array.isArray(value)) return value.every(isJsonValue);
	if (typeof value !== "object") return false;
	return Object.values(value).every(isJsonValue);
}
