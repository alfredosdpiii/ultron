/**
 * A46 live demonstration: a real model runs fix, review, retain, and improve through the typed RLM API.
 *
 * Metered and opt-in (ULTRON_LIVE_EVAL=1, model from ULTRON_LIVE_EVAL_MODEL, default cliproxyapi/gpt-6-sol).
 * The model chooses its own decomposition; this test only checks the recorded outcome against facts
 * computed here, and reattaches a second client to read everything back from the session's records.
 */
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { describe, expect, test } from "vitest";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";

const LIVE = process.env.ULTRON_LIVE_EVAL === "1";
const MODEL = process.env.ULTRON_LIVE_EVAL_MODEL ?? "cliproxyapi/gpt-6-sol";
const PROFILE = process.env.ULTRON_LIVE_EVAL_PROFILE ?? join(homedir(), ".ultron", "agent");
const RESULTS_DIR = resolve(__dirname, "../../../acceptance/demonstrations");
const cliPath = resolve(__dirname, "../src/cli.ts");
const sourceResolverPath = resolve(__dirname, "../src/experimental/source-resolver.ts");
const SESSION_ID = "a46-live-demo";
const BUGGY = "def average(values):\n    return sum(values) / (len(values) - 1)\n";

function diagnostics(): { csv: string; top: string } {
	const locations = ["calc.py:average", "calc.py:total", "io.py:read", "calc.py:average"];
	const rows = ["id,status,location"];
	for (let index = 0; index < 200_000; index += 1)
		rows.push(`row-${index},${index % 7 === 0 ? "fail" : "ok"},${locations[index % locations.length]}`);
	return { csv: `${rows.join("\n")}\n`, top: "calc.py:average" };
}

const TASK = `This project is disposable. repo/test_calc.py fails (run it with \`python3 test_calc.py\` inside repo/). diagnostics.csv has 200,000 log rows: never read it into your own context; analyze it only inside the rlm tool.

Do all of the following through the rlm tool's Python API:
1. Over the full diagnostics.csv, find the location with the most failing rows.
2. Fix the bug in an isolated copy of repo/ (e.g. under tempfile.mkdtemp()); do not modify repo/ itself. Run the tests on the original and on the copy with \`await bash(...)\`, then \`await gates.define('live-unit', [{'name': 'unit', 'required': True}], <sha256 of repo/test_calc.py>)\` and \`await gates.compare(...)\` with the baseline and candidate results.
3. Start an independent review of your diff with \`review = await agents.spawn('correctness-reviewer@1', {'request': <diff>})\` and wait for \`await review.result()\`.
4. Retain that reviewer with \`await instances.retain(review.id)\` and ask it one follow-up with \`await instances.invoke(<instance id>, {'request': 'Does the fix handle an empty list?'})\`, then wait for that task's result.
5. Store the diff with \`await rlm.host_request('artifacts.put', {'text': <diff>})\`.
6. Propose and activate an instruction refinement for target 'instruction:correctness-reviewer' with evidence from this run, run one more review of the same diff, then roll the refinement back. Record each of the two reviews with \`await rlm.host_request('experiments.record', {'run': {'variant': ..., 'fixtureHash': 'live-v1', 'outcome': 'passed' or 'failed'}})\`.

Finish by replying with only a JSON object: {"top_location": ..., "gate_decision": ..., "review_task_id": ..., "instance_id": ...}.`;

describe.skipIf(!LIVE)("A46 live demonstration", () => {
	test(
		`${MODEL} fixes, reviews, retains, and improves through the typed RLM API`,
		async () => {
			const root = mkdtempSync(join(tmpdir(), "ultron-a46-live-"));
			const agentDir = join(root, "agent");
			const projectDir = join(root, "project");
			mkdirSync(agentDir, { recursive: true });
			mkdirSync(join(projectDir, "repo"), { recursive: true });
			for (const file of ["models.json", "auth.json"])
				if (existsSync(join(PROFILE, file))) copyFileSync(join(PROFILE, file), join(agentDir, file));
			const data = diagnostics();
			writeFileSync(join(projectDir, "diagnostics.csv"), data.csv);
			writeFileSync(join(projectDir, "repo", "calc.py"), BUGGY);
			writeFileSync(
				join(projectDir, "repo", "test_calc.py"),
				"from calc import average\nassert average([2, 4, 6]) == 4, average([2, 4, 6])\nprint('ok')\n",
			);
			const split = MODEL.indexOf("/");
			const serverDir = mkdtempSync(join("/tmp", "u-a46l-"));
			const client = (args: string[]) =>
				new RpcClient({
					cliPath,
					cwd: projectDir,
					provider: MODEL.slice(0, split),
					model: MODEL.slice(split + 1),
					args,
					env: {
						NODE_OPTIONS: `--import ${sourceResolverPath}`,
						ULTRON_CODING_AGENT_DIR: agentDir,
						ULTRON_HINDSIGHT_URL: "off",
						ULTRON_SERVER_DIR: serverDir,
					},
				});
			const inspect = async (rpc: RpcClient, request: string, payload: object = {}) => {
				const response = (await (
					rpc as unknown as {
						send(command: object): Promise<{ success: boolean; data?: unknown; error?: string }>;
					}
				).send({
					type: "inspect",
					request,
					payload,
				})) as { success: boolean; data?: unknown; error?: string };
				if (!response.success) throw new Error(response.error);
				return response.data as any;
			};
			const first = client(["--session-id", SESSION_ID]);
			const record: Record<string, unknown> = { model: MODEL, recordedAt: new Date().toISOString() };
			try {
				await first.start();
				// RpcClient does not notice its process dying; record it so a crash is not mistaken for a slow model.
				(first as unknown as { process?: import("node:child_process").ChildProcess }).process?.on(
					"exit",
					(code, signal) => {
						record.cliExit = { code, signal, at: new Date().toISOString() };
					},
				);
				const started = Date.now();
				await first.promptAndWait(TASK, undefined, 40 * 60 * 1000);
				record.durationMs = Date.now() - started;
				const answerText = (await first.getLastAssistantText()) ?? "";
				record.answer = answerText.slice(0, 2000);
				const answer = JSON.parse(answerText.match(/\{[\s\S]*\}/)?.[0] ?? "{}") as Record<string, string>;
				const messages = await first.getMessages();
				record.transcriptChars = JSON.stringify(messages).length;
				record.toolOutputs = messages
					.filter((message) => message.role === "toolResult")
					.map((message) => JSON.stringify((message as { content: unknown }).content).slice(0, 1500));
				await first.stop();

				// Reattach from a new client process and reconstruct everything from records.
				const second = client(["--session-id", SESSION_ID]);
				await second.start();
				try {
					const status = await inspect(second, "agents.status");
					const gates = await inspect(second, "gates.history", { gate_id: "live-unit" });
					const instances = await inspect(second, "instances.list");
					const experiments = await inspect(second, "experiments.list");
					const refinements = (await inspect(second, "refinements.list")) as Array<{
						target: string;
						state: string;
					}>;
					record.tasks = status.tasks.map(
						(task: { definition: string; state: string; result?: { error?: string } }) =>
							`${task.definition}:${task.state}${task.result?.error ? ` (${task.result.error.slice(0, 300)})` : ""}`,
					);
					record.gates = await inspect(second, "gates.list");
					record.gate = gates.attempts.map((attempt: { decision: string }) => attempt.decision);
					record.instances = instances.length;
					record.experiments = experiments.length;
					record.refinements = refinements.map((item) => `${item.target}:${item.state}`);

					expect(answer.top_location).toBe(data.top);
					expect(gates.attempts.at(-1)).toMatchObject({ decision: "passed" });
					expect(
						status.tasks.some(
							(task: { id: string; definition: string; state: string }) =>
								task.id === answer.review_task_id &&
								task.definition === "correctness-reviewer@1" &&
								task.state === "completed",
						),
					).toBe(true);
					expect(instances).toEqual(
						expect.arrayContaining([
							expect.objectContaining({ task_id: answer.review_task_id, invocations: [expect.any(Object)] }),
						]),
					);
					expect(experiments.length).toBeGreaterThanOrEqual(2);
					expect(
						refinements.some(
							(item) => item.target === "instruction:correctness-reviewer" && item.state === "rolled_back",
						),
					).toBe(true);
					// The fix happened in an isolated copy; the original repository is untouched.
					expect(readFileSync(join(projectDir, "repo", "calc.py"), "utf8")).toBe(BUGGY);
					// The dataset never entered the conversation.
					expect(JSON.stringify(messages)).not.toContain("row-199999");
					expect(record.transcriptChars as number).toBeLessThan(data.csv.length / 10);
					record.passed = true;
				} finally {
					await second.stop();
				}
			} catch (error) {
				record.passed = false;
				record.error = error instanceof Error ? error.message : String(error);
				throw error;
			} finally {
				mkdirSync(RESULTS_DIR, { recursive: true });
				writeFileSync(
					join(RESULTS_DIR, `a46-live-${MODEL.replace(/[^a-z0-9.-]+/gi, "_")}.json`),
					`${JSON.stringify(record, null, 2)}\n`,
				);
				await first.stop().catch(() => {});
				rmSync(root, { recursive: true, force: true });
			}
		},
		60 * 60 * 1000,
	);
});
