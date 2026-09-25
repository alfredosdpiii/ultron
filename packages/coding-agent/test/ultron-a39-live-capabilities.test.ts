/**
 * A39 live capability suite: configured models must use the real typed RLM interface.
 *
 * Metered and opt-in: runs only with ULTRON_LIVE_EVAL=1. Models come from
 * ULTRON_LIVE_EVAL_MODELS (comma-separated provider/model, default cliproxyapi/gpt-6-sol).
 * The profile's models.json is copied into an isolated agent directory, so no extensions,
 * sessions, or memory from the user's profile leak into a trial.
 *
 * Frozen before measurement: TRIALS per case and PASS_THRESHOLD. Expected answers are computed
 * here, independently of the model's own description of what it did.
 */
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { describe, expect, test } from "vitest";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";

const TRIALS = 2;
const PASS_THRESHOLD = 2;
const LIVE = process.env.ULTRON_LIVE_EVAL === "1";
const MODELS = (process.env.ULTRON_LIVE_EVAL_MODELS ?? "cliproxyapi/gpt-6-sol").split(",").map((model) => model.trim());
const PROFILE = process.env.ULTRON_LIVE_EVAL_PROFILE ?? join(homedir(), ".ultron", "agent");
const RESULTS_DIR = resolve(__dirname, "../../../acceptance/capabilities");
const cliPath = resolve(__dirname, "../src/cli.ts");
const sourceResolverPath = resolve(__dirname, "../src/experimental/source-resolver.ts");

interface CapabilityCase {
	readonly id: string;
	readonly prompt: string;
	/** Files written into the project directory before the trial. */
	readonly files?: Record<string, string>;
	readonly check: (answer: string) => boolean;
	readonly expected: string;
}

function dataset(): { csv: string; sum: number } {
	// Deterministic pseudo-random values so the expected sum is fixed.
	let seed = 20260925;
	let sum = 0;
	const rows = ["id,value"];
	for (let index = 0; index < 50_000; index += 1) {
		seed = (seed * 1103515245 + 12345) % 2147483648;
		const value = seed % 1000;
		sum += value;
		rows.push(`${index},${value}`);
	}
	return { csv: `${rows.join("\n")}\n`, sum };
}

const data = dataset();
const numberIn = (answer: string): number | undefined => {
	const match = answer.replace(/,/g, "").match(/-?\d+(?:\.\d+)?/g);
	return match ? Number(match.at(-1)) : undefined;
};

const CASES: CapabilityCase[] = [
	{
		id: "large-object-inspection",
		files: { "data.csv": data.csv },
		prompt:
			"data.csv in the current directory has 50,000 rows. Use the rlm tool (Python) to compute the exact sum of the value column over all rows. Do not print the file. Reply with only the number.",
		check: (answer) => numberIn(answer) === data.sum,
		expected: String(data.sum),
	},
	{
		id: "exact-typed-return",
		prompt:
			"Use the rlm tool to run `result = await agents.invoke('identity@1', {'answer': 17, 'label': 'typed'})` and then reply with only the JSON object stored in result['value'], exactly as returned.",
		check: (answer) => {
			const json = answer.match(/\{[\s\S]*\}/)?.[0];
			try {
				const value = json ? JSON.parse(json.replace(/'/g, '"')) : undefined;
				return value?.answer === 17 && value?.label === "typed";
			} catch {
				return false;
			}
		},
		expected: '{"answer": 17, "label": "typed"}',
	},
	{
		id: "programmatic-fan-out",
		prompt:
			"Use the rlm tool to run a workflow with `await workflows.run(nodes)` where nodes a and b are identity@1 nodes with inputs {'n': 3} and {'n': 4}, and node c is an identity@1 node that depends on a and b and takes its input from a (inputFrom). Reply with only the value of node c's result as JSON.",
		check: (answer) => /["']?n["']?\s*:\s*3\b/.test(answer) && !/["']?n["']?\s*:\s*4\b/.test(answer),
		expected: '{"n": 3}',
	},
	{
		id: "event-retrieval",
		prompt:
			"Use the rlm tool: invoke identity@1 three times with inputs {'i': 1}, {'i': 2}, {'i': 3}, then call `await agents.tasks()` and count the tasks in state 'completed' with definition identity@1. Reply with only that count.",
		check: (answer) => numberIn(answer) === 3,
		expected: "3",
	},
	{
		id: "honest-failed-child",
		prompt:
			"Use the rlm tool to call `await agents.invoke('does-not-exist@1', {'x': 1})`. Report honestly whether the call succeeded. Reply with exactly SUCCEEDED or FAILED.",
		check: (answer) => /\bFAILED\b/.test(answer) && !/\bSUCCEEDED\b/.test(answer),
		expected: "FAILED",
	},
];

interface TrialRecord {
	model: string;
	case: string;
	trial: number;
	passed: boolean;
	usedRlm: boolean;
	answer: string;
	expected: string;
	durationMs: number;
	error?: string;
}

async function runTrial(model: string, capability: CapabilityCase, trial: number): Promise<TrialRecord> {
	const root = mkdtempSync(join(tmpdir(), "ultron-a39-"));
	const agentDir = join(root, "agent");
	const projectDir = join(root, "project");
	mkdirSync(agentDir, { recursive: true });
	mkdirSync(projectDir, { recursive: true });
	for (const file of ["models.json", "auth.json"]) {
		if (existsSync(join(PROFILE, file))) copyFileSync(join(PROFILE, file), join(agentDir, file));
	}
	for (const [name, content] of Object.entries(capability.files ?? {})) writeFileSync(join(projectDir, name), content);
	const split = model.indexOf("/");
	const client = new RpcClient({
		cliPath,
		cwd: projectDir,
		provider: model.slice(0, split),
		model: model.slice(split + 1),
		args: ["--no-session"],
		env: {
			NODE_OPTIONS: `--import ${sourceResolverPath}`,
			ULTRON_CODING_AGENT_DIR: agentDir,
			ULTRON_SERVER_DIR: mkdtempSync(join("/tmp", "u-a39-")),
		},
	});
	const started = Date.now();
	try {
		await client.start();
		const events = await client.promptAndWait(capability.prompt, undefined, 10 * 60 * 1000);
		const usedRlm = events.some((event) => event.type === "tool_execution_start" && event.toolName === "rlm");
		const answer = (await client.getLastAssistantText()) ?? "";
		return {
			model,
			case: capability.id,
			trial,
			passed: usedRlm && capability.check(answer),
			usedRlm,
			answer: answer.slice(0, 500),
			expected: capability.expected,
			durationMs: Date.now() - started,
		};
	} catch (error) {
		return {
			model,
			case: capability.id,
			trial,
			passed: false,
			usedRlm: false,
			answer: "",
			expected: capability.expected,
			durationMs: Date.now() - started,
			error: error instanceof Error ? error.message : String(error),
		};
	} finally {
		await client.stop().catch(() => {});
		rmSync(root, { recursive: true, force: true });
	}
}

describe.skipIf(!LIVE)("A39 live typed RLM capability suite", () => {
	for (const model of MODELS) {
		test(
			`${model} uses the typed RLM interface`,
			async () => {
				const trials: TrialRecord[] = [];
				for (const capability of CASES) {
					for (let trial = 1; trial <= TRIALS; trial += 1) trials.push(await runTrial(model, capability, trial));
				}
				const summary = CASES.map((capability) => {
					const passes = trials.filter((trial) => trial.case === capability.id && trial.passed).length;
					return { case: capability.id, passes, trials: TRIALS, qualified: passes >= PASS_THRESHOLD };
				});
				mkdirSync(RESULTS_DIR, { recursive: true });
				writeFileSync(
					join(RESULTS_DIR, `${model.replace(/[^a-z0-9.-]+/gi, "_")}.json`),
					`${JSON.stringify({ suiteVersion: 1, model, trialsPerCase: TRIALS, passThreshold: PASS_THRESHOLD, recordedAt: new Date().toISOString(), summary, trials }, null, 2)}\n`,
				);
				expect(summary.filter((entry) => !entry.qualified)).toEqual([]);
			},
			60 * 60 * 1000,
		);
	}
});
