/**
 * Always-on skills (`alwaysSkills` setting), gated by engineering mode (`/engineering true|false`): while the mode is
 * on, a named skill's full text goes in every system prompt, native and Claude Code lanes alike, so the model follows
 * it without reading the file or being told to. Off, the skill is listed like any other.
 */
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { DefaultResourceLoader } from "../src/core/resource-loader.ts";
import { SettingsManager } from "../src/core/settings-manager.ts";
import { ALWAYS_SKILLS_MAX_CHARS } from "../src/core/skills.ts";
import { buildSystemPrompt } from "../src/core/system-prompt.ts";
import { setEngineeringMode } from "../src/experimental/client-tui-pi-commands.ts";
import { loadClaudePromptResources, renderClaudePromptResources } from "../src/ultron/claude/resources.ts";

describe("always-on skills", () => {
	let root: string;
	let agentDir: string;
	let cwd: string;

	const writeSkill = (name: string, description: string, body: string): string => {
		const dir = join(agentDir, "skills", name);
		mkdirSync(dir, { recursive: true });
		const file = join(dir, "SKILL.md");
		writeFileSync(file, `---\nname: ${name}\ndescription: ${description}\n---\n\n${body}\n`);
		return file;
	};
	const setAlways = (names: string[], engineering = true) =>
		writeFileSync(join(agentDir, "settings.json"), JSON.stringify({ alwaysSkills: names, engineering }));
	const load = async (options: { noSkills?: boolean } = {}) => {
		const loader = new DefaultResourceLoader({ cwd, agentDir, noExtensions: true, ...options });
		await loader.reload();
		return loader.getSkills();
	};
	/** Only this feature's warnings: the loader also reads the machine's own ~/.agents skills. */
	const alwaysWarnings = (diagnostics: Array<{ message: string }>) =>
		diagnostics.map((d) => d.message).filter((message) => message.startsWith("alwaysSkills:"));
	const prompt = (skills: Awaited<ReturnType<typeof load>>["skills"], engineering = true) =>
		buildSystemPrompt({ cwd, skills, selectedTools: ["rlm"], engineering } as never);

	beforeEach(() => {
		root = mkdtempSync(join(tmpdir(), "ultron-always-skills-"));
		agentDir = join(root, "agent");
		cwd = join(root, "project");
		mkdirSync(cwd, { recursive: true });
		mkdirSync(agentDir, { recursive: true });
		process.env.ULTRON_BUNDLED_SKILLS = "off";
	});

	afterEach(() => {
		delete process.env.ULTRON_BUNDLED_SKILLS;
		rmSync(root, { recursive: true, force: true });
	});

	test("a named skill's text is in the prompt and out of the read-when-it-matches list; others stay listed", async () => {
		const ponytail = writeSkill("ponytail", "Laziest solution that works.", "Use the standard library first.");
		writeSkill("always-test-other", "Another skill, listed as usual.", "Other body text.");
		setAlways(["ponytail"]);

		const { skills, diagnostics } = await load();
		expect(alwaysWarnings(diagnostics)).toEqual([]);
		const text = prompt(skills);
		expect(text).toContain("Always-on skills: the user set these to apply to every task.");
		expect(text).toContain(`<skill name="ponytail" location="${ponytail}">`);
		expect(text).toContain("Use the standard library first.");
		// Not offered for reading as well: the model would load what it already has.
		expect(text).not.toContain("<name>ponytail</name>");
		expect(text).toContain("<name>always-test-other</name>");
		expect(text).not.toContain("Other body text.");
	});

	test("ultron claude and its subagents get the same text in their system prompt", async () => {
		writeSkill("ponytail", "Laziest solution that works.", "Use the standard library first.");
		setAlways(["ponytail"]);
		const sections = renderClaudePromptResources(await loadClaudePromptResources(cwd, {}, agentDir)).join("\n");
		expect(sections).toContain('<skill name="ponytail"');
		expect(sections).toContain("Use the standard library first.");
	});

	test("an unknown name and a skill past the size limit are warnings, and the big skill stays listed", async () => {
		writeSkill("ponytail", "Laziest solution that works.", "Use the standard library first.");
		writeSkill("huge", "A very long skill.", "x".repeat(ALWAYS_SKILLS_MAX_CHARS));
		setAlways(["ponytail", "missing", "huge"]);

		const { skills, diagnostics } = await load();
		expect(alwaysWarnings(diagnostics)).toEqual([
			'alwaysSkills: no skill named "missing" is loaded',
			expect.stringContaining("alwaysSkills: huge"),
		]);
		const text = prompt(skills);
		expect(text).toContain("Use the standard library first.");
		expect(text).toContain("<name>huge</name>");
		expect(text).not.toContain("x".repeat(100));
	});

	test("outside engineering mode an always-on skill is listed like any other, in native and Claude prompts", async () => {
		writeSkill("ponytail", "Laziest solution that works.", "Use the standard library first.");
		setAlways(["ponytail"], false);
		const off = prompt((await load()).skills, false);
		expect(off).not.toContain("Always-on skills");
		expect(off).not.toContain("Use the standard library first.");
		expect(off).toContain("<name>ponytail</name>");
		const claude = renderClaudePromptResources(await loadClaudePromptResources(cwd, {}, agentDir)).join("\n");
		expect(claude).not.toContain("Use the standard library first.");
		expect(claude).toContain("<name>ponytail</name>");
	});

	test("without the setting, or with --no-skills, nothing is always on", async () => {
		writeSkill("ponytail", "Laziest solution that works.", "Use the standard library first.");
		expect(prompt((await load()).skills)).not.toContain("Always-on skills");

		setAlways(["ponytail"]);
		const off = await load({ noSkills: true });
		expect(alwaysWarnings(off.diagnostics)).toEqual([]);
		expect(prompt(off.skills)).not.toContain("Always-on skills");
	});

	test("/engineering reports the mode, sets it through the worker and refuses anything but true or false", async () => {
		const settingsManager = SettingsManager.inMemory({ alwaysSkills: ["ponytail"] });
		const setSetting = vi.fn(async (key: string, value: unknown) => {
			if (key === "engineering") settingsManager.setEngineering(value === true);
			return { applied: "live" as const };
		});
		const status: string[] = [];
		const host = {
			settingsManager,
			settingsMirror: { apply: async () => {} },
			control: () => ({ setSetting }),
			showStatus: (text: string) => status.push(text),
			applySettingLocally: () => {},
		} as never;

		await setEngineeringMode(host, "");
		await setEngineeringMode(host, "true");
		await setEngineeringMode(host, "");
		await setEngineeringMode(host, "off");
		await setEngineeringMode(host, "maybe");
		expect(setSetting.mock.calls.map(([key, value]) => [key, value])).toEqual([
			["engineering", true],
			["engineering", false],
		]);
		expect(status).toEqual([
			"Engineering mode is off (always-on skills: ponytail)",
			"Engineering mode on: always-on skills (ponytail) are in the system prompt from the next message",
			"Engineering mode is on (always-on skills: ponytail)",
			"Engineering mode off: always-on skills are listed like other skills",
			"Usage: /engineering true|false",
		]);
	});
});
