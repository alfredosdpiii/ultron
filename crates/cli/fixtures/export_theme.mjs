// Records crates/cli/fixtures/export_theme.json: the TypeScript `/export` page (`generateHtml(sessionData,
// theme.name)` of core/export-html) for the theme names the client TUI can hand it — the built-ins, a custom theme in
// `<agent dir>/themes`, a registered theme with a source file, a registered theme without one, a missing name and no
// name (the default theme from COLORFGBG) — as the page's sha256 and its <style> text, or the thrown message; and
// `getShareViewerUrl` under unset, empty and set PI_SHARE_VIEWER_URL. The theme files are written to a private temp
// agent dir from the documents recorded in the fixture, so the Rust test recreates them byte for byte.
// crates/cli/tests/export_theme.rs replays it. Usage: node crates/cli/fixtures/export_theme.mjs
import "./offline-guard.mjs";
import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const src = join(here, "../../../packages/coding-agent/src");
const root = mkdtempSync(join(tmpdir(), "ultron-export-theme-"));
const agentDir = join(root, "agent");
for (const key of Object.keys(process.env)) {
	if (/^(ULTRON|PI)_|COLORFGBG|_API_KEY$/.test(key)) delete process.env[key];
}
process.env.ULTRON_CODING_AGENT_DIR = agentDir;
process.env.HOME = root;

const dark = JSON.parse(readFileSync(join(src, "modes/interactive/theme/dark.json"), "utf8"));
const light = JSON.parse(readFileSync(join(src, "modes/interactive/theme/light.json"), "utf8"));
// A custom theme: light's colors, its own vars, an export section through a var and a 256-color index.
const ocean = { ...light, name: "ocean", vars: { ...light.vars, deep: "#102030", userMsgBg: "#dfe8f0" }, export: { pageBg: "deep", cardBg: 24 } };
// A registered theme (a package's file): dark's colors without an export section, a blank text color.
const harbor = { ...dark, name: "harbor", vars: { ...dark.vars, userMsgBg: "#2a3a4a" }, colors: { ...dark.colors, text: "" } };
delete harbor.export;
const files = {
	"agent/themes/ocean.json": `${JSON.stringify(ocean, null, "\t")}\n`,
	"pkg/harbor.json": `${JSON.stringify(harbor, null, "\t")}\n`,
	"pkg/ghost.json": `${JSON.stringify({ ...dark, name: "ghost" }, null, "\t")}\n`,
};
for (const [path, content] of Object.entries(files)) {
	mkdirSync(dirname(join(root, path)), { recursive: true });
	writeFileSync(join(root, path), content);
}

const T = await import(join(src, "modes/interactive/theme/theme.ts"));
const { validateThemeJson } = await import(join(src, "modes/interactive/theme/theme-json.ts"));
T.setThemeJsonValidator(validateThemeJson);
const { generateHtml } = await import(join(src, "core/export-html/index.ts"));
const { getShareViewerUrl } = await import(join(src, "config.ts"));

const harborTheme = T.loadThemeFromPath(join(root, "pkg/harbor.json"));
const ghostLoaded = T.loadThemeFromPath(join(root, "pkg/ghost.json"));
// A registered theme with no file (an in-memory theme): its export fails.
const ghostTheme = Object.assign(Object.create(Object.getPrototypeOf(ghostLoaded)), ghostLoaded, { sourcePath: undefined });
T.setRegisteredThemes([harborTheme, ghostTheme]);
const registered = [
	{ name: "harbor", path: "pkg/harbor.json" },
	{ name: "ghost", path: null },
];

const sessionData = {
	header: { type: "session", version: 3, id: "s-1", timestamp: "2026-10-17T00:00:00.000Z", cwd: "/work" },
	entries: [
		{ type: "message", id: "e1", parentId: null, timestamp: "2026-10-17T00:00:01.000Z", message: { role: "user", content: "hi", timestamp: 1 } },
	],
	leafId: "e1",
};
const style = (html) => html.slice(html.indexOf("<style>"), html.indexOf("</style>") + "</style>".length);
const cases = [];
for (const [theme, colorfgbg] of [
	["dark", null],
	["light", null],
	["ocean", null],
	["harbor", null],
	["ghost", null],
	["missing", null],
	[null, null],
	[null, "0;15"],
]) {
	if (colorfgbg === null) delete process.env.COLORFGBG;
	else process.env.COLORFGBG = colorfgbg;
	try {
		const html = generateHtml(sessionData, theme ?? undefined);
		cases.push({ theme, colorfgbg, sha256: createHash("sha256").update(html).digest("hex"), style: style(html) });
	} catch (error) {
		cases.push({ theme, colorfgbg, error: error.message.replaceAll(root, "<ROOT>") });
	}
}
delete process.env.COLORFGBG;

const share = [];
for (const value of [undefined, "", "https://viewer.test/s/"]) {
	if (value === undefined) delete process.env.PI_SHARE_VIEWER_URL;
	else process.env.PI_SHARE_VIEWER_URL = value;
	share.push({ env: value ?? null, gistId: "abc123", url: getShareViewerUrl("abc123") });
}
rmSync(root, { recursive: true, force: true });
writeFileSync(join(here, "export_theme.json"), `${JSON.stringify({ files, registered, sessionData, cases, share }, null, 1)}\n`);
console.log(`wrote ${cases.length} export cases, ${share.length} share cases`);
