import { createHash } from "node:crypto";
import { chmodSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, test } from "vitest";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";
import { loadOrCreateSnapshotKey, SNAPSHOT_KEY_FILE } from "../src/ultron/rlm/snapshot-auth.ts";

// Snapshots are signed and verified by the host with a key the kernel (running model code) never holds.
const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));
const directories: string[] = [];
const kernels: RlmKernel[] = [];

afterEach(async () => {
	await Promise.all(kernels.splice(0).map((kernel) => kernel.shutdown()));
	for (const directory of directories.splice(0)) rmSync(directory, { recursive: true, force: true });
});

function scratch(): string {
	const directory = mkdtempSync(join(tmpdir(), "ultron-snapshot-auth-"));
	directories.push(directory);
	return directory;
}

function kernel(snapshotKey: Uint8Array): RlmKernel {
	const instance = new RlmKernel({ cwd: process.cwd(), runtimePath, snapshotKey }, () => null);
	kernels.push(instance);
	return instance;
}

async function saved(key: Uint8Array): Promise<string> {
	const path = join(scratch(), "lane.snapshot");
	const source = kernel(key);
	expect(await source.execute("state['n'] = 1\nbalance = 100")).toMatchObject({ status: "ok" });
	expect((await source.snapshot(path)).status).toBe("ok");
	return path;
}

describe("host-signed RLM snapshots", () => {
	test("the profile key is created once, private, and stable", () => {
		const agentDir = scratch();
		const key = loadOrCreateSnapshotKey(agentDir);
		const path = join(agentDir, SNAPSHOT_KEY_FILE);
		expect(key).toHaveLength(32);
		expect(readFileSync(path, "utf8")).toMatch(/^[0-9a-f]{64}\n$/);
		if (process.platform !== "win32") expect(statSync(path).mode & 0o777).toBe(0o600);
		expect(loadOrCreateSnapshotKey(agentDir).equals(key)).toBe(true);
		if (process.platform !== "win32") {
			chmodSync(path, 0o644);
			expect(loadOrCreateSnapshotKey(agentDir).equals(key)).toBe(true);
			expect(statSync(path).mode & 0o777).toBe(0o600);
		}
		expect(loadOrCreateSnapshotKey(scratch()).equals(key)).toBe(false);
		writeFileSync(path, "not a key");
		expect(() => loadOrCreateSnapshotKey(agentDir)).toThrow(/malformed/);
	});

	test("a valid signed snapshot restores; the file is private; another profile's key is refused", async () => {
		const key = loadOrCreateSnapshotKey(scratch());
		const path = await saved(key);
		if (process.platform !== "win32") expect(statSync(path).mode & 0o777).toBe(0o600);
		expect(readFileSync(path, "utf8").split("\n")[0]).toMatch(/"format":"ultron-rlm-snapshot-signature".*"hmac"/);

		const target = kernel(key);
		expect(await target.restore(path)).toMatchObject({ status: "ok", restore: { restored: ["balance", "state"] } });
		expect(await target.execute("(balance, state)")).toMatchObject({ result: "(100, {'n': 1})" });

		const stranger = kernel(loadOrCreateSnapshotKey(scratch()));
		expect(await stranger.restore(path)).toMatchObject({
			status: "error",
			error: { evalue: expect.stringContaining("HMAC signature mismatch") },
		});
	});

	test("a tampered body with a correctly recomputed sha256 is refused and the namespace is untouched", async () => {
		const key = loadOrCreateSnapshotKey(scratch());
		const path = await saved(key);
		const [signature, , body] = readFileSync(path, "utf8").split("\n");
		const forgedBody = body.replace("100", "999999");
		expect(forgedBody).not.toBe(body);
		const header = JSON.stringify({
			format: "ultron-rlm-snapshot",
			version: 2,
			sha256: createHash("sha256").update(forgedBody).digest("hex"),
			bytes: Buffer.byteLength(forgedBody),
		});
		writeFileSync(path, `${signature}\n${header}\n${forgedBody}`);

		const target = kernel(key);
		expect(await target.execute("marker = 'kept'")).toMatchObject({ status: "ok" });
		const restored = await target.restore(path);
		expect(restored).toMatchObject({
			status: "error",
			error: { ename: "SnapshotIntegrityError", evalue: expect.stringContaining("HMAC signature mismatch") },
		});
		expect(restored.restore).toBeUndefined();
		expect(await target.execute("(marker, 'balance' in globals(), state)")).toMatchObject({
			result: "('kept', False, {})",
		});
	});

	test("unsigned snapshots, including pre-signing v2 files, are refused with a clear message", async () => {
		const key = loadOrCreateSnapshotKey(scratch());
		const path = await saved(key);
		const unsigned = readFileSync(path, "utf8").split("\n").slice(1).join("\n");
		writeFileSync(path, unsigned);
		const target = kernel(key);
		expect(await target.restore(path)).toMatchObject({
			status: "error",
			error: { evalue: expect.stringMatching(/missing host signature \(unsigned snapshots.*are refused\)/) },
		});
		// A kernel configured with that snapshot refuses to start with unauthenticated state.
		const configured = new RlmKernel(
			{ cwd: process.cwd(), runtimePath, snapshotKey: key, snapshotPath: path },
			() => null,
		);
		kernels.push(configured);
		await expect(configured.execute("1")).rejects.toThrow(/startup restore failed: .*missing host signature/);
	});

	test("the key never reaches the kernel's environment, arguments, memory objects, or the snapshot", async () => {
		const agentDir = scratch();
		const key = loadOrCreateSnapshotKey(agentDir);
		const hex = key.toString("hex");
		const path = await saved(key);
		const target = kernel(key);
		expect((await target.restore(path)).status).toBe("ok");
		expect((await target.snapshot(path)).status).toBe("ok");
		const probe = await target.execute(
			[
				"import gc, os, sys",
				`needle_hex = ${JSON.stringify(hex.slice(0, 16))} + ${JSON.stringify(hex.slice(16))}`,
				`needle_raw = bytes.fromhex(needle_hex)`,
				"def found(text):",
				"    return needle_hex in text or needle_hex.upper() in text",
				"hits = []",
				"if any(found(f'{k}={v}') for k, v in os.environ.items()): hits.append('environ')",
				"for name in ('environ', 'cmdline'):",
				"    try:",
				"        raw = open(f'/proc/self/{name}', 'rb').read()",
				"        if needle_hex.encode() in raw or needle_raw in raw: hits.append(name)",
				"    except OSError:",
				"        pass",
				"if any(found(repr(v)) for k, v in list(globals().items()) if k not in ('needle_hex', 'needle_raw')): hits.append('namespace')",
				"for obj in gc.get_objects():",
				"    if obj is needle_hex or obj is needle_raw: continue",
				"    if isinstance(obj, (bytes, bytearray)) and needle_raw in obj: hits.append('object'); break",
				"    if isinstance(obj, str) and len(obj) < 1_000_000 and obj is not needle_hex and found(obj) and obj != needle_hex: hits.append('object'); break",
				"hits",
			].join("\n"),
		);
		expect(probe).toMatchObject({ status: "ok", result: "[]" });
		const file = readFileSync(path);
		expect(file.includes(hex)).toBe(false);
		expect(file.includes(key)).toBe(false);
	});
});
