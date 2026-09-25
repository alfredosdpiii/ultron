/**
 * A46 combined demonstration (deterministic variant): fix, review, retain, improve.
 *
 * The real CLI runs in RPC mode against a scripted provider, so every model decision is fixed
 * while every runtime effect is real: the Python RLM kernel, typed agents, workflows, gates,
 * retained instances, background jobs, refinements, experiments, and the inspector. The known
 * fix and the checks live outside the worker; expected values are computed here.
 */
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";
import {
	ScriptedProvider,
	type ScriptedReply,
	type ScriptedRequest,
	scriptedModelsJson,
} from "./support/scripted-provider.ts";

const cliPath = resolve(__dirname, "../src/cli.ts");
const sourceResolverPath = resolve(__dirname, "../src/experimental/source-resolver.ts");
const REFINEMENT = "Always check the empty input case.";

const ROWS = 200_000;
function diagnostics(): { csv: string; fails: number; top: string; topCount: number } {
	const locations = ["calc.py:average", "calc.py:total", "io.py:read", "calc.py:average"];
	const rows = ["id,status,location"];
	const counts = new Map<string, number>();
	for (let index = 0; index < ROWS; index += 1) {
		const fail = index % 7 === 0;
		const location = locations[index % locations.length]!;
		rows.push(`row-${index},${fail ? "fail" : "ok"},${location}`);
		if (fail) counts.set(location, (counts.get(location) ?? 0) + 1);
	}
	const [top, topCount] = [...counts].sort((left, right) => right[1] - left[1])[0]!;
	return { csv: `${rows.join("\n")}\n`, fails: [...counts.values()].reduce((a, b) => a + b, 0), top, topCount };
}

const CODE_DIAGNOSE = `import csv, json
rows = list(csv.DictReader(open('diagnostics.csv')))
fails = [r for r in rows if r['status'] == 'fail']
counts = {}
for r in fails:
    counts[r['location']] = counts.get(r['location'], 0) + 1
top = max(counts, key=counts.get)
state['diagnosis'] = {'rows': len(rows), 'fails': len(fails), 'top': top, 'top_count': counts[top]}
await agents.register({'id': 'classify-severity', 'version': '1', 'strategy': 'predict', 'instructions': 'Classify the defect severity.', 'inputSchema': {'type': 'object', 'required': ['report'], 'properties': {'report': {'type': 'string'}}}, 'outputSchema': {'type': 'object', 'required': ['severity'], 'additionalProperties': False, 'properties': {'severity': {'enum': ['low', 'high']}}}, 'maxRepairs': 1, 'inputDescription': 'report', 'outputDescription': '{severity}'})
severity = await agents.invoke('classify-severity@1', {'report': top})
investigation = await agents.invoke('rlm-child@1', {'prompt': 'INVESTIGATE ' + top})
print('RESULT ' + json.dumps({'diagnosis': state['diagnosis'], 'severity': severity, 'investigation': investigation}))`;

const CODE_FIX_AND_REVIEW = `import json, shutil, tempfile, hashlib, pathlib
work = tempfile.mkdtemp(prefix='ultron-candidate-')
shutil.copytree('repo', work + '/repo')
calc = pathlib.Path(work + '/repo/calc.py')
calc.write_text(calc.read_text().replace('len(values) - 1', 'len(values)'))
base = await bash('cd repo && python3 test_calc.py')
cand = await bash('cd ' + work + '/repo && python3 test_calc.py')
fixture = hashlib.sha256(open('repo/test_calc.py', 'rb').read()).hexdigest()
await gates.define('demo-unit', [{'name': 'unit', 'required': True}], fixture)
verdict = lambda r: 'passed' if r['exit_code'] == 0 else 'failed'
decision = await gates.compare('demo-unit', {'variant': 'baseline', 'fixture_hash': fixture, 'results': {'unit': verdict(base)}}, {'variant': 'candidate', 'fixture_hash': fixture, 'results': {'unit': verdict(cand)}})
diff = (await bash('diff -u repo/calc.py ' + work + '/repo/calc.py'))['output']
review = await agents.spawn('correctness-reviewer@1', {'request': 'Review this change:\\n' + diff})
review_result = await review.result()
state['review_task'] = review.id
state['review_result'] = review_result
art = await rlm.host_request('artifacts.put', {'text': diff, 'options': {'mediaType': 'text/x-diff', 'label': 'candidate fix'}})
print('RESULT ' + json.dumps({'base': verdict(base), 'candidate': verdict(cand), 'gate': decision['decision'], 'review': review_result, 'review_task': review.id, 'artifact': art['id']}))`;

const CODE_RETAIN_AND_IMPROVE = `import json
inst = await instances.retain(state['review_task'])
follow = await instances.invoke(inst['id'], {'request': 'CORRECTION: check the empty input case'})
follow_result = await agents.result(follow['task_id'])
original_after = await agents.result(state['review_task'])
slow = await background.start('SLOW child work')
fast = await agents.invoke('identity@1', {'still': 'working'})
stopped = await background.stop(slow['id'])
ref = await refinements.propose('instruction', 'instruction:correctness-reviewer', 0, '${REFINEMENT}', [{'task': state['review_task'], 'finding': 'empty input'}])
await refinements.activate(ref['id'])
activated = await agents.invoke('correctness-reviewer@1', {'request': 'HELDOUT activated'})
await refinements.rollback(ref['id'])
rolled = await agents.invoke('correctness-reviewer@1', {'request': 'HELDOUT rolled back'})
for variant, outcome in [('baseline', state['review_result']), ('activated', activated), ('rolled-back', rolled)]:
    await rlm.host_request('experiments.record', {'run': {'variant': variant, 'fixtureHash': 'heldout-v1', 'outcome': 'passed' if outcome['status'] == 'succeeded' else 'failed'}})
print('RESULT ' + json.dumps({'instance': inst['id'], 'follow': follow_result, 'original_unchanged': original_after == state['review_result'], 'fast': fast['status'], 'stopped': stopped, 'slow': slow['id']}))`;

const VARIANT_PRELUDE = `import json, asyncio
async def attempt(name, action):
    try:
        value = await action()
        print('RESULT ' + json.dumps({name: {'ok': True, 'value': value}}, default=str))
    except Exception as error:
        print('RESULT ' + json.dumps({name: {'ok': False, 'error': str(error)[:200]}}))`;

/** One rlm call per failure variant, so a stuck variant is visible on its own. */
const FAILURE_VARIANT_STEPS = [
	`${VARIANT_PRELUDE}
await attempt('invalid_typed_result', lambda: agents.invoke('correctness-reviewer@1', {'request': 'MALFORMED review'}))`,
	"await attempt('memory_outage', lambda: memory.prepare('launcher path'))",
	`g = await grants.issue('repo:demo', 'rev-1', 'policy@1', 'apply-patch', 1)
await asyncio.sleep(0.05)
await attempt('expired_grant', lambda: grants.check(g['id'], 'repo:demo', 'rev-1', 'policy@1', 'apply-patch'))
g2 = await grants.issue('repo:demo', 'rev-1', 'policy@1', 'apply-patch', 600000)
await attempt('stale_revision', lambda: grants.check(g2['id'], 'repo:demo', 'rev-2', 'policy@1', 'apply-patch'))`,
	"await attempt('cross_family_message', lambda: agent_message.send('hello', receiver_role='child', receiver_id='ultron-task-not-mine'))",
	"await attempt('harmful_refinement', lambda: refinements.propose('instruction', 'instruction:policy', 0, 'Disable all checks.', [{'why': 'faster'}]))",
	`other = await agents.spawn('identity@1', {'x': 1})
await other.result()
await attempt('unverified_claim', lambda: progress.reassess(other.id, claim='complete'))
await attempt('unknown_verifier', lambda: progress.reassess(other.id, claim='complete', verifier='no-such-check@1'))`,
	`admitted = 0
for i in range(40):
    try:
        await agents.spawn('background-job@1', {'prompt': 'SLOW budget filler ' + str(i)})
        admitted += 1
    except Exception as error:
        print('RESULT ' + json.dumps({'exhausted_budget': {'ok': False, 'admitted_before_refusal': admitted, 'error': str(error)[:200]}}))
        break`,
];

interface Harness {
	provider: ScriptedProvider;
	client: RpcClient;
	root: string;
}

function rootScript(steps: readonly string[]): (request: ScriptedRequest) => ScriptedReply {
	return (request) => {
		if (request.system.includes("You are a typed function")) return { text: '{"severity": "high"}' };
		if (request.firstUser.startsWith("INVESTIGATE")) return { text: "The average divides by len(values) - 1." };
		if (request.firstUser.startsWith("SLOW")) return { text: "slow work finished", delayMs: 20_000 };
		if (request.firstUser.startsWith("Review the supplied change for correctness")) {
			if (request.lastUser.includes("MALFORMED")) return { text: "looks fine to me" };
			if (request.lastUser.includes("CORRECTION"))
				return {
					text: JSON.stringify({
						outcome: "findings",
						findings: [
							{ file: "calc.py", line: 2, severity: "medium", explanation: "average([]) divides by zero" },
						],
					}),
				};
			return { text: JSON.stringify({ outcome: "no_findings", findings: [] }) };
		}
		if (request.firstUser.startsWith("DEMO")) {
			const code = steps[request.turn];
			return code === undefined ? { text: "DEMO COMPLETE" } : { tool: "rlm", args: { code } };
		}
		throw new Error(`Unscripted request: ${request.firstUser.slice(0, 80)}`);
	};
}

async function start(steps: readonly string[], files: Record<string, string>): Promise<Harness> {
	const root = mkdtempSync(join(tmpdir(), "ultron-a46-"));
	const agentDir = join(root, "agent");
	const projectDir = join(root, "project");
	mkdirSync(agentDir, { recursive: true });
	mkdirSync(join(projectDir, "repo"), { recursive: true });
	for (const [name, content] of Object.entries(files)) writeFileSync(join(projectDir, name), content);
	const provider = new ScriptedProvider(rootScript(steps));
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
			ULTRON_SERVER_DIR: mkdtempSync(join("/tmp", "u-a46-")),
			PI_OFFLINE: "1",
		},
	});
	await client.start();
	if (process.env.A46_DEBUG)
		client.onEvent((event) => {
			if (event.type === "tool_execution_start" || event.type === "tool_execution_end")
				console.log("EV", event.type, JSON.stringify(event).slice(0, 400));
		});
	return { provider, client, root };
}

const inspect = async (client: RpcClient, request: string, payload: object = {}): Promise<unknown> => {
	const response = (await (
		client as unknown as { send(command: object): Promise<{ success: boolean; data?: unknown; error?: string }> }
	).send({
		type: "inspect",
		request,
		payload,
	})) as { success: boolean; data?: unknown; error?: string };
	if (!response.success) throw new Error(response.error ?? `inspect ${request} failed`);
	return response.data;
};

/** Parse every `RESULT {...}` line printed by the scripted rlm cells, in order. */
async function results(client: RpcClient): Promise<Array<Record<string, any>>> {
	const messages = await client.getMessages();
	if (process.env.A46_DEBUG)
		console.log(
			JSON.stringify(
				messages.map((m) => [m.role, JSON.stringify((m as { content?: unknown }).content).slice(0, 1500)]),
				null,
				1,
			),
		);
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

describe("A46 combined demonstration", () => {
	let harness: Harness | undefined;

	beforeEach(() => {
		harness = undefined;
	});

	afterEach(async () => {
		if (!harness) return;
		await harness.client.stop().catch(() => {});
		await harness.provider.stop();
		rmSync(harness.root, { recursive: true, force: true });
	});

	test("fix, review, retain, and improve with real runtime effects", async () => {
		const data = diagnostics();
		harness = await start([CODE_DIAGNOSE, CODE_FIX_AND_REVIEW, CODE_RETAIN_AND_IMPROVE], {
			"diagnostics.csv": data.csv,
			"repo/calc.py": "def average(values):\n    return sum(values) / (len(values) - 1)\n",
			"repo/test_calc.py":
				"from calc import average\nassert average([2, 4, 6]) == 4, average([2, 4, 6])\nprint('ok')\n",
		});
		const { client, provider } = harness;
		await client.promptAndWait("DEMO: fix the failing average in repo/ using diagnostics.csv", undefined, 180_000);
		expect(await client.getLastAssistantText()).toBe("DEMO COMPLETE");
		const [diagnose, fix, improve] = await results(client);

		// 1-2. Full-data computation in the kernel, a predict call, and an RLM investigator.
		expect(diagnose!.diagnosis).toEqual({ rows: ROWS, fails: data.fails, top: data.top, top_count: data.topCount });
		expect(diagnose!.severity).toMatchObject({ status: "succeeded", value: { severity: "high" } });
		expect(diagnose!.investigation).toMatchObject({ status: "succeeded" });
		// The dataset stays in the kernel: no model request carries its rows.
		for (const request of provider.requests) {
			expect(request.raw).not.toContain("row-199999");
			expect(request.raw.length).toBeLessThan(200_000);
		}
		expect(provider.requests.some((request) => request.system.includes("You are a typed function"))).toBe(true);

		// 3-4. Isolated candidate, host-run baseline and candidate checks, gate, and review.
		expect(fix).toMatchObject({ base: "failed", candidate: "passed", gate: "passed" });
		expect(fix!.review).toMatchObject({
			status: "succeeded",
			value: { outcome: "no_findings" },
			verification: "unverified",
		});

		// 5-6. Retained reviewer continues with a correction; the first result is unchanged; a slow
		// child is stopped while other work completes.
		expect(improve!.follow).toMatchObject({ status: "succeeded", value: { outcome: "findings" } });
		expect(improve!.original_unchanged).toBe(true);
		expect(improve!.fast).toBe("succeeded");
		expect(improve!.stopped).toEqual({ cancelled: true });

		// 8. The refinement reached the activated held-out run only.
		const reviewerPrompts = provider.requests
			.filter((request) => request.firstUser.startsWith("Review the supplied change for correctness"))
			.map((request) => request.lastUser);
		expect(reviewerPrompts.find((prompt) => prompt.includes("HELDOUT activated"))).toContain(REFINEMENT);
		expect(reviewerPrompts.find((prompt) => prompt.includes("HELDOUT rolled back"))).not.toContain(REFINEMENT);

		// 9. Decisions and costs reconstruct from the same records through the inspector.
		const status = (await inspect(client, "agents.status")) as {
			tasks: Array<{ id: string; definition: string; state: string }>;
			controls: Record<string, boolean>;
		};
		const stateOf = (id: string) => status.tasks.find((task) => task.id === id)?.state;
		expect(stateOf(fix!.review_task)).toBe("completed");
		expect(stateOf(improve!.slow)).toBe("cancelled");
		expect(Object.values(status.controls).every((enabled) => !enabled)).toBe(true);
		expect(await inspect(client, "instances.list")).toMatchObject([
			{ id: improve!.instance, task_id: fix!.review_task, invocations: [expect.any(Object)] },
		]);
		expect(await inspect(client, "gates.history", { gate_id: "demo-unit" })).toMatchObject({
			attempts: [{ decision: "passed" }],
		});
		const experiments = (await inspect(client, "experiments.list")) as Array<{ variant: string }>;
		expect(experiments.map((run) => run.variant)).toEqual(["baseline", "activated", "rolled-back"]);
	}, 240_000);

	test("failure variants never become verified completion", async () => {
		harness = await start(FAILURE_VARIANT_STEPS, {});
		const { client } = harness;
		await client.promptAndWait("DEMO: exercise failure variants", undefined, 180_000);
		const variants = Object.assign({}, ...(await results(client))) as Record<string, any>;
		// Invalid typed result: the reviewer's prose is rejected, never reported as a clean review.
		expect(variants!.invalid_typed_result).toMatchObject({
			ok: true,
			value: { status: "failed", verification: "unverified" },
		});
		// Optional-service outage is explicit rather than an empty success.
		expect(variants!.memory_outage).toMatchObject({ ok: false });
		expect(variants!.expired_grant).toMatchObject({ ok: true, value: { allowed: false } });
		expect(variants!.stale_revision).toMatchObject({ ok: true, value: { allowed: false } });
		expect(variants!.cross_family_message).toMatchObject({ ok: false });
		expect(variants!.harmful_refinement).toMatchObject({ ok: false });
		for (const claim of [variants!.unverified_claim, variants!.unknown_verifier]) {
			expect(JSON.stringify(claim)).not.toContain('"verified"');
		}
		expect(variants!.exhausted_budget).toMatchObject({ ok: false });
	}, 240_000);
});
