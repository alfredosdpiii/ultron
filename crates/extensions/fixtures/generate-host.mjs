// Generates crates/extensions/fixtures/host.json from the TypeScript product, for tests/host_fixture.rs:
//
// - `globs`: minimatch(text, pattern) with the package manager's options (none: case-sensitive);
// - `patterns`: DefaultPackageManager.resolve() over a temp tree whose extensions are directories with an `index.ts`
//   (the Rust build: the same directories with an `extension.json`) and settings `extensions` entries with patterns;
//   `{entry}` in a pattern is the entry file name (`index.ts` here, `extension.json` in Rust). Recorded: each
//   extension directory (relative to the temp root) with its enabled state, in resolve order;
// - `shortcuts`: ExtensionRunner.getShortcuts over scripted extensions and keybinding overrides of the default
//   keybindings (recorded too): the shortcuts in force (key and extension) and the diagnostics;
// - `flags`: registerFlag / getFlag / getFlagValues over scripted extensions: load errors, every extension's view of
//   every flag name, and the session's values.
//
// Usage: node crates/extensions/fixtures/generate-host.mjs   (from the repository root; deterministic)
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { minimatch } from "minimatch";
import { createEventBus } from "../../../packages/coding-agent/src/core/event-bus.ts";
import {
	createExtensionRuntime,
	loadExtensionFromFactory,
} from "../../../packages/coding-agent/src/core/extensions/loader.ts";
import { ExtensionRunner } from "../../../packages/coding-agent/src/core/extensions/runner.ts";
import { KeybindingsManager } from "../../../packages/coding-agent/src/core/keybindings.ts";
import { DefaultPackageManager } from "../../../packages/coding-agent/src/core/package-manager.ts";
import { SettingsManager } from "../../../packages/coding-agent/src/core/settings-manager.ts";

const here = dirname(fileURLToPath(import.meta.url));

// ------------------------------------------------------------------------------------------------------------ globs

const GLOB_CASES = [
	["extensions/beta/extension.json", "extensions/beta/**"],
	["extensions/beta/extension.json", "**/beta/**"],
	["extensions/beta/extension.json", "extensions/*"],
	["extensions/beta/extension.json", "extensions/*/*"],
	["extensions/Beta/extension.json", "extensions/beta/*"],
	["extensions/beta/extension.json", "extensions/BETA/*"],
	["Extension.json", "extension.json"],
	["extension.json", "extension.json"],
	["extension.json", "*.json"],
	["extension.json", "*.JSON"],
	["extensions/.hidden/extension.json", "extensions/*/extension.json"],
	["extensions/.hidden/extension.json", "extensions/**/extension.json"],
	["extensions/.hidden/extension.json", "extensions/.hidden/*"],
	["extensions/a1/extension.json", "extensions/a[0-9]/*"],
	["extensions/aB/extension.json", "extensions/a[a-z]/*"],
	["extensions/ab/extension.json", "extensions/a[[:upper:]]/*"],
	["extensions/aB/extension.json", "extensions/a[[:upper:]]/*"],
	["extensions/ab/extension.json", "extensions/a[[:lower:]]/*"],
	["extensions/beta/extension.json", "extensions/{alpha,beta}/*"],
	["extensions/gamma/extension.json", "extensions/{alpha,beta}/*"],
	["extensions/beta/extension.json", "extensions/@(alpha|beta)/*"],
	["extensions/beta/extension.json", "extensions/!(beta)/*"],
	["extensions/beta/extension.json", "extensions/b?ta/*"],
	["../outside/beta/extension.json", "../outside/**"],
	["/abs/extensions/beta/extension.json", "/abs/**/extension.json"],
	["extensions/beta/extension.json", "#extensions/**"],
	["extensions/beta/extension.json", "!extensions/alpha/**"],
	["local/one/extension.json", "local/*/extension.json"],
];

const globs = GLOB_CASES.map(([text, pattern]) => ({ text, pattern, matches: minimatch(text, pattern) }));

// --------------------------------------------------------------------------------------------------------- patterns

/** Extension directories (relative to the temp root) and settings entries per scope; `<TMP>` is the temp root. */
const PATTERN_SCENARIOS = [
	{
		name: "plain globs filter listed directories; ! excludes",
		dirs: ["agent/pkgs/alpha", "agent/pkgs/beta", "agent/pkgs/gamma"],
		user: ["pkgs", "pkgs/{alpha,gamma}/**", "!pkgs/gamma/**"],
		project: [],
	},
	{
		name: "+ adds back an exact path, - removes one whatever else said",
		dirs: ["agent/pkgs/alpha", "agent/pkgs/beta", "agent/pkgs/gamma"],
		user: ["pkgs", "!pkgs/**", "+pkgs/beta/{entry}", "+pkgs/gamma/{entry}", "-pkgs/gamma/{entry}"],
		project: [],
	},
	{
		name: "overrides apply to the scope's auto-discovered directory",
		dirs: ["agent/extensions/alpha", "agent/extensions/beta", "agent/extensions/gamma"],
		user: ["!extensions/*/{entry}", "+extensions/beta/{entry}"],
		project: [],
	},
	{
		name: "- disables one auto-discovered extension; ./ is ignored in exact paths",
		dirs: ["agent/extensions/alpha", "agent/extensions/beta"],
		user: ["-./extensions/alpha/{entry}"],
		project: [],
	},
	{
		name: "project overrides do not reach the profile's extensions",
		dirs: ["agent/extensions/alpha", "cwd/.pi/extensions/local", "cwd/.pi/extensions/other"],
		user: [],
		project: ["!extensions/other/**", "!**/alpha/**"],
	},
	{
		name: "a disabled extension keeps its first place: a later listing does not enable it",
		dirs: ["agent/extensions/alpha", "agent/extensions/beta"],
		user: ["extensions/alpha", "!extensions/alpha/{entry}"],
		project: [],
	},
	{
		name: "a glob by file name matches every entry",
		dirs: ["agent/pkgs/alpha", "agent/pkgs/beta"],
		user: ["pkgs", "!{entry}"],
		project: [],
	},
	{
		name: "case matters",
		dirs: ["agent/pkgs/Alpha", "agent/pkgs/beta"],
		user: ["pkgs", "!pkgs/alpha/**"],
		project: [],
	},
	{
		name: "absolute exact paths and absolute globs",
		dirs: ["agent/pkgs/alpha", "agent/pkgs/beta", "agent/pkgs/gamma"],
		user: ["pkgs", "!<TMP>/agent/pkgs/beta/**", "-<TMP>/agent/pkgs/gamma/{entry}"],
		project: [],
	},
];

async function resolvePatterns(scenario) {
	const tmp = realpathSync(mkdtempSync(join(tmpdir(), "ultron-ext-patterns-")));
	try {
		const agentDir = join(tmp, "agent");
		const cwd = join(tmp, "cwd");
		mkdirSync(join(cwd, ".pi"), { recursive: true });
		mkdirSync(agentDir, { recursive: true });
		for (const dir of scenario.dirs) {
			mkdirSync(join(tmp, dir), { recursive: true });
			writeFileSync(join(tmp, dir, "index.ts"), "export default function () {}\n");
		}
		const entries = (list) => list.map((e) => e.split("<TMP>").join(tmp).split("{entry}").join("index.ts"));
		writeFileSync(join(agentDir, "settings.json"), JSON.stringify({ extensions: entries(scenario.user) }));
		writeFileSync(join(cwd, ".pi", "settings.json"), JSON.stringify({ extensions: entries(scenario.project) }));
		const settingsManager = SettingsManager.create(cwd, agentDir, { projectTrusted: true });
		const manager = new DefaultPackageManager({ cwd, agentDir, settingsManager });
		const resolved = await manager.resolve();
		return resolved.extensions.map((r) => ({ dir: relative(tmp, dirname(r.path)), enabled: r.enabled }));
	} finally {
		rmSync(tmp, { recursive: true, force: true });
	}
}

const patterns = [];
for (const scenario of PATTERN_SCENARIOS) {
	patterns.push({ ...scenario, resolved: await resolvePatterns(scenario) });
}

// -------------------------------------------------------------------------------------------------------- shortcuts

const defaultKeybindings = new KeybindingsManager().getEffectiveConfig();
const pasteImage = [defaultKeybindings["app.clipboard.pasteImage"]].flat()[0];

const SHORTCUT_SCENARIOS = [
	{ name: "reserved built-in key is skipped", extensions: [{ path: "a", keys: ["ctrl+c"] }], overrides: {} },
	{
		name: "a freed reserved default key is allowed",
		extensions: [{ path: "a", keys: ["ctrl+p"] }],
		overrides: { "app.model.cycleForward": "ctrl+n" },
	},
	{ name: "non-reserved built-in key warns but is used", extensions: [{ path: "a", keys: [pasteImage] }], overrides: {} },
	{
		name: "reserved action rebound",
		extensions: [{ path: "a", keys: ["ctrl+x"] }],
		overrides: { "app.interrupt": "ctrl+x" },
	},
	{
		name: "reserved key shared with non-reserved actions",
		extensions: [{ path: "a", keys: ["ctrl+p"] }],
		overrides: {},
	},
	{
		name: "reserved action with several keys",
		extensions: [{ path: "a", keys: ["ctrl+y"] }],
		overrides: { "app.clear": ["ctrl+x", "ctrl+y"] },
	},
	{
		name: "non-reserved action with several keys",
		extensions: [{ path: "a", keys: ["ctrl+y"] }],
		overrides: { "app.clipboard.pasteImage": ["ctrl+x", "ctrl+y"] },
	},
	{
		name: "two extensions, same key: the later wins in the first one's place",
		extensions: [
			{ path: "a", keys: ["ctrl+shift+x", "ctrl+shift+q"] },
			{ path: "b", keys: ["CTRL+SHIFT+X"] },
		],
		overrides: {},
	},
	{
		name: "upper-case registration of a reserved key",
		extensions: [{ path: "a", keys: ["Ctrl+C", "alt+z"] }],
		overrides: {},
	},
	{
		name: "unbound action frees its key",
		extensions: [{ path: "a", keys: ["ctrl+l"] }],
		overrides: { "app.model.select": [] },
	},
];

async function resolveShortcuts(scenario) {
	const runtime = createExtensionRuntime();
	const eventBus = createEventBus();
	const extensions = [];
	for (const spec of scenario.extensions) {
		const factory = (pi) => {
			for (const key of spec.keys) pi.registerShortcut(key, { description: `${spec.path} ${key}`, handler: async () => {} });
		};
		extensions.push(await loadExtensionFromFactory(factory, "/w", eventBus, runtime, spec.path));
	}
	const runner = new ExtensionRunner(extensions, runtime, "/w", {}, {});
	runner.setUIContext(undefined, "interactive");
	runner.hasUI = () => true;
	const keybindings = { ...defaultKeybindings, ...scenario.overrides };
	const shortcuts = runner.getShortcuts(keybindings);
	return {
		keybindings: Object.entries(keybindings).map(([action, keys]) => [action, [keys ?? []].flat()]),
		shortcuts: [...shortcuts.entries()].map(([key, s]) => ({
			key,
			registered: s.shortcut,
			description: s.description,
			extensionPath: s.extensionPath,
		})),
		diagnostics: runner.getShortcutDiagnostics().map((d) => ({ message: d.message, path: d.path })),
	};
}

const shortcuts = [];
for (const scenario of SHORTCUT_SCENARIOS) {
	shortcuts.push({ name: scenario.name, extensions: scenario.extensions, ...(await resolveShortcuts(scenario)) });
}

// ------------------------------------------------------------------------------------------------------------ flags

const FLAG_SCENARIOS = [
	{
		name: "first default wins; getFlag only for the registering extension",
		extensions: [
			{ path: "a", flags: [{ name: "plan", type: "boolean", default: false }, { name: "mode", type: "string", default: "fast" }] },
			{ path: "b", flags: [{ name: "plan", type: "boolean", default: true }, { name: "depth", type: "string" }] },
			{ path: "c", flags: [] },
		],
	},
	{
		name: "an invalid default fails the extension's load",
		extensions: [
			{ path: "a", flags: [{ name: "plan", type: "boolean", default: "yes" }] },
			{ path: "b", flags: [{ name: "level", type: "string", default: 3 }] },
			{ path: "c", flags: [{ name: "ok", type: "string", default: "x" }, { name: "bad", type: "boolean", default: null }] },
			{ path: "d", flags: [{ name: "plan", type: "boolean", default: true }] },
		],
	},
];

async function resolveFlags(scenario) {
	const runtime = createExtensionRuntime();
	const eventBus = createEventBus();
	const loaded = [];
	const errors = [];
	for (const spec of scenario.extensions) {
		const factory = (pi) => {
			for (const flag of spec.flags) {
				const { name, ...options } = flag;
				pi.registerFlag(name, options);
			}
			spec.api = pi;
		};
		try {
			loaded.push({ spec, extension: await loadExtensionFromFactory(factory, "/w", eventBus, runtime, spec.path) });
		} catch (error) {
			errors.push({ path: spec.path, error: error instanceof Error ? error.message : String(error) });
		}
	}
	const names = [...new Set(scenario.extensions.flatMap((e) => e.flags.map((f) => f.name)))];
	const runner = new ExtensionRunner(
		loaded.map((l) => l.extension),
		runtime,
		"/w",
		{},
		{},
	);
	const views = loaded.map(({ spec }) => ({
		path: spec.path,
		flags: Object.fromEntries(names.map((n) => [n, spec.api.getFlag(n) ?? null])),
	}));
	return {
		loaded: loaded.map((l) => l.spec.path),
		errors,
		views,
		values: Object.fromEntries(runner.getFlagValues()),
		registered: [...runner.getFlags().values()].map((f) => ({ name: f.name, type: f.type, extensionPath: f.extensionPath })),
	};
}

const flags = [];
for (const scenario of FLAG_SCENARIOS) {
	const extensions = scenario.extensions.map((e) => ({ path: e.path, flags: e.flags }));
	flags.push({ name: scenario.name, extensions, ...(await resolveFlags(scenario)) });
}

assertOffline();
writeFileSync(join(here, "host.json"), `${JSON.stringify({ globs, patterns, shortcuts, flags }, null, "\t")}\n`);
console.log(
	`wrote ${globs.length} globs, ${patterns.length} pattern, ${shortcuts.length} shortcut, ${flags.length} flag scenarios`,
);
