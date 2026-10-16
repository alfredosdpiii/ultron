// Generates crates/config/fixtures/settings-keys.json: the settings-manager.ts getters for the keys the Rust build
// reads since wave 31 (shellPath, shellCommandPrefix, thinkingBudgets, websocketConnectTimeoutMs, the terminal
// capability overrides, markdown.codeBlockIndent, lastChangelogVersion, warnings, defaultTools) over settings
// objects that cover their defaults, valid and invalid values; and setLastChangelogVersion's file bytes.
// crates/config/tests/settings_keys.rs replays it. HOME points into a temp dir (written as <home>).
// Usage: node crates/config/fixtures/generate-settings-keys.mjs
import "../../cli/fixtures/offline-guard.mjs";
import { mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../../..");
const root = realpathSync(mkdtempSync(join(tmpdir(), "settings-keys-")));
const home = join(root, "fakehome");
mkdirSync(home, { recursive: true });
process.env.HOME = home;
process.env.ULTRON_CODING_AGENT_DIR = join(root, "agent");
const { SettingsManager } = await import(pathToFileURL(join(repo, "packages/coding-agent/src/core/settings-manager.ts")).href);

const clean = (v) => JSON.parse(JSON.stringify(v === undefined ? null : v).split(home).join("<home>"));
const attempt = (f) => {
	try {
		return { value: clean(f()) };
	} catch (error) {
		return { error: error instanceof Error ? error.message : String(error) };
	}
};

const SETTINGS = [
	{},
	{ shellPath: "/usr/local/bin/bash", shellCommandPrefix: "shopt -s expand_aliases" },
	{ shellPath: "~/bin/zsh", shellCommandPrefix: "" },
	{ shellPath: "~", shellCommandPrefix: "set -e\nalias ll='ls -l'" },
	{ shellPath: "", shellPath2: "x" },
	{ shellPath: "file:///opt/sh%20ell/bash" },
	{ thinkingBudgets: { minimal: 512, low: 1024, medium: 4096, high: 16000 } },
	{ thinkingBudgets: { low: 2000 } },
	{ websocketConnectTimeoutMs: 15000 },
	{ websocketConnectTimeoutMs: 0 },
	{ websocketConnectTimeoutMs: 1234.9 },
	{ websocketConnectTimeoutMs: "2500" },
	{ websocketConnectTimeoutMs: " disabled " },
	{ websocketConnectTimeoutMs: "  " },
	{ websocketConnectTimeoutMs: -5 },
	{ websocketConnectTimeoutMs: "soon" },
	{ websocketConnectTimeoutMs: true },
	{ websocketConnectTimeoutMs: null },
	{ websocketConnectTimeoutMs: [1, 2] },
	{ websocketConnectTimeoutMs: { ms: 1 } },
	{ websocketConnectTimeoutMs: 1.5e21 },
	{ terminal: { trueColor: false, hyperlinks: true, images: "kitty" } },
	{ terminal: { trueColor: true, hyperlinks: false, images: "iterm2" } },
	{ terminal: { images: false } },
	{ terminal: { images: true, trueColor: "yes", hyperlinks: 1 } },
	{ terminal: { images: "sixel" } },
	{ terminal: {} },
	{ markdown: { codeBlockIndent: "" } },
	{ markdown: { codeBlockIndent: "\t" } },
	{ markdown: { codeBlockIndent: "    " } },
	{ markdown: { mermaid: "off" } },
	{ lastChangelogVersion: "0.4.2" },
	{ warnings: { anthropicExtraUsage: false } },
	{ warnings: { anthropicExtraUsage: true } },
	{ warnings: {} },
	{ defaultTools: ["read", "bash"] },
	{ defaultTools: [] },
];

const cases = SETTINGS.map((settings) => {
	const m = SettingsManager.inMemory(settings);
	return {
		settings,
		shellPath: attempt(() => m.getShellPath()),
		shellCommandPrefix: attempt(() => m.getShellCommandPrefix()),
		thinkingBudgets: attempt(() => m.getThinkingBudgets()),
		websocketConnectTimeoutMs: attempt(() => m.getWebSocketConnectTimeoutMs()),
		terminalCapabilityOverrides: attempt(() => m.getTerminalCapabilityOverrides()),
		codeBlockIndent: attempt(() => m.getCodeBlockIndent()),
		lastChangelogVersion: attempt(() => m.getLastChangelogVersion()),
		warnings: attempt(() => m.getWarnings()),
		defaultTools: attempt(() => m.getDefaultTools()),
	};
});

// setLastChangelogVersion: written to the global file, merged into what is there; a project value is not touched.
const writes = [];
for (const before of [null, { theme: "dark", lastChangelogVersion: "0.1.0" }]) {
	const dir = mkdtempSync(join(root, "w-"));
	const agent = join(dir, "agent");
	const cwd = join(dir, "cwd");
	mkdirSync(agent, { recursive: true });
	mkdirSync(join(cwd, ".pi"), { recursive: true });
	if (before) writeFileSync(join(agent, "settings.json"), JSON.stringify(before));
	writeFileSync(join(cwd, ".pi", "settings.json"), JSON.stringify({ lastChangelogVersion: "9.9.9" }));
	const m = SettingsManager.create(cwd, agent, { projectTrusted: true });
	const seen = m.getLastChangelogVersion();
	m.setLastChangelogVersion("1.2.3");
	await m.flush();
	writes.push({
		before,
		seen: seen ?? null,
		after: m.getLastChangelogVersion() ?? null,
		global: readFileSync(join(agent, "settings.json"), "utf-8"),
		project: readFileSync(join(cwd, ".pi", "settings.json"), "utf-8"),
	});
}

rmSync(root, { recursive: true, force: true });
writeFileSync(join(here, "settings-keys.json"), `${JSON.stringify({ cases, writes }, null, 1)}\n`);
console.log(`wrote ${cases.length} cases, ${writes.length} writes`);
