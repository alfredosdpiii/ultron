// Generates crates/tui/fixtures/osc11-parity.json: packages/tui/src/terminal-colors.ts
// `isOsc11BackgroundColorResponse` / `parseOsc11BackgroundColor` over terminal replies to the OSC 11 background query
// (`ESC ] 11 ; ? BEL`): BEL and ST endings, 1- to 5-digit channels, `#` forms and garbage.
// crates/tui/tests/osc11.rs replays it.
// Usage: node crates/tui/fixtures/generate-osc11.mjs
import "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const colors = await import(pathToFileURL(join(here, "../../../packages/tui/src/terminal-colors.ts")).href);

const BODIES = [
	"rgb:0000/0000/0000",
	"rgb:ffff/ffff/ffff",
	"rgb:FFFF/FFFF/FFFF",
	"rgb:1e1e/1e1e/2e2e",
	"rgb:fafa/fafa/fafa",
	"rgb:ff/ff/ff",
	"rgb:80/80/80",
	"rgb:7f/7f/7f",
	"rgb:bc/bc/bc",
	"rgb:bb/bb/bb",
	"rgb:f/0/f",
	"rgb:8/8/8",
	"rgb:fff/000/abc",
	"rgb:1/22/333",
	"rgb:12345/abcde/fffff",
	"RGB:ffff/0000/0000",
	"rgba:ffff/ffff/ffff/ffff",
	"rgba:0000/0000/0000",
	"  rgb:ffff/ffff/ffff  ",
	"ffff/ffff/ffff",
	"#ffffff",
	"#000000",
	"#FaFaFa",
	"#fff",
	"#ffffffffffff",
	"#1e1e1e1e2e2e",
	"#12345",
	"#gggggg",
	"rgb:ffff/ffff",
	"rgb:ffff//ffff",
	"rgb:ggg/000/000",
	"rgb:-1/00/00",
	"rgb:+f/00/00",
	"rgb:ffff/ffff/ffff/",
	"",
	"?",
	"cmy:0/0/0",
];
const ends = ["\x07", "\x1b\\"];
const replies = [];
for (const body of BODIES) for (const end of ends) replies.push(`\x1b]11;${body}${end}`);
replies.push(
	"\x1b]11;rgb:ffff/ffff/ffff",
	"\x1b]10;rgb:ffff/ffff/ffff\x07",
	"\x1b]11;rgb:ffff/ffff/ffff\x07x",
	"x\x1b]11;rgb:ffff/ffff/ffff\x07",
	"\x1b]11;rgb:ffff/ffff/ffff\x1b",
	"\x1b]11;rgb:ff\x1bff/ffff/ffff\x07",
	"\x1b]11;rgb:ff\x07ff/ffff/ffff\x07",
	"\x1b]11;rgb:ffff/ffff/ffff\x07\x1b]11;rgb:0000/0000/0000\x07",
	"\x1b]111;rgb:ffff/ffff/ffff\x07",
	"\x1b]11rgb:ffff/ffff/ffff\x07",
	"\x1b[?997;1n",
	"a",
);
const cases = replies.map((data) => ({
	data,
	is: colors.isOsc11BackgroundColorResponse(data),
	rgb: colors.parseOsc11BackgroundColor(data) ?? null,
}));

writeFileSync(join(here, "osc11-parity.json"), `${JSON.stringify({ cases }, null, 1)}\n`);
console.log(`wrote ${cases.length} cases`);
