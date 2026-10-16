// Generates crates/tui/fixtures/capability-overrides.json: terminal-image.ts `getCapabilities()` after
// `setCapabilityOverrides(settingsManager.getTerminalCapabilityOverrides())` (interactive-mode.ts, startup-ui.ts) for
// terminal environments x the `terminal.*` settings, including the PI_HYPERLINKS / PI_IMAGE_PROTOCOL / PI_TRUE_COLOR
// variables the settings outrank. tmux cases always carry a hyperlinks answer (the setting or PI_HYPERLINKS), so the
// tmux probe never runs. crates/tui/tests/capability_overrides.rs replays it.
// Usage: node crates/tui/fixtures/generate-capability-overrides.mjs
import "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const ti = await import(pathToFileURL(join(root, "packages/tui/src/terminal-image.ts")).href);
const { SettingsManager } = await import(pathToFileURL(join(root, "packages/coding-agent/src/core/settings-manager.ts")).href);

const ENVS = [
	{},
	{ TERM: "xterm-256color" },
	{ TERM: "xterm-256color", COLORTERM: "truecolor" },
	{ TERM_PROGRAM: "kitty" },
	{ TERM_PROGRAM: "ghostty" },
	{ TERM_PROGRAM: "iTerm.app" },
	{ TERM_PROGRAM: "vscode" },
	{ TERM: "screen-256color" },
	{ TMUX: "/tmp/tmux-1/default,1,0", PI_HYPERLINKS: "1" },
	{ TERM_PROGRAM: "kitty", PI_HYPERLINKS: "0", PI_IMAGE_PROTOCOL: "none", PI_TRUE_COLOR: "0" },
	{ TERM: "xterm", PI_IMAGE_PROTOCOL: "iterm2", PI_TRUE_COLOR: "1" },
];
const TERMINAL = [
	undefined,
	{ trueColor: false },
	{ trueColor: true },
	{ hyperlinks: true },
	{ hyperlinks: false },
	{ images: "kitty" },
	{ images: "iterm2" },
	{ images: false },
	{ images: true, trueColor: "x" },
	{ trueColor: true, hyperlinks: true, images: "iterm2" },
];
const VARS = ["TERM", "TERM_PROGRAM", "TERMINAL_EMULATOR", "COLORTERM", "TMUX", "KITTY_WINDOW_ID", "GHOSTTY_RESOURCES_DIR",
	"WEZTERM_PANE", "WARP_SESSION_ID", "WARP_TERMINAL_SESSION_UUID", "ITERM_SESSION_ID", "WT_SESSION", "PI_HYPERLINKS",
	"PI_IMAGE_PROTOCOL", "PI_TRUE_COLOR", "KONSOLE_VERSION", "VTE_VERSION"];
const cases = [];
for (const env of ENVS) {
	for (const terminal of TERMINAL) {
		if (env.TMUX && terminal?.hyperlinks === undefined && env.PI_HYPERLINKS === undefined) continue;
		for (const key of VARS) delete process.env[key];
		Object.assign(process.env, env);
		const settings = terminal === undefined ? {} : { terminal };
		ti.setCapabilityOverrides(SettingsManager.inMemory(settings).getTerminalCapabilityOverrides());
		ti.resetCapabilitiesCache();
		cases.push({ env, settings, capabilities: ti.getCapabilities() });
	}
}
writeFileSync(join(here, "capability-overrides.json"), `${JSON.stringify({ cases }, null, 1)}\n`);
console.log(`wrote ${cases.length} cases`);
