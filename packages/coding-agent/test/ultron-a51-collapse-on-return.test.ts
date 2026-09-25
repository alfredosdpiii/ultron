/**
 * A51: after 20 completed tasks the root's next request is bounded, while `agents.result` still returns full
 * values. Each task returns a 4 KB value that the root cell prints; once the model has answered after a cell's
 * output, the host collapses it to one line per task with a context edit, so request size stops growing with the
 * number of finished tasks. The durable transcript and the task journal keep everything.
 */
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { describe, expect, test } from "vitest";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";
import { ScriptedProvider, scriptedModelsJson } from "./support/scripted-provider.ts";

const TASKS = 20;
const VALUE_BYTES = 4000;
const ROOT_PROMPT = "A51: run twenty tasks and keep going";

/** A child's 4 KB answer, distinct per task so a leak of any one of them is visible. */
function childValue(index: number): string {
	const tag = `RESULT_${index}_`;
	return tag.repeat(Math.ceil(VALUE_BYTES / tag.length)).slice(0, VALUE_BYTES);
}

describe("A51 collapse on return keeps the root's requests bounded", () => {
	test("20 completed tasks: bounded next request, full values through agents.result", async () => {
		const root = mkdtempSync(join(tmpdir(), "ultron-a51-"));
		const agentDir = join(root, "agent");
		const projectDir = join(root, "project");
		mkdirSync(agentDir, { recursive: true });
		mkdirSync(projectDir, { recursive: true });
		const provider = new ScriptedProvider((request) => {
			const child = /CHILD (\d+)/.exec(request.firstUser);
			if (child) return { text: childValue(Number(child[1])) };
			if (request.firstUser !== ROOT_PROMPT) return { text: "unexpected" };
			if (request.turn < TASKS) {
				return {
					tool: "rlm",
					args: {
						code: `r${request.turn} = await agents.invoke("rlm-child@1", {"prompt": "CHILD ${request.turn}"}, key="a51-${request.turn}")\nprint(r${request.turn})`,
					},
				};
			}
			if (request.turn === TASKS) {
				return {
					tool: "rlm",
					args: {
						code: [
							'tasks = [t for t in (await agents.tasks())["tasks"] if t["definition"] == "rlm-child@1"]',
							"values = [await agents.result(t['id']) for t in tasks]",
							"print('FULL', len(values), sum(len(v['value']) for v in values), sorted(v['value'][:12] for v in values)[:2])",
						].join("\n"),
					},
				};
			}
			return { text: "A51_DONE" };
		});
		await provider.start();
		writeFileSync(join(agentDir, "models.json"), scriptedModelsJson(provider.baseUrl));
		const client = new RpcClient({
			cliPath: resolve(__dirname, "../src/cli.ts"),
			cwd: projectDir,
			provider: "scripted",
			model: "scripted",
			args: ["--no-session"],
			env: {
				NODE_OPTIONS: `--import ${resolve(__dirname, "../src/experimental/source-resolver.ts")}`,
				ULTRON_CODING_AGENT_DIR: agentDir,
				ULTRON_SERVER_DIR: mkdtempSync(join("/tmp", "u-a51-")),
				ULTRON_HINDSIGHT_URL: "off",
				ULTRON_TOOL_ROUNDS_NUDGE: "0",
				PI_OFFLINE: "1",
			},
		});
		try {
			await client.start();
			await client.promptAndWait(ROOT_PROMPT, undefined, 240_000);
			expect(await client.getLastAssistantText()).toBe("A51_DONE");

			const rootRequests = provider.requests.filter((request) => request.firstUser === ROOT_PROMPT);
			expect(rootRequests).toHaveLength(TASKS + 2);
			const sizes = rootRequests.map((request) => Buffer.byteLength(request.raw));
			const base = sizes[0]!;
			// Without collapse every request would carry every earlier 4 KB value (the last one over 80 KB more).
			// With it a request carries the newest cell or two in full and one line per earlier task.
			const bound = base + 3 * (VALUE_BYTES + 600) + TASKS * 900;
			for (const size of sizes) expect(size).toBeLessThan(bound);
			expect(sizes.at(-1)! - base).toBeLessThan(TASKS * VALUE_BYTES * 0.5);

			const last = rootRequests.at(-1)!.raw;
			// Early results are gone from the model's view (a 200-character head stays); one-line summaries and
			// handles remain.
			for (const index of [0, 1, 5, 10]) expect(last).not.toContain(childValue(index).slice(0, 600));
			expect(last).toContain("collapsed on return");
			expect(last).toContain("rlm-child@1 key=a51-0 succeeded");
			expect(last).toMatch(/await agents\.result\(\\"[^"\\]+\\"\)/);
			// The journal still returns every full value.
			expect(last).toContain(`FULL ${TASKS} ${TASKS * VALUE_BYTES}`);
		} finally {
			await client.stop().catch(() => {});
			await provider.stop();
			rmSync(root, { recursive: true, force: true });
		}
	}, 300_000);
});
