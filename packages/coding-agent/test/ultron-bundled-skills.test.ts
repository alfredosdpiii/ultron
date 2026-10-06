/**
 * Skills bundled with Ultron (packages/coding-agent/skills/<source>: the pstack port, HumanLayer's diagram-it):
 * - they load after the user's and project's skills, so a skill of the same name there wins;
 * - `ULTRON_BUNDLED_SKILLS=off` and `noSkills` leave them out;
 * - every bundled SKILL.md loads without diagnostics, names its source and license, and its references to other
 *   skills and to principles resolve; no Cursor-only mechanics remain; every Python block compiles in the REPL's
 *   dialect (top-level await).
 */
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { getBundledSkillsDir } from "../src/config.ts";
import { DefaultResourceLoader } from "../src/core/resource-loader.ts";
import { bundledSkillsNote } from "../src/core/skills.ts";
import { buildSystemPrompt } from "../src/core/system-prompt.ts";

const bundled = getBundledSkillsDir()!;
const pstack = join(bundled, "pstack");
const python = process.env.ULTRON_PYTHON ?? "/usr/bin/python3";

/** Every SKILL.md and reference markdown file under the bundled directory. */
function markdownFiles(dir: string): string[] {
	return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
		const path = join(dir, entry.name);
		if (entry.isDirectory()) return markdownFiles(path);
		return entry.name.endsWith(".md") ? [path] : [];
	});
}

/** The skill a file belongs to: the nearest directory up from it that holds a SKILL.md (paths resolve from there). */
function skillRoot(file: string): string {
	let dir = dirname(file);
	while (!existsSync(join(dir, "SKILL.md")) && dir !== bundled) dir = dirname(dir);
	return dir;
}

const skillDirs = readdirSync(pstack, { withFileTypes: true })
	.filter((entry) => entry.isDirectory() && existsSync(join(pstack, entry.name, "SKILL.md")))
	.map((entry) => entry.name);

describe("bundled skills", () => {
	let root: string;
	let previous: string | undefined;

	beforeEach(() => {
		root = mkdtempSync(join(tmpdir(), "ultron-bundled-skills-"));
		previous = process.env.ULTRON_BUNDLED_SKILLS;
		delete process.env.ULTRON_BUNDLED_SKILLS;
	});

	afterEach(() => {
		if (previous === undefined) delete process.env.ULTRON_BUNDLED_SKILLS;
		else process.env.ULTRON_BUNDLED_SKILLS = previous;
		rmSync(root, { recursive: true, force: true });
	});

	const load = async (options: { noSkills?: boolean } = {}) => {
		const loader = new DefaultResourceLoader({
			cwd: join(root, "project"),
			agentDir: join(root, "agent"),
			...options,
		});
		await loader.reload();
		return loader.getSkills();
	};

	test("load after user skills, which override them; off switches leave them out", async () => {
		mkdirSync(join(root, "project"), { recursive: true });
		const all = await load();
		const names = all.skills.map((skill) => skill.name);
		expect(names).toEqual(expect.arrayContaining(["rigor", "diagram-it", ...skillDirs]));
		expect(all.diagnostics.filter((d) => d.path?.startsWith(bundled))).toEqual([]);

		const own = join(root, "agent", "skills", "swarm");
		mkdirSync(own, { recursive: true });
		writeFileSync(
			join(own, "SKILL.md"),
			"---\nname: swarm\ndescription: My own swarm skill for this test.\n---\n\nMine.\n",
		);
		const overridden = await load();
		const swarm = overridden.skills.filter((skill) => skill.name === "swarm");
		expect(swarm).toHaveLength(1);
		expect(swarm[0]!.filePath).toBe(join(own, "SKILL.md"));
		// Replacing a bundled skill is intended: no collision warning.
		expect(overridden.diagnostics.filter((d) => d.type === "collision" && d.path?.startsWith(bundled))).toEqual([]);

		expect((await load({ noSkills: true })).skills.some((skill) => skill.name === "rigor")).toBe(false);
		process.env.ULTRON_BUNDLED_SKILLS = "off";
		expect((await load()).skills.some((skill) => skill.name === "rigor")).toBe(false);
	});

	test("the system prompt indexes the bundled workflow skills in one line each, not the principles", async () => {
		mkdirSync(join(root, "project"), { recursive: true });
		const own = join(root, "agent", "skills", "swarm");
		mkdirSync(own, { recursive: true });
		writeFileSync(
			join(own, "SKILL.md"),
			"---\nname: swarm\ndescription: My own swarm skill for this test.\n---\n\nMine.\n",
		);
		const { skills } = await load();
		const note = bundledSkillsNote(skills);
		expect(note).toContain(`in ${bundled}:`);
		expect(note).toMatch(/^- pstack\/rigor: Rigorous engineering mode/m);
		expect(note).toMatch(/^- pstack\/how: /m);
		expect(note).toMatch(/^- humanlayer\/diagram-it: Explain the current topic visually/m);
		expect(note).not.toMatch(/principle-[a-z-]+:/);
		for (const line of note.split("\n").slice(1))
			expect(existsSync(join(bundled, line.slice(2, line.indexOf(":")), "SKILL.md")), line).toBe(true);
		// An overridden skill is the user's (in the regular skill list when visible), not the bundled one.
		expect(note).not.toMatch(/^- pstack\/swarm: /m);
		for (const line of note.split("\n").slice(1)) expect(line.length, line).toBeLessThan(120);
		expect(note.length).toBeLessThan(3500);
		const prompt = buildSystemPrompt({ cwd: root, skills, selectedTools: ["rlm"] } as never);
		expect(prompt).toContain(note);
		process.env.ULTRON_BUNDLED_SKILLS = "off";
		expect(bundledSkillsNote((await load()).skills)).toBe("");
	});

	test("every bundled skill names its source and license; the license ships beside them", () => {
		expect(existsSync(join(pstack, "LICENSE"))).toBe(true);
		expect(readFileSync(join(pstack, "LICENSE"), "utf8")).toContain("Lauren Tan");
		expect(skillDirs.length).toBeGreaterThanOrEqual(24);
		for (const name of skillDirs) {
			const text = readFileSync(join(pstack, name, "SKILL.md"), "utf8");
			expect(text, name).toMatch(new RegExp(`^---\\nname: ${name}\\n`));
			expect(text, name).toContain("license: MIT");
			expect(text, name).toContain("github.com/cursor/plugins/pstack");
		}
		// HumanLayer's diagram-it (from show-me), with its license.
		expect(readFileSync(join(bundled, "humanlayer", "LICENSE"), "utf8")).toContain("HumanLayer");
		const diagram = readFileSync(join(bundled, "humanlayer", "diagram-it", "SKILL.md"), "utf8");
		expect(diagram).toMatch(/^---\nname: diagram-it\n/);
		expect(diagram).toContain("github.com/humanlayer/skills/plugins/show-me");
	});

	test("cross-references resolve and no Cursor-only mechanics remain", () => {
		const cursorOnly =
			/\bTask tool\b|\bsubagent_type\b|\bAskQuestion\b|~\/\.cursor|\.mdc\b|\bpoteto-mode\b|\bpoteto-agent\b|\bsetup-pstack\b|\bpoteto-help\b|\bmake-bot-ui\b|\bcursor-team-kit\b|\/loop\b|\brun_in_background\b|\bTodoWrite\b|principle:[a-z]/;
		for (const file of markdownFiles(bundled)) {
			if (file.endsWith("README.md") && !existsSync(join(dirname(file), "SKILL.md"))) continue;
			const text = readFileSync(file, "utf8");
			// The attribution line names the original skill (`source: .../poteto-mode`).
			expect(cursorOnly.exec(text.replace(/^\s*source: .*$/gm, ""))?.[0], file).toBeUndefined();
			for (const match of text.matchAll(/\]\(((?:\.\.\/|\.\/)?[^)\s#]+\.md)(?:#[^)]*)?\)/g)) {
				if (/^[a-z]+:/.test(match[1]!)) continue;
				expect(existsSync(join(dirname(file), match[1]!)), `${file} -> ${match[1]}`).toBe(true);
			}
			// Paths in backticks are relative to the skill's directory, as Ultron tells the model when a skill loads.
			for (const match of text.matchAll(/`(\.\.\/[a-z0-9-]+\/[a-z0-9_./-]+\.(?:md|py|sh))`/g))
				expect(existsSync(join(skillRoot(file), match[1]!)), `${file} -> ${match[1]}`).toBe(true);
		}
	});

	test("every Python block compiles with top-level await", () => {
		const blocks: Array<{ file: string; code: string }> = [];
		for (const file of markdownFiles(bundled))
			for (const match of readFileSync(file, "utf8").matchAll(/```python\n([\s\S]*?)```/g))
				blocks.push({ file, code: match[1]! });
		expect(blocks.length).toBeGreaterThan(0);
		const script = [
			"import ast, json, sys",
			"bad = []",
			"for b in json.load(sys.stdin):",
			"    try: compile(b['code'], b['file'], 'exec', flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT)",
			"    except SyntaxError as e: bad.append(f\"{b['file']}: {e.msg} (line {e.lineno})\")",
			"print(json.dumps(bad))",
		].join("\n");
		const run = spawnSync(python, ["-c", script], { input: JSON.stringify(blocks), encoding: "utf8" });
		expect(run.status, run.stderr).toBe(0);
		expect(JSON.parse(run.stdout)).toEqual([]);
	});
});
