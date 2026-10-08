import { describe, expect, test } from "vitest";
import { containsCodeSecret } from "../src/ultron/sensitive.ts";
import { SkillExtractionNudger, skillNudgeFromEnv } from "../src/ultron/tool-round-nudge.ts";

describe("code skill secret check", () => {
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
		expect(skillNudgeFromEnv(undefined)).toBe(0);
		expect(skillNudgeFromEnv("on")).toBe(8);
		expect(skillNudgeFromEnv("0")).toBe(0);
		expect(skillNudgeFromEnv("off")).toBe(0);
		expect(skillNudgeFromEnv("5")).toBe(5);
		expect(skillNudgeFromEnv("junk")).toBe(0);
		const steered: string[] = [];
		const off = new SkillExtractionNudger(0, async (message) => steered.push(message));
		for (let round = 0; round < 20; round += 1) off.turnEnded("run", 1, 0);
		expect(steered).toEqual([]);
	});
});
