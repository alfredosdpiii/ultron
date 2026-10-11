// Generates crates/tui/fixtures/paste-parity.json by running omp's TypeScript (oh-my-pi): BracketedPasteHandler and
// decodeReencodedPasteControls (packages/tui/src/bracketed-paste.ts), the OSC 5522 EnhancedPasteController
// (packages/coding-agent/src/utils/enhanced-paste.ts) over scripted packet streams, and the pasted-path helpers of
// packages/tui/src/prompt/custom-editor.ts. Those helpers are pure but live in a module with heavy imports, so their
// block (BRACKETED_IMAGE_PATH_REGEX .. extractImagePathFromText) is copied verbatim into a temporary module together
// with omp's isVideoPath.
//
// Usage: OMP_SRC=<oh-my-pi checkout> node crates/tui/fixtures/generate-paste.mjs   (deterministic)
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const omp = resolve(process.env.OMP_SRC ?? join(here, "../../../../../omp-reference-src/oh-my-pi"));
const tui = join(omp, "packages/tui/src");
const dir = mkdtempSync(join(tmpdir(), "paste-parity-"));
const copy = (from, to, edit = (s) => s) => writeFileSync(join(dir, to), edit(readFileSync(from, "utf8")));
copy(join(tui, "bracketed-paste.ts"), "bracketed-paste.ts");
copy(join(omp, "packages/coding-agent/src/utils/enhanced-paste.ts"), "enhanced-paste.ts");
const editorSrc = readFileSync(join(tui, "prompt/custom-editor.ts"), "utf8");
const from = editorSrc.indexOf("const BRACKETED_IMAGE_PATH_REGEX");
const to = editorSrc.indexOf("/**\n * Resolve the {@link EditorTheme}");
if (from < 0 || to < from) throw new Error("path helper block not found in custom-editor.ts");
const video = readFileSync(join(tui, "prompt/video.ts"), "utf8");
const vFrom = video.indexOf("/** Container extensions");
const vTo = video.indexOf("/** A contact-sheet image");
writeFileSync(
	join(dir, "paths.ts"),
	`import * as url from "node:url";
import * as path from "node:path";
const BRACKETED_PASTE_START = "\\x1b[200~";
const BRACKETED_PASTE_END = "\\x1b[201~";
${video.slice(vFrom, vTo)}
${editorSrc.slice(from, to)}`,
);
const load = (f) => import(pathToFileURL(join(dir, f)).href);
const { BracketedPasteHandler, decodeReencodedPasteControls } = await load("bracketed-paste.ts");
const { EnhancedPasteController } = await load("enhanced-paste.ts");
const paths = await load("paths.ts");
rmSync(dir, { recursive: true });

const S = "\x1b[200~";
const E = "\x1b[201~";
const bracketed = [
	["plain text"],
	[`${S}hello${E}`],
	[`${S}hel`, "lo", `${E}rest`],
	[`a${S}b${E}c`],
	[`${S}x${E}${S}y${E}`],
	[`${S}${S}nested${E}`],
	[`${S}one`, `${S}two${E}`],
	["before", `${S}p${E}`, "after"],
	[`${S}multi\nline\r\n${E}`],
	[`${S}${E}`],
	[`${S}é日本${E}tail`],
];
const bracketedLimit = [[`${S}0123456789abcdef`], [`${S}0123`, "4567", `89${E}`]];
const runHandler = (chunks, limit) => {
	const h = new BracketedPasteHandler(limit === undefined ? {} : { byteLimit: limit });
	return chunks.map((c) => h.process(c));
};

const reencoded = [
	"line1\x1b[106;5uline2",
	"a\x1b[27;5;106~b",
	"tab\x1b[105;5u\x1b[73;5u",
	"keep\x1b[49;5u\x1b[27;5;27~",
	"mixed\x1b[109;5u\x1b[27;5;77~end",
	"none",
];

const b64 = (s) => Buffer.from(s, "utf8").toString("base64");
const OSC = "\x1b]5522;";
const pk = (meta, payload, term = "\x1b\\") => `${OSC}${meta}${payload === undefined ? "" : `;${payload}`}${term}`;
const png = b64("image/png");
const jpeg = b64("image/jpeg");
const text = b64("text/plain");
const html = b64("text/html");
const dot = b64(".");
const pw = b64("secret");
const streams = {
	image_with_password: [
		pk(`type=read:status=OK:pw=${pw}`),
		pk(`type=read:status=DATA:mime=${text}`),
		pk(`type=read:status=DATA:mime=${png}`),
		pk("type=read:status=DONE"),
		pk("type=read:status=OK"),
		pk(`type=read:status=DATA:mime=${png}`, b64("image-")),
		pk(`type=read:status=DATA:mime=${png}`, b64("bytes")),
		pk("type=read:status=DONE"),
	],
	text_primary: [
		"plain text",
		pk(`type=read:status=OK:loc=primary:pw=${pw}`),
		pk(`type=read:status=DATA:mime=${text}`),
		pk("type=read:status=DONE"),
		pk("type=read:status=OK"),
		pk(`type=read:status=DATA:mime=${text}`, b64("hello ")),
		pk(`type=read:status=DATA:mime=${text}`, b64("world")),
		pk("type=read:status=DONE", undefined, "\x07"),
	],
	unsupported: [pk("type=read:status=OK"), pk(`type=read:status=DATA:mime=${html}`), pk("type=read:status=DONE")],
	kitty_dot_text: [
		pk(`type=read:status=OK:pw=${pw}`),
		pk(`type=read:status=DATA:mime=${dot}:pw=${pw}`, b64("text/plain\n")),
		pk(`type=read:status=DONE:pw=${pw}`),
		pk("type=read:status=OK"),
		pk(`type=read:status=DATA:mime=${text}`, b64("hello")),
		pk(`type=read:status=DATA:mime=${text}`, b64(" world")),
		pk("type=read:status=DONE"),
	],
	kitty_dot_prefers_image: [
		pk("type=read:status=OK"),
		pk(`type=read:status=DATA:mime=${dot}`, b64("text/plain image/png text/html\n")),
		pk("type=read:status=DONE"),
	],
	jpeg_over_text: [
		pk("type=read:status=OK"),
		pk(`type=read:status=DATA:mime=${text}`),
		pk(`type=read:status=DATA:mime=${jpeg}`),
		pk("type=read:status=DONE"),
		pk("type=read:status=OK"),
		pk(`type=read:status=DATA:mime=${text}`, b64("ignored")),
		pk(`type=read:status=DATA:mime=${jpeg}`, b64("\xff\xd8jpeg")),
		pk("type=read:status=DONE"),
	],
	png_over_jpeg: [
		pk("type=read:status=OK"),
		pk(`type=read:status=DATA:mime=${jpeg}`),
		pk(`type=read:status=DATA:mime=${png}`),
		pk("type=read:status=DONE"),
	],
	empty_read: [
		pk("type=read:status=OK"),
		pk(`type=read:status=DATA:mime=${text}`),
		pk("type=read:status=DONE"),
		pk("type=read:status=OK"),
		pk("type=read:status=DONE"),
	],
	error_status: [pk("type=read:status=OK"), pk("type=read:status=ENOSYS"), pk("type=read:status=DONE")],
	other_types: [pk("type=write:status=OK"), pk("status=OK"), pk("type=read"), `${OSC}type=read:status=OK`],
	dot_without_payload: [
		pk("type=read:status=OK"),
		pk(`type=read:status=DATA:mime=${dot}`),
		pk(`type=read:status=DATA:mime=${text}`),
		pk("type=read:status=DONE"),
	],
	malformed_meta: [pk("type=read:status=OK:=x:noeq:loc=clipboard"), pk(`type=read:status=DATA:mime=${text}`), pk("type=read:status=DONE")],
};
const enhanced = {};
for (const [name, packets] of Object.entries(streams)) {
	const events = [];
	const c = new EnhancedPasteController({
		write: (d) => events.push(["write", d]),
		pasteText: (t) => events.push(["text", t]),
		pasteImage: (img) => events.push(["image", img.data, img.mimeType]),
		showStatus: (m) => events.push(["status", m]),
	});
	const steps = [];
	for (const p of packets) {
		events.length = 0;
		const handled = c.handleInput(p);
		await new Promise((r) => setImmediate(r));
		steps.push({ input: p, handled, events: [...events] });
	}
	enhanced[name] = steps;
}

const pathInputs = [
	"/tmp/screenshot.png",
	"/Users/me/Pictures/photo.jpeg",
	"C:\\Users\\me\\img.gif",
	"/Users/me/Movies/launch.mp4",
	"  /tmp/photo.webp\n",
	"icon.png",
	"/tmp/report.csv",
	"/tmp/notes.txt",
	"see /tmp/screenshot.png",
	"/tmp/a.png /tmp/b.png",
	"",
	"   ",
	"file:///Users/me/Pictures/photo.png",
	"file:///tmp/My%20Shot.png",
	"/Users/me/Desktop/Screenshot 2026-06-25 at 1.23.45 PM.png",
	"~/Pictures/Cleanshot 2026-06-25 at 12.00.png",
	"C:\\Users\\me\\My Pictures\\img with space.jpg",
	"/tmp/a.png /tmp/b shot.png",
	"see /Users/me/Desktop/Screenshot 1.png",
	"'/tmp/quoted file.png'",
	'"/tmp/dq.png" "/tmp/two.jpg"',
	"/tmp/My\\ Photos/x.png",
	"./rel/img.png ../up.gif",
	"https://example.com/a.png",
	"dir/b shot.png",
	"/tmp/a.png\n/tmp/b.png",
	"\\~/home.png",
	"'/tmp/unterminated.png",
	"/tmp/odd dir /sub/x.png",
	"/tmp/x.PNG",
	"/tmp/clip.MOV",
	"~/x.webp ./y.mkv",
	"\\\\server\\share\\pic.png",
	"/tmp/trailing\\",
	"/tmp/My Dir\\ /x.png",
	"/tmp/My Dir /x.png",
];
const pathCases = pathInputs.map((input) => ({
	input,
	paths: paths.extractPastePathsFromText(input) ?? null,
	imagePaths: paths.extractImagePastePathsFromText(input) ?? null,
	imagePath: paths.extractImagePathFromText(input) ?? null,
	bracketed: paths.extractBracketedPastePaths(`${S}${input}${E}`) ?? null,
	bracketedImages: paths.extractBracketedImagePastePaths(`${S}${input}${E}`) ?? null,
}));

const out = {
	source: "oh-my-pi bracketed-paste.ts, enhanced-paste.ts, prompt/custom-editor.ts path helpers",
	bracketed: bracketed.map((chunks) => ({ chunks, limit: null, results: runHandler(chunks) })),
	bracketedLimit: bracketedLimit.map((chunks) => ({ chunks, limit: 8, results: runHandler(chunks, 8) })),
	reencoded: reencoded.map((input) => ({ input, output: decodeReencodedPasteControls(input) })),
	enhanced,
	paths: pathCases,
};
writeFileSync(join(here, "paste-parity.json"), `${JSON.stringify(out, null, 1)}\n`);
console.log(`${pathCases.length} path cases, ${Object.keys(enhanced).length} 5522 streams`);
