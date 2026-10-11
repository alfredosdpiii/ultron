// Generates crates/tui/fixtures/theme-registry-parity.json from packages/coding-agent theme.ts: the theme list
// (built-in, custom files, registered), initTheme / setTheme with the dark fallback, getThemeByName, the change
// callback and setRegisteredThemes validation, over custom theme files written to a temp agent dir (recorded too).
// Usage: node crates/tui/fixtures/generate-theme-registry.mjs   (deterministic; output is written next to this file)
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const here = dirname(fileURLToPath(import.meta.url));
const agentDir = mkdtempSync(join(tmpdir(), "theme-registry-"));
process.env.ULTRON_CODING_AGENT_DIR = agentDir;
const themeDir = join(here, "../../../packages/coding-agent/src/modes/interactive/theme");
const T = await import(join(themeDir, "theme.ts"));
const { validateThemeJson } = await import(join(themeDir, "theme-json.ts"));
T.setThemeJsonValidator(validateThemeJson);

const dark = JSON.parse(readFileSync(join(themeDir, "dark.json"), "utf-8"));
const variant = (name, accent) => JSON.stringify({ ...dark, name, colors: { ...dark.colors, accent } }, null, 2);
const files = {
	"Zed.json": variant("Zed", "#112233"),
	"alpha.json": variant("alpha", "#445566"),
	"beta.json": variant("Beta", "#778899"),
	"beta-lower.json": variant("beta", "#123456"),
	"dup.json": variant("alpha", "#000000"),
	"bad.json": JSON.stringify({ name: "bad", colors: { accent: "#fff" } }),
	"broken.json": "{ not json",
	"notes.txt": "ignored",
	"reg-source.json": variant("reg", "#abcdef"),
};
const custom = join(agentDir, "themes");
mkdirSync(custom);
for (const [file, content] of Object.entries(files)) writeFileSync(join(custom, file), content);

const norm = (s) => (s == null ? null : s.split(custom).join("<custom>").split(themeDir).join("<builtin>"));
const list = () => T.getAvailableThemesWithPaths().map((t) => ({ name: t.name, path: norm(t.path) }));
const current = () => ({ name: T.theme.name ?? null, accent: T.theme.fg("accent", "x") });
const err = (f) => { try { f(); return null; } catch (e) { return norm(e.message); } };

const steps = [];
const record = (op, arg, result) => steps.push({ op, arg, result: result ?? null, current: current(), changes });
let changes = 0;
T.onThemeChange(() => { changes++; });

steps.push({ op: "list", result: list() });
for (const name of ["Zed", "nope", "light", "bad"]) {
	T.initTheme(name);
	record("init", name);
}
for (const name of ["alpha", "Beta", "bad", "broken", "missing", "light", "reg"]) {
	const r = T.setTheme(name);
	record("set", name, { success: r.success, error: norm(r.error) });
}
const reg = T.loadThemeFromPath(join(custom, "reg-source.json"), "truecolor");
T.setRegisteredThemes([reg]);
steps.push({ op: "list", result: list() });
const r = T.setTheme("reg");
record("set", "reg", { success: r.success, error: norm(r.error) });
for (const name of ["reg", "bad", "Zed", "missing"]) steps.push({ op: "byName", arg: name, result: T.getThemeByName(name)?.fg("accent", "x") ?? null });
T.setThemeInstance(T.getThemeByName("alpha"));
record("instance", "alpha");
const slash = T.loadThemeFromPath(join(custom, "Zed.json"), "truecolor");
slash.name = "a/b";
steps.push({ op: "registerInvalid", result: err(() => T.setRegisteredThemes([slash])) });
steps.push({ op: "list", result: list() });
rmSync(agentDir, { recursive: true, force: true });
writeFileSync(join(here, "theme-registry-parity.json"), `${JSON.stringify({ files, steps }, null, "\t")}\n`);
console.log(`theme-registry-parity.json: ${steps.length} steps`);
