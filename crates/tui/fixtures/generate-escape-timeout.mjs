// Generates crates/tui/fixtures/escape-timeout.json: packages/tui/src/terminal.ts `resolveEscapeTimeoutMs(env)`, how
// long a lone ESC waits for the rest of a sequence (PI_TUI_ESC_TIMEOUT, else longer over SSH, else 10 ms), over
// environments. crates/tui/tests/escape_timeout.rs replays it.
// Usage: node crates/tui/fixtures/generate-escape-timeout.mjs
import "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const { resolveEscapeTimeoutMs } = await import(pathToFileURL(join(here, "../../../packages/tui/src/terminal.ts")).href);
const ENVS = [
	{},
	{ PI_TUI_ESC_TIMEOUT: "50" },
	{ PI_TUI_ESC_TIMEOUT: " 75 " },
	{ PI_TUI_ESC_TIMEOUT: "12.5" },
	{ PI_TUI_ESC_TIMEOUT: "0" },
	{ PI_TUI_ESC_TIMEOUT: "-5" },
	{ PI_TUI_ESC_TIMEOUT: "" },
	{ PI_TUI_ESC_TIMEOUT: "abc" },
	{ PI_TUI_ESC_TIMEOUT: "Infinity" },
	{ PI_TUI_ESC_TIMEOUT: "1e2" },
	{ PI_TUI_ESC_TIMEOUT: "0x20" },
	{ SSH_CONNECTION: "10.0.0.1 5555 10.0.0.2 22" },
	{ SSH_TTY: "/dev/pts/3" },
	{ SSH_TTY: "" },
	{ SSH_CONNECTION: "x", PI_TUI_ESC_TIMEOUT: "30" },
	{ SSH_CONNECTION: "x", PI_TUI_ESC_TIMEOUT: "0" },
];
const cases = ENVS.map((env) => ({ env, ms: resolveEscapeTimeoutMs(env) }));
writeFileSync(join(here, "escape-timeout.json"), `${JSON.stringify({ cases }, null, 1)}\n`);
console.log(`wrote ${cases.length} cases`);
