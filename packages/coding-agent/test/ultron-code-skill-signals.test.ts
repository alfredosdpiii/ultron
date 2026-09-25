import { describe, expect, test } from "vitest";
import { containsCodeSecret, NativeJevClient } from "../src/ultron/jev.ts";
import { JevDecisionLog, recordingJevClient } from "../src/ultron/jev-decisions.ts";
import { SkillExtractionNudger, skillNudgeFromEnv } from "../src/ultron/tool-round-nudge.ts";

function client(fetch: typeof globalThis.fetch): NativeJevClient {
	return new NativeJevClient({ apiKey: "test-key", baseUrl: "https://jev.example.test", fetch });
}

describe("Jev skill policy", () => {
	test("asks Jev with the skill, and refuses secrets without calling it", async () => {
		const bodies: Array<Record<string, unknown>> = [];
		const jev = client(async (_url, init) => {
			bodies.push(JSON.parse(String(init?.body)) as Record<string, unknown>);
			return new Response(JSON.stringify({ answers: { action: { choice: "keep", confidence: 0.7 } } }), {
				status: 200,
			});
		});
		expect(await jev.skillPolicy("slug", "blog slugs", "def slugify(text):\n    return text")).toEqual({
			action: "keep",
			confidence: 0.7,
		});
		expect(bodies[0]).toMatchObject({ state: { skill_name: "slug", evidence: "blog slugs" } });
		expect(await jev.skillPolicy("deploy", "x", 'TOKEN = "ghp_abcdefghijklmnopqrstuvwx"')).toEqual({
			action: "sensitive",
			confidence: 1,
		});
		expect(bodies).toHaveLength(1);
	});

	test("code secrets are narrower than memory patterns", () => {
		for (const benign of [
			"skill_version_number = 3",
			"password = input()",
			"skeleton_builder_x",
			"secret_name = name",
		])
			expect(containsCodeSecret(benign)).toBe(false);
		for (const secret of [
			'api_key = "abcd1234efgh5678"',
			"sk-proj-abcdefghijklmnop",
			"AKIAABCDEFGHIJKLMNOP",
			"-----BEGIN RSA PRIVATE KEY-----",
		])
			expect(containsCodeSecret(secret)).toBe(true);
	});

	test("skill decisions are recorded as retention decisions without their input", async () => {
		let stored: unknown;
		const log = new JevDecisionLog({
			read: async () => stored as never,
			write: async (value) => {
				stored = value;
			},
		});
		const wrapped = recordingJevClient(
			{
				triage: async () => ({}) as never,
				memoryGate: async () => ({}) as never,
				memoryPolicy: async () => ({}) as never,
				memoryRecall: async () => ({}) as never,
				skillPolicy: async () => ({ action: "skip", confidence: 0.6 }),
			},
			log,
		);
		await wrapped.skillPolicy?.("slug", "evidence", "source");
		const [decision] = await log.list();
		expect(decision).toMatchObject({ kind: "retain", status: "ok", action: "skip", confidence: 0.6 });
		expect(JSON.stringify(decision)).not.toContain("source");
	});
});

describe("skill extraction nudge", () => {
	test("suggests saving a skill once after a successful streak; failures and answers reset it", () => {
		const steered: string[] = [];
		const nudger = new SkillExtractionNudger(3, async (message) => steered.push(message));
		nudger.turnEnded("run", 1, 0);
		nudger.turnEnded("run", 1, 1); // a failed tool call breaks the streak
		nudger.turnEnded("run", 1, 0);
		nudger.turnEnded("run", 0, 0); // an answer breaks it too
		nudger.turnEnded("run", 1, 0);
		nudger.turnEnded("run", 2, 0);
		expect(steered).toEqual([]);
		nudger.turnEnded("run", 1, 0);
		expect(steered).toHaveLength(1);
		expect(steered[0]).not.toContain("\n");
		expect(steered[0]).toContain("skills.propose_code");
		for (let round = 0; round < 10; round += 1) nudger.turnEnded("run", 1, 0);
		expect(steered).toHaveLength(1);
		nudger.runEnded("run");
		for (let round = 0; round < 3; round += 1) nudger.turnEnded("run", 1, 0);
		expect(steered).toHaveLength(2);
	});

	test("is configured by ULTRON_SKILL_NUDGE and can be turned off", () => {
		expect(skillNudgeFromEnv(undefined)).toBe(8);
		expect(skillNudgeFromEnv("0")).toBe(0);
		expect(skillNudgeFromEnv("off")).toBe(0);
		expect(skillNudgeFromEnv("5")).toBe(5);
		expect(skillNudgeFromEnv("junk")).toBe(8);
		const steered: string[] = [];
		const off = new SkillExtractionNudger(0, async (message) => steered.push(message));
		for (let round = 0; round < 20; round += 1) off.turnEnded("run", 1, 0);
		expect(steered).toEqual([]);
	});
});
