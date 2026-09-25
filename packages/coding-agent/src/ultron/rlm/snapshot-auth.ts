import { createHash, createHmac, randomBytes, timingSafeEqual } from "node:crypto";
import { chmodSync, closeSync, openSync, readFileSync, statSync, writeSync } from "node:fs";
import { join } from "node:path";

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
