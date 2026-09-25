import { EventEmitter } from "node:events";
import { BACKGROUND_CONTEXT, type JsonlSessionMetadata } from "@ultron/agent-core";
import { describe, expect, test, vi } from "vitest";
import type { CoordinatorConnectionEvent } from "../src/experimental/coordinator.ts";
import { SESSION_WORKER_CONTROL_TOKEN_ENV, SESSION_WORKER_PEER_ID_ENV } from "../src/experimental/session-worker.ts";
import { SessionWorkerManager } from "../src/experimental/session-worker-manager.ts";

/** Spawned worker stand-ins: the manager's child process events are driven by each test. */
const spawned: { child: FakeChild; env: NodeJS.ProcessEnv }[] = [];

class FakeChild extends EventEmitter {
	readonly pid = 4242;
	exitCode: number | null = null;
	signalCode: NodeJS.Signals | null = null;
	kill(): boolean {
		return true;
	}
	exit(code: number | null, signal: NodeJS.Signals | null = null): void {
		this.exitCode = code;
		this.signalCode = signal;
		this.emit("exit", code, signal);
	}
}

vi.mock("../src/experimental/process.ts", async (importOriginal) => ({
	...(await importOriginal<typeof import("../src/experimental/process.ts")>()),
	spawnInternalProcess: (_role: string, _args: readonly string[], options: { env?: NodeJS.ProcessEnv } = {}) => {
		const child = new FakeChild();
		spawned.push({ child, env: options.env ?? {} });
		return child;
	},
}));

const metadata: JsonlSessionMetadata = {
	id: "session-1",
	createdAt: 1,
	storageVersion: 1,
	cwd: "/tmp",
	path: "/tmp/session-1.jsonl",
	modifiedAt: 1,
};

class FakeCoordinator {
	readonly controlPath = "/tmp/control.sock";
	readonly serverConnectionId = "server-generation-1";
	readonly wasReplaced = false;
	readonly #listeners = new Set<(event: CoordinatorConnectionEvent) => void>();

	onEvent(listener: (event: CoordinatorConnectionEvent) => void): () => void {
		this.#listeners.add(listener);
		return () => this.#listeners.delete(listener);
	}

	async send(): Promise<void> {}

	async broadcast(): Promise<void> {}

	emit(event: CoordinatorConnectionEvent): void {
		for (const listener of this.#listeners) listener(event);
	}
}

/** Launch a worker through the manager and answer its readiness, as the real worker would. */
async function launchWorker() {
	spawned.length = 0;
	const coordinator = new FakeCoordinator();
	const workers = new SessionWorkerManager(coordinator, "/tmp");
	const opening = workers.openSession(metadata, BACKGROUND_CONTEXT, []);
	await vi.waitFor(() => expect(spawned).toHaveLength(1));
	const { child, env } = spawned[0]!;
	const peerId = env[SESSION_WORKER_PEER_ID_ENV]!;
	const token = env[SESSION_WORKER_CONTROL_TOKEN_ENV]!;
	coordinator.emit({
		type: "message",
		from: peerId,
		payload: {
			type: "worker_ready",
			token,
			sessionKey: metadata.path,
			sessionId: metadata.id,
			pid: child.pid,
			metadata,
			pluginManifestPaths: [],
		},
	});
	const handle = await opening;
	const announceRetirement = (): void =>
		coordinator.emit({
			type: "message",
			from: peerId,
			payload: { type: "worker_retiring", token, sessionKey: metadata.path },
		});
	const disconnect = (): void => coordinator.emit({ type: "peer_disconnected", peerId });
	return { workers, child, handle, announceRetirement, disconnect };
}

describe("Session worker retirement", () => {
	test("a retirement announced before the process exits is not unexpected, whichever of exit or disconnect is seen first", async () => {
		const exitFirst = await launchWorker();
		exitFirst.announceRetirement();
		exitFirst.child.exit(0);
		exitFirst.disconnect();
		await expect(exitFirst.handle.terminated).resolves.toBeUndefined();

		const disconnectFirst = await launchWorker();
		disconnectFirst.announceRetirement();
		disconnectFirst.disconnect();
		disconnectFirst.child.exit(0);
		await expect(disconnectFirst.handle.terminated).resolves.toBeUndefined();
	});

	test("the process exit may be observed before the announcement that the coordinator relays", async () => {
		const worker = await launchWorker();
		worker.child.exit(0);
		// The Session is free to start again immediately.
		expect(worker.workers.trackedSessions).toEqual([]);
		worker.announceRetirement();
		worker.disconnect();
		await expect(worker.handle.terminated).resolves.toBeUndefined();
	});

	test("an exit or disconnect without an announced retirement is still unexpected", async () => {
		const cleanExit = await launchWorker();
		cleanExit.child.exit(0);
		cleanExit.disconnect();
		await expect(cleanExit.handle.terminated).resolves.toHaveProperty(
			"message",
			"Session worker session-1 exited unexpectedly (0)",
		);

		const crashed = await launchWorker();
		crashed.child.exit(1);
		await expect(crashed.handle.terminated).resolves.toHaveProperty(
			"message",
			"Session worker session-1 exited unexpectedly (1)",
		);

		const disconnected = await launchWorker();
		disconnected.disconnect();
		await expect(disconnected.handle.terminated).resolves.toHaveProperty(
			"message",
			"Session worker session-1 disconnected unexpectedly",
		);
	});

	test("an announcement cannot excuse a crash", async () => {
		const worker = await launchWorker();
		worker.announceRetirement();
		worker.child.exit(1);
		await expect(worker.handle.terminated).resolves.toHaveProperty(
			"message",
			"Session worker session-1 exited unexpectedly (1)",
		);
	});
});
