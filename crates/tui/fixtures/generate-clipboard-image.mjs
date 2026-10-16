// Generates crates/tui/fixtures/clipboard-image.json: utils/clipboard-image.ts `readClipboardImage({ env, platform:
// "linux" })` over fake `wl-paste` and `xclip` (scripts on a temp PATH that answer from a scenario directory and log
// each call), for Wayland/X11/WSL environments (`WAYLAND_DISPLAY`, `XDG_SESSION_TYPE`, `WSL_DISTRO_NAME`). Every
// scenario ends before the native-clipboard fallback, so no real clipboard is touched. The scenario table is
// recorded too; crates/tui/tests/clipboard_image.rs replays it through the same fake tools.
// Usage: node crates/tui/fixtures/generate-clipboard-image.mjs
import "../../cli/fixtures/offline-guard.mjs";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const tmp = realpathSync(mkdtempSync(join(tmpdir(), "ultron-clipboard-image-")));
const bin = join(tmp, "bin");
mkdirSync(bin);
// The fake tool: the call's key (its arguments joined by "_", "/" as "-") names <scenario>/<tool>.<key>.out; a missing
// file exits 1. Each call is appended to <scenario>/log.
export const FAKE = (tool) =>
	`#!/bin/sh\nkey=$(printf '%s_' "$@" | tr '/' '-')\nprintf '%s %s\\n' "${tool}" "$key" >> "$CLIP_SCENARIO/log"\nf="$CLIP_SCENARIO/${tool}.$key.out"\n[ -f "$f" ] || exit 1\ncat "$f"\n`;
for (const tool of ["wl-paste", "xclip"]) {
	writeFileSync(join(bin, tool), FAKE(tool));
	chmodSync(join(bin, tool), 0o755);
}
const PNG = "\x89PNG\r\n\x1a\nfake-png";
const WL_LIST = "--list-types_";
const wlType = (t) => `--type_${t.replace(/\//g, "-")}_--no-newline_`;
const X_TARGETS = "-selection_clipboard_-t_TARGETS_-o_";
const xType = (t) => `-selection_clipboard_-t_${t.replace(/\//g, "-")}_-o_`;
const SCENARIOS = {
	"wayland-png": { [`wl-paste.${WL_LIST}`]: "text/plain\nimage/png\n", [`wl-paste.${wlType("image/png")}`]: PNG },
	"wayland-jpeg-first": { [`wl-paste.${WL_LIST}`]: "image/jpeg\nimage/png;charset=x\n", [`wl-paste.${wlType("image/png;charset=x")}`]: PNG },
	"wayland-no-image": { [`wl-paste.${WL_LIST}`]: "text/plain\n", [`xclip.${X_TARGETS}`]: "image/png\n", [`xclip.${xType("image/png")}`]: PNG },
	"wayland-empty-data": { [`wl-paste.${WL_LIST}`]: "image/png\n", [`wl-paste.${wlType("image/png")}`]: "", [`xclip.${xType("image/png")}`]: PNG },
	"wayland-broken-x11": { [`xclip.${X_TARGETS}`]: "TARGETS\nimage/png\n", [`xclip.${xType("image/png")}`]: PNG },
	"x11-png": { [`wl-paste.${WL_LIST}`]: "image/png\n", [`xclip.${X_TARGETS}`]: "TARGETS\nimage/png\n", [`xclip.${xType("image/png")}`]: PNG },
	"x11-no-image": { [`xclip.${X_TARGETS}`]: "TARGETS\nUTF8_STRING\n" },
	"x11-no-targets": { [`xclip.${xType("image/jpeg")}`]: "jpeg-bytes" },
	"x11-gif-only": { [`xclip.${X_TARGETS}`]: "image/gif\n", [`xclip.${xType("image/gif")}`]: "GIF89a" },
};
const ENVS = [
	{ WAYLAND_DISPLAY: "wayland-0" },
	{ XDG_SESSION_TYPE: "wayland" },
	{ XDG_SESSION_TYPE: "x11", DISPLAY: ":0" },
	{ DISPLAY: ":0" },
	{ WSL_DISTRO_NAME: "Ubuntu" },
	{ XDG_SESSION_TYPE: "wayland", TERMUX_VERSION: "0.118" },
];
const PAIRS = [
	["wayland-png", 0], ["wayland-png", 1], ["wayland-png", 4], ["wayland-jpeg-first", 1], ["wayland-no-image", 0],
	["wayland-empty-data", 1], ["wayland-broken-x11", 0], ["x11-png", 2], ["x11-png", 3], ["x11-no-image", 3],
	["x11-no-targets", 3], ["x11-gif-only", 2], ["wayland-png", 5],
];
process.env.PATH = `${bin}:/usr/bin:/bin`;
const { readClipboardImage } = await import(pathToFileURL(join(here, "../../../packages/coding-agent/src/utils/clipboard-image.ts")).href);
const cases = [];
for (const [scenario, envIndex] of PAIRS) {
	const dir = join(tmp, `${scenario}-${envIndex}`);
	mkdirSync(dir);
	for (const [name, text] of Object.entries(SCENARIOS[scenario])) writeFileSync(join(dir, `${name}.out`), Buffer.from(text, "latin1"));
	writeFileSync(join(dir, "log"), "");
	process.env.CLIP_SCENARIO = dir;
	const image = await readClipboardImage({ env: ENVS[envIndex], platform: "linux" });
	cases.push({
		scenario,
		env: ENVS[envIndex],
		image: image ? { bytes: Buffer.from(image.bytes).toString("latin1"), mimeType: image.mimeType } : null,
		calls: readFileSync(join(dir, "log"), "utf-8"),
	});
}
rmSync(tmp, { recursive: true, force: true });
writeFileSync(join(here, "clipboard-image.json"), `${JSON.stringify({ scenarios: SCENARIOS, cases }, null, 1)}\n`);
console.log(`wrote ${cases.length} cases`);
