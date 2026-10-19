// Generates crates/cli/fixtures/local_themes.json: the themes the client TUI registers at startup (client-tui.ts
// `runClientTui`: `new DefaultResourceLoader({ cwd, agentDir, settingsManager })`, `reload()`, then
// `setRegisteredThemes(resourceLoader.getThemes().themes)`), and what `/settings` then offers
// (`getAvailableThemesWithPaths`). The profile has local themes only (Pi packages are not supported by ultron-rs):
// settings `themes` entries naming a file, a directory with a non-json file in it, a second directory with a
// colliding name, a file that does not load and a missing one, and a custom theme in `<agentDir>/themes`.
// Each case runs in a fresh temp root (HOME, agent dir, server dir, cwd), PATH=/usr/bin:/bin, PI_OFFLINE=1;
// offline-guard.mjs is loaded first and checked at the end. Paths are recorded as <root> and <builtin>.
// crates/cli/tests/local_themes.rs replays it.
// Usage: node crates/cli/fixtures/local_themes.mjs
import { assertOffline } from "./offline-guard.mjs";
import { mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, "../../..");
const src = join(repo, "packages/coding-agent/src");
const themeDir = join(src, "modes/interactive/theme");
const dark = JSON.parse(readFileSync(join(themeDir, "dark.json"), "utf8"));
const themeJson = (name) => JSON.stringify({ ...dark, name }, null, 2);

const root = realpathSync(mkdtempSync(join(tmpdir(), "ultron-local-themes-")));
const agentDir = join(root, "agent");
const cwd = join(root, "cwd");
for (const key of Object.keys(process.env)) delete process.env[key];
Object.assign(process.env, {
	HOME: root,
	PATH: "/usr/bin:/bin",
	PI_OFFLINE: "1",
	ULTRON_CODING_AGENT_DIR: agentDir,
	ULTRON_SERVER_DIR: join(root, "server"),
});

const { DefaultResourceLoader } = await import(pathToFileURL(join(src, "core/resource-loader.ts")).href);
const { SettingsManager } = await import(pathToFileURL(join(src, "core/settings-manager.ts")).href);
const theme = await import(pathToFileURL(join(themeDir, "theme.ts")).href);

const write = (path, text) => {
	mkdirSync(dirname(path), { recursive: true });
	writeFileSync(path, text);
};

// A settings `themes` directory, with a non-json file in it.
write(join(root, "extra/ocean/ocean.json"), themeJson("ocean"));
write(join(root, "extra/ocean/abyss.json"), themeJson("abyss"));
write(join(root, "extra/ocean/README.txt"), "not a theme");
// A second directory, one of whose names collides with the first's.
write(join(root, "extra/second/ocean.json"), themeJson("ocean"));
write(join(root, "extra/second/zest.json"), themeJson("zest"));
// A settings `themes` file, and a custom theme in the agent dir (auto-discovered).
write(join(root, "extra/solo.json"), themeJson("solo"));
write(join(agentDir, "themes/custom.json"), themeJson("custom"));
// A theme file that does not load.
write(join(root, "extra/broken.json"), "{ not json");
write(
	join(agentDir, "settings.json"),
	JSON.stringify({
		themes: [
			join(root, "extra/solo.json"),
			join(root, "extra/ocean"),
			join(root, "extra/second"),
			join(root, "extra/broken.json"),
			join(root, "extra/missing.json"),
		],
	}),
);
mkdirSync(cwd, { recursive: true });

const settingsManager = SettingsManager.create(cwd, agentDir);
const loader = new DefaultResourceLoader({ cwd, agentDir, settingsManager });
await loader.reload();
const loaded = loader.getThemes();
theme.setRegisteredThemes(loaded.themes);

const builtin = realpathSync(theme.getThemesDir?.() ?? themeDir);
const clean = (value) =>
	value === undefined || value === null
		? null
		: String(value).replaceAll(builtin, "<builtin>").replaceAll(themeDir, "<builtin>").replaceAll(root, "<root>");
const out = {
	registered: loaded.themes.map((t) => ({ name: t.name, sourcePath: clean(t.sourcePath) })),
	diagnostics: loaded.diagnostics.map((d) => ({ type: d.type, message: clean(d.message), path: clean(d.path) })),
	available: theme.getAvailableThemesWithPaths().map((t) => ({ name: t.name, path: clean(t.path) })),
};
rmSync(root, { recursive: true, force: true });
writeFileSync(join(here, "local_themes.json"), `${JSON.stringify(out, null, "\t")}\n`);
assertOffline();
