import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it, vi } from "vitest";
import { BACKGROUND_CONTEXT } from "../../src/harness/context.ts";
import { NodeExecutionEnv } from "../../src/harness/env/nodejs.ts";
import { getOrThrow } from "../../src/harness/types.ts";
import { createTempDir } from "./session-test-utils.ts";

// Another process removes `*.vanishing` entries between readdir and lstat, as a Session worker releases its
// `<session>.jsonl.lock` while the server lists the sessions directory.
vi.mock("node:fs/promises", async (importOriginal) => {
	const actual = await importOriginal<typeof import("node:fs/promises")>();
	return {
		...actual,
		lstat: ((path: Parameters<typeof actual.lstat>[0], ...rest: unknown[]) => {
			if (String(path).endsWith(".vanishing")) rmSync(String(path), { recursive: true, force: true });
			return (actual.lstat as (...args: unknown[]) => unknown)(path, ...rest);
		}) as typeof actual.lstat,
	};
});

describe("NodeExecutionEnv.listDir under concurrent removal", () => {
	it("omits an entry removed after readdir returned it instead of failing the listing", async () => {
		const root = createTempDir();
		writeFileSync(join(root, "session.jsonl"), "{}\n");
		mkdirSync(join(root, "session.jsonl.vanishing"));
		writeFileSync(join(root, "other.vanishing"), "");
		const env = new NodeExecutionEnv({ cwd: root });

		const entries = getOrThrow(await env.listDir(".", BACKGROUND_CONTEXT));

		expect(entries.map((entry) => entry.name)).toEqual(["session.jsonl"]);
	});
});
