import { spawnSync } from "node:child_process";
import { createHash, createHmac, randomBytes, timingSafeEqual } from "node:crypto";
import {
	chmodSync,
	closeSync,
	existsSync,
	openSync,
	readFileSync,
	rmSync,
	statSync,
	writeFileSync,
	writeSync,
} from "node:fs";
import { join, resolve } from "node:path";

/**
 * Host-side snapshot authentication. The key lives only in the host (worker) process: the Python
 * kernel runs model-written code, so any key it held could be read by that code. The host signs a
 * snapshot after the kernel writes it and verifies the signature before the kernel may read it.
 *
 * Signed file: one signature line, then the kernel's content (sha256 header line + JSON body):
 *   {"format":"ultron-rlm-snapshot-signature","version":1,"alg":"HMAC-SHA256","hmac":"<hex>"}\n<content>
 * The HMAC covers the content bytes exactly.
 */
export const SNAPSHOT_KEY_FILE = "rlm-snapshot.key";
const SIGNATURE_FORMAT = "ultron-rlm-snapshot-signature";
const KEY_BYTES = 32;

export class SnapshotAuthenticationError extends Error {
	constructor(message: string) {
		super(message);
		this.name = "SnapshotIntegrityError";
	}
}

function parseKey(text: string, path: string): Buffer {
	const hex = text.trim();
	if (!/^[0-9a-f]{64}$/.test(hex)) {
		throw new Error(
			`RLM snapshot key ${path} is malformed; delete it to create a new key (existing snapshots become unrestorable)`,
		);
	}
	return Buffer.from(hex, "hex");
}

/** Read the per-profile snapshot key, creating it once (0600, crypto random) when absent. */
export function loadOrCreateSnapshotKey(agentDir: string): Buffer {
	const path = join(agentDir, SNAPSHOT_KEY_FILE);
	try {
		const descriptor = openSync(path, "wx", 0o600);
		try {
			writeSync(descriptor, `${randomBytes(KEY_BYTES).toString("hex")}\n`);
		} finally {
			closeSync(descriptor);
		}
	} catch (error) {
		if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
	}
	// Keep the key private even if something loosened its mode (umask never widens past 0600 on create).
	if (process.platform !== "win32" && (statSync(path).mode & 0o077) !== 0) chmodSync(path, 0o600);
	return parseKey(readFileSync(path, "utf8"), path);
}

/** Marker left in place of the key file once the key lives in the OS keyring; it holds no secret. */
export const SNAPSHOT_KEYRING_MARKER = "rlm-snapshot.key.keyring";

/**
 * A secret store outside the profile directory. `lookup` returns `{ ok: false }` when the store cannot be
 * reached (no Secret Service, locked keyring, timeout), and `{ ok: true }` without a value when it has no key.
 */
export type SnapshotKeyring = {
	lookup(): { ok: true; value?: string } | { ok: false };
	store(hex: string): boolean;
};

/**
 * The login keyring through libsecret's `secret-tool` (GNOME Keyring, KWallet's Secret Service, KeePassXC).
 * Entries are keyed by the resolved profile directory so profiles keep separate keys. A call that does not
 * answer within `timeoutMs` (for example an unlock prompt nobody answers) counts as unavailable.
 */
export function secretToolKeyring(
	agentDir: string,
	options: { command?: string; timeoutMs?: number } = {},
): SnapshotKeyring {
	const command = options.command ?? "secret-tool";
	const timeout = options.timeoutMs ?? 5_000;
	const attributes = ["application", "ultron", "purpose", "rlm-snapshot-key", "profile", resolve(agentDir)];
	return {
		lookup: () => {
			const result = spawnSync(command, ["lookup", ...attributes], {
				encoding: "utf8",
				timeout,
				stdio: ["ignore", "pipe", "pipe"],
			});
			if (result.error || result.signal) return { ok: false };
			if (result.status === 0) return { ok: true, value: result.stdout };
			// secret-tool exits 1 silently when nothing matches, and 1 with a message when the service fails.
			return result.status === 1 && !result.stderr.trim() ? { ok: true } : { ok: false };
		},
		store: (hex) => {
			const result = spawnSync(command, ["store", "--label=Ultron RLM snapshot signing key", ...attributes], {
				input: hex,
				encoding: "utf8",
				timeout,
				stdio: ["pipe", "ignore", "ignore"],
			});
			return !result.error && !result.signal && result.status === 0;
		},
	};
}

export type SnapshotKeyStore = "auto" | "keyring" | "file";

export type LoadedSnapshotKey = {
	key: Buffer;
	/** Where the key lives: the OS keyring, the profile file, or only this process (snapshots then do not outlive it). */
	source: "keyring" | "file" | "process";
	warning?: string;
};

/**
 * Loads the per-profile snapshot key from the OS keyring when one is reachable, else from `<agentDir>/rlm-snapshot.key`.
 * `ULTRON_RLM_SNAPSHOT_KEY_STORE` (auto|keyring|file, default auto) selects the store.
 *
 * In auto mode a reachable keyring takes over the key: an existing key file is copied into the keyring, read back,
 * and deleted, and a marker file records that the keyring holds the key. Later, if the keyring cannot be reached
 * (an SSH session, a locked keyring), the marker stops a second, different file key from being created; the
 * process uses a key of its own and says so, so snapshots keep verifying under one key once the keyring is back.
 *
 * This is exposure reduction, not secrecy: model-written Python runs as the same user and can query the keyring
 * itself. It removes the key from the profile directory, where any file listing, grep, or copy of the profile
 * would pick it up.
 */
export function loadSnapshotKey(
	agentDir: string,
	options: { store?: SnapshotKeyStore; keyring?: SnapshotKeyring; env?: NodeJS.ProcessEnv } = {},
): LoadedSnapshotKey {
	const requested = (
		options.store ??
		options.env?.ULTRON_RLM_SNAPSHOT_KEY_STORE ??
		process.env.ULTRON_RLM_SNAPSHOT_KEY_STORE
	)
		?.trim()
		.toLowerCase();
	const store: SnapshotKeyStore = requested === "file" || requested === "keyring" ? requested : "auto";
	if (store === "file") return { key: loadOrCreateSnapshotKey(agentDir), source: "file" };
	const path = join(agentDir, SNAPSHOT_KEY_FILE);
	const marker = join(agentDir, SNAPSHOT_KEYRING_MARKER);
	const keyring = options.keyring ?? secretToolKeyring(agentDir);
	const unreachable = (reason: string): LoadedSnapshotKey => {
		if (store === "auto" && !existsSync(marker)) return { key: loadOrCreateSnapshotKey(agentDir), source: "file" };
		return {
			key: randomBytes(KEY_BYTES),
			source: "process",
			warning: `RLM snapshot key: ${reason}; using a key for this process only, so its snapshots will not restore in later sessions`,
		};
	};
	const found = keyring.lookup();
	if (!found.ok) return unreachable("the OS keyring holding the key is unreachable");
	if (found.value?.trim()) {
		const key = parseKey(found.value, "in the OS keyring");
		markKeyring(marker);
		if (existsSync(path)) {
			let fileKey: Buffer | undefined;
			try {
				fileKey = parseKey(readFileSync(path, "utf8"), path);
			} catch {
				fileKey = undefined;
			}
			if (fileKey?.equals(key)) rmSync(path, { force: true });
			else
				return {
					key,
					source: "keyring",
					warning: `RLM snapshot key: ${path} differs from the key in the OS keyring; the keyring key is used and the file was left in place`,
				};
		}
		return { key, source: "keyring" };
	}
	// Nothing stored yet: move the file key into the keyring (so existing snapshots keep verifying), or make one.
	const key = existsSync(path) ? parseKey(readFileSync(path, "utf8"), path) : randomBytes(KEY_BYTES);
	const hex = key.toString("hex");
	const readBack = keyring.store(hex) ? keyring.lookup() : { ok: false as const };
	if (!readBack.ok || readBack.value?.trim() !== hex) {
		if (store === "keyring") return unreachable("the OS keyring did not store the key");
		return { key: loadOrCreateSnapshotKey(agentDir), source: "file" };
	}
	markKeyring(marker);
	rmSync(path, { force: true });
	return { key, source: "keyring" };
}

function markKeyring(marker: string): void {
	if (existsSync(marker)) return;
	writeFileSync(
		marker,
		"The RLM snapshot signing key is stored in the OS keyring (secret-tool, application=ultron purpose=rlm-snapshot-key). This file holds no secret.\n",
		{ mode: 0o600 },
	);
}

let processKey: Buffer | undefined;

/** Fallback for kernels built without a profile key: snapshots are restorable only within this process. */
export function processSnapshotKey(): Buffer {
	processKey ??= randomBytes(KEY_BYTES);
	return processKey;
}

export function sha256Hex(content: Uint8Array): string {
	return createHash("sha256").update(content).digest("hex");
}

function hmacHex(key: Uint8Array, content: Uint8Array): string {
	return createHmac("sha256", key).update(content).digest("hex");
}

export function signSnapshot(key: Uint8Array, content: Uint8Array): Buffer {
	const line = JSON.stringify({
		format: SIGNATURE_FORMAT,
		version: 1,
		alg: "HMAC-SHA256",
		hmac: hmacHex(key, content),
	});
	return Buffer.concat([Buffer.from(`${line}\n`, "utf8"), content]);
}

/** Verify a signed snapshot and return the sha256 of its content, which the kernel re-checks on read. */
export function verifySnapshot(key: Uint8Array, raw: Uint8Array): { contentSha256: string } {
	const bytes = Buffer.from(raw.buffer, raw.byteOffset, raw.byteLength);
	const newline = bytes.indexOf(10);
	let signature: unknown;
	try {
		signature = newline === -1 ? undefined : JSON.parse(bytes.subarray(0, newline).toString("utf8"));
	} catch {
		signature = undefined;
	}
	const record = signature && typeof signature === "object" ? (signature as Record<string, unknown>) : undefined;
	if (record?.format !== SIGNATURE_FORMAT) {
		throw new SnapshotAuthenticationError(
			"snapshot integrity check failed: missing host signature (unsigned snapshots, including ones written before snapshots were signed, are refused)",
		);
	}
	if (record.version !== 1 || record.alg !== "HMAC-SHA256" || typeof record.hmac !== "string") {
		throw new SnapshotAuthenticationError("snapshot integrity check failed: unsupported host signature");
	}
	const content = bytes.subarray(newline + 1);
	const expected = Buffer.from(hmacHex(key, content), "hex");
	const actual = /^[0-9a-f]{64}$/.test(record.hmac) ? Buffer.from(record.hmac, "hex") : Buffer.alloc(0);
	if (actual.length !== expected.length || !timingSafeEqual(actual, expected)) {
		throw new SnapshotAuthenticationError(
			"snapshot integrity check failed: HMAC signature mismatch (the snapshot was modified or signed with another profile's key)",
		);
	}
	return { contentSha256: sha256Hex(content) };
}
