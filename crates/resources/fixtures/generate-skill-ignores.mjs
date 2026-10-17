// Generates crates/resources/fixtures/skill-ignores.json from this worktree's TypeScript sources (core/skills.ts):
// skill directories with `.gitignore`, `.ignore` and `.fdignore` files, loaded by `loadSkills` from the default
// locations and from explicit paths. Roots are replaced by $ROOT; the Rust test rebuilds the tree and requires the
// same skills and diagnostics.
// Usage: node --import ./crates/cli/fixtures/offline-guard.mjs crates/resources/fixtures/generate-skill-ignores.mjs
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const src = join(here, "../../../packages/coding-agent/src");
const { loadSkills } = await import(join(src, "core/skills.ts"));

const fm = (name, description) => `---\nname: ${name}\ndescription: ${description}\n---\n\n# Body of ${name}\nUse it well.\n`;

// Ignore files inside skill directories (`.gitignore`, `.ignore`, `.fdignore`), each applying to its directory and
// below, relative to the walked root.
const TREE = [
	["agent/skills/.gitignore", "beta/\n"],
	["agent/skills/alpha/SKILL.md", fm("alpha", "Kept default skill.")],
	["agent/skills/beta/SKILL.md", fm("beta", "Ignored default skill.")],
	["extra/skills/.gitignore", "# drafts are private\ndrafts/\n\nsecret.md\n"],
	["extra/skills/.ignore", "*.wip.md\n!keep.wip.md\n"],
	["extra/skills/drafts/one/SKILL.md", fm("draft-one", "Ignored by .gitignore.")],
	["extra/skills/kept/SKILL.md", fm("kept", "Loaded.")],
	["extra/skills/root.md", fm("root", "Root file loaded.")],
	["extra/skills/root.wip.md", fm("root-wip", "Ignored by .ignore.")],
	["extra/skills/keep.wip.md", fm("keep-wip", "Negated back in.")],
	["extra/skills/secret.md", fm("secret", "Ignored root file.")],
	["extra/skills/nested/.fdignore", "/inner/\n"],
	["extra/skills/nested/inner/SKILL.md", fm("inner", "Ignored by a nested .fdignore.")],
	["extra/skills/nested/other/SKILL.md", fm("other", "Loaded beside it.")],
	["extra/skills/nested/other/inner/SKILL.md", fm("deep-inner", "Not reached: other is a skill root.")],
	["extra/skills/selfignored/.gitignore", "SKILL.md\n"],
	["extra/skills/selfignored/SKILL.md", fm("selfignored", "Ignored by its own directory.")],
	["extra/skills/selfignored/sub/SKILL.md", fm("selfignored-sub", "Found below the ignored SKILL.md.")],
	["extra/skills/upper/.gitignore", "LOUD/\n"],
	["extra/skills/upper/loud/SKILL.md", fm("loud", "Ignored case-insensitively.")],
	["proj/.keep", ""],
];


const root = realpathSync(mkdtempSync(join(tmpdir(), "skill-ignores-")));
for (const [path, content] of TREE) {
	const full = join(root, path);
	mkdirSync(dirname(full), { recursive: true });
	writeFileSync(full, content);
}
const scrub = (value) => JSON.parse(JSON.stringify(value ?? null).split(root).join("$ROOT"));
const cases = [
	{ includeDefaults: true, skillPaths: ["../extra/skills"] },
	{ includeDefaults: false, skillPaths: [join(root, "extra/skills/nested"), join(root, "extra/skills/selfignored")] },
	{ includeDefaults: false, skillPaths: [join(root, "extra/skills/upper"), join(root, "extra/skills/drafts")] },
];
const out = { tree: TREE, cases: [] };
for (const c of cases) {
	const result = loadSkills({ cwd: join(root, "proj"), agentDir: join(root, "agent"), ...c });
	out.cases.push({
		...scrub(c),
		skills: scrub(result.skills.map((s) => ({ name: s.name, filePath: s.filePath, baseDir: s.baseDir }))),
		diagnostics: scrub(result.diagnostics.map((d) => ({ type: d.type, message: d.message, path: d.path ?? null }))),
	});
}
rmSync(root, { recursive: true, force: true });
writeFileSync(join(here, "skill-ignores.json"), `${JSON.stringify(out, null, "\t")}\n`);
