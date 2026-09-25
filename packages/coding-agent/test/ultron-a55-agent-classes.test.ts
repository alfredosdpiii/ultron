/**
 * A55: an agent class defined in an rlm cell is invokable typed end to end, its state persists across
 * cells, and a bad return is rejected with the schema error (A14: never a success record).
 *
 * The real CLI runs in RPC mode against a scripted provider: the root model's cells and the class
 * method's model answers are fixed, while the kernel, host, registry, journal, and child lanes are real.
 * Snapshot persistence and generations are covered at kernel level in ultron-agent-classes.test.ts.
 */
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterEach, describe, expect, test } from "vitest";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";
import { ScriptedProvider, type ScriptedRequest, scriptedModelsJson } from "./support/scripted-provider.ts";
import { tempServerDir } from "./support/server-dir.ts";

const cliPath = resolve(__dirname, "../src/cli.ts");
const sourceResolverPath = resolve(__dirname, "../src/experimental/source-resolver.ts");

const DEFINE_AND_CALL = `import json
from dataclasses import dataclass
from typing import Literal

@dataclass
class Label:
    kind: Literal["bug", "feature"]
    confidence: float

@agent
class Triage:
    """TRIAGE ROLE: you label incoming reports."""
    seen: int = 0
    labels: list[str] = []

    async def classify(self, report: str) -> Label:
        """Label the report as a bug or a feature."""
        ...

    def record(self, label: Label) -> int:
        self.seen += 1
        self.labels.append(label.kind)
        return self.seen

t = Triage("main")
label = await t.classify("the app crashes on start")
t.record(label)
print('RESULT ' + json.dumps({'type': type(label).__name__, 'kind': label.kind, 'confidence': label.confidence, 'seen': t.seen, 'generation': Triage.describe()['generation']}))`;

const NEXT_CELL = `import json
t = Triage("main")
label = await t.classify("please add dark mode")
t.record(label)
print('RESULT ' + json.dumps({'kind': label.kind, 'seen': t.seen, 'labels': t.labels}))`;

const BAD_RETURN = `import json
try:
    await Triage("main").classify("BAD report")
    outcome = {'raised': False}
except AgentCallError as error:
    outcome = {'raised': True, 'message': str(error), 'status': error.result['status'], 'verification': error.result['verification']}
tasks = [{'definition': task['definition'], 'state': task['state'], 'result': task.get('result')} for task in (await agents.tasks())['tasks']]
print('RESULT ' + json.dumps({'outcome': outcome, 'seen': Triage("main").seen, 'tasks': tasks}))`;

const STEPS = [DEFINE_AND_CALL, NEXT_CELL, BAD_RETURN];

function script(request: ScriptedRequest) {
	if (request.firstUser.startsWith("TRIAGE ROLE")) {
		const input = /\nInput data:\n(.*)\n\nOutput contract:\n/.exec(request.firstUser)?.[1] ?? "{}";
		const report = String((JSON.parse(input) as { args: { report: string } }).args.report);
		if (report.includes("BAD")) return { text: JSON.stringify({ kind: "urgent", confidence: 0.4 }) };
		return { text: JSON.stringify({ kind: report.includes("crash") ? "bug" : "feature", confidence: 0.9 }) };
	}
	if (request.firstUser.startsWith("A55")) {
		const code = STEPS[request.turn];
		return code === undefined ? { text: "A55 COMPLETE" } : { tool: "rlm", args: { code } };
	}
	throw new Error(`Unscripted request: ${request.firstUser.slice(0, 80)}`);
}

async function results(client: RpcClient): Promise<Array<Record<string, any>>> {
	const messages = await client.getMessages();
	return messages
		.filter((message) => message.role === "toolResult")
		.flatMap((message) =>
			(message.content as Array<{ type: string; text?: string }>)
				.filter((part) => part.type === "text")
				.flatMap((part) => (part.text ?? "").split("\n"))
				.filter((line) => line.startsWith("RESULT "))
				.map((line) => JSON.parse(line.slice("RESULT ".length)) as Record<string, any>),
		);
}

describe("A55 agents as Python classes", () => {
	let cleanup: (() => Promise<void>) | undefined;

	afterEach(async () => {
		await cleanup?.();
		cleanup = undefined;
	});

	test("a class defined in a cell is invokable typed end to end, keeps state across cells, and rejects a bad return with the schema error", async () => {
		const root = mkdtempSync(join(tmpdir(), "ultron-a55-"));
		const agentDir = join(root, "agent");
		const projectDir = join(root, "project");
		mkdirSync(agentDir, { recursive: true });
		mkdirSync(projectDir, { recursive: true });
		const provider = new ScriptedProvider(script);
		await provider.start();
		writeFileSync(join(agentDir, "models.json"), scriptedModelsJson(provider.baseUrl));
		const client = new RpcClient({
			cliPath,
			cwd: projectDir,
			provider: "scripted",
			model: "scripted",
			args: ["--no-session"],
			env: {
				NODE_OPTIONS: `--import ${sourceResolverPath}`,
				ULTRON_CODING_AGENT_DIR: agentDir,
				ULTRON_SERVER_DIR: tempServerDir("u-a55-"),
				ULTRON_HINDSIGHT_URL: "off",
				ULTRON_RLM_SPILL_DIR: join(root, "spill"),
				PI_OFFLINE: "1",
			},
		});
		cleanup = async () => {
			await client.stop().catch(() => {});
			await provider.stop();
			rmSync(root, { recursive: true, force: true });
		};
		await client.start();
		await client.promptAndWait("A55: triage two reports with an agent class", undefined, 120_000);
		expect(await client.getLastAssistantText()).toBe("A55 COMPLETE");
		const [defined, next, bad] = await results(client);

		// Typed end to end: the method returned a dataclass built from the validated value.
		expect(defined).toEqual({ type: "Label", kind: "bug", confidence: 0.9, seen: 1, generation: 1 });
		// State from the first cell is live in the next one, and was shown to the model as data.
		expect(next).toEqual({ kind: "feature", seen: 2, labels: ["bug", "feature"] });
		const methodRequests = provider.requests.filter((request) => request.firstUser.startsWith("TRIAGE ROLE"));
		expect(methodRequests.length).toBeGreaterThanOrEqual(3);
		expect(methodRequests.map((request) => request.firstUser)).toContainEqual(
			expect.stringContaining('"args":{"report":"please add dark mode"},"state":{"seen":1,"labels":["bug"]}'),
		);
		// The root model is told about agent classes in the rlm tool description.
		expect(provider.requests.find((request) => request.firstUser.startsWith("A55"))!.raw).toContain(
			"Agents as classes",
		);

		// A bad return raises with the schema error; state is unchanged; the journal records a failure.
		expect(bad!.outcome).toEqual({
			raised: true,
			message:
				'Triage.classify (triage--classify@1) failed: triage--classify@1 output does not match its schema: $.kind must be equal to one of the allowed values ("bug", "feature")',
			status: "failed",
			verification: "unverified",
		});
		expect(bad!.seen).toBe(2);
		const tasks = bad!.tasks as Array<{ definition: string; state: string; result: Record<string, unknown> }>;
		expect(tasks.map((task) => [task.definition, task.state])).toEqual([
			["triage--classify@1", "completed"],
			["triage--classify@1", "completed"],
			["triage--classify@1", "failed"],
		]);
		for (const task of tasks) expect(task.result.verification).toBe("unverified");
		expect(tasks[2]!.result.value).toBeUndefined();
	}, 180_000);
});
