// Generates crates/claude-root/fixtures/resources.json: the skills and context files `ultron claude` and
// `ultron guide --for claude` put in Claude Code's system prompt (claude/resources.ts `loadClaudePromptResources`:
// the session worker's DefaultResourceLoader, settings and project trust), from the TypeScript CLI in this checkout
// (`node packages/coding-agent/src/cli.ts`). Each case runs in a private temp root <T> (HOME, ULTRON_CODING_AGENT_DIR,
// ULTRON_SERVER_DIR, XDG_RUNTIME_DIR, TMPDIR, a working directory <T>/w) with a FAKE `claude` (fake_claude.py) first
// on PATH, so the real Claude Code never runs, behind offline-guard.mjs (nothing may leave loopback) with the
// provider variables scrubbed. File contents may use <T>. Output is normalized as generate.mjs does.
// Usage: node crates/claude-root/fixtures/generate-resources.mjs
import { assertOffline, guardNodeOptions } from "../../cli/fixtures/offline-guard.mjs";
import { spawnSync } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../../..");
const cli = join(repo, "packages/coding-agent/src/cli.ts");
const python = process.env.ULTRON_PYTHON || "/usr/bin/python3";
const fake = readFileSync(join(here, "fake_claude.py"), "utf8");
const guardLog = join(mkdtempSync("/tmp/ucr-guard-"), "guard.log");

function makeRoot() {
	const root = mkdtempSync("/tmp/ucr-");
	for (const dir of ["h", "a", "s", "r", "w", "bin", "tmp"]) mkdirSync(join(root, dir));
	writeFileSync(join(root, "bin", "claude"), `#!${python} -I\n${fake}`);
	chmodSync(join(root, "bin", "claude"), 0o755);
	return root;
}

const sub = (text, root) => text.split("<T>").join(root);

function normalize(text, root) {
	return text
		.split(realpathSync(root))
		.join("<T>")
		.split(root)
		.join("<T>")
		.split(repo)
		.join("<REPO>")
		.replace(/- Date: \d{4}-\d{2}-\d{2}/g, "- Date: <DATE>");
}

const skill = (name, description, extra = "") => `---\nname: ${name}\ndescription: ${description}\n${extra}---\nBody of ${name}.\n`;

// Every case's working directory is <T>/w; `.git` makes it the git root (ancestor `.agents/skills` stop there).
const BASE = {
	"w/.git/HEAD": "ref: refs/heads/main\n",
	"w/AGENTS.md": "# Project rules\nUse tabs.\n",
};

const CASES = [
	{
		name: "settings-skill-entries",
		files: {
			...BASE,
			"a/settings.json": '{"skills": ["<T>/extra/one", "../extra-rel"]}\n',
			"extra/one/SKILL.md": skill("one", "A skill from a settings path."),
			"extra-rel/two/SKILL.md": skill("two", "A skill from a relative settings path."),
		},
	},
	{
		name: "home-agents-skills",
		files: {
			...BASE,
			"h/.agents/skills/roam/SKILL.md": skill("roam", "A skill in the home .agents directory."),
			"a/skills/notes/SKILL.md": skill("notes", "Keep notes tidy."),
		},
	},
	{
		name: "project-agents-skills-and-pi",
		files: {
			...BASE,
			"w/.agents/skills/proj-agents/SKILL.md": skill("proj-agents", "A project .agents skill."),
			"w/.pi/skills/deploy/SKILL.md": skill("deploy", "Deploy the app to staging."),
			"a/skills/notes/SKILL.md": skill("notes", "Keep notes tidy."),
		},
	},
	{
		name: "user-skill-shadows-bundled",
		files: {
			...BASE,
			"a/skills/pstack/SKILL.md": skill("pstack", "The user's own pstack, which wins over the bundled one."),
		},
	},
	{
		name: "project-settings-skills-trusted",
		files: {
			...BASE,
			"w/.pi/settings.json": '{"skills": ["../vendor-skills", "../../vendor-skills"]}\n',
			"vendor-skills/vend/SKILL.md": skill("vend", "A project settings skill."),
		},
	},
	{
		name: "distrusted-project",
		files: {
			...BASE,
			"a/settings.json": '{"defaultProjectTrust": "never"}\n',
			"w/.pi/settings.json": '{"skills": ["../vendor-skills", "../../vendor-skills"]}\n',
			"vendor-skills/vend/SKILL.md": skill("vend", "A project settings skill."),
			"w/.pi/skills/deploy/SKILL.md": skill("deploy", "Deploy the app to staging."),
			"w/.agents/skills/proj-agents/SKILL.md": skill("proj-agents", "A project .agents skill."),
			"h/.agents/skills/roam/SKILL.md": skill("roam", "A skill in the home .agents directory."),
		},
	},
	{
		name: "settings-skill-exclusion",
		files: {
			...BASE,
			"a/settings.json": '{"skills": ["!**/notes/**"]}\n',
			"a/skills/notes/SKILL.md": skill("notes", "Keep notes tidy."),
			"a/skills/keep/SKILL.md": skill("keep", "Kept."),
		},
	},
	{
		name: "no-bundled",
		env: { ULTRON_BUNDLED_SKILLS: "off" },
		files: { ...BASE, "h/.agents/skills/roam/SKILL.md": skill("roam", "A skill in the home .agents directory.") },
	},
	{
		name: "no-skills-flag",
		extra: ["-ns"],
		files: { ...BASE, "h/.agents/skills/roam/SKILL.md": skill("roam", "A skill in the home .agents directory.") },
	},
];

function run(c, args) {
	const root = makeRoot();
	for (const [path, content] of Object.entries(c.files ?? {})) {
		const target = join(root, path);
		mkdirSync(dirname(target), { recursive: true });
		writeFileSync(target, sub(content, root));
	}
	const env = {
		PATH: `${join(root, "bin")}:/usr/bin:/bin`,
		HOME: join(root, "h"),
		ULTRON_CODING_AGENT_DIR: join(root, "a"),
		ULTRON_SERVER_DIR: join(root, "s"),
		XDG_RUNTIME_DIR: join(root, "r"),
		TMPDIR: join(root, "tmp"),
		ULTRON_SELF_COMMAND: '["/opt/ultron/bin/ultron"]',
		ULTRON_LOKI: "off",
		ULTRON_PYTHON: python,
		ULTRON_OFFLINE: "1",
		FAKE_CLAUDE_OUT: join(root, "launch.json"),
		LANG: "C.UTF-8",
		NODE_OPTIONS: guardNodeOptions,
		ULTRON_GUARD_LOG: guardLog,
		...(c.env ?? {}),
	};
	const result = spawnSync(process.execPath, [cli, ...args.map((a) => sub(a, root))], {
		cwd: join(root, "w"),
		env,
		input: "",
		encoding: "utf8",
		timeout: 60_000,
	});
	const out = { stdout: normalize(result.stdout, root), stderr: normalize(result.stderr, root), code: result.status };
	rmSync(root, { recursive: true, force: true });
	return out;
}

const cases = [];
for (const c of CASES) {
	const guide = run(c, ["guide", "--for", "claude", "--cwd", "<T>/w", "-nc", ...(c.extra ?? [])]);
	const printed = run(c, ["claude", "--print-config", "-nc", ...(c.extra ?? [])]);
	// `--print-config` names the context files and model-visible skills (the prompt's length depends on paths).
	let resources = null;
	try {
		const { contextFiles, skills } = JSON.parse(printed.stdout).systemPrompt;
		resources = { contextFiles, skills };
	} catch {}
	cases.push({ ...c, guide, printConfig: { resources, stderr: printed.stderr, code: printed.code } });
}
assertOffline(guardLog);
rmSync(dirname(guardLog), { recursive: true, force: true });
writeFileSync(join(here, "resources.json"), `${JSON.stringify({ cases }, null, "\t")}\n`);
console.log(`${cases.length} cases`);
