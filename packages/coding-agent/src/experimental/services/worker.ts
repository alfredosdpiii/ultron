import type { AgentHarness, AgentLane, Session } from "@ultron/agent-core";
import {
	type Context,
	createFacetHost,
	createRemoteServiceEndpoint,
	createStaticFacetLoader,
	defineFacet,
	type FacetHost,
	type FacetLoader,
	type JsonValue,
	type RemoteServiceEndpoint,
	type ServiceCall,
	type ServiceProviderUpdate,
} from "@ultron/chord";
import type { ModelRuntime } from "../../core/model-runtime.ts";
import type { SettingsManager } from "../../core/settings-manager.ts";
import { AgentController } from "./agent-controller.ts";
import { createAgentController } from "./agent-controller-provider.ts";
import { ExtensionUI } from "./extension-ui.ts";
import { ExtensionUIBridge } from "./extension-ui-provider.ts";
import { type CommandSourceInfo, LegacyExtensionCommands } from "./legacy-extensions.ts";
import { createModelsServiceFacet } from "./models-provider.ts";
import { SessionPlugins } from "./plugins.ts";
import { SessionControl } from "./session-control.ts";
import { createSessionControl, type ExtensionSessionEvents } from "./session-control-provider.ts";
import { createTranscriptServiceFacet } from "./transcript-provider.ts";

export interface SessionWorkerRuntime {
	readonly harness: AgentHarness;
	readonly closeRlm?: () => Promise<void>;
	readonly lane?: AgentLane;
	readonly modelRuntime?: ModelRuntime;
	readonly settingsManager?: SettingsManager;
	readonly facetLoader?: FacetLoader;
	readonly legacyExtensionCommands?: LegacyExtensionCommands;
	/** Extension dialogs and notifications, served to attached presentations through `ExtensionUI`. */
	readonly extensionUI?: ExtensionUIBridge;
	/** Pi `SourceInfo` of prompt templates and skills by command name, for `get_commands`. */
	readonly resourceSourceInfo?: () => ReadonlyMap<string, CommandSourceInfo>;
	/** Receives the worker's activity hold so detached background work keeps the worker alive. */
	readonly bindActivity?: (hold: () => () => void) => void;
	/** Read-only access to the RLM host, used by inspection commands. */
	readonly inspect?: (request: string, payload: Record<string, unknown>, context: Context) => Promise<unknown>;
	/** Pi's fork events in the worker's extensions. */
	readonly extensionSessionEvents?: ExtensionSessionEvents;
}

export interface WorkerServiceScope {
	readonly serverConnectionId: string;
	readonly attachmentId: string;
}

interface ScopedServiceEndpoint {
	readonly scope: WorkerServiceScope;
	readonly endpoint: RemoteServiceEndpoint;
}

export interface SessionWorkerServices {
	invoke(call: ServiceCall, scope: WorkerServiceScope, context: Context): Promise<JsonValue | undefined>;
	removeSubscriptions(matches: (scope: WorkerServiceScope) => boolean): void;
	dispose(): Promise<void>;
}

export async function createSessionWorkerServices(options: {
	readonly lane: AgentLane;
	/** Enables SessionControl; omitted by workers that do not own a harness. */
	readonly harness?: AgentHarness;
	/** The worker's Session, for whole-tree reads (all branches). */
	readonly session?: Session;
	readonly cwd?: string;
	readonly modelRuntime: ModelRuntime | undefined;
	readonly settingsManager?: SettingsManager;
	readonly facetLoader?: FacetLoader;
	readonly legacyExtensionCommands?: LegacyExtensionCommands;
	readonly extensionUI?: ExtensionUIBridge;
	readonly resourceSourceInfo?: SessionWorkerRuntime["resourceSourceInfo"];
	readonly inspect?: SessionWorkerRuntime["inspect"];
	readonly extensionSessionEvents?: ExtensionSessionEvents;
	publish(scope: WorkerServiceScope, subscriptionId: string, update: ServiceProviderUpdate): Promise<void>;
}): Promise<SessionWorkerServices> {
	const agentControllerRuntimeFacet = defineFacet({
		id: "@pi/agent-controller-runtime",
		setup(env) {
			env.provide(AgentController, createAgentController(options.lane));
		},
	});
	const legacyExtensionFacet = defineFacet({
		id: "@pi/legacy-extension-commands",
		setup(env) {
			env.provide(
				LegacyExtensionCommands,
				options.legacyExtensionCommands ?? {
					list: async () => [],
					run: async () => ({ notifications: [] }),
				},
			);
		},
	});
	const extensionUI = options.extensionUI ?? new ExtensionUIBridge();
	const extensionUIFacet = defineFacet({
		id: "@ultron/extension-ui",
		setup(env) {
			env.provide(ExtensionUI, extensionUI.service);
			env.own(() => extensionUI.close());
		},
	});
	const sessionControlFacet = defineFacet({
		id: "@ultron/session-control",
		setup(env) {
			if (options.harness === undefined || options.cwd === undefined) return;
			env.provide(
				SessionControl,
				createSessionControl({
					harness: options.harness,
					lane: options.lane,
					session: options.session,
					cwd: options.cwd,
					settingsManager: options.settingsManager,
					extensionCommands: options.legacyExtensionCommands,
					resourceSourceInfo: options.resourceSourceInfo,
					inspect: options.inspect,
					extensionSessionEvents: options.extensionSessionEvents,
					...(options.modelRuntime === undefined ? {} : { modelRuntime: options.modelRuntime }),
				}),
			);
		},
	});
	let reloadPlugins = (): Promise<void> => Promise.reject(new Error("Session plugins are not ready"));
	const pluginRuntimeFacet = defineFacet({
		id: "@pi/session-plugins-runtime",
		setup(env) {
			env.provide(SessionPlugins, { reload: () => reloadPlugins() });
		},
	});
	const builtins = await createStaticFacetLoader([
		agentControllerRuntimeFacet,
		pluginRuntimeFacet,
		legacyExtensionFacet,
		sessionControlFacet,
		extensionUIFacet,
		createModelsServiceFacet(options),
		createTranscriptServiceFacet(options.lane),
	]).load();
	const pluginLoader = options.facetLoader ?? createStaticFacetLoader([]);
	let loadedPlugins = await pluginLoader.load();
	let facetHost: FacetHost;
	try {
		facetHost = await createFacetHost({ facets: [...builtins.facets, ...loadedPlugins.facets] });
	} catch (error) {
		const cleanup = await Promise.allSettled([loadedPlugins.dispose(), builtins.dispose()]);
		const cleanupErrors = cleanup.flatMap((result) => (result.status === "rejected" ? [result.reason] : []));
		if (cleanupErrors.length > 0) {
			throw new AggregateError([error, ...cleanupErrors], "Session facets failed to start and clean up");
		}
		throw error;
	}
	let reloadTail = Promise.resolve();
	reloadPlugins = () => {
		const operation = reloadTail.then(async () => {
			const candidate = await pluginLoader.load();
			try {
				await facetHost.reload(candidate.facets);
			} catch (error) {
				try {
					await candidate.dispose();
				} catch (cleanupError) {
					throw new AggregateError([error, cleanupError], "Session plugin reload and cleanup failed");
				}
				throw error;
			}
			const retired = loadedPlugins;
			loadedPlugins = candidate;
			await retired.dispose();
		});
		reloadTail = operation.catch(() => {});
		return operation;
	};
	const provider = facetHost.services;

	const endpoints = new Map<string, ScopedServiceEndpoint>();
	const removeSubscriptions = (matches: (scope: WorkerServiceScope) => boolean): void => {
		for (const [key, entry] of endpoints) {
			if (!matches(entry.scope)) continue;
			entry.endpoint.dispose();
			endpoints.delete(key);
		}
	};

	return {
		invoke(call, scope, context) {
			const key = serviceScopeKey(scope);
			let entry = endpoints.get(key);
			if (entry === undefined) {
				entry = { scope, endpoint: createRemoteServiceEndpoint(provider) };
				endpoints.set(key, entry);
			}
			return entry.endpoint.invoke(
				call,
				(subscriptionId, update) => options.publish(scope, subscriptionId, update),
				context,
			);
		},
		removeSubscriptions,
		async dispose() {
			removeSubscriptions(() => true);
			await reloadTail;
			const errors: unknown[] = [];
			try {
				await facetHost.dispose();
			} catch (error) {
				errors.push(error);
			}
			const results = await Promise.allSettled([loadedPlugins.dispose(), builtins.dispose()]);
			errors.push(...results.flatMap((result) => (result.status === "rejected" ? [result.reason] : [])));
			if (errors.length === 1) throw errors[0];
			if (errors.length > 1) throw new AggregateError(errors, "Failed to dispose Session facets");
		},
	};
}

function serviceScopeKey(scope: WorkerServiceScope): string {
	return `${scope.serverConnectionId}\0${scope.attachmentId}`;
}
