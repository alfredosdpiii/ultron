import { createHash } from "node:crypto";
import { chmod, mkdir, mkdtemp, readdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { BACKGROUND_CONTEXT, type Entry, JsonlSessionRepo } from "@earendil-works/pi-agent-core";
import { NodeExecutionEnv } from "@earendil-works/pi-agent-core/node";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { loadEntriesFromFile, type SessionEntry, SessionManager } from "../src/core/session-manager.ts";
import {
	backupProfile,
	exportNativeSessionToPi,
	importPiSession,
	nativeSessionIdForPiSession,
	PiSessionAlreadyImportedError,
	ProfileBackupVerificationError,
	restoreProfile,
} from "../src/ultron/migration.ts";
import { runMigrationCommand } from "../src/ultron/migration-cli.ts";
import { readSessionName } from "./experimental-session-support.ts";

const PI_SESSION_ID = "0192a0b0-0000-7000-8000-000000000001";
const SECRET = "sk-test-super-secret-credential";

let root: string;

beforeEach(async () => {
	root = await mkdtemp(join(tmpdir(), "ultron-migration-"));
});

afterEach(async () => {
	process.exitCode = undefined;
	await rm(root, { recursive: true, force: true });
});

const usage = {
	input: 10,
	output: 5,
	cacheRead: 0,
	cacheWrite: 0,
	totalTokens: 15,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
};

function assistant(content: unknown[], stopReason = "stop", at = 0) {
	return {
		role: "assistant",
		content,
		api: "anthropic-messages",
		provider: "anthropic",
		model: "claude-test",
		usage,
		stopReason,
		timestamp: 1_760_000_000_000 + at,
	};
}

function user(text: string, at = 0) {
	return { role: "user", content: [{ type: "text", text }], timestamp: 1_760_000_000_000 + at };
}

function ts(seconds: number): string {
	return new Date(1_760_000_000_000 + seconds * 1000).toISOString();
}

/** A realistic Pi session: tool calls, an abandoned branch, a branch summary, a compaction, a label, and a name. */
async function writePiFixture(cwd: string): Promise<string> {
	const lines = [
		{ type: "session", version: 3, id: PI_SESSION_ID, timestamp: ts(0), cwd },
		{
			type: "model_change",
			id: "e1",
			parentId: null,
			timestamp: ts(1),
			provider: "anthropic",
			modelId: "claude-test",
		},
		{ type: "thinking_level_change", id: "e2", parentId: "e1", timestamp: ts(2), thinkingLevel: "high" },
		{ type: "message", id: "m1", parentId: "e2", timestamp: ts(3), message: user("list the files", 3) },
		{
			type: "message",
			id: "m2",
			parentId: "m1",
			timestamp: ts(4),
			message: assistant(
				[
					{ type: "text", text: "Listing." },
					{ type: "toolCall", id: "call_1", name: "bash", arguments: { command: "ls" } },
				],
				"toolUse",
				4,
			),
		},
		{
			type: "message",
			id: "m3",
			parentId: "m2",
			timestamp: ts(5),
			message: {
				role: "toolResult",
				toolCallId: "call_1",
				toolName: "bash",
				content: [{ type: "text", text: "a.txt\nb.txt" }],
				isError: false,
				timestamp: 1_760_000_005_000,
			},
		},
		{
			type: "message",
			id: "m4",
			parentId: "m3",
			timestamp: ts(6),
			message: assistant([{ type: "text", text: "Two files." }], "stop", 6),
		},
		{ type: "message", id: "m5", parentId: "m4", timestamp: ts(7), message: user("try approach X", 7) },
		// Abandoned branch: not on the active path.
		{
			type: "message",
			id: "x1",
			parentId: "m5",
			timestamp: ts(8),
			message: assistant([{ type: "text", text: "abandoned answer" }], "stop", 8),
		},
		{ type: "message", id: "x2", parentId: "x1", timestamp: ts(9), message: user("abandoned follow-up", 9) },
		{
			type: "branch_summary",
			id: "b1",
			parentId: "m5",
			timestamp: ts(10),
			fromId: "x2",
			summary: "Approach X failed.",
			fromHook: false,
		},
		{
			type: "message",
			id: "m6",
			parentId: "b1",
			timestamp: ts(11),
			message: assistant([{ type: "text", text: "Using approach Y." }], "stop", 11),
		},
		{
			type: "compaction",
			id: "c1",
			parentId: "m6",
			timestamp: ts(12),
			summary: "Listed files, X failed, Y chosen.",
			firstKeptEntryId: "m5",
			tokensBefore: 1234,
			details: { readFiles: [], modifiedFiles: [] },
		},
		{ type: "label", id: "l1", parentId: "c1", timestamp: ts(13), targetId: "m4", label: "checkpoint" },
		{ type: "message", id: "m7", parentId: "l1", timestamp: ts(14), message: user("continue", 14) },
		{ type: "session_info", id: "s1", parentId: "m7", timestamp: ts(15), name: "Imported work" },
		{
			type: "message",
			id: "m8",
			parentId: "s1",
			timestamp: ts(16),
			message: assistant([{ type: "text", text: "Continuing with Y." }], "stop", 16),
		},
	];
	const path = join(root, "pi", "session.jsonl");
	await mkdir(join(root, "pi"), { recursive: true });
	await writeFile(path, `${lines.map((line) => JSON.stringify(line)).join("\n")}\n`);
	return path;
}

function piBranch(path: string): { branch: SessionEntry[]; manager: SessionManager } {
	const entries = loadEntriesFromFile(path);
	const manager = SessionManager.inMemory(root, undefined, entries);
	return { branch: manager.getBranch(), manager };
}

function messagesOf(entries: readonly (SessionEntry | Entry)[]) {
	return entries.flatMap((entry) => (entry.type === "message" ? [entry.message] : []));
}

async function readNative(sessionsRoot: string, sessionId: string) {
	const fileSystem = new NodeExecutionEnv({ cwd: process.cwd() });
	const repo = new JsonlSessionRepo({ fileSystem, sessionsRoot });
	const all = await repo.list(undefined, BACKGROUND_CONTEXT);
	const metadata = all.filter((candidate) => candidate.id === sessionId);
	expect(metadata).toHaveLength(1);
	const session = await repo.open(metadata[0]!, BACKGROUND_CONTEXT);
	try {
		const main = await session.branch("main", BACKGROUND_CONTEXT);
		const entries = (await main?.findEntries({ order: "oldestFirst" }, BACKGROUND_CONTEXT)) ?? [];
		const tip = (await main?.getTipId(BACKGROUND_CONTEXT)) ?? null;
		const allEntries = await session.findEntries({ order: "asc" }, BACKGROUND_CONTEXT);
		return { metadata: metadata[0]!, entries, tip, allEntries, sessionCount: all.length };
	} finally {
		await session.close(BACKGROUND_CONTEXT);
		await repo.close(BACKGROUND_CONTEXT);
		await fileSystem.cleanup(BACKGROUND_CONTEXT);
	}
}

describe("importPiSession", () => {
	it("imports the active branch with exact messages, compaction tail, skipped types, name, and tip", async () => {
		const cwd = join(root, "project");
		const piPath = await writePiFixture(cwd);
		const sessionsRoot = join(root, "native");
		const before = await readFile(piPath, "utf8");

		const result = await importPiSession({ piSessionPath: piPath, sessionsRoot });

		expect(await readFile(piPath, "utf8")).toBe(before);
		expect(result.sessionId).toBe(nativeSessionIdForPiSession(PI_SESSION_ID));
		expect(result.sessionId).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-8[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
		expect(result.imported).toBe(10);
		expect(result.skipped).toEqual([
			{ type: "model_change", count: 1 },
			{ type: "thinking_level_change", count: 1 },
			{ type: "label", count: 1 },
		]);

		const native = await readNative(sessionsRoot, result.sessionId);
		expect(native.metadata.cwd).toBe(cwd);
		expect(native.metadata.path).toBe(result.path);
		expect(native.metadata.createdAt).toBe(Date.parse(ts(0)));
		expect((await readFile(result.path, "utf8")).split("\n", 1)[0]).toContain('"kind":"header"');

		const { branch, manager } = piBranch(piPath);
		expect(messagesOf(native.entries)).toEqual(messagesOf(branch));
		expect(native.entries.map((entry) => entry.id)).toEqual([
			"m1",
			"m2",
			"m3",
			"m4",
			"m5",
			"b1",
			"m6",
			"c1",
			"m7",
			"m8",
		]);
		expect(native.entries.map((entry) => entry.timestamp)).toEqual(
			native.entries.map((entry) => Date.parse(branch.find((pi) => pi.id === entry.id)!.timestamp)),
		);
		// The abandoned branch is not imported.
		expect(native.allEntries.map((entry) => entry.id)).not.toContain("x1");
		expect(native.allEntries).toHaveLength(10);

		// Resumes at the Pi leaf.
		expect(manager.getLeafId()).toBe("m8");
		expect(native.tip).toBe("m8");

		const summary = native.entries.find((entry) => entry.type === "branch_summary");
		expect(summary).toMatchObject({ fromId: "x2", summary: "Approach X failed.", fromHook: false, parentId: "m5" });

		// Native compaction keeps exactly the messages Pi would have kept in context.
		const compaction = native.entries.find((entry) => entry.type === "compaction");
		const piContext = manager.buildSessionContext().messages;
		expect(compaction).toMatchObject({ summary: "Listed files, X failed, Y chosen.", tokensBefore: 1234 });
		if (compaction?.type !== "compaction") throw new Error("expected compaction");
		expect(compaction.retainedTail).toEqual(piContext.slice(1, 4));
		expect(compaction.retainedTail.map((message) => message.role)).toEqual(["user", "branchSummary", "assistant"]);

		expect(await readSessionName(sessionsRoot, result.sessionId)).toBe("Imported work");
	});

	it("refuses to re-import the same Pi session", async () => {
		const piPath = await writePiFixture(join(root, "project"));
		const sessionsRoot = join(root, "native");
		const first = await importPiSession({ piSessionPath: piPath, sessionsRoot });

		const second = importPiSession({ piSessionPath: piPath, sessionsRoot });
		await expect(second).rejects.toBeInstanceOf(PiSessionAlreadyImportedError);
		await expect(importPiSession({ piSessionPath: piPath, sessionsRoot })).rejects.toMatchObject({
			sessionId: first.sessionId,
			path: first.path,
		});
		expect((await readNative(sessionsRoot, first.sessionId)).sessionCount).toBe(1);
	});

	it("rejects files that are not Pi sessions", async () => {
		const path = join(root, "native.jsonl");
		await writeFile(path, `${JSON.stringify({ v: 4, kind: "header", id: "x" })}\n`);
		await expect(importPiSession({ piSessionPath: path, sessionsRoot: join(root, "native") })).rejects.toThrow(
			/Not a Pi session/,
		);
	});
});

describe("exportNativeSessionToPi", () => {
	it("round-trips Pi -> native -> Pi with the same active-branch messages and context", async () => {
		const piPath = await writePiFixture(join(root, "project"));
		const sessionsRoot = join(root, "native");
		const imported = await importPiSession({ piSessionPath: piPath, sessionsRoot });
		const outputPath = join(root, "rollback", "restored.jsonl");

		const exported = await exportNativeSessionToPi({ sessionPath: imported.path, outputPath });
		expect(exported.path).toBe(outputPath);
		expect((await stat(outputPath)).mode & 0o777).toBe(0o600);

		const original = piBranch(piPath);
		const restored = SessionManager.open(outputPath);
		expect(restored.getHeader()).toMatchObject({ type: "session", version: 3, cwd: join(root, "project") });
		expect(messagesOf(restored.getBranch())).toEqual(messagesOf(original.branch));
		expect(restored.buildSessionContext().messages).toEqual(original.manager.buildSessionContext().messages);
		expect(restored.getSessionName()).toBe("Imported work");
		const compaction = restored.getBranch().find((entry) => entry.type === "compaction");
		expect(compaction).toMatchObject({ firstKeptEntryId: "m5", tokensBefore: 1234 });

		// Refuses to overwrite an existing file.
		await expect(exportNativeSessionToPi({ sessionPath: imported.path, outputPath })).rejects.toThrow();
	});
});

// =============================================================================
// Profile backup / restore
// =============================================================================

const PROFILE_FILES: Record<string, string> = {
	"settings.json": JSON.stringify({ defaultModel: "claude-test" }),
	"models.json": JSON.stringify({ providers: {} }),
	"auth.json": JSON.stringify({ anthropic: { type: "api_key", key: SECRET } }),
	"extensions/demo/index.ts": "export default () => {};\n",
	"skills/review/SKILL.md": "# Review\n",
	"prompts/fix.md": "Fix it.\n",
	"themes/night.json": "{}\n",
	"mcp-oauth/server.json": JSON.stringify({ token: SECRET }),
};

const NON_PROFILE_FILES = [
	"sessions/--project--/2026-01-01_x.jsonl",
	"experimental/sessions/--project--/y.jsonl",
	"traces/trace.log",
	"npm/node_modules/pkg/index.js",
];

async function writeProfile(agentDir: string): Promise<void> {
	for (const [path, content] of Object.entries(PROFILE_FILES)) {
		await mkdir(join(agentDir, path, ".."), { recursive: true });
		await writeFile(join(agentDir, path), content);
	}
	await chmod(join(agentDir, "auth.json"), 0o600);
	for (const path of NON_PROFILE_FILES) {
		await mkdir(join(agentDir, path, ".."), { recursive: true });
		await writeFile(join(agentDir, path), "session data");
	}
}

async function listFiles(dir: string, prefix = ""): Promise<string[]> {
	const out: string[] = [];
	for (const entry of await readdir(join(dir, prefix), { withFileTypes: true })) {
		const rel = prefix === "" ? entry.name : `${prefix}/${entry.name}`;
		if (entry.isDirectory()) out.push(...(await listFiles(dir, rel)));
		else out.push(rel);
	}
	return out.sort();
}

async function listDirs(dir: string, prefix = ""): Promise<string[]> {
	const out: string[] = [];
	for (const entry of await readdir(join(dir, prefix), { withFileTypes: true })) {
		if (!entry.isDirectory()) continue;
		const rel = prefix === "" ? entry.name : `${prefix}/${entry.name}`;
		out.push(rel, ...(await listDirs(dir, rel)));
	}
	return out;
}

describe("profile backup and restore", () => {
	it("backs up the profile privately with a manifest and excludes sessions", async () => {
		const agentDir = join(root, "agent");
		await writeProfile(agentDir);
		const { path, manifest } = await backupProfile({ agentDir, backupDir: join(root, "backups") });

		expect(path.startsWith(join(root, "backups", "ultron-profile-"))).toBe(true);
		expect(manifest.entries.map((entry) => entry.path).sort()).toEqual(Object.keys(PROFILE_FILES).sort());
		for (const entry of manifest.entries) {
			if (entry.type !== "file") throw new Error("expected files only");
			const expected = createHash("sha256").update(PROFILE_FILES[entry.path]!).digest("hex");
			expect(entry.sha256).toBe(expected);
		}
		expect(manifest.entries.find((entry) => entry.path === "auth.json")).toMatchObject({ mode: 0o600 });

		expect((await stat(path)).mode & 0o777).toBe(0o700);
		for (const dir of await listDirs(path)) expect((await stat(join(path, dir))).mode & 0o777).toBe(0o700);
		const files = await listFiles(path);
		expect(files).toContain("manifest.json");
		for (const file of files) expect((await stat(join(path, file))).mode & 0o777).toBe(0o600);
		expect(files.some((file) => file.includes("sessions") || file.includes("traces"))).toBe(false);
	});

	it("rehearses restore: every profile file comes back byte-identical and sessions are untouched", async () => {
		const agentDir = join(root, "agent");
		await writeProfile(agentDir);
		const { path } = await backupProfile({ agentDir, backupDir: join(root, "backups") });
		const snapshot = new Map<string, Buffer>();
		for (const file of await listFiles(agentDir)) snapshot.set(file, await readFile(join(agentDir, file)));

		// Damage the profile.
		await writeFile(join(agentDir, "settings.json"), "{broken");
		await rm(join(agentDir, "auth.json"));
		await rm(join(agentDir, "extensions"), { recursive: true });
		await writeFile(join(agentDir, "sessions/--project--/2026-01-01_x.jsonl"), "newer session data");

		const result = await restoreProfile({ backupPath: path, agentDir });
		expect(result.restored).toBe(Object.keys(PROFILE_FILES).length);

		// Profile parity: every backed-up file is byte-identical to the original.
		for (const file of Object.keys(PROFILE_FILES)) {
			expect((await readFile(join(agentDir, file))).equals(snapshot.get(file)!)).toBe(true);
		}
		expect((await stat(join(agentDir, "auth.json"))).mode & 0o777).toBe(0o600);
		// Sessions are not part of the profile and are never overwritten.
		expect(await readFile(join(agentDir, "sessions/--project--/2026-01-01_x.jsonl"), "utf8")).toBe(
			"newer session data",
		);

		// Restoring into a fresh directory also reproduces the full profile.
		const fresh = join(root, "fresh-agent");
		await restoreProfile({ backupPath: path, agentDir: fresh });
		expect(await listFiles(fresh)).toEqual(Object.keys(PROFILE_FILES).sort());
		for (const file of Object.keys(PROFILE_FILES)) {
			expect((await readFile(join(fresh, file))).equals(snapshot.get(file)!)).toBe(true);
		}
	});

	it("refuses a tampered backup without touching the profile", async () => {
		const agentDir = join(root, "agent");
		await writeProfile(agentDir);
		const { path } = await backupProfile({ agentDir, backupDir: join(root, "backups") });
		await writeFile(join(agentDir, "settings.json"), "current");

		await writeFile(join(path, "files", "models.json"), JSON.stringify({ providers: { evil: {} } }));
		const error = await restoreProfile({ backupPath: path, agentDir }).catch((caught: unknown) => caught);
		expect(error).toBeInstanceOf(ProfileBackupVerificationError);
		expect((error as ProfileBackupVerificationError).problems).toEqual(["hash mismatch: models.json"]);
		expect((error as Error).message).not.toContain(SECRET);
		expect(await readFile(join(agentDir, "settings.json"), "utf8")).toBe("current");
		expect(await readFile(join(agentDir, "models.json"), "utf8")).toBe(PROFILE_FILES["models.json"]);
	});

	it("refuses backups with unlisted files or unsafe manifest paths", async () => {
		const agentDir = join(root, "agent");
		await writeProfile(agentDir);
		const first = await backupProfile({ agentDir, backupDir: join(root, "backups") });
		await writeFile(join(first.path, "files", "extensions", "injected.ts"), "evil");
		await expect(restoreProfile({ backupPath: first.path, agentDir })).rejects.toMatchObject({
			problems: ["not in manifest: extensions/injected.ts"],
		});

		const second = await backupProfile({ agentDir, backupDir: join(root, "backups") });
		expect(second.path).not.toBe(first.path);
		const manifestPath = join(second.path, "manifest.json");
		const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
		manifest.entries[0].path = "../escape.json";
		await writeFile(manifestPath, JSON.stringify(manifest));
		await expect(restoreProfile({ backupPath: second.path, agentDir })).rejects.toBeInstanceOf(
			ProfileBackupVerificationError,
		);
	});
});

// =============================================================================
// CLI
// =============================================================================

function captureIo() {
	const stdout: string[] = [];
	const stderr: string[] = [];
	return {
		io: { stdout: (line: string) => stdout.push(line), stderr: (line: string) => stderr.push(line) },
		stdout,
		stderr,
	};
}

describe("runMigrationCommand", () => {
	it("ignores non-migrate commands", async () => {
		const { io, stdout, stderr } = captureIo();
		expect(await runMigrationCommand([], io)).toBe(false);
		expect(await runMigrationCommand(["install", "migrate"], io)).toBe(false);
		expect(stdout).toEqual([]);
		expect(stderr).toEqual([]);
	});

	it("prints usage and rejects bad arguments", async () => {
		let capture = captureIo();
		expect(await runMigrationCommand(["migrate"], capture.io)).toBe(true);
		expect(capture.stdout.join("\n")).toContain("import-pi");
		expect(process.exitCode).toBe(1);

		process.exitCode = undefined;
		capture = captureIo();
		expect(await runMigrationCommand(["migrate", "--help"], capture.io)).toBe(true);
		expect(process.exitCode).toBeUndefined();

		for (const args of [
			["migrate", "bogus"],
			["migrate", "import-pi"],
			["migrate", "export-pi", "only-one.jsonl"],
			["migrate", "restore"],
			["migrate", "backup", "a", "b"],
			["migrate", "import-pi", "x.jsonl", "--nope", "1"],
			["migrate", "import-pi", "x.jsonl", "--sessions-root"],
		]) {
			process.exitCode = undefined;
			capture = captureIo();
			expect(await runMigrationCommand(args, capture.io)).toBe(true);
			expect(process.exitCode).toBe(1);
			expect(capture.stderr[0]).toMatch(/^Error: /);
		}
	});

	it("runs import, export, backup, and restore end to end without printing secrets", async () => {
		const piPath = await writePiFixture(join(root, "project"));
		const sessionsRoot = join(root, "native");
		let capture = captureIo();
		expect(
			await runMigrationCommand(["migrate", "import-pi", piPath, "--sessions-root", sessionsRoot], capture.io),
		).toBe(true);
		expect(capture.stderr).toEqual([]);
		expect(process.exitCode).toBeUndefined();
		const sessionId = nativeSessionIdForPiSession(PI_SESSION_ID);
		expect(capture.stdout[0]).toBe(`Imported 10 entries as native session ${sessionId}`);
		expect(capture.stdout).toContain("  skipped 1 label entry");

		capture = captureIo();
		await runMigrationCommand(["migrate", "import-pi", piPath, `--sessions-root=${sessionsRoot}`], capture.io);
		expect(process.exitCode).toBe(1);
		expect(capture.stderr[0]).toContain("already imported");
		process.exitCode = undefined;

		const nativePath = (await readNative(sessionsRoot, sessionId)).metadata.path;
		const outPath = join(root, "out.jsonl");
		capture = captureIo();
		await runMigrationCommand(["migrate", "export-pi", nativePath, outPath], capture.io);
		expect(process.exitCode).toBeUndefined();
		expect(messagesOf(SessionManager.open(outPath).getBranch())).toEqual(messagesOf(piBranch(piPath).branch));

		const agentDir = join(root, "agent");
		await writeProfile(agentDir);
		capture = captureIo();
		await runMigrationCommand(["migrate", "backup", join(root, "backups"), "--agent-dir", agentDir], capture.io);
		expect(process.exitCode).toBeUndefined();
		const backupPath = capture.stdout[0]!.split(" to ")[1]!;
		await writeFile(join(agentDir, "auth.json"), "{}");

		const restoreCapture = captureIo();
		await runMigrationCommand(["migrate", "restore", backupPath, "--agent-dir", agentDir], restoreCapture.io);
		expect(process.exitCode).toBeUndefined();
		expect(await readFile(join(agentDir, "auth.json"), "utf8")).toBe(PROFILE_FILES["auth.json"]);

		await writeFile(join(backupPath, "files", "auth.json"), "tampered");
		const tamperCapture = captureIo();
		await runMigrationCommand(["migrate", "restore", backupPath, "--agent-dir", agentDir], tamperCapture.io);
		expect(process.exitCode).toBe(1);
		expect(tamperCapture.stderr.join("\n")).toContain("hash mismatch: auth.json");

		const printed = [capture, restoreCapture, tamperCapture].flatMap((c) => [...c.stdout, ...c.stderr]).join("\n");
		expect(printed).not.toContain(SECRET);
	});
});
