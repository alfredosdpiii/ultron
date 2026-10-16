// Generates crates/cli/fixtures/session_resources.json: the skills and prompt templates a session loads
// (`DefaultResourceLoader.reload`, as the TypeScript session worker builds it) when `-e` sources and settings
// `packages` provide them, beside the profile's and project's own. Every source is a local directory under a private
// temp root (no npm, no git); the packages hold only skills and prompts (no JavaScript extensions to load). Each case
// records the skills (name, file, sourceInfo) and prompts (name, file) under the root, in order, with the root as
// <TMP>. crates/cli/tests/session_resources.rs replays them through the CLI's `-e` resolution and the session's.
// Usage: node crates/cli/fixtures/session_resources.mjs
import { assertOffline } from "./offline-guard.mjs";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../../..");
const src = join(repo, "packages/coding-agent/src");
const { DefaultResourceLoader } = await import(join(src, "core/resource-loader.ts"));
const { SettingsManager } = await import(join(src, "core/settings-manager.ts"));

for (const name of Object.keys(process.env)) {
	if (/^(PI_|ULTRON_|HTTPS?_PROXY$)/i.test(name)) delete process.env[name];
}
process.env.PI_OFFLINE = "1";

const skill = (name, description = `The ${name} skill.`) => `---\nname: ${name}\ndescription: ${description}\n---\n\n# ${name}\n`;
const prompt = (description) => `---\ndescription: ${description}\n---\nDo the thing with $1.\n`;

// The packages: alpha (user settings), beta (-e), gamma (project settings), each with skills/ and prompts/.
const FILES = {
	"agent/skills/mine/SKILL.md": skill("mine", "The profile's own skill."),
	"agent/skills/shared/SKILL.md": skill("shared", "The profile's shared skill."),
	"agent/prompts/own.md": prompt("The profile's prompt"),
	"pkgs/alpha/package.json": JSON.stringify({ name: "alpha", version: "1.0.0" }),
	"pkgs/alpha/skills/alpha-skill/SKILL.md": skill("alpha-skill"),
	"pkgs/alpha/skills/shared/SKILL.md": skill("shared", "Alpha's shared skill."),
	"pkgs/alpha/prompts/alpha-prompt.md": prompt("Alpha's prompt"),
	"pkgs/alpha/prompts/own.md": prompt("Alpha's own prompt"),
	"pkgs/beta/package.json": JSON.stringify({ name: "beta", version: "1.0.0" }),
	"pkgs/beta/skills/beta-skill/SKILL.md": skill("beta-skill"),
	"pkgs/beta/skills/mine/SKILL.md": skill("mine", "Beta's mine skill."),
	"pkgs/beta/prompts/beta-prompt.md": prompt("Beta's prompt"),
	"pkgs/gamma/package.json": JSON.stringify({ name: "gamma", version: "1.0.0", pi: { skills: ["./skills"], prompts: ["./prompts"] } }),
	"pkgs/gamma/skills/gamma-skill/SKILL.md": skill("gamma-skill"),
	"pkgs/gamma/prompts/gamma-prompt.md": prompt("Gamma's prompt"),
};

const cases = [
	{ name: "user-package-and-cli-source", settings: { packages: ["../pkgs/alpha"] }, extensions: ["../pkgs/beta"] },
	{ name: "no-skills-keeps-the-cli-source", settings: { packages: ["../pkgs/alpha"] }, extensions: ["../pkgs/beta"], noSkills: true },
	{ name: "user-package-only", settings: { packages: ["../pkgs/alpha"] }, extensions: [] },
	{ name: "cli-source-only", settings: {}, extensions: ["../pkgs/beta"] },
	{
		name: "package-filter-picks-one-skill",
		settings: { packages: [{ source: "../pkgs/alpha", skills: ["skills/alpha-skill"], prompts: [] }] },
		extensions: [],
	},
	{
		name: "project-package-ranks-before-the-user-one",
		settings: { packages: ["../pkgs/alpha"] },
		project: { packages: ["../../pkgs/gamma"] },
		trusted: true,
		extensions: [],
	},
	{
		name: "untrusted-project-package-is-ignored",
		settings: { packages: ["../pkgs/alpha"] },
		project: { packages: ["../../pkgs/gamma"] },
		trusted: false,
		extensions: [],
	},
];

const results = [];
for (const c of cases) {
	const root = realpathSync(mkdtempSync(join(tmpdir(), "session-resources-")));
	const scrub = (value) => JSON.parse(JSON.stringify(value).replaceAll(root, "<TMP>"));
	for (const [rel, text] of Object.entries(FILES)) {
		mkdirSync(dirname(join(root, rel)), { recursive: true });
		writeFileSync(join(root, rel), text);
	}
	mkdirSync(join(root, "work"), { recursive: true });
	writeFileSync(join(root, "agent/settings.json"), JSON.stringify(c.settings));
	if (c.project) {
		mkdirSync(join(root, "work/.pi"), { recursive: true });
		writeFileSync(join(root, "work/.pi/settings.json"), JSON.stringify(c.project));
	}
	process.env.HOME = root;
	const cwd = join(root, "work");
	const agentDir = join(root, "agent");
	const settingsManager = SettingsManager.create(cwd, agentDir);
	settingsManager.setProjectTrusted(c.trusted === true);
	const loader = new DefaultResourceLoader({
		cwd,
		agentDir,
		settingsManager,
		bundledSkillPaths: [],
		additionalExtensionPaths: c.extensions,
		...(c.noSkills ? { noSkills: true } : {}),
	});
	await loader.reload();
	const under = (path) => path.startsWith(`${root}/`);
	results.push(
		scrub({
			name: c.name,
			files: FILES,
			settings: c.settings,
			project: c.project ?? null,
			trusted: c.trusted === true,
			extensions: c.extensions,
			noSkills: c.noSkills === true,
			skills: loader
				.getSkills()
				.skills.filter((s) => under(s.filePath))
				.map((s) => ({ name: s.name, filePath: s.filePath, sourceInfo: s.sourceInfo })),
			prompts: loader
				.getPrompts()
				.prompts.filter((p) => under(p.filePath))
				.map((p) => ({ name: p.name, filePath: p.filePath })),
		}),
	);
	rmSync(root, { recursive: true, force: true });
}
assertOffline();
const out = join(here, "session_resources.json");
writeFileSync(out, `${JSON.stringify({ generator: "crates/cli/fixtures/session_resources.mjs", cases: results }, null, "\t")}\n`);
console.log(`wrote ${results.length} cases to ${out}`);
