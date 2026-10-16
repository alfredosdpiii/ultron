// Records Pi's easter-egg components (packages/coding-agent/src/modes/interactive/components): `/arminsayshi`'s
// ArminComponent (each of its seven effects, picked and played with a seeded `Math.random`, frame by frame through a
// captured `setInterval`) and `/dementedelves`' EarendilAnnouncementComponent (a terminal without image support:
// the image's fallback line). Colors are SGR codes of a fixed table (`COLORS`), so widths are measured as on a
// terminal; the Rust test compares the text and checks the colors against its own theme.
// crates/interactive/tests/easter_eggs.rs replays them. Usage: node crates/interactive/fixtures/easter_eggs.mjs
import "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../../..");
const component = (name) => import(pathToFileURL(join(repo, "packages/coding-agent/src/modes/interactive/components", name)).href);

export const COLORS = ["accent", "muted", "mdLink", "text", "border", "dim", "error"];
globalThis[Symbol.for("@earendil-works/pi-coding-agent:theme")] = {
	fg: (color, text) => `\x1b[38;5;${COLORS.indexOf(color) + 1}m${text}\x1b[39m`,
	bold: (text) => `\x1b[1m${text}\x1b[22m`,
};

/** mulberry32: the seeded generator the Rust test ports. */
export function mulberry32(seed) {
	let a = seed >>> 0;
	return () => {
		a = (a + 0x6d2b79f5) >>> 0;
		let t = a;
		t = Math.imul(t ^ (t >>> 15), t | 1);
		t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
		return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
	};
}

const { ArminComponent } = await component("armin.ts");
const ui = { requestRender: () => {} };
const WIDTHS = [40, 12];
const MAX_TICKS = 5000;
const armin = [];
for (let effect = 0; effect < 7; effect++) {
	const seeded = mulberry32(1000 + effect);
	let first = true;
	Math.random = () => {
		if (first) {
			first = false;
			return (effect + 0.5) / 7;
		}
		return seeded();
	};
	let tick = null;
	let fps = null;
	let stopped = false;
	globalThis.setInterval = (fn, ms) => {
		tick = fn;
		fps = Math.round(1000 / ms);
		return 1;
	};
	globalThis.clearInterval = () => {
		stopped = true;
	};
	const c = new ArminComponent(ui);
	const frames = [WIDTHS.map((w) => c.render(w))];
	let ticks = 0;
	while (!stopped && ticks < MAX_TICKS) {
		tick();
		ticks++;
		frames.push(WIDTHS.map((w) => c.render(w)));
	}
	// Every frame of short effects; first, sampled and last frames of long ones.
	const keep = frames.length <= 40 ? frames.map((_, i) => i) : [0, 1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144, 233, 377, frames.length - 1].filter((i) => i < frames.length);
	armin.push({ effect, seed: 1000 + effect, fps, ticks, ended: stopped, frames: [...new Set(keep)].map((i) => ({ tick: i, lines: frames[i] })) });
}

const { EarendilAnnouncementComponent } = await component("earendil-announcement.ts");
const earendil = WIDTHS.concat([80]).map((width) => ({ width, lines: new EarendilAnnouncementComponent().render(width) }));

writeFileSync(join(here, "easter_eggs.json"), `${JSON.stringify({ colors: COLORS, widths: WIDTHS, armin, earendil }, null, 1)}\n`);
