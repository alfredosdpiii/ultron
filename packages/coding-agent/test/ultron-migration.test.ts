import { createHash } from "node:crypto";
import { chmod, mkdir, mkdtemp, readdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
	type AgentMessage,
	BACKGROUND_CONTEXT,
	branchTip,
	type Entry,
	entryLabel,
	type JsonlSessionMetadata,
	JsonlSessionRepo,
	type LaneConfiguration,
	laneConfig,
	type Session,
	StorageBackedSession,
	value,
} from "@ultron/agent-core";
import { NodeExecutionEnv } from "@ultron/agent-core/node";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { buildSessionContext as buildNativeContext } from "../../agent/src/harness/session/context.ts";
import {
	buildSessionContext,
	loadEntriesFromFile,
	type SessionEntry,
	SessionManager,
} from "../src/core/session-manager.ts";
import {
	backupProfile,
	exportNativeSessionToPi,
	importPiSession,
	nativeSessionIdForPiSession,
	PI_ENTRY_CUSTOM_TYPE_PREFIX,
	PI_LEAF_MARKER_CUSTOM_TYPE,
	PI_OMITTED_MESSAGE_CUSTOM_TYPE,
	PiSessionAlreadyImportedError,
	ProfileBackupVerificationError,
	restoreProfile,
} from "../src/ultron/migration.ts";
import { runMigrationCommand } from "../src/ultron/migration-cli.ts";
import { reconcileRecords } from "../src/ultron/reconcile.ts";
import { createSessionUsageLedger } from "../src/ultron/usage.ts";
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

interface NativeView {
	metadata: JsonlSessionMetadata;
	entries: Entry[];
	tip: string | null;
	allEntries: Entry[];
	sessionCount: number;
	labels: Map<string, string>;
	configuration: LaneConfiguration | undefined;
	/** Native model context at the given entry, as the harness builds it. */
	contextAt(leafId: string): Promise<AgentMessage[]>;
	session: Session<JsonlSessionMetadata>;
}

async function withNative<T>(
	sessionsRoot: string,
	sessionId: string,
	body: (native: NativeView) => Promise<T>,
): Promise<T> {
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
		const labels = new Map(
			(await session.scanValues(entryLabel(""), BACKGROUND_CONTEXT)).map((stored) => [
				stored.address.key,
				stored.value,
			]),
		);
		const configuration = (await session.getValue(laneConfig("main"), BACKGROUND_CONTEXT))?.value;
		const contextAt = async (leafId: string) =>
			buildNativeContext(
				await session.scanBranch({ start: leafId, order: "oldestFirst" }, BACKGROUND_CONTEXT),
				undefined,
				BACKGROUND_CONTEXT,
			);
		return await body({
			metadata: metadata[0]!,
			entries,
			tip,
			allEntries,
			sessionCount: all.length,
			labels,
			configuration,
			contextAt,
			session,
		});
	} finally {
		await session.close(BACKGROUND_CONTEXT);
		await repo.close(BACKGROUND_CONTEXT);
		await fileSystem.cleanup(BACKGROUND_CONTEXT);
	}
}

function readNative(sessionsRoot: string, sessionId: string) {
	return withNative(sessionsRoot, sessionId, async ({ contextAt: _contextAt, session: _session, ...view }) => view);
}

/**
 * A multi-branch Pi session: an abandoned side branch with its own model change, label, compaction, usage,
 * and a context edit; a main branch with a branch summary, custom message, compaction, labels (one cleared),
 * a thinking change, a replacing and an omitting context edit, usage, an unknown future entry type, a custom
 * entry, and a final model change at the leaf.
 */
async function writeTreeFixture(cwd: string): Promise<string> {
	const warmUsage = { ...usage, input: 3, output: 0, totalTokens: 3 };
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
		// Side branch (abandoned).
		{
			type: "message",
			id: "x1",
			parentId: "m5",
			timestamp: ts(8),
			message: assistant([{ type: "text", text: "Trying X." }], "stop", 8),
		},
		{ type: "label", id: "xl", parentId: "x1", timestamp: ts(9), targetId: "x1", label: "abandoned-x" },
		{ type: "model_change", id: "xm", parentId: "xl", timestamp: ts(10), provider: "openai", modelId: "gpt-test" },
		{ type: "message", id: "x2", parentId: "xm", timestamp: ts(11), message: user("X step two draft", 11) },
		{
			type: "message",
			id: "x3",
			parentId: "x2",
			timestamp: ts(12),
			message: {
				...assistant([{ type: "text", text: "X half done." }], "stop", 12),
				provider: "openai",
				model: "gpt-test",
			},
		},
		{
			type: "compaction",
			id: "xc",
			parentId: "x3",
			timestamp: ts(13),
			summary: "X so far.",
			firstKeptEntryId: "x2",
			tokensBefore: 500,
		},
		{
			type: "usage",
			id: "xu",
			parentId: "xc",
			timestamp: ts(14),
			kind: "cache_warm",
			provider: "openai",
			model: "gpt-test",
			usage: warmUsage,
		},
		{
			type: "context_edit",
			id: "xe",
			parentId: "xu",
			timestamp: ts(15),
			targetId: "x2",
			replacement: { content: [{ type: "text", text: "X step two (redacted)" }] },
		},
		{ type: "message", id: "x4", parentId: "xe", timestamp: ts(16), message: user("X failed", 16) },
		// Main branch.
		{
			type: "branch_summary",
			id: "b1",
			parentId: "m5",
			timestamp: ts(17),
			fromId: "x4",
			summary: "Approach X failed.",
			fromHook: false,
		},
		{
			type: "custom_message",
			id: "cm",
			parentId: "b1",
			timestamp: ts(18),
			customType: "note",
			content: "Remember Y",
			display: true,
		},
		{
			type: "message",
			id: "m6",
			parentId: "cm",
			timestamp: ts(19),
			message: assistant([{ type: "text", text: "Using approach Y." }], "stop", 19),
		},
		{
			type: "compaction",
			id: "c1",
			parentId: "m6",
			timestamp: ts(20),
			summary: "Listed files, X failed, Y chosen.",
			firstKeptEntryId: "m5",
			tokensBefore: 1234,
			details: { readFiles: [], modifiedFiles: [] },
		},
		{ type: "label", id: "la", parentId: "c1", timestamp: ts(21), targetId: "m4", label: "checkpoint" },
		{ type: "label", id: "lt", parentId: "la", timestamp: ts(22), targetId: "m2", label: "temp" },
		{ type: "label", id: "lc", parentId: "lt", timestamp: ts(23), targetId: "m2" },
		{ type: "thinking_level_change", id: "t1", parentId: "lc", timestamp: ts(24), thinkingLevel: "low" },
		{
			type: "context_edit",
			id: "ce1",
			parentId: "t1",
			timestamp: ts(25),
			targetId: "m6",
			replacement: { content: [{ type: "text", text: "Using approach Y (edited)." }] },
		},
		{ type: "message", id: "m7", parentId: "ce1", timestamp: ts(26), message: user("noise", 26) },
		{ type: "context_edit", id: "ce2", parentId: "m7", timestamp: ts(27), targetId: "m7", replacement: null },
		{ type: "session_info", id: "s1", parentId: "ce2", timestamp: ts(28), name: "Imported work" },
		{ type: "future_thing", id: "f1", parentId: "s1", timestamp: ts(29), payload: { answer: 42 } },
		{
			type: "usage",
			id: "u1",
			parentId: "f1",
			timestamp: ts(30),
			kind: "cache_warm",
			provider: "anthropic",
			model: "claude-test",
			usage: warmUsage,
			note: "warm",
		},
		{ type: "custom", id: "cx", parentId: "u1", timestamp: ts(31), customType: "ext-state", data: { n: 1 } },
		{
			type: "message",
			id: "m8",
			parentId: "cx",
			timestamp: ts(32),
			message: assistant([{ type: "text", text: "Continuing with Y." }], "stop", 32),
		},
		{
			type: "model_change",
			id: "mc",
			parentId: "m8",
			timestamp: ts(33),
			provider: "anthropic",
			modelId: "claude-next",
		},
	];
	const path = join(root, "pi", "tree.jsonl");
	await mkdir(join(root, "pi"), { recursive: true });
	await writeFile(path, `${lines.map((line) => JSON.stringify(line)).join("\n")}\n`);
	return path;
}

function openPi(path: string): SessionManager {
	return SessionManager.inMemory(root, undefined, loadEntriesFromFile(path));
}

function piContextAt(manager: SessionManager, leafId: string) {
	return buildSessionContext(manager.getEntries(), leafId);
}

function json(value: unknown): unknown {
	return JSON.parse(JSON.stringify(value));
}

function allLabels(manager: SessionManager): Record<string, string | undefined> {
	return Object.fromEntries(manager.getEntries().map((entry) => [entry.id, manager.getLabel(entry.id)]));
}

describe("importPiSession", () => {
	it("imports every entry of the linear fixture, with nothing skipped, and resumes at the Pi leaf", async () => {
		const cwd = join(root, "project");
		const piPath = await writePiFixture(cwd);
		const sessionsRoot = join(root, "native");
		const before = await readFile(piPath, "utf8");

		const result = await importPiSession({ piSessionPath: piPath, sessionsRoot });

		expect(await readFile(piPath, "utf8")).toBe(before);
		expect(result.sessionId).toBe(nativeSessionIdForPiSession(PI_SESSION_ID));
		expect(result.sessionId).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-8[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
		expect(result.imported).toBe(16);
		expect(result.skipped).toEqual([]);
		expect(result.preserved).toEqual([]);
		expect(result.unappliedContextEdits).toEqual([]);

		const native = await readNative(sessionsRoot, result.sessionId);
		expect(native.metadata.cwd).toBe(cwd);
		expect(native.metadata.path).toBe(result.path);
		expect(native.metadata.createdAt).toBe(Date.parse(ts(0)));
		expect((await readFile(result.path, "utf8")).split("\n", 1)[0]).toContain('"kind":"header"');

		const { branch, manager } = piBranch(piPath);
		expect(messagesOf(native.entries)).toEqual(messagesOf(branch));
		expect(native.entries.map((entry) => entry.id)).toEqual(branch.map((entry) => entry.id));
		expect(native.entries.map((entry) => entry.parentId)).toEqual(branch.map((entry) => entry.parentId));
		expect(native.entries.map((entry) => entry.timestamp)).toEqual(
			branch.map((entry) => Date.parse(entry.timestamp)),
		);
		// The abandoned branch is imported too, with its parents.
		expect(native.allEntries).toHaveLength(16);
		expect(native.allEntries.find((entry) => entry.id === "x2")).toMatchObject({ parentId: "x1" });

		// Resumes at the Pi leaf with Pi's resolved labels, model, and thinking level.
		expect(manager.getLeafId()).toBe("m8");
		expect(native.tip).toBe("m8");
		expect(native.labels).toEqual(new Map([["m4", "checkpoint"]]));
		expect(native.configuration).toEqual({
			model: { provider: "anthropic", modelId: "claude-test" },
			thinkingLevel: "high",
			activeToolNames: [],
		});
		expect(native.entries.find((entry) => entry.id === "e1")).toMatchObject({
			type: "custom",
			customType: `${PI_ENTRY_CUSTOM_TYPE_PREFIX}model_change`,
			data: { provider: "anthropic", modelId: "claude-test" },
		});

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

	it("imports the whole tree with labels, model history, usage, context edits, and unknown types", async () => {
		const piPath = await writeTreeFixture(join(root, "project"));
		const sessionsRoot = join(root, "native");
		const before = await readFile(piPath, "utf8");
		const result = await importPiSession({ piSessionPath: piPath, sessionsRoot });
		expect(await readFile(piPath, "utf8")).toBe(before);

		const pi = openPi(piPath);
		const piEntries = pi.getEntries();
		expect(result.imported).toBe(piEntries.length);
		expect(result.skipped).toEqual([]);
		expect(result.preserved).toEqual([{ type: "future_thing", count: 1 }]);
		expect(result.unappliedContextEdits).toEqual([]);

		await withNative(sessionsRoot, result.sessionId, async (native) => {
			// Same entries, ids, parents, and timestamps; tip at the Pi leaf.
			expect(native.allEntries.map((entry) => [entry.id, entry.parentId, entry.timestamp])).toEqual(
				piEntries.map((entry) => [entry.id, entry.parentId, Date.parse(entry.timestamp)]),
			);
			expect(pi.getLeafId()).toBe("mc");
			expect(native.tip).toBe("mc");

			// Labels: Pi's resolved labels on both branches (a cleared label stays cleared).
			expect(native.labels).toEqual(
				new Map([
					["x1", "abandoned-x"],
					["m4", "checkpoint"],
				]),
			);
			expect(await native.session.getLabel("m2", BACKGROUND_CONTEXT)).toBeUndefined();

			// Lane configuration: the model and thinking level Pi resolves at the leaf.
			expect(piContextAt(pi, "mc")).toMatchObject({
				model: { provider: "anthropic", modelId: "claude-next" },
				thinkingLevel: "low",
			});
			expect(native.configuration).toEqual({
				model: { provider: "anthropic", modelId: "claude-next" },
				thinkingLevel: "low",
				activeToolNames: [],
			});

			// Every Pi-only entry is a documented custom entry carrying the Pi fields.
			const byId = new Map(native.allEntries.map((entry) => [entry.id, entry]));
			expect(byId.get("xm")).toMatchObject({
				type: "custom",
				customType: "pi-session:model_change",
				data: { provider: "openai", modelId: "gpt-test" },
			});
			expect(byId.get("t1")).toMatchObject({
				customType: "pi-session:thinking_level_change",
				data: { thinkingLevel: "low" },
			});
			expect(byId.get("la")).toMatchObject({
				customType: "pi-session:label",
				data: { targetId: "m4", label: "checkpoint" },
			});
			expect(byId.get("lc")).toMatchObject({ customType: "pi-session:label", data: { targetId: "m2" } });
			expect(byId.get("s1")).toMatchObject({
				customType: "pi-session:session_info",
				data: { name: "Imported work" },
			});
			expect(byId.get("u1")).toMatchObject({
				customType: "pi-session:usage",
				data: { kind: "cache_warm", provider: "anthropic", model: "claude-test", note: "warm" },
			});
			expect(byId.get("ce1")).toMatchObject({ customType: "pi-session:context_edit", data: { targetId: "m6" } });
			expect(byId.get("f1")).toMatchObject({
				customType: "pi-session:future_thing",
				data: { payload: { answer: 42 } },
			});
			expect(byId.get("cx")).toMatchObject({ type: "custom", customType: "ext-state", data: { n: 1 } });
			expect(byId.get("cm")).toMatchObject({ type: "message", message: { role: "custom", customType: "note" } });

			// Context edits are baked into their targets: m6 is replaced, m7 is omitted, x2 is replaced.
			expect(byId.get("m6")).toMatchObject({
				type: "message",
				message: { content: [{ type: "text", text: "Using approach Y (edited)." }] },
			});
			expect(byId.get("m7")).toMatchObject({
				type: "custom",
				customType: PI_OMITTED_MESSAGE_CUSTOM_TYPE,
				data: { message: { role: "user" } },
			});
			expect(JSON.stringify(byId.get("x2"))).not.toContain("draft");

			// The effective model context is identical to Pi's at the main leaf and at the side branch's leaf.
			for (const leafId of ["mc", "x4", "m8", "m5"]) {
				expect(json(await native.contextAt(leafId))).toEqual(json(piContextAt(pi, leafId).messages));
			}
			// And the context edits really change what Pi shows.
			expect(JSON.stringify(piContextAt(pi, "mc").messages)).toContain("Using approach Y (edited).");
			expect(JSON.stringify(piContextAt(pi, "mc").messages)).not.toContain("noise");
			expect(JSON.stringify(piContextAt(pi, "x4").messages)).toContain("X step two (redacted)");

			// The abandoned branch is navigable: a native branch at its leaf resumes it.
			const side = await native.session.createBranch("side", "x4", BACKGROUND_CONTEXT);
			const sidePath = await side.findEntries({ order: "oldestFirst" }, BACKGROUND_CONTEXT);
			expect(sidePath.map((entry) => entry.id)).toEqual(pi.getBranch("x4").map((entry) => entry.id));
		});
	});

	it("applies a context edit that only some branches carry on those branches only", async () => {
		const cwd = join(root, "project");
		const lines = [
			{ type: "session", version: 3, id: PI_SESSION_ID, timestamp: ts(0), cwd },
			{ type: "message", id: "a", parentId: null, timestamp: ts(1), message: user("shared", 1) },
			{
				type: "message",
				id: "b",
				parentId: "a",
				timestamp: ts(2),
				message: assistant([{ type: "text", text: "one" }], "stop", 2),
			},
			{
				type: "message",
				id: "c",
				parentId: "a",
				timestamp: ts(3),
				message: assistant([{ type: "text", text: "two" }], "stop", 3),
			},
			{
				type: "context_edit",
				id: "e",
				parentId: "c",
				timestamp: ts(4),
				targetId: "a",
				replacement: { content: "edited" },
			},
		];
		const piPath = join(root, "edit.jsonl");
		await writeFile(piPath, `${lines.map((line) => JSON.stringify(line)).join("\n")}\n`);
		const sessionsRoot = join(root, "native");
		const result = await importPiSession({ piSessionPath: piPath, sessionsRoot });
		expect(result.unappliedContextEdits).toEqual([]);
		expect(result).toMatchObject({ imported: 4, branchCopies: 1 });

		const pi = openPi(piPath);
		await withNative(sessionsRoot, result.sessionId, async (native) => {
			// The sibling branch keeps the untouched original; the edit's branch runs through an edited copy.
			for (const leafId of ["b", "e"])
				expect(json(await native.contextAt(leafId))).toEqual(json(piContextAt(pi, leafId).messages));
			expect(native.allEntries.find((entry) => entry.id === "a")).toMatchObject({ message: user("shared", 1) });
			expect(native.allEntries.find((entry) => entry.id === "a~e")).toMatchObject({
				parentId: null,
				message: { content: "edited" },
			});
			expect(native.allEntries.find((entry) => entry.id === "c")?.parentId).toBe("a~e");
			expect(native.tip).toBe("e");
			expect(JSON.stringify(await native.contextAt("e"))).toContain("edited");
		});
		// Pi still honors the edit after a round trip.
		const outputPath = join(root, "edit-out.jsonl");
		await exportNativeSessionToPi({ sessionPath: result.path, outputPath });
		const restored = SessionManager.open(outputPath);
		expect(restored.buildSessionContext()).toEqual(pi.buildSessionContext());
		expect(JSON.stringify(restored.buildSessionContext().messages)).toContain("edited");
	});

	it("matches Pi's context at every leaf with nested, competing, compacted, and omitting branch-local edits", async () => {
		const cwd = join(root, "project");
		const text = (id: string, at: number, role: "user" | "assistant" = "user") => ({
			type: "message",
			id,
			timestamp: ts(at),
			message:
				role === "user" ? user(`${id} text`, at) : assistant([{ type: "text", text: `${id} text` }], "stop", at),
		});
		const edit = (id: string, parentId: string, at: number, targetId: string, content: string | null) => ({
			type: "context_edit",
			id,
			parentId,
			timestamp: ts(at),
			targetId,
			replacement: content === null ? null : { content },
		});
		const lines = [
			{ type: "session", version: 3, id: PI_SESSION_ID, timestamp: ts(0), cwd },
			{ ...text("r", 1), parentId: null },
			{ ...text("t", 2, "assistant"), parentId: "r" },
			{
				type: "custom_message",
				id: "n",
				parentId: "t",
				timestamp: ts(3),
				customType: "note",
				content: "n text",
				display: true,
			},
			// Fork at n: branch A edits t and n; branch B edits t differently; branch C leaves them alone.
			{ ...text("a1", 4), parentId: "n" },
			edit("ea", "a1", 5, "t", "t edited on A"),
			{ ...text("a2", 6, "assistant"), parentId: "ea" },
			edit("en", "a2", 7, "n", null),
			// A sub-fork below both A edits: A1 edits t again (later edit wins), A2 compacts keeping t.
			{ ...text("a3", 8), parentId: "en" },
			edit("ea1", "a3", 9, "t", "t edited again on A1"),
			{ ...text("a4", 10, "assistant"), parentId: "ea1" },
			{
				type: "compaction",
				id: "ca",
				parentId: "en",
				timestamp: ts(11),
				summary: "A so far",
				firstKeptEntryId: "t",
				tokensBefore: 50,
			},
			edit("ea2", "ca", 12, "a1", "a1 edited after compaction"),
			{ ...text("a5", 13), parentId: "ea2" },
			{ ...text("a6", 14), parentId: "ca" },
			{ ...text("b1", 15), parentId: "n" },
			edit("eb", "b1", 16, "t", "t edited on B"),
			{ ...text("b2", 17, "assistant"), parentId: "eb" },
			{ ...text("c1", 18), parentId: "n" },
			// An edit whose target is on another branch: Pi applies it nowhere.
			edit("ex", "c1", 19, "b1", "never visible"),
		];
		const piPath = join(root, "edits.jsonl");
		await writeFile(piPath, `${lines.map((line) => JSON.stringify(line)).join("\n")}\n`);
		const sessionsRoot = join(root, "native");
		const result = await importPiSession({ piSessionPath: piPath, sessionsRoot });
		expect(result.unappliedContextEdits, JSON.stringify(result)).toEqual(["ex"]);
		expect(result.branchCopies).toBeGreaterThan(0);

		const pi = openPi(piPath);
		const piIds = pi.getEntries().map((entry) => entry.id);
		const parents = new Set(pi.getEntries().map((entry) => entry.parentId));
		const leaves = piIds.filter((id) => !parents.has(id));
		expect(leaves).toEqual(["a4", "a5", "a6", "b2", "ex"]);
		expect(result.imported).toBe(piIds.length);
		await withNative(sessionsRoot, result.sessionId, async (native) => {
			// Every Pi leaf has the same model context natively, and no native leaf exists that Pi lacks.
			for (const id of leaves)
				expect(json(await native.contextAt(id)), id).toEqual(json(piContextAt(pi, id).messages));
			const nativeParents = new Set(native.allEntries.map((entry) => entry.parentId));
			expect(
				native.allEntries
					.filter((entry) => !nativeParents.has(entry.id))
					.map((entry) => entry.id)
					.sort(),
			).toEqual(leaves);
			// The edits really differ per branch in Pi.
			const shown = (id: string) => JSON.stringify(piContextAt(pi, id).messages);
			expect(shown("c1")).toContain("t text");
			expect(shown("b2")).toContain("t edited on B");
			expect(shown("a2")).toContain("t edited on A");
			expect(shown("a4")).toContain("t edited again on A1");
			expect(shown("a4")).not.toContain("n text");
			expect(shown("a5")).toContain("a1 edited after compaction");
			expect(shown("a6")).not.toContain("a1 edited after compaction");
		});

		// Export drops the copies: the Pi file has exactly the original entries and the same context everywhere.
		const outputPath = join(root, "edits-out.jsonl");
		await exportNativeSessionToPi({ sessionPath: result.path, outputPath });
		const restored = SessionManager.open(outputPath);
		expect(restored.getEntries().map((entry) => [entry.id, entry.parentId])).toEqual(
			pi.getEntries().map((entry) => [entry.id, entry.parentId]),
		);
		for (const id of piIds) expect(json(piContextAt(restored, id)), id).toEqual(json(piContextAt(pi, id)));
	});

	it("imports Pi-reported usage into the usage ledger as a marked historical root", async () => {
		const priced = (input: number, output: number, cost: number) => ({
			input,
			output,
			cacheRead: 1,
			cacheWrite: 2,
			totalTokens: input + output + 3,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: cost },
		});
		const cwd = join(root, "project");
		const lines = [
			{ type: "session", version: 3, id: PI_SESSION_ID, timestamp: ts(0), cwd },
			{ type: "message", id: "u", parentId: null, timestamp: ts(1), message: user("hi", 1) },
			{
				type: "message",
				id: "a",
				parentId: "u",
				timestamp: ts(2),
				message: { ...assistant([{ type: "text", text: "hello" }], "stop", 2), usage: priced(100, 20, 0.25) },
			},
			{
				type: "usage",
				id: "w",
				parentId: "a",
				timestamp: ts(3),
				kind: "cache_warm",
				provider: "anthropic",
				model: "claude-test",
				usage: priced(7, 0, 0.125),
			},
			{
				type: "compaction",
				id: "c",
				parentId: "w",
				timestamp: ts(4),
				summary: "s",
				firstKeptEntryId: "a",
				tokensBefore: 10,
				usage: priced(50, 5, 0.5),
			},
		];
		const piPath = join(root, "usage.jsonl");
		await writeFile(piPath, `${lines.map((line) => JSON.stringify(line)).join("\n")}\n`);
		const sessionsRoot = join(root, "native");
		const result = await importPiSession({ piSessionPath: piPath, sessionsRoot });
		expect(result.importedUsage).toEqual({ root: `import:pi:${PI_SESSION_ID}`, entries: 3, cost: 0.875 });

		// Pi's own session totals, as its stats compute them.
		const piTotals = { input: 0, output: 0, cost: 0 };
		for (const entry of openPi(piPath).getEntries()) {
			const reported =
				entry.type === "usage" || entry.type === "compaction"
					? entry.usage
					: entry.type === "message" && entry.message.role === "assistant"
						? entry.message.usage
						: undefined;
			if (!reported) continue;
			piTotals.input += reported.input;
			piTotals.output += reported.output;
			piTotals.cost += reported.cost.total;
		}
		await withNative(sessionsRoot, result.sessionId, async (native) => {
			const ledger = createSessionUsageLedger(native.session);
			const status = await ledger.status();
			expect(status.session.usage).toMatchObject({
				calls: 3,
				modelCalls: 3,
				inputTokens: piTotals.input,
				outputTokens: piTotals.output,
				totalTokens: 100 + 20 + 7 + 50 + 5 + 9,
				cost: piTotals.cost,
				unknownCalls: 0,
			});
			expect(status.session.spentUsd).toBe(piTotals.cost);
			const stored = (
				await native.session.getValue(value<Record<string, unknown>>("ultron.usage", "root"), BACKGROUND_CONTEXT)
			)?.value as { roots: Record<string, { imported?: unknown }> };
			expect(stored.roots[`import:pi:${PI_SESSION_ID}`]?.imported).toEqual({ source: "pi", entries: 3 });
			// The imported root never admits new work, and reconciliation accounts it without discrepancies.
			await expect(ledger.reserve({ kind: "task", rootId: `import:pi:${PI_SESSION_ID}` })).rejects.toThrow(
				/imported history root/,
			);
			const reconciliation = reconcileRecords({ tasks: undefined, usage: stored as never });
			expect(reconciliation.discrepancies).toEqual([]);
			expect(reconciliation.imported).toMatchObject({ modelCalls: 3, cost: piTotals.cost });
		});
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

	it("removes a partial import when an entry cannot be written, so a retry is possible", async () => {
		const piPath = await writeTreeFixture(join(root, "project"));
		const before = await readFile(piPath, "utf8");
		const sessionsRoot = join(root, "native");
		let calls = 0;
		const original = StorageBackedSession.prototype.mutate;
		const spy = vi.spyOn(StorageBackedSession.prototype, "mutate").mockImplementation(function (
			this: StorageBackedSession,
			...args: Parameters<StorageBackedSession["mutate"]>
		) {
			if (++calls === 10) return Promise.reject(new Error("disk full"));
			return original.apply(this, args);
		} as StorageBackedSession["mutate"]);
		try {
			await expect(importPiSession({ piSessionPath: piPath, sessionsRoot })).rejects.toThrow("disk full");
		} finally {
			spy.mockRestore();
		}
		expect(calls).toBe(10);
		expect(await readFile(piPath, "utf8")).toBe(before);
		const fileSystem = new NodeExecutionEnv({ cwd: process.cwd() });
		const repo = new JsonlSessionRepo({ fileSystem, sessionsRoot });
		expect(await repo.list(undefined, BACKGROUND_CONTEXT)).toEqual([]);
		await repo.close(BACKGROUND_CONTEXT);
		await fileSystem.cleanup(BACKGROUND_CONTEXT);

		await expect(importPiSession({ piSessionPath: piPath, sessionsRoot })).resolves.toMatchObject({
			imported: openPi(piPath).getEntries().length,
		});
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

	it("round-trips the whole tree: identical getTree, contexts at every leaf, labels, and name", async () => {
		for (const fixture of [writePiFixture, writeTreeFixture]) {
			await rm(join(root, "native"), { recursive: true, force: true });
			const piPath = await fixture(join(root, "project"));
			const sessionsRoot = join(root, "native");
			const imported = await importPiSession({ piSessionPath: piPath, sessionsRoot });
			const outputPath = join(root, "rollback", `${fixture.name}.jsonl`);
			const exported = await exportNativeSessionToPi({ sessionPath: imported.path, outputPath });

			const original = openPi(piPath);
			const restored = SessionManager.open(outputPath);
			expect(exported.entries).toBe(original.getEntries().length);
			expect(json(restored.getTree())).toEqual(json(original.getTree()));
			expect(restored.getLeafId()).toBe(original.getLeafId());
			expect(restored.buildSessionContext()).toEqual(original.buildSessionContext());
			const leaves = original
				.getEntries()
				.filter((entry) => original.getChildren(entry.id).length === 0)
				.map((entry) => entry.id);
			expect(leaves.length).toBe(2);
			for (const leafId of leaves) {
				expect(piContextAt(restored, leafId)).toEqual(piContextAt(original, leafId));
			}
			expect(allLabels(restored)).toEqual(allLabels(original));
			expect(restored.getSessionName()).toBe(original.getSessionName());
		}
	});

	it("exports native changes made after import: new entries, labels, name, model, and thinking level", async () => {
		const piPath = await writeTreeFixture(join(root, "project"));
		const sessionsRoot = join(root, "native");
		const imported = await importPiSession({ piSessionPath: piPath, sessionsRoot });
		await withNative(sessionsRoot, imported.sessionId, async ({ session }) => {
			const main = (await session.branch("main", BACKGROUND_CONTEXT))!;
			await main.appendMessage(user("native follow-up", 40) as AgentMessage, BACKGROUND_CONTEXT);
			await session.setLabel("m1", "start", BACKGROUND_CONTEXT);
			await session.setLabel("m4", undefined, BACKGROUND_CONTEXT);
			await session.setName("Renamed", BACKGROUND_CONTEXT);
			await session.setValue(
				laneConfig("main"),
				{ model: { provider: "openai", modelId: "gpt-next" }, thinkingLevel: "medium", activeToolNames: [] },
				BACKGROUND_CONTEXT,
			);
		});
		const outputPath = join(root, "changed.jsonl");
		await exportNativeSessionToPi({ sessionPath: imported.path, outputPath });

		const restored = SessionManager.open(outputPath);
		const context = restored.buildSessionContext();
		expect(context.model).toEqual({ provider: "openai", modelId: "gpt-next" });
		expect(context.thinkingLevel).toBe("medium");
		expect(JSON.stringify(context.messages.at(-1))).toContain("native follow-up");
		expect(restored.getLabel("m1")).toBe("start");
		expect(restored.getLabel("m4")).toBeUndefined();
		expect(restored.getLabel("x1")).toBe("abandoned-x");
		expect(restored.getSessionName()).toBe("Renamed");
		// The abandoned branch is still there.
		expect(piContextAt(restored, "x4")).toEqual(piContextAt(openPi(piPath), "x4"));
	});

	it("resumes Pi at the native tip when the tip is not the newest entry", async () => {
		const piPath = await writeTreeFixture(join(root, "project"));
		const sessionsRoot = join(root, "native");
		const imported = await importPiSession({ piSessionPath: piPath, sessionsRoot });
		const original = openPi(piPath);

		// Tip moved to a leaf of the abandoned branch.
		await withNative(sessionsRoot, imported.sessionId, ({ session }) =>
			session.setValue(branchTip("main"), "x4", BACKGROUND_CONTEXT),
		);
		const leafOut = join(root, "leaf.jsonl");
		await exportNativeSessionToPi({ sessionPath: imported.path, outputPath: leafOut });
		let restored = SessionManager.open(leafOut);
		// Pi resumes on the x4 path; the main lane's model and thinking level follow it as change entries.
		expect(restored.getBranch().map((entry) => entry.id)).toEqual([
			...original.getBranch("x4").map((entry) => entry.id),
			"ultron-model",
			"ultron-thinking",
		]);
		expect(restored.buildSessionContext()).toEqual({
			messages: piContextAt(original, "x4").messages,
			model: { provider: "anthropic", modelId: "claude-next" },
			thinkingLevel: "low",
		});

		// Tip moved to an interior entry: a marker child makes Pi resume there.
		await withNative(sessionsRoot, imported.sessionId, ({ session }) =>
			session.setValue(branchTip("main"), "m6", BACKGROUND_CONTEXT),
		);
		const interiorOut = join(root, "interior.jsonl");
		await exportNativeSessionToPi({ sessionPath: imported.path, outputPath: interiorOut });
		restored = SessionManager.open(interiorOut);
		expect(restored.getBranch().slice(-3)).toMatchObject([
			{ type: "custom", customType: PI_LEAF_MARKER_CUSTOM_TYPE, parentId: "m6" },
			{ type: "model_change" },
			{ type: "thinking_level_change" },
		]);
		expect(restored.buildSessionContext().messages).toEqual(piContextAt(original, "m6").messages);
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
		expect(capture.stdout[0]).toBe(`Imported 16 entries as native session ${sessionId}`);
		expect(capture.stdout.some((line) => line.includes("skipped"))).toBe(false);

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
