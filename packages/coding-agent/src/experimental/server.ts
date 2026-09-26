import { randomUUID } from "node:crypto";
import { chmod, lstat, mkdir, mkdtemp, readdir, readFile, rm, unlink, writeFile } from "node:fs/promises";
import { createConnection } from "node:net";
import { homedir, tmpdir } from "node:os";
import { isAbsolute, join, resolve } from "node:path";
import { BACKGROUND_CONTEXT, type JsonlSessionMetadata, JsonlSessionRepo, TODO_CONTEXT } from "@ultron/agent-core";
import { NodeExecutionEnv } from "@ultron/agent-core/node";
import type { Context } from "@ultron/chord";
import type { FacetBundleArtifact } from "@ultron/chord/node";
import { Client, ServerError as ClientServerError, DisconnectedError } from "@ultron/client";
import { createUnixTransportFactory, type UnixServerRoute } from "@ultron/client/unix";
import { isServerId, type ServerId } from "@ultron/protocol";
import {
	ServerError as RoutedServerError,
	type Server,
	type ServerHost,
	SessionAmbiguousError,
	SessionNotFoundError,
} from "@ultron/server";
import { createUnixServer, getUnixSocketPath } from "@ultron/server/unix";
import lockfile from "proper-lockfile";
import type { AuthInput } from "../cli/experimental/command-options.ts";
import { ENV_SESSION_DIR, getAgentDir } from "../config.ts";
import type { ExtensionMode } from "../core/extensions/types.ts";
import { importPiSession, PiSessionAlreadyImportedError } from "../ultron/migration.ts";
import { resolvePath } from "../utils/paths.ts";
import { CoordinatorConnection, type CoordinatorStartupLease, ensureCoordinator } from "./coordinator.ts";
import { createPresentationFacetData } from "./plugins/bundled.ts";
import {
	createServerPluginPackage,
	normalizePluginPackagePaths,
	readSessionPluginPackageProfile,
	removeSessionPluginPackageProfile,
	restoreServerPluginPackageProfile,
	writeSessionPluginPackageProfile,
} from "./plugins/package.ts";
import {
	consumeInternalProcessRole,
	isDirectInternalProcessEntry,
	spawnInternalProcess,
	terminateInternalProcess,
} from "./process.ts";
import { RadiusRelayAuthResolver } from "./radius-auth.ts";
import { RadiusRelayHost, type RadiusRelayHostStatus } from "./radius-relay.ts";
import { recordServerError, serverErrorLogPath } from "./server-log.ts";
import { createExperimentalServerServices } from "./services/server.ts";
import type { PiSessionImport, SessionCreateOptions, SessionListing, SessionSummary } from "./services/sessions.ts";
import { readNativeSessionDetails } from "./session-listing.ts";
import { forkedSessionStart } from "./session-start.ts";
import { SessionPluginSelectionConflictError, SessionWorkerManager } from "./session-worker-manager.ts";
import { traceStartup } from "./startup-trace.ts";

export const ENV_SERVER_DIR = "ULTRON_SERVER_DIR";
export const ENV_SERVER_ID = "ULTRON_SERVER_ID";
const LEGACY_ENV_SERVER_DIR = "PI_SERVER_DIR";
const LEGACY_ENV_SERVER_ID = "PI_SERVER_ID";

export function resolveServerDirectory(directory?: string): string {
	return resolvePath(directory ?? process.env[ENV_SERVER_DIR] ?? join(homedir(), ".ultron", "server"));
}

export async function ensurePrivateServerDirectory(directory: string): Promise<void> {
	if (typeof process.getuid !== "function") throw new Error("Unix socket directory requires a POSIX user ID");
	await mkdir(directory, { recursive: true, mode: 0o700 });
	const stats = await lstat(directory);
	if (!stats.isDirectory()) throw new Error(`Unix socket directory is not a directory: ${directory}`);
	if (stats.uid !== process.getuid()) {
		throw new Error(`Unix socket directory is not owned by the current user: ${directory}`);
	}
	await chmod(directory, 0o700);
}

export function resolveSessionDirectory(sessionDir?: string): string {
	return resolvePath(sessionDir ?? process.env[ENV_SESSION_DIR] ?? join(getAgentDir(), "experimental", "sessions"));
}

const LOCK_STALE_MS = 30_000;
const LOCK_RETRY_MS = 25;
const LOCK_WAIT_MS = 30_000;
const DEFAULT_SERVER_ID_FILE = "default-server-id";

export interface ServerProfile {
	readonly serverId: ServerId;
	release(): Promise<void>;
}

/** Lock one logical server ID in a shared experimental server directory. */
export async function acquireServerProfile(directory: string, requestedServerId?: string): Promise<ServerProfile> {
	await mkdir(directory, { recursive: true, mode: 0o700 });
	let serverId: ServerId;
	if (requestedServerId !== undefined) {
		if (!isServerId(requestedServerId)) throw new Error(`Invalid experimental server ID: ${requestedServerId}`);
		serverId = requestedServerId;
	} else {
		const path = join(directory, DEFAULT_SERVER_ID_FILE);
		try {
			const value = (await readFile(path, "utf8")).trim();
			if (!isServerId(value)) throw new Error(`Invalid default experimental server identity in ${path}`);
			serverId = value;
		} catch (error) {
			const code = error instanceof Error && "code" in error ? error.code : undefined;
			if (code !== "ENOENT") throw error;
			const candidate = randomUUID();
			try {
				await writeFile(path, candidate, { encoding: "utf8", mode: 0o600, flag: "wx" });
				serverId = candidate;
			} catch (writeError) {
				const writeCode = writeError instanceof Error && "code" in writeError ? writeError.code : undefined;
				if (writeCode !== "EEXIST") throw writeError;
				const value = (await readFile(path, "utf8")).trim();
				if (!isServerId(value)) throw new Error(`Invalid default experimental server identity in ${path}`);
				serverId = value;
			}
		}
	}

	const release = await lockfile.lock(join(directory, `launcher-${serverId}`), {
		realpath: false,
		stale: LOCK_STALE_MS,
		update: LOCK_STALE_MS / 3,
		retries: {
			retries: Math.ceil(LOCK_WAIT_MS / LOCK_RETRY_MS),
			factor: 1,
			minTimeout: LOCK_RETRY_MS,
			maxTimeout: LOCK_RETRY_MS,
			maxRetryTime: LOCK_WAIT_MS,
		},
	});
	return { serverId, release };
}

const ACTIVATION_TIMEOUT_MS = 10_000;
const ACTIVATION_RETRY_MS = 10;

export interface ActivatedServer {
	readonly client: Client;
	readonly route: UnixServerRoute;
}

export interface ActivateServerOptions {
	readonly directory: string;
	readonly requestedServerId?: ServerId | string;
	readonly sessionDir: string;
	readonly provider?: string;
	readonly model?: string;
}

/** Ensure the selected logical server is reachable, launching the current Pi installation if needed. */
export async function activateServer(options: ActivateServerOptions): Promise<ActivatedServer> {
	if (options.provider !== undefined && options.model === undefined) {
		throw new Error("Server model provider requires a model");
	}
	await ensurePrivateServerDirectory(options.directory);
	const profile = await acquireServerProfile(options.directory, options.requestedServerId);
	const serverId = profile.serverId;
	await profile.release();
	const route = { serverId, path: getUnixSocketPath(serverId, options.directory) };
	const release = await acquireServerActivation(options.directory, serverId);
	try {
		const existing = await connect(route);
		if (existing) {
			// Another activator won the race, so startup-only selections can no longer be applied.
			if (options.model !== undefined) {
				await existing.dispose();
				throw new Error("Model selection is only valid when automatically activating a new server");
			}
			return { client: existing, route };
		}
		const modelArgs =
			options.model === undefined
				? []
				: [
						JSON.stringify({
							...(options.provider === undefined ? {} : { provider: options.provider }),
							model: options.model,
						}),
					];
		const child = spawnInternalProcess("server", [options.directory, serverId, options.sessionDir, ...modelArgs]);
		let spawnError: Error | undefined;
		child.once("error", (error) => {
			spawnError = error;
		});
		try {
			const deadline = Date.now() + ACTIVATION_TIMEOUT_MS;
			while (true) {
				const client = await connect(route);
				if (client) return { client, route };
				if (spawnError) throw new Error("Failed to automatically activate server", { cause: spawnError });
				if (child.exitCode !== null || child.signalCode !== null) {
					throw new Error("Automatically activated server exited during startup");
				}
				if (Date.now() >= deadline) throw new Error("Timed out waiting for automatically activated server");
				await new Promise<void>((resolve) => setTimeout(resolve, ACTIVATION_RETRY_MS));
			}
		} catch (error) {
			await terminateInternalProcess(child);
			throw error;
		}
	} finally {
		await release();
	}
}

export function acquireServerActivation(directory: string, serverId: ServerId): Promise<() => Promise<void>> {
	return lockfile.lock(join(directory, `activation-${serverId}`), {
		realpath: false,
		stale: ACTIVATION_TIMEOUT_MS * 2,
		update: ACTIVATION_TIMEOUT_MS,
		retries: {
			retries: Math.ceil(ACTIVATION_TIMEOUT_MS / 25),
			factor: 1,
			minTimeout: 25,
			maxTimeout: 25,
			maxRetryTime: ACTIVATION_TIMEOUT_MS,
		},
	});
}

async function connect(route: UnixServerRoute): Promise<Client | undefined> {
	const client = new Client({
		serverId: route.serverId,
		transportFactory: createUnixTransportFactory({ path: route.path }),
	});
	try {
		await client.connect();
		return client;
	} catch (error) {
		await client.dispose();
		if (error instanceof DisconnectedError || (error instanceof ClientServerError && error.code === "version")) {
			return undefined;
		}
		let current = error;
		const seen = new Set<unknown>();
		while (current instanceof Error && !seen.has(current)) {
			seen.add(current);
			if (
				"code" in current &&
				["ENOENT", "ECONNREFUSED", "ECONNRESET", "EPIPE", "ETIMEDOUT"].includes(String(current.code))
			) {
				return undefined;
			}
			current = current.cause;
		}
		throw error;
	}
}

const AUTO_SERVER_STARTUP_GRACE_MS = 10_000;
const AUTO_SERVER_IDLE_GRACE_MS = 1_000;

/** Reconcile operator, startup, client, and worker holds for one server generation. */
export class ServerLifetime {
	readonly #keepAlive: boolean;
	#connectionCount = 0;
	#workerCount = 0;
	#startupHeld: boolean;
	#startupTimer: NodeJS.Timeout | undefined;
	#retirementTimer: NodeJS.Timeout | undefined;
	#retire: (() => void) | undefined;
	#stopped = false;

	constructor(keepAlive: boolean) {
		this.#keepAlive = keepAlive;
		this.#startupHeld = !keepAlive;
	}

	start(retire: () => void): void {
		this.#retire = retire;
		if (this.#startupHeld) {
			this.#startupTimer = setTimeout(() => {
				this.#startupTimer = undefined;
				this.#startupHeld = false;
				this.#reconcile();
			}, AUTO_SERVER_STARTUP_GRACE_MS);
			this.#startupTimer.unref();
		}
		this.#reconcile();
	}

	setConnectionCount(count: number): void {
		this.#connectionCount = count;
		if (count > 0 && this.#startupHeld) {
			this.#startupHeld = false;
			if (this.#startupTimer) clearTimeout(this.#startupTimer);
			this.#startupTimer = undefined;
		}
		this.#reconcile();
	}

	setWorkerCount(count: number): void {
		this.#workerCount = count;
		this.#reconcile();
	}

	stop(): void {
		this.#stopped = true;
		if (this.#startupTimer) clearTimeout(this.#startupTimer);
		if (this.#retirementTimer) clearTimeout(this.#retirementTimer);
		this.#startupTimer = undefined;
		this.#retirementTimer = undefined;
	}

	#reconcile(): void {
		if (
			this.#stopped ||
			this.#keepAlive ||
			this.#startupHeld ||
			this.#connectionCount !== 0 ||
			this.#workerCount !== 0
		) {
			if (this.#retirementTimer) clearTimeout(this.#retirementTimer);
			this.#retirementTimer = undefined;
			return;
		}
		const retire = this.#retire;
		if (this.#retirementTimer || !retire) return;
		this.#retirementTimer = setTimeout(() => {
			this.#retirementTimer = undefined;
			if (!this.#stopped && !this.#startupHeld && this.#connectionCount === 0 && this.#workerCount === 0) {
				retire();
			}
		}, AUTO_SERVER_IDLE_GRACE_MS);
		this.#retirementTimer.unref();
	}
}

export interface RunningServer {
	readonly serverId: string;
	readonly sessionDir: string;
	readonly socketPath: string;
	readonly server: Server;
	readonly workerPids: ReadonlyMap<string, number>;
	readonly closed: Promise<void>;
	close(): Promise<void>;
}

export interface StartServerOptions {
	/** Server profile and socket directory. Defaults to ULTRON_SERVER_DIR or ~/.ultron/server. */
	readonly directory?: string;
	/** Logical service ID. Defaults to PI_SERVER_ID or the directory's default-server-id. */
	readonly serverId?: ServerId;
	/** Durable session directory. Defaults to the experimental directory under the configured agent directory. */
	readonly sessionDir?: string;
	/** Optional provider for an explicitly selected Session worker model. */
	readonly provider?: string;
	/** Optional model override for newly started Session workers. */
	readonly model?: string;
	/** Runtime API key override for the selected model. */
	readonly apiKey?: string;
	/** Runtime thinking level override. */
	readonly thinking?: string;
	/** Runtime system prompt override. */
	readonly systemPrompt?: string;
	readonly noTools?: "all" | "builtin";
	readonly tools?: readonly string[];
	readonly excludeTools?: readonly string[];
	/** Pi `-e` extension paths for new Session workers (resolved against the client's cwd). */
	readonly extensionPaths?: readonly string[];
	/** Pi `--no-extensions` for new Session workers. */
	readonly noExtensions?: boolean;
	/** Pi's extension mode of this server's client ("tui", "rpc", "print", "json") for new Session workers. */
	readonly extensionMode?: ExtensionMode;
	/** Hold the server open without client or Session demand. Defaults to true for foreground servers. */
	readonly keepAlive?: boolean;
	/** Optional explicit Radius credential. Stored Radius auth is used when omitted. */
	readonly relayAuth?: AuthInput;
	/** Explicit plugin packages. Undefined restores the logical server profile; an empty list clears it. */
	readonly pluginPackages?: readonly string[];
	readonly onRelayStatus?: (status: RadiusRelayHostStatus) => void;
}

interface ResolvedSessionPlugins {
	readonly packagePaths: readonly string[];
	readonly manifestPaths: readonly string[];
	readonly presentationArtifacts: readonly FacetBundleArtifact[];
}

interface StartServerBackendOptions {
	readonly path: string;
	readonly serverId: ServerId;
	/** Where internal errors are recorded; see serverErrorLogPath. */
	readonly errorLogPath: string;
	readonly sessionDir?: string;
	resolveSessionPlugins(
		metadata: JsonlSessionMetadata,
		packagePaths: readonly string[] | undefined,
		context: Context,
	): Promise<ResolvedSessionPlugins>;
	removeSessionPlugins(metadata: JsonlSessionMetadata): Promise<void>;
	reloadPresentationFacetBundles(packagePaths: readonly string[]): Promise<readonly FacetBundleArtifact[]>;
}

interface RunningServerBackend extends RunningServer {
	refreshSessions(): Promise<void>;
}

async function startServerBackend(
	options: StartServerBackendOptions,
	workers: SessionWorkerManager,
	onConnectionCountChanged?: (count: number) => void,
): Promise<RunningServerBackend> {
	const serverId = options.serverId;
	const sessionDir = resolveSessionDirectory(options.sessionDir);
	const executionEnv = new NodeExecutionEnv({ cwd: process.cwd() });
	const repo = new JsonlSessionRepo({ fileSystem: executionEnv, sessionsRoot: sessionDir });
	const listSessions = async (context: Context): Promise<JsonlSessionMetadata[]> => {
		const sessions = new Map((await repo.list(undefined, context)).map((metadata) => [metadata.path, metadata]));
		for (const metadata of workers.trackedSessions) sessions.set(metadata.path, metadata);
		return [...sessions.values()];
	};
	const resolveSession = async (sessionId: string, context: Context): Promise<JsonlSessionMetadata> => {
		const matches = (await listSessions(context)).filter((metadata) => metadata.id === sessionId);
		if (matches.length === 0) throw new SessionNotFoundError(`Unknown session: ${sessionId}`);
		if (matches.length > 1) throw new SessionAmbiguousError();
		return matches[0]!;
	};
	const createSession = async (
		createOptions: SessionCreateOptions,
		context: Context,
	): Promise<JsonlSessionMetadata> => {
		if (createOptions.forkFromSessionId !== undefined) {
			const source = await resolveSession(createOptions.forkFromSessionId, context);
			const forkPath = createOptions.forkPath;
			const session = await repo.fork(
				source,
				forkPath === undefined
					? { scope: "tree", id: createOptions.id }
					: {
							scope: "branch",
							branch: "main",
							...(forkPath.entryId === undefined ? {} : { entryId: forkPath.entryId, anyTreeEntry: true }),
							position: forkPath.position,
							id: createOptions.id,
						},
				context,
			);
			try {
				if (forkPath !== undefined) {
					await session.setValue(forkedSessionStart, { previousSessionFile: source.path }, context);
				}
				if (createOptions.name !== undefined) await session.setName(createOptions.name, context);
				return session.metadata;
			} finally {
				await session.close(context);
			}
		}
		const session = await repo.create(
			{
				cwd: process.cwd(),
				id: createOptions.id,
				parentSessionId: createOptions.parentSessionId,
			},
			context,
		);
		if (createOptions.name !== undefined) await session.setName(createOptions.name, context);
		try {
			return session.metadata;
		} finally {
			await session.close(context);
		}
	};
	const summarize = (metadata: JsonlSessionMetadata): SessionSummary => ({
		serverId,
		sessionId: metadata.id,
		createdAt: metadata.createdAt,
		modifiedAt: metadata.modifiedAt,
	});
	const closeStorage = async (): Promise<void> => {
		const cleanup = await Promise.allSettled([repo.close(TODO_CONTEXT), executionEnv.cleanup(TODO_CONTEXT)]);
		const errors = cleanup.flatMap((result) => (result.status === "rejected" ? [result.reason] : []));
		if (errors.length === 1) throw errors[0];
		if (errors.length > 1) throw new AggregateError(errors, "Experimental session storage cleanup failed");
	};
	const serverServices = await createExperimentalServerServices({
		list: async (context) =>
			(await listSessions(context))
				.map(summarize)
				.sort((left, right) => left.sessionId.localeCompare(right.sessionId) || left.createdAt - right.createdAt),
		describe: async (cwd, context) => {
			const wanted = cwd === null ? undefined : resolve(cwd);
			const sessions = (await listSessions(context)).filter(
				(metadata) => wanted === undefined || resolve(metadata.cwd) === wanted,
			);
			return Promise.all(
				sessions.map(async (metadata): Promise<SessionListing> => {
					const details = await readNativeSessionDetails(metadata.path);
					return {
						...summarize(metadata),
						cwd: metadata.cwd,
						sessionFile: metadata.path,
						name: details.name,
						firstMessage: details.firstMessage,
						messageCount: details.messageCount,
						parentSessionId: metadata.parentSessionId ?? null,
					};
				}),
			);
		},
		importPi: async (source: PiSessionImport, context) => {
			// The client sends the file's text: it may live on another machine than this server.
			const directory = await mkdtemp(join(tmpdir(), "ultron-pi-import-"));
			try {
				const piSessionPath = join(directory, "session.jsonl");
				await writeFile(piSessionPath, source.content, { mode: 0o600 });
				try {
					const result = await importPiSession({ piSessionPath, sessionsRoot: sessionDir });
					return {
						session: summarize(await resolveSession(result.sessionId, context)),
						alreadyImported: false,
						imported: result.imported,
					};
				} catch (error) {
					if (!(error instanceof PiSessionAlreadyImportedError)) throw error;
					return {
						session: summarize(await resolveSession(error.sessionId, context)),
						alreadyImported: true,
						imported: 0,
					};
				}
			} finally {
				await rm(directory, { recursive: true, force: true });
			}
		},
		create: async (createOptions, context) => {
			const created = await createSession(createOptions, context);
			return { ...summarize(created), sessionFile: created.path };
		},
		remove: async (sessionId, context) => {
			const metadata = await resolveSession(sessionId, context);
			await workers.closeSession(metadata, context);
			await repo.delete(metadata, context);
			await options.removeSessionPlugins(metadata);
		},
		rename: async (sessionId, name, context) => {
			const metadata = await resolveSession(sessionId, context);
			// A running worker owns the session file; a second writer would interleave entries.
			if (workers.workerPids.has(metadata.id)) {
				throw new RoutedServerError("service_invalid_value", `Session ${sessionId} is open in a worker`);
			}
			const session = await repo.open(metadata, context);
			try {
				await session.setName(name, context);
			} finally {
				await session.close(context);
			}
		},
		async prepareSessionPlugins(sessionId, packagePaths, context) {
			const metadata = await resolveSession(sessionId, context);
			let selected: ResolvedSessionPlugins;
			try {
				selected = await options.resolveSessionPlugins(metadata, packagePaths, context);
				workers.assertSessionPluginManifestPaths(metadata, selected.manifestPaths);
			} catch (error) {
				if (error instanceof SessionPluginSelectionConflictError) {
					throw new RoutedServerError("service_invalid_value", error.message);
				}
				throw error;
			}
			return {
				packagePaths: selected.packagePaths,
				presentationPlugins: createPresentationFacetData(selected.presentationArtifacts),
			};
		},
		async reloadPresentationPlugins(packagePaths) {
			return createPresentationFacetData(await options.reloadPresentationFacetBundles(packagePaths));
		},
	}).catch(async (error: unknown) => {
		try {
			await closeStorage();
		} catch (cleanupError) {
			throw new AggregateError([error, cleanupError], "Server service startup and cleanup failed");
		}
		throw error;
	});
	const host: ServerHost<JsonlSessionMetadata> = {
		serverServices: serverServices.host,
		resolveSession,
		openSession: async (metadata, context) => {
			const selected = await options.resolveSessionPlugins(metadata, undefined, context);
			return workers.openSession(metadata, context, selected.manifestPaths);
		},
	};
	const socketPath = options.path;
	const closeCatalog = async (): Promise<void> => {
		const cleanup = await Promise.allSettled([serverServices.dispose(), closeStorage()]);
		const errors = cleanup.flatMap((result) => (result.status === "rejected" ? [result.reason] : []));
		if (errors.length === 1) throw errors[0];
		if (errors.length > 1) throw new AggregateError(errors, "Experimental session catalog cleanup failed");
	};
	const server = createUnixServer(host, {
		serverId,
		path: socketPath,
		mode: 0o600,
		onConnectionCountChanged,
		onError: (error) => recordServerError(options.errorLogPath, "[ultron server error]", error),
	});
	try {
		await server.start();
	} catch (error) {
		const cleanup = await Promise.allSettled([server.close(), closeCatalog()]);
		const cleanupErrors = cleanup.flatMap((result) => (result.status === "rejected" ? [result.reason] : []));
		if (cleanupErrors.length > 0) {
			throw new AggregateError([error, ...cleanupErrors], "Experimental server startup and cleanup failed");
		}
		throw error;
	}

	let closePromise: Promise<void> | undefined;
	const closed = server.closed.then(
		() => closeCatalog(),
		async (serverError: unknown) => {
			try {
				await closeCatalog();
			} catch (repoError) {
				throw new AggregateError([serverError, repoError], "Server and repository shutdown failed");
			}
			throw serverError;
		},
	);
	return {
		serverId,
		sessionDir,
		socketPath,
		server,
		workerPids: workers.workerPids,
		closed,
		refreshSessions: () => serverServices.refresh(BACKGROUND_CONTEXT),
		close() {
			closePromise ??= server.close().then(
				() => closed,
				() => closed,
			);
			return closePromise;
		},
	};
}

/** Start a replaceable experimental server behind the stable coordinator endpoint. */
export async function startServer(options: StartServerOptions = {}): Promise<RunningServer> {
	if (options.provider !== undefined && options.model === undefined) {
		throw new Error("Server model provider requires a model");
	}
	const workerModel =
		options.model === undefined &&
		options.provider === undefined &&
		options.apiKey === undefined &&
		options.thinking === undefined &&
		options.systemPrompt === undefined &&
		options.noTools === undefined &&
		options.tools === undefined &&
		options.excludeTools === undefined &&
		options.extensionPaths === undefined &&
		options.noExtensions === undefined &&
		options.extensionMode === undefined
			? undefined
			: {
					...(options.provider === undefined ? {} : { provider: options.provider }),
					...(options.model === undefined ? {} : { model: options.model }),
					...(options.apiKey === undefined ? {} : { apiKey: options.apiKey }),
					...(options.thinking === undefined ? {} : { thinking: options.thinking }),
					...(options.systemPrompt === undefined ? {} : { systemPrompt: options.systemPrompt }),
					...(options.noTools === undefined ? {} : { noTools: options.noTools }),
					...(options.tools === undefined ? {} : { tools: options.tools }),
					...(options.excludeTools === undefined ? {} : { excludeTools: options.excludeTools }),
					...(options.extensionPaths === undefined ? {} : { extensionPaths: options.extensionPaths }),
					...(options.noExtensions === undefined ? {} : { noExtensions: options.noExtensions }),
					...(options.extensionMode === undefined ? {} : { extensionMode: options.extensionMode }),
				};
	const directory = resolveServerDirectory(options.directory ?? process.env[LEGACY_ENV_SERVER_DIR]);
	const { serverId, release } = await acquireServerProfile(
		directory,
		options.serverId ?? process.env[ENV_SERVER_ID] ?? process.env[LEGACY_ENV_SERVER_ID],
	);
	const lifetime = new ServerLifetime(options.keepAlive ?? true);
	let backend: RunningServerBackend | undefined;
	let coordinator: CoordinatorConnection | undefined;
	let startupLease: CoordinatorStartupLease | undefined;
	let workers: SessionWorkerManager | undefined;
	let relay: RadiusRelayHost | undefined;
	let released = false;
	try {
		await ensurePrivateServerDirectory(directory);
		const pluginPackagePaths = await restoreServerPluginPackageProfile(directory, serverId, options.pluginPackages);
		const pluginPackages = new Map<string, ReturnType<typeof createServerPluginPackage>>();
		const getPluginPackage = (packagePath: string): ReturnType<typeof createServerPluginPackage> => {
			let plugin = pluginPackages.get(packagePath);
			if (plugin === undefined) {
				plugin = createServerPluginPackage(directory, serverId, packagePath);
				pluginPackages.set(packagePath, plugin);
			}
			return plugin;
		};
		const buildPluginSelection = async (packagePaths: readonly string[]): Promise<ResolvedSessionPlugins> => {
			const normalizedPackagePaths = normalizePluginPackagePaths(packagePaths);
			const plugins = normalizedPackagePaths.map(getPluginPackage);
			const built = (await Promise.all(plugins.map((plugin) => plugin.build()))).flat();
			return {
				packagePaths: normalizedPackagePaths,
				manifestPaths: plugins.map((plugin) => plugin.manifestPath),
				presentationArtifacts: built,
			};
		};
		let defaultPluginSelection = await buildPluginSelection(pluginPackagePaths);
		const sessionPluginSelections = new Map<string, ResolvedSessionPlugins>();
		const resolveSessionPlugins = async (
			metadata: JsonlSessionMetadata,
			requestedPackagePaths: readonly string[] | undefined,
			_context: Context,
		): Promise<ResolvedSessionPlugins> => {
			if (requestedPackagePaths !== undefined) {
				const normalizedPackagePaths = normalizePluginPackagePaths(requestedPackagePaths);
				const current = sessionPluginSelections.get(metadata.path);
				if (current !== undefined && sameStrings(current.packagePaths, normalizedPackagePaths)) return current;
				const requestedManifestPaths = normalizedPackagePaths.map(
					(packagePath) => getPluginPackage(packagePath).manifestPath,
				);
				workers?.assertSessionPluginManifestPaths(metadata, requestedManifestPaths);
				const candidate = await buildPluginSelection(normalizedPackagePaths);
				workers?.assertSessionPluginManifestPaths(metadata, candidate.manifestPaths);
				await writeSessionPluginPackageProfile(directory, serverId, metadata.path, candidate.packagePaths);
				sessionPluginSelections.set(metadata.path, candidate);
				return candidate;
			}
			const cached = sessionPluginSelections.get(metadata.path);
			if (cached !== undefined) return cached;
			const storedPackagePaths = await readSessionPluginPackageProfile(directory, serverId, metadata.path);
			const selected =
				storedPackagePaths === undefined ? defaultPluginSelection : await buildPluginSelection(storedPackagePaths);
			if (storedPackagePaths === undefined) {
				await writeSessionPluginPackageProfile(directory, serverId, metadata.path, selected.packagePaths);
			}
			sessionPluginSelections.set(metadata.path, selected);
			return selected;
		};
		const removeSessionPlugins = async (metadata: JsonlSessionMetadata): Promise<void> => {
			sessionPluginSelections.delete(metadata.path);
			await removeSessionPluginPackageProfile(directory, serverId, metadata.path);
		};
		const reloadPresentationFacetBundles = async (
			packagePaths: readonly string[],
		): Promise<readonly FacetBundleArtifact[]> => {
			const reloaded = await buildPluginSelection(packagePaths);
			if (sameStrings(defaultPluginSelection.packagePaths, reloaded.packagePaths)) {
				defaultPluginSelection = reloaded;
			}
			for (const [sessionPath, selected] of sessionPluginSelections) {
				if (sameStrings(selected.packagePaths, reloaded.packagePaths)) {
					sessionPluginSelections.set(sessionPath, reloaded);
				}
			}
			return reloaded.presentationArtifacts;
		};
		const socketPath = getUnixSocketPath(serverId, directory);
		const controlPath = join(directory, `control-${serverId}.sock`);
		const serverNonce = randomUUID().replaceAll("-", "").slice(0, 12);
		const serverPath = join(directory, `server-${serverId}-${serverNonce}.sock`);
		await removeDeadServerEndpoints(directory, serverId);
		traceStartup("server.coordinator");
		startupLease = await ensureCoordinator(socketPath, controlPath);
		traceStartup("server.coordinator-ready");
		coordinator = new CoordinatorConnection({ controlPath, endpoint: serverPath });
		const sessionDir = resolveSessionDirectory(options.sessionDir);
		workers = new SessionWorkerManager(coordinator, sessionDir, workerModel, (count) =>
			lifetime.setWorkerCount(count),
		);
		backend = await startServerBackend(
			{
				path: serverPath,
				serverId,
				errorLogPath: serverErrorLogPath(directory, serverId),
				sessionDir: options.sessionDir,
				resolveSessionPlugins,
				removeSessionPlugins,
				reloadPresentationFacetBundles,
			},
			workers,
			(count) => lifetime.setConnectionCount(count),
		);
		await coordinator.connect();
		startupLease.close();
		traceStartup("server.connected");
		startupLease = undefined;
		await workers.discover(coordinator.peerIds);
		await backend.refreshSessions();
		relay = new RadiusRelayHost({
			serverId,
			server: backend.server,
			auth: new RadiusRelayAuthResolver(options.relayAuth),
			onStatus: options.onRelayStatus,
		});
		relay.start();

		const activeBackend = backend;
		const activeCoordinator = coordinator;
		const activeWorkers = workers;
		const activeRelay = relay;
		void activeCoordinator.replaced
			.then(async () => {
				lifetime.stop();
				activeWorkers.detach();
				await activeRelay.close();
				await activeBackend.close();
			})
			.finally(() => activeCoordinator.close())
			.catch(() => {});
		let closePromise: Promise<void> | undefined;
		const runtime: RunningServer = {
			serverId,
			sessionDir: activeBackend.sessionDir,
			socketPath,
			server: activeBackend.server,
			workerPids: activeWorkers.workerPids,
			closed: activeBackend.closed.finally(() => activeRelay.close()),
			close() {
				lifetime.stop();
				closePromise ??= (async () => {
					if (!activeCoordinator.wasReplaced) activeWorkers.prepareShutdown();
					try {
						await activeRelay.close();
						await activeBackend.close();
					} finally {
						try {
							if (activeCoordinator.wasReplaced) activeWorkers.detach();
							else await activeWorkers.shutdown();
						} finally {
							activeCoordinator.close();
						}
					}
				})();
				return closePromise;
			},
		};
		lifetime.start(() => {
			void runtime.close().catch(() => {});
		});
		released = true;
		await release();
		return runtime;
	} catch (error) {
		lifetime.stop();
		startupLease?.close();
		if (coordinator?.wasReplaced) workers?.detach();
		const cleanup = await Promise.allSettled([
			relay?.close(),
			backend?.close(),
			coordinator?.wasReplaced ? undefined : workers?.shutdown(),
			Promise.resolve(coordinator?.close()),
			released ? undefined : release(),
		]);
		const cleanupErrors = cleanup.flatMap((result) => (result.status === "rejected" ? [result.reason] : []));
		if (cleanupErrors.length > 0) {
			throw new AggregateError([error, ...cleanupErrors], "Server runtime startup and cleanup failed");
		}
		throw error;
	}
}

/** Start an operator-held server while serializing against automatic cold activation. */
export async function startForegroundServer(
	options: Omit<StartServerOptions, "keepAlive"> = {},
): Promise<RunningServer> {
	const directory = resolveServerDirectory(options.directory);
	await ensurePrivateServerDirectory(directory);
	const profile = await acquireServerProfile(directory, options.serverId ?? process.env[ENV_SERVER_ID]);
	const serverId = profile.serverId;
	await profile.release();
	const release = await acquireServerActivation(directory, serverId);
	try {
		return await startServer({
			...options,
			directory,
			serverId,
			keepAlive: true,
		});
	} finally {
		await release();
	}
}

/**
 * A hard-killed server leaves its private endpoint socket behind. Remove this server ID's endpoints that no longer
 * accept connections; a live one (a server being replaced) is left alone.
 */
async function removeDeadServerEndpoints(directory: string, serverId: string): Promise<void> {
	const prefix = `server-${serverId}-`;
	const names = (await readdir(directory)).filter((name) => name.startsWith(prefix) && name.endsWith(".sock"));
	await Promise.all(
		names.map(async (name) => {
			const path = join(directory, name);
			const live = await new Promise<boolean>((resolve) => {
				const socket = createConnection(path);
				socket.once("connect", () => {
					socket.destroy();
					resolve(true);
				});
				socket.once("error", (error: NodeJS.ErrnoException) => {
					socket.destroy();
					resolve(error.code !== "ECONNREFUSED" && error.code !== "ENOENT");
				});
			});
			if (!live) await unlink(path).catch(() => {});
		}),
	);
}

function parseServerModelOptions(value: string | undefined): { provider?: string; model: string } | undefined {
	if (value === undefined) return undefined;
	let parsed: unknown;
	try {
		parsed = JSON.parse(value);
	} catch (error) {
		throw new Error("Internal server received invalid model options", { cause: error });
	}
	if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
		throw new Error("Internal server received invalid model options");
	}
	const keys = Object.keys(parsed);
	const model = "model" in parsed ? parsed.model : undefined;
	const provider = "provider" in parsed ? parsed.provider : undefined;
	if (
		keys.some((key) => key !== "provider" && key !== "model") ||
		typeof model !== "string" ||
		model.length === 0 ||
		(provider !== undefined && (typeof provider !== "string" || provider.length === 0))
	) {
		throw new Error("Internal server received invalid model options");
	}
	return provider === undefined ? { model } : { provider, model };
}

/** Run an automatically activated server until its client and Session demand disappears. */
export async function runServerProcess(args: readonly string[]): Promise<void> {
	const [directory, serverId, sessionDir, serializedModel] = args;
	if (args.length > 4) throw new Error("Internal server received unexpected arguments");
	if (!directory || !isAbsolute(directory)) throw new Error("Internal server requires an absolute server directory");
	if (!isServerId(serverId)) throw new Error("Internal server requires a canonical server ID");
	if (!sessionDir || !isAbsolute(sessionDir))
		throw new Error("Internal server requires an absolute Session directory");
	const workerModel = parseServerModelOptions(serializedModel);

	const runtime = await startServer({
		directory,
		serverId,
		sessionDir,
		keepAlive: false,
		...workerModel,
	});
	const close = (): void => {
		void runtime.close().catch(() => {});
	};
	process.once("SIGINT", close);
	process.once("SIGTERM", close);
	try {
		await runtime.closed;
	} finally {
		process.off("SIGINT", close);
		process.off("SIGTERM", close);
		await runtime.close();
	}
}

function sameStrings(left: readonly string[], right: readonly string[]): boolean {
	return left.length === right.length && left.every((value, index) => value === right[index]);
}

if (isDirectInternalProcessEntry(import.meta.url)) {
	const role = consumeInternalProcessRole();
	if (role !== "server") throw new Error("Server entrypoint requires an internal server invocation");
	void runServerProcess(process.argv.slice(2)).catch((error: unknown) => {
		console.error(error);
		process.exit(1);
	});
}
