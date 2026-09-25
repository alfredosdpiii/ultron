import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { NodeExecutionEnv } from "@ultron/agent-core/node";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { describe, expect, test } from "vitest";
import { createUltronRlmTool } from "../src/experimental/session-worker.ts";

describe("A43 kernel pool in the rlm tool", () => {
	test("a second lane evicts the idle first one after a snapshot, and the first lane's state comes back", async () => {
		const dir = mkdtempSync(join(tmpdir(), "ultron-a43-pool-"));
		let lane = "lane-a";
		const tool = createUltronRlmTool(
			dir,
			async () => {
				throw new Error("No host requests expected");
			},
			async () => lane,
			{ snapshotDir: join(dir, "snapshots"), maxLive: 1 },
		);
		const env = new NodeExecutionEnv({ cwd: dir });
		let call = 0;
		const run = async (code: string) => {
			call += 1;
			const invocation = {
				invocationId: `pool-${call}`,
				operationId: `pool-op-${call}`,
				turnId: "pool-turn",
				getMemo: async () => undefined,
				setMemo: async () => undefined,
			};
			const result = (await tool.execute(
				`pool-${call}`,
				{ code },
				() => {},
				{ env },
				invocation,
				BACKGROUND_CONTEXT,
			)) as {
				content: Array<{ text: string }>;
			};
			return result.content.map((part) => part.text).join("");
		};
		try {
			expect(await run("state['answer'] = 42\nimport os\nos.getpid()")).toMatch(/\d+/);
			const firstPid = await run("os.getpid()");
			lane = "lane-b";
			// Only one kernel may live: lane A is snapshotted and evicted to make room.
			expect(await run("'answer' in state")).toContain("False");
			lane = "lane-a";
			// A new process restores lane A's declared state.
			expect(await run("import os\n(state['answer'], os.getpid())")).toContain("42");
			expect(await run("os.getpid()")).not.toBe(firstPid);
		} finally {
			await tool.close();
			rmSync(dir, { recursive: true, force: true });
		}
	});
});
