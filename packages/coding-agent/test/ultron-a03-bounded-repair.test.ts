import { afterEach, describe, expect, test } from "vitest";
import type { NativeDefinitionAdapterRequest } from "../src/ultron/rlm/definition-registry.ts";
import { NativeUsageLedger } from "../src/ultron/usage.ts";
import { definition, hostFixture, journal } from "./ultron-host-fixtures.ts";

/**
 * A03: an invalid result triggers bounded repair (predict) or a typed failure (all strategies).
 * Repair attempts are bounded by maxRepairs (itself capped at 2), observable to the adapter and in
 * the usage ledger, and exhaustion returns a typed failure instead of the last malformed answer.
 */

const fixtures: Array<ReturnType<typeof hostFixture>> = [];
afterEach(async () => {
	for (const fixture of fixtures.splice(0)) await fixture.host.close();
});

/** `doubled` must be a nonnegative integer: -4 is well-typed but violates a declared constraint. */
const constrained = {
	outputSchema: {
		type: "object",
		properties: { doubled: { type: "integer", minimum: 0 } },
		required: ["doubled"],
		additionalProperties: false,
	},
};

function predictFixture(answers: unknown[], maxRepairs: number) {
	const calls: Array<NativeDefinitionAdapterRequest["repair"]> = [];
	const usage = new NativeUsageLedger();
	const fixture = hostFixture({
		usage,
		predict: async ({ repair }) => {
			calls.push(repair);
			const answer = answers[Math.min(calls.length - 1, answers.length - 1)];
			if (answer instanceof Error) throw answer;
			return answer;
		},
	});
	fixtures.push(fixture);
	const register = () =>
		fixture.call("agents.register", { definition: definition("judge", "predict", { ...constrained, maxRepairs }) });
	return { fixture, calls, usage, register };
}

describe("A03 bounded repair and typed failure", () => {
	test("predict repairs a semantically invalid result within maxRepairs and reports the repair", async () => {
		const { fixture, calls, usage, register } = predictFixture([{ doubled: -4 }, { doubled: 4 }], 2);
		await register();
		const result = await fixture.call("agents.invoke", { definition: "judge@1", input: { n: 2 } });
		expect(result).toEqual({ status: "succeeded", value: { doubled: 4 }, verification: "unverified" });
		expect(calls).toEqual([
			undefined,
			{ attempt: 1, previous: { doubled: -4 }, error: "judge@1 output does not match its schema" },
		]);
		// Observable: one model call per attempt, all charged to the same root and settled.
		const status = await usage.status();
		expect(status.usage).toMatchObject({ taskCalls: 1, modelCalls: 2 });
		expect(status.activeReservations).toBe(0);
	});

	test("predict stops after maxRepairs and fails typed without returning the malformed answer", async () => {
		const { fixture, calls, usage, register } = predictFixture([{ doubled: -4 }], 2);
		await register();
		const result = (await fixture.call("agents.invoke", { definition: "judge@1", input: { n: 2 } })) as Record<
			string,
			unknown
		>;
		expect(result).toEqual({
			status: "failed",
			error: "judge@1 output does not match its schema after 2 repair attempts",
			verification: "unverified",
		});
		expect(result).not.toHaveProperty("value");
		expect(calls.map((repair) => repair?.attempt ?? 0)).toEqual([0, 1, 2]);
		expect((await journal(fixture))[0]).toMatchObject({ state: "failed", result });
		expect((await usage.status()).usage).toMatchObject({ taskCalls: 1, modelCalls: 3 });
	});

	test("maxRepairs 0 makes exactly one attempt, and more than 2 repairs cannot be registered", async () => {
		const { fixture, calls, register } = predictFixture([{ doubled: "four" }], 0);
		await register();
		expect(await fixture.call("agents.invoke", { definition: "judge@1", input: { n: 2 } })).toMatchObject({
			status: "failed",
			error: "judge@1 output does not match its schema",
		});
		expect(calls).toHaveLength(1);
		await expect(
			fixture.call("agents.register", { definition: definition("greedy", "predict", { maxRepairs: 3 }) }),
		).rejects.toThrow("Invalid native agent definition");
	});

	test("non-JSON and throwing predict adapters fail typed", async () => {
		const nonJson = predictFixture([Number.NaN], 1);
		await nonJson.register();
		expect(await nonJson.fixture.call("agents.invoke", { definition: "judge@1", input: { n: 1 } })).toMatchObject({
			status: "failed",
			verification: "unverified",
		});
		expect(nonJson.calls).toHaveLength(2);
		const throwing = predictFixture([new Error("provider refused")], 2);
		await throwing.register();
		expect(await throwing.fixture.call("agents.invoke", { definition: "judge@1", input: { n: 1 } })).toEqual({
			status: "failed",
			error: "provider refused",
			verification: "unverified",
		});
		// A provider error is not a validation failure and is not silently retried.
		expect(throwing.calls).toHaveLength(1);
	});

	test("rlm malformed JSON and schema-invalid output fail typed after one bounded attempt", async () => {
		const replies: Record<string, string> = {
			'{"n":1}': "this is not json {",
			'{"n":2}': '{"doubled":-4}',
			'{"n":3}': '```json\n{"doubled":"six"}\n```',
		};
		const fixture = hostFixture({ script: (_lane, prompt) => replies[/Input data:\n(.*)\n/.exec(prompt)![1]]! });
		fixtures.push(fixture);
		// maxRepairs is declared but the rlm strategy never loops on a bad answer: one prompt per task.
		await fixture.call("agents.register", {
			definition: definition("investigator", "rlm", { ...constrained, maxRepairs: 2 }),
		});
		const malformed = (await fixture.call("agents.invoke", {
			definition: "investigator@1",
			input: { n: 1 },
		})) as Record<string, unknown>;
		expect(malformed).toMatchObject({ status: "failed", verification: "unverified" });
		expect(malformed.error).toMatch(/JSON/);
		expect(malformed).not.toHaveProperty("value");
		for (const n of [2, 3]) {
			expect(await fixture.call("agents.invoke", { definition: "investigator@1", input: { n } })).toEqual({
				status: "failed",
				error: "investigator@1 output does not match its schema",
				verification: "unverified",
			});
		}
		expect(fixture.prompts).toHaveLength(3);
		expect((await journal(fixture)).map((task) => task.state)).toEqual(["failed", "failed", "failed"]);
	});
});
