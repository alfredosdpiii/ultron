/**
 * The local control socket of one `ultron mcp` server (one Claude Code session): Claude Code hooks
 * (`ultron hook ...`), the companion viewer (`ultron watch`) and subagent servers (`ultron mcp --child`) reach the
 * server's runtime through it. Newline-delimited JSON, one request and one response per line:
 *
 *   {"id": 1, "op": "status"}                     -> {"id": 1, "ok": true, "result": {...}}
 *   {"id": 2, "op": "inspect", "request": "agents.status", "payload": {"graph": true}}
 *
 * Sockets live in a per-user runtime directory (`$XDG_RUNTIME_DIR/ultron-claude`, else a `ultron-claude-<uid>`
 * directory in the temp dir), created 0700 with the socket itself 0600, so only the owner can connect. Each server
 * also writes `<name>.json` there (pid, Claude Code session id and pid, cwd, socket), which is how hooks and the
 * viewer find a server they were not told about. Both are removed when the server exits.
 */
import { chmodSync, existsSync, lstatSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createConnection, createServer, type Server, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

export interface ServerRecord {
	/** Registry name: the Claude Code session id for a root, `child-<pid>-<random>` for a subagent server. */
	readonly name: string;
	readonly pid: number;
	readonly socket: string;
	readonly cwd: string;
	readonly startedAt: number;
	readonly claudeSessionId?: string;
	readonly claudePid?: number;
	/** The Ultron session the server runs on. */
	readonly sessionId?: string;
	/** Set for a subagent server: the parent server's registry name. */
	readonly parent?: string;
}

export type ControlRequest = { id: number; op: string } & Record<string, unknown>;
export type ControlHandler = (request: ControlRequest) => Promise<unknown>;

/** Lines longer than this are refused. */
const MAX_CONTROL_LINE = 4 * 1024 * 1024;

/** The per-user directory for control sockets and registry files (created 0700). */
export function controlDir(env: NodeJS.ProcessEnv = process.env): string {
	const runtime = env.XDG_RUNTIME_DIR?.trim();
	const uid = typeof process.getuid === "function" ? process.getuid() : "user";
	const dir = runtime ? join(runtime, "ultron-claude") : join(tmpdir(), `ultron-claude-${uid}`);
	mkdirSync(dir, { recursive: true, mode: 0o700 });
	// A directory someone else made (or made world-readable) must not hold our sockets.
	const stat = lstatSync(dir);
	if (!stat.isDirectory() || (typeof process.getuid === "function" && stat.uid !== process.getuid()))
		throw new Error(`${dir} is not a directory owned by this user`);
	if ((stat.mode & 0o077) !== 0) chmodSync(dir, 0o700);
	return dir;
}

/** A registry name safe as a file name. */
export function safeName(name: string): string {
	return name.replace(/[^A-Za-z0-9._-]/g, "_").slice(0, 120) || "session";
}

/** The socket path for a registry name (Unix socket paths are limited to about 100 bytes; names stay short). */
export function socketPathFor(name: string, dir = controlDir()): string {
	return join(dir, `${safeName(name)}.sock`);
}

function pidAlive(pid: number): boolean {
	try {
		process.kill(pid, 0);
		return true;
	} catch (error) {
		return (error as NodeJS.ErrnoException).code === "EPERM";
	}
}

/** Live servers in the registry, newest first; records of dead processes are removed. */
export function listServers(dir = controlDir()): ServerRecord[] {
	const records: ServerRecord[] = [];
	for (const entry of readdirSync(dir)) {
		if (!entry.endsWith(".json")) continue;
		const path = join(dir, entry);
		try {
			const record = JSON.parse(readFileSync(path, "utf8")) as ServerRecord;
			if (typeof record.pid !== "number" || typeof record.socket !== "string") continue;
			if (!pidAlive(record.pid)) {
				rmSync(path, { force: true });
				rmSync(record.socket, { force: true });
				continue;
			}
			records.push(record);
		} catch {
			// A record being written or a stray file.
		}
	}
	return records.sort((left, right) => right.startedAt - left.startedAt);
}

/** Find the server of a Claude Code session, or the newest root server in `cwd`. */
export function findServer(
	query: { claudeSessionId?: string; cwd?: string; name?: string },
	dir = controlDir(),
): ServerRecord | undefined {
	const servers = listServers(dir);
	if (query.name !== undefined) return servers.find((server) => server.name === query.name);
	if (query.claudeSessionId !== undefined) {
		const match = servers.find((server) => server.claudeSessionId === query.claudeSessionId && !server.parent);
		if (match) return match;
	}
	if (query.cwd !== undefined) return servers.find((server) => server.cwd === query.cwd && !server.parent);
	return undefined;
}

export interface ControlServer {
	readonly socket: string;
	close(): Promise<void>;
}

/** Listen on `record.socket` (owner-only) and register the server; `handle` answers each request. */
export async function startControlServer(
	record: ServerRecord,
	handle: ControlHandler,
	dir = controlDir(),
): Promise<ControlServer> {
	rmSync(record.socket, { force: true });
	const connections = new Set<Socket>();
	const server: Server = createServer((socket) => {
		connections.add(socket);
		socket.on("close", () => connections.delete(socket));
		socket.on("error", () => socket.destroy());
		socket.setEncoding("utf8");
		let buffered = "";
		socket.on("data", (chunk: string) => {
			buffered += chunk;
			if (buffered.length > MAX_CONTROL_LINE) {
				socket.destroy();
				return;
			}
			let newline = buffered.indexOf("\n");
			while (newline !== -1) {
				const line = buffered.slice(0, newline);
				buffered = buffered.slice(newline + 1);
				newline = buffered.indexOf("\n");
				if (!line.trim()) continue;
				let request: ControlRequest;
				try {
					request = JSON.parse(line) as ControlRequest;
				} catch {
					socket.write(`${JSON.stringify({ id: null, ok: false, error: "invalid JSON" })}\n`);
					continue;
				}
				void handle(request).then(
					(result) => {
						if (!socket.destroyed) socket.write(`${JSON.stringify({ id: request.id, ok: true, result })}\n`);
					},
					(error: unknown) => {
						if (!socket.destroyed)
							socket.write(
								`${JSON.stringify({ id: request.id, ok: false, error: error instanceof Error ? error.message : String(error) })}\n`,
							);
					},
				);
			}
		});
	});
	// The socket file is created with the process umask; tighten it before anyone is told where it is.
	const oldMask = process.umask(0o177);
	try {
		await new Promise<void>((resolve, reject) => {
			server.once("error", reject);
			server.listen(record.socket, () => resolve());
		});
	} finally {
		process.umask(oldMask);
	}
	chmodSync(record.socket, 0o600);
	const registry = join(dir, `${safeName(record.name)}.json`);
	writeFileSync(registry, `${JSON.stringify(record)}\n`, { mode: 0o600 });
	server.unref();
	let closed: Promise<void> | undefined;
	return {
		socket: record.socket,
		close: () => {
			closed ??= new Promise<void>((resolve) => {
				for (const socket of connections) socket.destroy();
				server.close(() => resolve());
				rmSync(registry, { force: true });
				rmSync(record.socket, { force: true });
			});
			return closed;
		},
	};
}

/** Remove the registry entry and socket synchronously (process exit). */
export function unregisterSync(record: ServerRecord, dir = controlDir()): void {
	rmSync(join(dir, `${safeName(record.name)}.json`), { force: true });
	rmSync(record.socket, { force: true });
}

export interface ControlClient {
	request(op: string, fields?: Record<string, unknown>, timeoutMs?: number): Promise<unknown>;
	close(): void;
}

/** Connect to a control socket, retrying while it does not exist yet (up to `waitMs`). */
export async function connectControl(socketPath: string, waitMs = 0): Promise<ControlClient> {
	const deadline = Date.now() + waitMs;
	let socket: Socket | undefined;
	for (;;) {
		try {
			socket = await new Promise<Socket>((resolve, reject) => {
				const candidate = createConnection(socketPath);
				candidate.once("connect", () => resolve(candidate));
				candidate.once("error", reject);
			});
			break;
		} catch (error) {
			if (Date.now() >= deadline) throw error;
			await new Promise((done) => setTimeout(done, 100));
		}
	}
	const connected = socket;
	connected.setEncoding("utf8");
	let next = 1;
	const waiting = new Map<number, { resolve(value: unknown): void; reject(error: Error): void }>();
	let buffered = "";
	connected.on("data", (chunk: string) => {
		buffered += chunk;
		let newline = buffered.indexOf("\n");
		while (newline !== -1) {
			const line = buffered.slice(0, newline);
			buffered = buffered.slice(newline + 1);
			newline = buffered.indexOf("\n");
			try {
				const response = JSON.parse(line) as { id: number; ok: boolean; result?: unknown; error?: string };
				const waiter = waiting.get(response.id);
				waiting.delete(response.id);
				if (response.ok) waiter?.resolve(response.result);
				else waiter?.reject(new Error(response.error ?? "control request failed"));
			} catch {
				// Ignore a malformed line.
			}
		}
	});
	const failAll = (error: Error): void => {
		for (const waiter of waiting.values()) waiter.reject(error);
		waiting.clear();
	};
	connected.on("error", (error) => failAll(error));
	connected.on("close", () => failAll(new Error("control socket closed")));
	return {
		request: (op, fields = {}, timeoutMs = 60_000) =>
			new Promise((resolve, reject) => {
				const id = next++;
				const timer = setTimeout(() => {
					waiting.delete(id);
					reject(new Error(`control request ${op} timed out`));
				}, timeoutMs);
				timer.unref?.();
				waiting.set(id, {
					resolve: (value) => {
						clearTimeout(timer);
						resolve(value);
					},
					reject: (error) => {
						clearTimeout(timer);
						reject(error);
					},
				});
				connected.write(`${JSON.stringify({ ...fields, id, op })}\n`);
			}),
		close: () => connected.destroy(),
	};
}

/** True when a socket path exists (and so a server may be listening). */
export function socketExists(path: string): boolean {
	return existsSync(path);
}
