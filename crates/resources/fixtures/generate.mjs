// Generates crates/resources/fixtures/prompts.json from this worktree's TypeScript sources.
// Builds scripted temp trees (context files, skill layouts, a nested git worktree), runs the TS loaders and prompt
// builders on them, and records the tree spec with every output; root paths are replaced by $ROOT, the bundled
// skills dir by $BUNDLED. The Rust tests rebuild each tree and require identical text.
// Usage: node crates/resources/fixtures/generate.mjs
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const src = join(here, "../../../packages/coding-agent/src");
const { buildSystemPrompt, buildSystemPromptSections, diffSystemPromptSections } = await import(join(src, "core/system-prompt.ts"));
const { applyAlwaysSkills, loadSkills, loadSkillsFromDir } = await import(join(src, "core/skills.ts"));
const { loadProjectContextFiles } = await import(join(src, "core/resource-loader.ts"));
const { rlmRuntimePrompt, rlmToolGuidelines } = await import(join(src, "ultron/rlm/prompt.ts"));
const { expandPromptTemplate, loadPromptTemplates, parseCommandArgs, substituteArgs } = await import(join(src, "core/prompt-templates.ts"));
const { DefaultResourceLoader } = await import(join(src, "core/resource-loader.ts"));
const { SettingsManager } = await import(join(src, "core/settings-manager.ts"));
const { getBundledSkillsDir, getDocsPath, getExamplesPath, getReadmePath } = await import(join(src, "config.ts"));

const BUNDLED = getBundledSkillsDir();
const PKG = dirname(getReadmePath());
const docPaths = { readme: getReadmePath(), docs: getDocsPath(), examples: getExamplesPath() };

const fm = (name, description, extra = "") =>
	`---\n${name === undefined ? "" : `name: ${name}\n`}${description === undefined ? "" : `description: ${description}\n`}${extra}---\n\n# Body of ${name ?? "unnamed"}\nUse it well.\n`;

// Tree specs: [path, content] writes a file, [path, null, target] makes a symlink.
const TREES = {
	context: [
		["agent/AGENTS.md", "Global rules.\n"],
		["AGENTS.md", "\uFEFFRoot agents with BOM.\n"],
		["proj/CLAUDE.md", "Project claude.\n"],
		["proj/CLAUDE.MD", "Never chosen: CLAUDE.md is first.\n"],
		["proj/a/AGENTS.override.md", "Override wins.\n"],
		["proj/a/AGENTS.md", "Shadowed by override.\n"],
		["proj/a/b/c/.keep", ""],
		["proj/a/b/AGENTS.md/.dir", ""],
	],
	skills: [
		["agent/skills/alpha/SKILL.md", fm("alpha", "Alpha skill. Does alpha things & more <tags>.")],
		["agent/skills/beta/SKILL.md", fm(undefined, "Beta from dir name.")],
		["agent/skills/beta/nested/SKILL.md", fm("never", "Not reached: parent is a skill root.")],
		["agent/skills/root-file.md", fm("root-file", "A root markdown skill.")],
		["agent/skills/no-desc.md", fm("no-desc", undefined)],
		["agent/skills/group/gamma/SKILL.md", fm("Gamma_Bad--", "Invalid name still loads.")],
		["agent/skills/group/loose.md", fm("loose", "Not loaded: not at root.")],
		["agent/skills/group/delta/SKILL.md", fm("delta", "  ")],
		["agent/skills/hidden/SKILL.md", fm("hidden", "Hidden from listing.", "disable-model-invocation: true\n")],
		["agent/skills/.dot/SKILL.md", fm("dot", "Skipped dot dir.")],
		["agent/skills/node_modules/x/SKILL.md", fm("nm", "Skipped node_modules.")],
		["agent/skills/multi/SKILL.md", fm("multi", "|\n  First line.\n  Second line with 'quotes'.")],
		["agent/skills/badyaml/SKILL.md", "---\nname: [unclosed\n---\nbody\n"],
		["agent/skills/link", null, "alpha"],
		["proj/.pi/skills/alpha/SKILL.md", fm("alpha", "Project alpha collides.")],
		["proj/.pi/skills/proj-only/SKILL.md", fm("proj-only", "Project skill.")],
		["extra/one.md", fm("extra-one", "Explicit file skill.")],
		["extra/dir/zeta/SKILL.md", fm("zeta", "Explicit dir skill.")],
		["extra/notes.txt", "not markdown"],
		["agent/AGENTS.md", "Global.\n"],
		["proj/AGENTS.md", "Project.\n"],
	],
	prompts: [
		["agent/prompts/review.md", "---\ndescription: Review code\nargument-hint: <file>\n---\nReview $1 then ${@:2}.\n"],
		["agent/prompts/long.md", `\n\n   \n${"x".repeat(70)}\nsecond\n`],
		["agent/prompts/short.md", "Short first line\nAll: $ARGUMENTS / ${1:-none} / ${@:-empty}\n"],
		["agent/prompts/.hidden.md", "Hidden but loaded.\n"],
		["agent/prompts/empty.md", ""],
		["agent/prompts/.md", "Unnamed template $1\n"],
		["agent/prompts/edge61.md", `${"y".repeat(61)}\n`],
		["agent/prompts/edge60.md", `${"z".repeat(60)}\n`],
		["agent/prompts/bad.md", "---\nfoo: [x\n---\nbody\n"],
		["agent/prompts/notes.txt", "ignored"],
		["agent/prompts/sub/nested.md", "Not loaded: non-recursive.\n"],
		["proj/.pi/prompts/review.md", "---\ndescription: 42\n---\nProject review $@\n"],
		["extra/one.md", "---\nargument-hint: \"\"\n---\nExplicit $1\n"],
		["extra/dir/two.md", "Two ${@:1:1}\n"],
	],
	systemFiles: [
		["agent/SYSTEM.md", "\uFEFFGlobal system.\n"],
		["agent/APPEND_SYSTEM.md", "Global append.\n"],
		["proj/.pi/SYSTEM.md", "Project system.\n"],
		["proj/.pi/APPEND_SYSTEM.md", "Project append.\n"],
		["bare/.pi/SYSTEM.md", "Untrusted unless trusted.\n"],
		["custom.md", "Custom file prompt.\n"],
		["somedir/.keep", ""],
	],
	worktree: [
		["repo/AGENTS.md", "Main repo agents.\n"],
		["repo/README", "x\n"],
	],
};

function makeTree(spec) {
	const root = realpathSync(mkdtempSync(join(tmpdir(), "resources-fixture-")));
	for (const [path, content, target] of spec) {
		const full = join(root, path);
		mkdirSync(dirname(full), { recursive: true });
		if (content === null) symlinkSync(target, full);
		else if (path.endsWith("/.dir")) mkdirSync(dirname(full), { recursive: true });
		else writeFileSync(full, content);
	}
	return root;
}

const scrub = (value, root) =>
	JSON.parse(
		JSON.stringify(value ?? null)
			.split(root)
			.join("$ROOT")
			.split(BUNDLED)
			.join("$BUNDLED")
			.split(PKG)
			.join("$PKG"),
	);
const diag = (d) => ({ type: d.type, message: d.message, path: d.path ?? null, collision: d.collision ? [d.collision.winnerPath, d.collision.loserPath] : null });
const skillJson = (s) => ({
	name: s.name, description: s.description, filePath: s.filePath, baseDir: s.baseDir,
	source: s.sourceInfo.scope ?? s.sourceInfo.source, disableModelInvocation: s.disableModelInvocation,
	alwaysOnContent: s.alwaysOnContent ?? null,
});

const out = { docPaths: null, trees: TREES, context: [], skills: [], prompts: [], runtime: [], guidelines: [], diffs: [] };
out.docPaths = scrub(docPaths, "$ROOT");

// Context files.
{
	const root = makeTree(TREES.context);
	for (const cwd of ["proj/a/b/c", "proj/a", "proj", "."]) {
		const files = loadProjectContextFiles({ cwd: join(root, cwd), agentDir: join(root, "agent") });
		out.context.push({ tree: "context", cwd, agentDir: "agent", files: scrub(files, root) });
	}
	rmSync(root, { recursive: true, force: true });
}
// Nested linked worktree: its own AGENTS.md shadows the main repo's.
{
	const root = makeTree(TREES.worktree);
	const git = (...args) => execFileSync("git", ["-C", join(root, "repo"), ...args], { stdio: "ignore", env: { ...process.env, GIT_CONFIG_GLOBAL: "/dev/null", GIT_AUTHOR_NAME: "t", GIT_AUTHOR_EMAIL: "t@t", GIT_COMMITTER_NAME: "t", GIT_COMMITTER_EMAIL: "t@t" } });
	git("init", "-q");
	git("add", ".");
	git("commit", "-qm", "init");
	git("worktree", "add", "-q", "--detach", "wt/one");
	const files = loadProjectContextFiles({ cwd: join(root, "repo/wt/one"), agentDir: join(root, "agent") });
	out.context.push({ tree: "worktree", git: true, cwd: "repo/wt/one", agentDir: "agent", files: scrub(files, root) });
	rmSync(root, { recursive: true, force: true });
}

// Skills and prompts.
{
	const root = makeTree(TREES.skills);
	const cases = [
		{ includeDefaults: true, skillPaths: [] },
		{ includeDefaults: true, skillPaths: ["../extra/one.md", " ../extra/dir ", "../extra/notes.txt", "../missing"] },
		{ includeDefaults: false, skillPaths: [join(root, "agent/skills/alpha"), "../extra/one.md"] },
	];
	for (const c of cases) {
		const result = loadSkills({ cwd: join(root, "proj"), agentDir: join(root, "agent"), ...c });
		out.skills.push({ tree: "skills", cwd: "proj", agentDir: "agent", ...scrub(c, root), skills: scrub(result.skills.map(skillJson), root), diagnostics: scrub(result.diagnostics.map(diag), root) });
	}

	const loaded = loadSkills({ cwd: join(root, "proj"), agentDir: join(root, "agent"), skillPaths: [], includeDefaults: true });
	const bundled = loadSkillsFromDir({ dir: BUNDLED, source: "bundled" }).skills.map((s) => ({ ...s, disableModelInvocation: true }));
	const always = ["multi", "nope", "alpha", "multi"];
	const alwaysDiagnostics = applyAlwaysSkills(loaded.skills, always);
	const contextFiles = loadProjectContextFiles({ cwd: join(root, "proj"), agentDir: join(root, "agent") });
	const allSkills = [...loaded.skills, ...bundled];
	out.always = { names: always, diagnostics: scrub(alwaysDiagnostics.map(diag), root), skills: scrub(loaded.skills.map(skillJson), root) };
	out.bundledCount = bundled.length;

	const base = { cwd: join(root, "proj"), contextFiles, skills: allSkills };
	const promptCases = {
		defaults: {},
		rlmOnly: { selectedTools: ["rlm"], toolSnippets: { rlm: "Python REPL" }, toolGuidelines: { rlm: rlmToolGuidelines(["rlm"]) }, includeHarnessDocs: false },
		engineering: { selectedTools: ["rlm"], engineering: true, includeHarnessDocs: false },
		bashOnly: { selectedTools: ["bash", "powershell"], toolSnippets: { bash: "Run shell", powershell: "" }, promptGuidelines: ["  Be concise in your responses  ", "Extra rule", ""] },
		grepTools: { selectedTools: ["read", "bash", "grep"], toolGuidelines: { read: ["Read first", "Extra rule"], grep: ["Grep rule"] }, promptGuidelines: ["Extra rule"] },
		pwsh: { selectedTools: ["powershell", "edit"] },
		custom: { customPrompt: "Custom preamble.", appendSystemPrompt: "Appended text.", sections: { tools: "override tools", extra_one: "Extra section.", empty: "" } },
		overrideSection: { sections: { rules: "Custom rules.", cwd: "elsewhere", after: "Last." } },
		noTools: { selectedTools: ["edit", "write"], contextFiles: [], skills: [] },
		windowsCwd: { cwd: "C:\\work\\proj", contextFiles: [], skills: [] },
		emptyCustom: { customPrompt: "", skills: loaded.skills.filter((s) => s.disableModelInvocation) },
	};
	for (const [name, extra] of Object.entries(promptCases)) {
		const input = { ...base, ...extra };
		const sections = buildSystemPromptSections(input);
		out.prompts.push({ name, input: scrub({ ...extra, skills: undefined, contextFiles: undefined }, root), skillsFrom: extra.skills ? (extra.skills.length ? "hidden" : "none") : "all", contextFrom: extra.contextFiles ? "none" : "loaded", sections: scrub(Object.entries(sections), root), text: scrub(buildSystemPrompt(input), root) });
	}
	for (const bad of ["Bad", "preamble", "1x", "a b"]) {
		try {
			buildSystemPromptSections({ cwd: "/", sections: { [bad]: "x" } });
		} catch (e) {
			out.prompts.push({ name: `invalid:${bad}`, error: e.message });
		}
	}
	const a = buildSystemPromptSections({ ...base });
	const b = buildSystemPromptSections({ ...base, selectedTools: ["rlm"], appendSystemPrompt: "Now appended." });
	out.diffs.push({ from: "defaults", to: "rlmAppend", patch: scrub(diffSystemPromptSections(a, b) ?? null, root) });
	out.diffs.push({ from: "defaults", to: "defaults", patch: diffSystemPromptSections(a, a) ?? null });
	rmSync(root, { recursive: true, force: true });
}

// Prompt templates.
{
	const root = makeTree(TREES.prompts);
	out.templates = [];
	for (const c of [
		{ includeDefaults: true, promptPaths: [] },
		{ includeDefaults: false, promptPaths: ["../extra/one.md", " ../extra/dir ", "../missing", "../extra", join(root, "agent/prompts/review.md")] },
	]) {
		const result = loadPromptTemplates({ cwd: join(root, "proj"), agentDir: join(root, "agent"), ...c });
		const templates = result.templates.map((t) => ({ name: t.name, description: t.description, argumentHint: t.argumentHint ?? null, content: t.content, scope: t.sourceInfo.scope, filePath: t.filePath }));
		out.templates.push({ tree: "prompts", ...scrub(c, root), templates: scrub(templates, root), diagnostics: scrub(result.diagnostics.map(diag), root) });
	}
	const loaded = loadPromptTemplates({ cwd: join(root, "proj"), agentDir: join(root, "agent"), promptPaths: [], includeDefaults: true }).templates;
	out.expand = ["/review a.ts b c", "/review", "/short  'quoted arg'  \"two words\"\tthree", "/short", "/missing x", "no slash", "/", "/ review", "/review\n\nmulti\nline"].map((text) => ({ text, out: expandPromptTemplate(text, loaded) }));
	rmSync(root, { recursive: true, force: true });
}
out.parseArgs = ["", "a b  c", "'a b' \"c d\" e'f g'h", "unterminated 'quote here", "tab\tsep\nnl\u00a0nbsp\u0085nel\ufeffbom", "\"\" ''"].map((input) => ({ input, args: parseCommandArgs(input) }));
const subArgs = [[], ["a"], ["a", "b", "c"], ["", "x"]];
const templatesText = ["$1 $2 $3 $4", "$@|$ARGUMENTS|$ARGUMENTSX|$@@", "${@:2}|${@:1:2}|${@:0:1}|${@:3:0}|${@:9}", "${4:-d}|${1:-x}|${@:-none}|${ARGUMENTS:-z}|${2:-}", "${@:-none} $5$ ${x} $ $$1 ${1} ${@:} ${@:1:} ${1:-no-close", "$0 $10 $99999999999999999999999 ${99999999999999999999:-big}", "unicode ✓ $1 ✓"];
out.substitute = [];
for (const args of subArgs) for (const content of templatesText) out.substitute.push({ content, args, out: substituteArgs(content, args) });

// SYSTEM.md / APPEND_SYSTEM.md through the real DefaultResourceLoader.
{
	const root = makeTree(TREES.systemFiles);
	out.promptFiles = [];
	const cases = [
		{ cwd: "proj", agentDir: "agent", trusted: true },
		{ cwd: "proj", agentDir: "agent", trusted: false },
		{ cwd: "bare", agentDir: "none", trusted: true },
		{ cwd: "bare", agentDir: "none", trusted: false },
		{ cwd: "proj", agentDir: "agent", trusted: true, systemPrompt: "Literal system text", appendSystemPrompt: [] },
		{ cwd: "proj", agentDir: "agent", trusted: true, systemPrompt: "$ROOT/custom.md", appendSystemPrompt: ["$ROOT/custom.md", "literal append", "$ROOT/somedir", ""] },
		{ cwd: "proj", agentDir: "agent", trusted: false, systemPrompt: "" },
	];
	for (const c of cases) {
		const settingsManager = SettingsManager.inMemory();
		settingsManager.setProjectTrusted(c.trusted);
		const fix = (v) => (typeof v === "string" ? v.replace("$ROOT", root) : v);
		const loader = new DefaultResourceLoader({
			cwd: join(root, c.cwd), agentDir: join(root, c.agentDir), settingsManager,
			noExtensions: true, noSkills: true, noPromptTemplates: true, noThemes: true, noContextFiles: true,
			...(c.systemPrompt === undefined ? {} : { systemPrompt: fix(c.systemPrompt) }),
			...(c.appendSystemPrompt === undefined ? {} : { appendSystemPrompt: c.appendSystemPrompt.map(fix) }),
		});
		await loader.reload();
		out.promptFiles.push({
			...c,
			systemPrompt: c.systemPrompt ?? null,
			appendSystemPrompt: c.appendSystemPrompt ?? null,
			out: scrub({
				systemPrompt: loader.getSystemPrompt() ?? null,
				systemPromptSource: loader.getSystemPromptSource()?.path ?? null,
				appendSystemPrompt: loader.getAppendSystemPrompt(),
				appendSources: loader.getAppendSystemPromptSources().map((s) => s.path),
			}, root),
		});
	}
	rmSync(root, { recursive: true, force: true });
}

// Runtime guide.
const extTools = Array.from({ length: 14 }, (_, i) => ({ name: i === 0 ? "mcp" : `tool${i}`, description: i === 1 ? "" : i === 2 ? `Long ${"word ".repeat(40)}end` : `Does   thing\n${i}.` }));
const runtimeCases = [
	{ tools: ["read", "bash"], options: {} },
	{ tools: ["rlm"], options: { hints: false } },
	{ tools: ["rlm"], options: { hints: true, delivery: "next-call", externalRoot: true } },
	{ tools: ["rlm", "bash", "edit"], options: { hints: false, asyncEvents: false } },
	{ tools: ["rlm"], options: { hints: false, delivery: "off", extensionTools: extTools, mcpServers: ["exa", "linear", ...Array.from({ length: 12 }, (_, i) => `s${i}`)] } },
	{ tools: ["rlm"], options: { hints: false, delivery: "next-call", extensionTools: extTools.slice(1, 3), mcpServers: [] } },
	{ tools: ["rlm"], options: { hints: false, extensionTools: extTools.slice(0, 2), nativeExtensionTools: ["tool1"] } },
	{ tools: ["rlm"], options: { hints: false, extensionTools: extTools.slice(0, 1), nativeExtensionTools: ["mcp"] } },
];
for (const c of runtimeCases) out.runtime.push({ ...c, text: rlmRuntimePrompt(c.tools, c.options) ?? null });
for (const tools of [["rlm"], ["rlm", "write"], []]) out.guidelines.push({ tools, lines: rlmToolGuidelines(tools) });

writeFileSync(join(here, "prompts.json"), `${JSON.stringify(out, null, "\t")}\n`);
