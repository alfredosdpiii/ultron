// Generates crates/session-core/fixtures/session_resources.json: the skills, prompt templates and system prompt files a
// session loads (`DefaultResourceLoader.reload`, as the TypeScript session worker builds it) from the settings' local
// `skills`/`prompts` entries (paths, `!`/`+`/`-` patterns), the auto-discovered directories (`<agentDir>/skills`,
// `<agentDir>/prompts`, `~/.agents/skills`, `.pi/skills`, `.pi/prompts`, `.agents/skills` from the cwd up to the git
// root), SYSTEM.md / APPEND_SYSTEM.md, and the bundled skills, for a trusted and an untrusted project. Each case
// records the skills (name, file, sourceInfo) and prompts (name, file, sourceInfo) in order, the system prompt texts,
// and `buildSystemPrompt` over them (the REPL-only tool set), with the private temp root as <TMP>.
// crates/session-core/tests/session_resources.rs replays them through `session_resources::resolve`.
// Usage: node crates/session-core/fixtures/generate-session-resources.mjs
import "../../cli/fixtures/offline-guard.mjs";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const src = join(here, "../../../packages/coding-agent/src");
const { DefaultResourceLoader } = await import(join(src, "core/resource-loader.ts"));
const { SettingsManager } = await import(join(src, "core/settings-manager.ts"));
const { buildSystemPrompt } = await import(join(src, "core/system-prompt.ts"));

for (const name of Object.keys(process.env)) {
	if (/^(PI_|ULTRON_|HTTPS?_PROXY$|NO_PROXY$)/i.test(name)) delete process.env[name];
}
process.env.PI_OFFLINE = "1";

const skill = (name, description = `The ${name} skill.`) =>
	`---\nname: ${name}\ndescription: ${description}\n---\n\n# ${name}\n`;
const prompt = (description) => `---\ndescription: ${description}\n---\nDo the thing with $1.\n`;

// The temp root holds a `.git` marker (the ancestor `.agents/skills` walk stops there), the profile (`agent/`), the
// home directory (`home/`), the project (`work/`), loose resources the settings list (`extra/`) and the bundled skills.
const FILES = {
	".git/HEAD": "ref: refs/heads/main\n",
	"agent/skills/mine/SKILL.md": skill("mine", "The profile's own skill."),
	"agent/skills/shared/SKILL.md": skill("shared", "The profile's shared skill."),
	"agent/prompts/own.md": prompt("The profile's prompt"),
	"agent/prompts/second.md": prompt("The profile's second prompt"),
	"agent/SYSTEM.md": "Profile system prompt.\n",
	// An auto-discovered extension directory that also holds a skill the settings list by path: the skill's exact
	// metadata (local) wins over the directory's (auto).
	"agent/extensions/tool/index.ts": "export default function () {}\n",
	"agent/extensions/tool/SKILL.md": skill("tool-skill"),
	"agent/APPEND_SYSTEM.md": "Profile addendum.\n",
	"home/.agents/skills/home-agent/SKILL.md": skill("home-agent"),
	".agents/skills/ancestor-agent/SKILL.md": skill("ancestor-agent"),
	"work/.agents/skills/work-agent/SKILL.md": skill("work-agent"),
	"work/.pi/skills/proj-skill/SKILL.md": skill("proj-skill"),
	"work/.pi/skills/mine/SKILL.md": skill("mine", "The project's mine skill."),
	"work/.pi/prompts/proj-prompt.md": prompt("The project's prompt"),
	"work/.pi/SYSTEM.md": "Project system prompt.\n",
	"work/.pi/APPEND_SYSTEM.md": "Project addendum.\n",
	"work/.pi/local/proj-listed/SKILL.md": skill("proj-listed"),
	"extra/skills/extra-one/SKILL.md": skill("extra-one"),
	"extra/skills/extra-two/SKILL.md": skill("extra-two"),
	"extra/skills/skip-me/SKILL.md": skill("skip-me"),
	"extra/loose/SKILL.md": skill("loose"),
	"extra/prompts/listed.md": prompt("A listed prompt"),
	"extra/prompts/other.md": prompt("Another listed prompt"),
	"extra/single.md": prompt("A single listed prompt"),
	"bundled/bundled-one/SKILL.md": skill("bundled-one"),
	"bundled/mine/SKILL.md": skill("mine", "The bundled mine skill."),
};

const PROJECT = { skills: ["local/proj-listed"] };

export const CASES = [
	{ name: "defaults-trusted-project", settings: {}, project: PROJECT, trusted: true },
	{ name: "untrusted-project-loads-no-project-resources", settings: {}, project: PROJECT, trusted: false },
	{
		name: "local-entries-with-patterns",
		settings: {
			skills: ["../extra/skills", "!skip-me", "../extra/loose/SKILL.md", "extensions/tool/SKILL.md"],
			prompts: ["../extra/prompts", "!other.md", "../extra/single.md"],
		},
		project: null,
		trusted: true,
	},
	{
		name: "overrides-on-auto-discovered",
		settings: { skills: ["!shared"], prompts: ["!own.md", "+prompts/own.md", "-prompts/second.md"] },
		project: { skills: ["-skills/proj-skill"] },
		trusted: true,
	},
	{
		name: "force-include-after-exclude",
		settings: { skills: ["!**/*", "+skills/shared"] },
		project: null,
		trusted: false,
	},
	{
		name: "no-skills-keeps-prompts",
		settings: { skills: ["../extra/skills"], prompts: ["../extra/single.md"] },
		project: null,
		trusted: true,
		noSkills: true,
	},
];

const results = [];
for (const c of CASES) {
	const root = realpathSync(mkdtempSync(join(tmpdir(), "session-resources-")));
	const scrub = (value) => JSON.parse(JSON.stringify(value ?? null).replaceAll(root, "<TMP>"));
	for (const [rel, text] of Object.entries(FILES)) {
		mkdirSync(dirname(join(root, rel)), { recursive: true });
		writeFileSync(join(root, rel), text);
	}
	writeFileSync(join(root, "agent/settings.json"), JSON.stringify(c.settings));
	if (c.project) writeFileSync(join(root, "work/.pi/settings.json"), JSON.stringify(c.project));
	process.env.HOME = join(root, "home");
	const cwd = join(root, "work");
	const agentDir = join(root, "agent");
	const settingsManager = SettingsManager.create(cwd, agentDir);
	settingsManager.setProjectTrusted(c.trusted === true);
	const loader = new DefaultResourceLoader({
		cwd,
		agentDir,
		settingsManager,
		bundledSkillPaths: [join(root, "bundled")],
		noExtensions: true,
		...(c.noSkills ? { noSkills: true } : {}),
	});
	await loader.reload();
	const under = (path) => path.startsWith(`${root}/`);
	const skills = loader.getSkills().skills;
	const systemPrompt = loader.getSystemPrompt();
	const append = loader.getAppendSystemPrompt();
	results.push(
		scrub({
			name: c.name,
			files: FILES,
			settings: c.settings,
			project: c.project,
			trusted: c.trusted === true,
			noSkills: c.noSkills === true,
			skills: skills
				.filter((s) => under(s.filePath))
				.map((s) => ({ name: s.name, filePath: s.filePath, sourceInfo: s.sourceInfo })),
			prompts: loader
				.getPrompts()
				.prompts.filter((p) => under(p.filePath))
				.map((p) => ({ name: p.name, filePath: p.filePath, sourceInfo: p.sourceInfo })),
			systemPrompt,
			appendSystemPrompt: append,
			prompt: buildSystemPrompt({
				cwd,
				selectedTools: ["rlm"],
				toolSnippets: { rlm: "Run Python in the persistent REPL." },
				customPrompt: systemPrompt,
				appendSystemPrompt: append.join("\n\n"),
				contextFiles: [],
				skills,
				includeHarnessDocs: false,
			}),
		}),
	);
	rmSync(root, { recursive: true, force: true });
}
assertOffline();
const out = join(here, "session_resources.json");
writeFileSync(
	out,
	`${JSON.stringify({ generator: "crates/session-core/fixtures/generate-session-resources.mjs", cases: results }, null, "\t")}\n`,
);
console.log(`wrote ${results.length} cases to ${out}`);
