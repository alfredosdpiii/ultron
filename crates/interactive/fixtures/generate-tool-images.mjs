// Records crates/interactive/fixtures/tool-images.json: tool results with images drawn inline on Kitty, as the
// client TUI's chat draws them (`ToolExecutionComponent(name, id, args, {}, renderers)`, `setArgsComplete`,
// `updateResult`; the rlm renderers for `rlm`, none for other tools), dark truecolor, at two widths, with Kitty image
// ids fixed (`Math.random` stubbed); plus `convertToPng` of JPEG (with and without an EXIF rotation) and WebP
// sources, whose results Kitty draws once converted. The images are made with photon (the converter TypeScript
// uses). crates/interactive/tests/tool_images.rs replays it.
// Usage: node crates/interactive/fixtures/generate-tool-images.mjs
process.env.FORCE_COLOR = "3";
delete process.env.PI_EXPERIMENTAL;
process.env.HOME = "/home/u";
import "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const ca = (p) => import(join(here, "../../../packages/coding-agent/src", p));
const T = await ca("modes/interactive/theme/theme.ts");
const { validateThemeJson } = await ca("modes/interactive/theme/theme-json.ts");
T.setThemeJsonValidator(validateThemeJson);
const { KeybindingsManager } = await ca("core/keybindings.ts");
const tuiUrl = pathToFileURL(createRequire(join(here, "../../../packages/coding-agent/package.json")).resolve("@ultron/tui")).href;
const { setKeybindings, setCapabilities } = await import(tuiUrl);
const { ToolExecutionComponent } = await ca("modes/interactive/components/tool-execution.ts");
const { rlmToolRenderers } = await ca("experimental/rlm-tool-renderer.ts");
const { convertToPng } = await ca("utils/image-convert.ts");
const { loadPhoton } = await ca("utils/photon.ts");

setKeybindings(new KeybindingsManager({}));
T.setThemeInstance(T.loadThemeFromPath(join(here, "../../../packages/coding-agent/src/modes/interactive/theme/dark.json"), "truecolor"));
setCapabilities({ images: "kitty", trueColor: true, hyperlinks: false });
Math.random = () => 0.5;

const photon = await loadPhoton();
const W = 6;
const H = 4;
const rgba = new Uint8Array(W * H * 4);
for (let y = 0; y < H; y++) {
	for (let x = 0; x < W; x++) {
		const i = (y * W + x) * 4;
		rgba.set([x < 3 ? 255 : 0, y < 2 ? 255 : 0, (x + y) % 2 ? 255 : 0, 255], i);
	}
}
const source = () => new photon.PhotonImage(rgba, W, H);
const b64 = (bytes) => Buffer.from(bytes).toString("base64");
const png = b64(source().get_bytes());
const jpegBytes = source().get_bytes_jpeg(100);
const jpeg = b64(jpegBytes);
// The JPEG with an APP1 EXIF block saying "rotate 90° clockwise" (orientation 6).
const exif = Buffer.from([
	0xff, 0xe1, 0x00, 0x22, 0x45, 0x78, 0x69, 0x66, 0x00, 0x00, 0x4d, 0x4d, 0x00, 0x2a, 0x00, 0x00, 0x00, 0x08, 0x00,
	0x01, 0x01, 0x12, 0x00, 0x03, 0x00, 0x00, 0x00, 0x01, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
]);
const rotated = b64(Buffer.concat([Buffer.from(jpegBytes.slice(0, 2)), exif, Buffer.from(jpegBytes.slice(2))]));
const webp = b64(source().get_bytes_webp());

const ui = { requestRender() {} };
const text = (t) => ({ type: "text", text: t });
const image = (data, mimeType) => ({ type: "image", data, mimeType });
const cases = [
	{ name: "rlm-png", tool: "rlm", args: { code: "show()" }, content: [text("plot"), image(png, "image/png")] },
	{ name: "rlm-two-images", tool: "rlm", args: { code: "show(2)" }, content: [image(png, "image/png"), text("two"), image(png, "image/png")] },
	{ name: "rlm-image-only", tool: "rlm", args: { code: "img" }, content: [image(png, "image/png")] },
	{ name: "view-png", tool: "view", args: { path: "a.png" }, content: [image(png, "image/png"), text("with image")] },
	{ name: "rlm-image-without-data", tool: "rlm", args: { code: "x" }, content: [text("meta"), image("", "image/png"), { type: "image", data: png }] },
	{ name: "rlm-jpeg", tool: "rlm", args: { code: "jpg" }, content: [text("photo"), image(jpeg, "image/jpeg")] },
	{ name: "rlm-jpeg-rotated", tool: "rlm", args: { code: "rot" }, content: [image(rotated, "image/jpeg")] },
	{ name: "view-webp", tool: "view", args: { path: "a.webp" }, content: [image(webp, "image/webp")] },
	{ name: "rlm-undecodable", tool: "rlm", args: { code: "bad" }, content: [text("bad"), image("bm90IGFuIGltYWdl", "image/jpeg")] },
];
const WIDTHS = [40, 80];
const recorded = [];
for (const c of cases) {
	const result = { content: c.content, isError: false };
	const components = WIDTHS.map(() => {
		const t = new ToolExecutionComponent(c.tool, "c1", c.args, {}, c.tool === "rlm" ? rlmToolRenderers : undefined, ui, "/tmp");
		t.setArgsComplete();
		t.updateResult(result);
		return t;
	});
	const before = components.map((t, i) => t.render(WIDTHS[i]));
	// Let every conversion the components started finish (the same conversions, started after theirs).
	const converted = [];
	for (const block of c.content) {
		if (block.type !== "image" || !block.data || !block.mimeType || block.mimeType === "image/png") continue;
		converted.push((await convertToPng(block.data, block.mimeType))?.data ?? null);
	}
	await new Promise((resolve) => setTimeout(resolve, 50));
	const after = components.map((t, i) => t.render(WIDTHS[i]));
	const hl = c.tool === "rlm" ? { input: c.args.code, lines: T.highlightCode(c.args.code, "python") } : null;
	recorded.push({ ...c, widths: WIDTHS, hl, before, after, converted });
}
writeFileSync(join(here, "tool-images.json"), `${JSON.stringify({ imageId: Math.floor(0.5 * 0xfffffffe) + 1, cases: recorded }, null, 1)}\n`);
console.log(`wrote ${recorded.length} cases`);
