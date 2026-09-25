import { chmodSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, test } from "vitest";
import {
	loadOrCreateSnapshotKey,
	loadSnapshotKey,
	SNAPSHOT_KEY_FILE,
	SNAPSHOT_KEYRING_MARKER,
	type SnapshotKeyring,
	secretToolKeyring,
} from "../src/ultron/rlm/snapshot-auth.ts";

// The snapshot signing key stays in the profile file unless the OS keyring is opted into.
const directories: string[] = [];
afterEach(() => {
	for (const directory of directories.splice(0)) rmSync(directory, { recursive: true, force: true });
});

function scratch(): string {
	const directory = mkdtempSync(join(tmpdir(), "ultron-snapshot-key-"));
	directories.push(directory);
	return directory;
}

function fakeKeyring(initial?: string, options: { reachable?: boolean; storeWorks?: boolean } = {}) {
	const state = { value: initial, stores: 0, reachable: options.reachable ?? true };
	const keyring: SnapshotKeyring = {
		lookup: () =>
			state.reachable ? { ok: true, ...(state.value === undefined ? {} : { value: state.value }) } : { ok: false },
		store: (hex) => {
			state.stores += 1;
			if (!state.reachable || options.storeWorks === false) return false;
			state.value = hex;
			return true;
		},
	};
	return { keyring, state };
}

describe("snapshot key store", () => {
	test("without a setting the key stays in the profile file and the keyring is never touched", () => {
		const agentDir = scratch();
		const { keyring, state } = fakeKeyring();
		const previous = process.env.ULTRON_RLM_SNAPSHOT_KEY_STORE;
		delete process.env.ULTRON_RLM_SNAPSHOT_KEY_STORE;
		try {
			const loaded = loadSnapshotKey(agentDir, { keyring });
			expect(loaded.source).toBe("file");
			expect(loaded.key.equals(loadOrCreateSnapshotKey(agentDir))).toBe(true);
			expect(state.stores).toBe(0);
			expect(existsSync(join(agentDir, SNAPSHOT_KEYRING_MARKER))).toBe(false);
		} finally {
			if (previous !== undefined) process.env.ULTRON_RLM_SNAPSHOT_KEY_STORE = previous;
		}
	});

	test("a reachable keyring takes over an existing file key: same key, file removed, marker left", () => {
		const agentDir = scratch();
		const fileKey = loadOrCreateSnapshotKey(agentDir);
		const { keyring, state } = fakeKeyring();
		const loaded = loadSnapshotKey(agentDir, { store: "auto", keyring });
		expect(loaded).toMatchObject({ source: "keyring" });
		expect(loaded.key.equals(fileKey)).toBe(true);
		expect(state.value).toBe(fileKey.toString("hex"));
		expect(existsSync(join(agentDir, SNAPSHOT_KEY_FILE))).toBe(false);
		expect(readFileSync(join(agentDir, SNAPSHOT_KEYRING_MARKER), "utf8")).toContain("holds no secret");
		// Stable on the next load, without storing again.
		expect(loadSnapshotKey(agentDir, { store: "auto", keyring }).key.equals(fileKey)).toBe(true);
		expect(state.stores).toBe(1);
	});

	test("a fresh profile with a keyring never writes a key file", () => {
		const agentDir = scratch();
		const { keyring, state } = fakeKeyring();
		const loaded = loadSnapshotKey(agentDir, { store: "auto", keyring });
		expect(loaded.source).toBe("keyring");
		expect(loaded.key).toHaveLength(32);
		expect(state.value).toBe(loaded.key.toString("hex"));
		expect(existsSync(join(agentDir, SNAPSHOT_KEY_FILE))).toBe(false);
	});

	test("without a reachable keyring the profile file is used, as before", () => {
		const agentDir = scratch();
		const { keyring } = fakeKeyring(undefined, { reachable: false });
		const loaded = loadSnapshotKey(agentDir, { store: "auto", keyring });
		expect(loaded.source).toBe("file");
		expect(loaded.key.equals(loadOrCreateSnapshotKey(agentDir))).toBe(true);
		// A keyring that refuses to store also falls back to the file, keeping the same key.
		const refusing = fakeKeyring(undefined, { storeWorks: false });
		const again = loadSnapshotKey(agentDir, { store: "auto", keyring: refusing.keyring });
		expect(again).toMatchObject({ source: "file" });
		expect(again.key.equals(loaded.key)).toBe(true);
		expect(existsSync(join(agentDir, SNAPSHOT_KEY_FILE))).toBe(true);
	});

	test("once the keyring holds the key, an unreachable keyring never forks a second persistent key", () => {
		const agentDir = scratch();
		const reachable = fakeKeyring();
		const stored = loadSnapshotKey(agentDir, { store: "auto", keyring: reachable.keyring }).key;
		reachable.state.reachable = false;
		const offline = loadSnapshotKey(agentDir, { store: "auto", keyring: reachable.keyring });
		expect(offline.source).toBe("process");
		expect(offline.warning).toMatch(/unreachable.*this process only/);
		expect(offline.key.equals(stored)).toBe(false);
		expect(existsSync(join(agentDir, SNAPSHOT_KEY_FILE))).toBe(false);
		// Back online: the original key.
		reachable.state.reachable = true;
		expect(loadSnapshotKey(agentDir, { store: "auto", keyring: reachable.keyring }).key.equals(stored)).toBe(true);
	});

	test("a file that disagrees with the keyring is left alone with a warning; file mode ignores the keyring", () => {
		const agentDir = scratch();
		const fileKey = loadOrCreateSnapshotKey(agentDir);
		const other = "ab".repeat(32);
		const { keyring, state } = fakeKeyring(other);
		const loaded = loadSnapshotKey(agentDir, { store: "auto", keyring });
		expect(loaded.key.toString("hex")).toBe(other);
		expect(loaded.warning).toMatch(/differs from the key in the OS keyring/);
		expect(existsSync(join(agentDir, SNAPSHOT_KEY_FILE))).toBe(true);
		const fileMode = loadSnapshotKey(agentDir, {
			keyring,
			env: { ULTRON_RLM_SNAPSHOT_KEY_STORE: "file" },
		});
		expect(fileMode).toMatchObject({ source: "file" });
		expect(fileMode.key.equals(fileKey)).toBe(true);
		expect(state.stores).toBe(0);
		// Keyring mode with an unreachable keyring degrades to a process key rather than the file.
		state.value = undefined;
		const forced = loadSnapshotKey(scratch(), {
			store: "keyring",
			keyring: fakeKeyring(undefined, { reachable: false }).keyring,
		});
		expect(forced.source).toBe("process");
	});

	test.skipIf(process.platform === "win32")("the secret-tool provider speaks secret-tool's CLI (fake binary)", () => {
		const directory = scratch();
		const vault = join(directory, "vault");
		const tool = join(directory, "secret-tool");
		// lookup: prints the secret or exits 1 silently; store: reads the secret from stdin; "broken" fails loudly.
		writeFileSync(
			tool,
			[
				"#!/bin/sh",
				`vault='${vault}'`,
				'[ -f "$vault.broken" ] && { echo "Cannot autolaunch D-Bus" >&2; exit 1; }',
				'case "$1" in',
				'  lookup) shift; [ -f "$vault" ] || exit 1; printf "%s" "$(cat "$vault")"; echo "$*" > "$vault.attrs" ;;',
				'  store) cat > "$vault" ;;',
				"esac",
			].join("\n"),
		);
		chmodSync(tool, 0o755);
		const agentDir = scratch();
		const keyring = secretToolKeyring(agentDir, { command: tool });
		expect(keyring.lookup()).toEqual({ ok: true });
		const loaded = loadSnapshotKey(agentDir, { store: "auto", keyring });
		expect(loaded.source).toBe("keyring");
		expect(readFileSync(vault, "utf8")).toBe(loaded.key.toString("hex"));
		expect(readFileSync(`${vault}.attrs`, "utf8")).toContain(`purpose rlm-snapshot-key profile ${agentDir}`);
		writeFileSync(`${vault}.broken`, "");
		expect(keyring.lookup()).toEqual({ ok: false });
		expect(secretToolKeyring(agentDir, { command: join(directory, "missing") }).lookup()).toEqual({ ok: false });
	});
});
