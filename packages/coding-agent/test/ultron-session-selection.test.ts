import { mkdtemp, rm, symlink, utimes, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { BACKGROUND_CONTEXT, JsonlSessionRepo } from "@earendil-works/pi-agent-core";
import { NodeExecutionEnv } from "@earendil-works/pi-agent-core/node";
import { afterEach, describe, expect, test } from "vitest";
import { chooseInteractiveResumeSession, NativeSessionSelector } from "../src/ultron/session-selection.ts";

const NOW = 1_700_000_000_000;
const tempDirectories: string[] = [];

async function createSelector(): Promise<{ root: string; selector: NativeSessionSelector; repo: JsonlSessionRepo }> {
	const root = await mkdtemp(join(tmpdir(), "ultron-session-selection-"));
	tempDirectories.push(root);
	const fileSystem = new NodeExecutionEnv({ cwd: root });
	const repo = new JsonlSessionRepo({ fileSystem, sessionsRoot: join(root, "sessions"), now: () => NOW });
	return {
		root,
		repo,
		selector: new NativeSessionSelector({
			sessionDir: join(root, "sessions"),
			cwd: root,
			fileSystem,
			repo,
		}),
	};
}

afterEach(async () => {
	await Promise.all(tempDirectories.splice(0).map((directory) => rm(directory, { recursive: true, force: true })));
});

describe("NativeSessionSelector", () => {
	test("uses the real JsonlSessionRepo and scopes discovery to cwd", async () => {
		const { selector, repo, root } = await createSelector();
		const current = await repo.create({ id: "current", cwd: root }, BACKGROUND_CONTEXT);
		const other = await repo.create({ id: "other", cwd: resolve(root, "other") }, BACKGROUND_CONTEXT);
		await current.close(BACKGROUND_CONTEXT);
		await other.close(BACKGROUND_CONTEXT);

		expect(selector.repo).toBe(repo);
		expect((await selector.discover()).map((session) => session.id)).toEqual(["current"]);
		await selector.close();
	});

	test("resolves exact ids, unique prefixes, and rejects ambiguous prefixes", async () => {
		const { selector, repo, root } = await createSelector();
		for (const id of ["abc123", "def456", "def789"]) {
			const session = await repo.create({ id, cwd: root }, BACKGROUND_CONTEXT);
			await session.close(BACKGROUND_CONTEXT);
		}

		expect((await selector.resolve("abc123")).matchedBy).toBe("id");
		expect((await selector.resolve("abc")).metadata.id).toBe("abc123");
		await expect(selector.resolve("def")).rejects.toMatchObject({
			code: "ambiguous",
		});
		await selector.close();
	});

	test("continue selects by modification time, not creation time", async () => {
		const { selector, repo, root } = await createSelector();
		const older = await repo.create({ id: "older", cwd: root }, BACKGROUND_CONTEXT);
		const newer = await repo.create({ id: "newer", cwd: root }, BACKGROUND_CONTEXT);
		await older.close(BACKGROUND_CONTEXT);
		await newer.close(BACKGROUND_CONTEXT);
		await utimes(older.metadata.path, new Date(2_000), new Date(20_000));
		await utimes(newer.metadata.path, new Date(1_000), new Date(10_000));

		const selected = await selector.continueSession();
		expect(selected?.metadata.id).toBe("older");
		expect(selected?.matchedBy).toBe("continue");
		await selector.close();
	});

	test("preserves an explicit path identity, including a symlink alias", async () => {
		const { selector, repo, root } = await createSelector();
		const session = await repo.create({ id: "path-id", cwd: root }, BACKGROUND_CONTEXT);
		await session.close(BACKGROUND_CONTEXT);
		const alias = join(root, "alias.jsonl");
		await symlink(session.metadata.path, alias);

		const selected = await selector.resolve(alias);
		expect(selected.metadata.path).toBe(alias);
		expect(selected.metadata.id).toBe("path-id");
		await selector.close();
	});

	test("rejects an explicit header beyond the byte bound without reading the whole file", async () => {
		const { selector, root } = await createSelector();
		const path = join(root, "oversized.jsonl");
		const header = JSON.stringify({
			v: 4,
			kind: "header",
			id: "oversized",
			storageVersion: 1,
			createdAt: NOW,
			cwd: root,
			padding: "x".repeat(10_000),
		});
		await writeFile(path, `${header}\n`, "utf8");

		const boundedSelector = new NativeSessionSelector({
			sessionDir: join(root, "sessions"),
			cwd: root,
			fileSystem: selector.fileSystem,
			maxHeaderBytes: 128,
		});
		await expect(boundedSelector.resolve(path)).rejects.toMatchObject({
			code: "invalid_header",
		});
		await boundedSelector.close();
	});

	test("interactive cancellation returns no selection and does not create a session", async () => {
		const { selector, repo, root } = await createSelector();
		for (const id of ["first", "second"]) {
			const session = await repo.create({ id, cwd: root }, BACKGROUND_CONTEXT);
			await session.close(BACKGROUND_CONTEXT);
		}
		const before = await selector.discover();

		const selected = await selector.select({
			resume: true,
			interactive: true,
			readAnswer: async () => undefined,
		});

		expect(selected).toBeUndefined();
		expect(await selector.discover()).toEqual(before);
		await selector.close();
	});
});

test("chooseInteractiveResumeSession treats invalid input as cancellation", async () => {
	const session = {
		id: "session",
		createdAt: NOW,
		storageVersion: 1,
		cwd: "/workspace",
		path: "/workspace/session.jsonl",
		modifiedAt: NOW,
	};
	const selected = await chooseInteractiveResumeSession([session, { ...session, id: "other" }], {
		readAnswer: async () => "not a number",
		write: () => {},
	});
	expect(selected).toBeUndefined();
});
