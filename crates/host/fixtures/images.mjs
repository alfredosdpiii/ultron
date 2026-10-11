// Generates crates/host/fixtures/images.json: the TypeScript view_image host side (ultron/rlm/view-image.ts
// CellImages.attach over utils/image-process.ts, Photon resize) on a set of images made with Photon or by hand.
// Inputs are stored (base64) with the reply or error, the attached image's mime type, dimensions and whether its
// bytes are the input's. Deterministic. Usage: node crates/host/fixtures/images.mjs
import { createHash } from "node:crypto";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const src = join(resolve(here, "../../.."), "packages/coding-agent/src");
const load = (path) => import(pathToFileURL(join(src, path)).href);
const { CellImages } = await load("ultron/rlm/view-image.ts");
const { loadPhoton } = await load("utils/photon.ts");
const { getImageDimensions } = await import(pathToFileURL(join(src, "../../tui/src/terminal-image.ts")).href);
const photon = await loadPhoton();

function pixels(width, height) {
	const raw = new Uint8Array(width * height * 4);
	for (let y = 0; y < height; y++)
		for (let x = 0; x < width; x++) {
			const i = (y * width + x) * 4;
			raw[i] = (x * 7) & 255;
			raw[i + 1] = (y * 3) & 255;
			raw[i + 2] = ((x >> 4) * 40) & 255;
			raw[i + 3] = 255;
		}
	return new photon.PhotonImage(raw, width, height);
}
const png = (w, h) => Buffer.from(pixels(w, h).get_bytes());
const jpeg = (w, h) => Buffer.from(pixels(w, h).get_bytes_jpeg(80));
const webp = (w, h) => Buffer.from(pixels(w, h).get_bytes_webp());
// A 3x2 GIF89a and a 2x2 24-bit BMP, by hand.
const gif = Buffer.from("R0lGODlhAwACAPAAAP8AAAAAACH5BAAAAAAALAAAAAADAAIAAAIChF8AOw==", "base64");
const bmp = Buffer.alloc(70);
bmp.write("BM", 0); bmp.writeUInt32LE(70, 2); bmp.writeUInt32LE(54, 10); bmp.writeUInt32LE(40, 14);
bmp.writeInt32LE(2, 18); bmp.writeInt32LE(2, 22); bmp.writeUInt16LE(1, 26); bmp.writeUInt16LE(24, 28); bmp.writeUInt32LE(16, 34);
for (let i = 54; i < 70; i++) bmp[i] = (i * 37) & 255;

const model = (resize, input = ["text", "image"]) => ({ provider: "faux", id: "m1", input, inputLimits: resize ? { images: { resize } } : undefined });
const cases = [
	{ name: "small png", bytes: png(10, 7) },
	{ name: "small jpeg", bytes: jpeg(40, 30) },
	{ name: "webp", bytes: webp(16, 9) },
	{ name: "gif", bytes: gif },
	{ name: "bmp converted", bytes: bmp },
	{ name: "wide png downscaled", bytes: png(2400, 300) },
	{ name: "tall png downscaled", bytes: png(100, 2600) },
	{ name: "low detail", bytes: png(1024, 512), detail: "low" },
	{ name: "low detail small", bytes: png(300, 200), detail: "low" },
	{ name: "model limit", bytes: png(900, 450), options: { model: model({ maxWidth: 600, maxHeight: 600 }) } },
	{ name: "model limit low", bytes: png(900, 450), detail: "low", options: { model: model({ maxWidth: 300 }) } },
	{ name: "no resize setting", bytes: png(2400, 300), options: { autoResizeImages: false } },
	{ name: "non vision model", bytes: png(4, 4), options: { model: model(undefined, ["text"]) } },
	{ name: "base64 over budget", bytes: png(10, 7), options: { model: model({ maxBytes: 200 }) } },
	{ name: "too small budget", bytes: png(64, 64), options: { model: model({ maxBytes: 1 }) } },
	{ name: "not an image", bytes: Buffer.from("hello, not an image") },
	{ name: "truncated png", bytes: png(10, 7).subarray(0, 40) },
	{ name: "missing path", bytes: null },
];
const dir = mkdtempSync(join(tmpdir(), "ultron-image-fixture-"));
const out = [];
try {
	for (const [index, test] of cases.entries()) {
		const path = join(dir, `img-${index}`);
		if (test.bytes) writeFileSync(path, test.bytes);
		const images = new CellImages(test.options ?? {});
		const record = {
			name: test.name,
			input: test.bytes ? test.bytes.toString("base64") : null,
			detail: test.detail ?? null,
			auto_resize: test.options?.autoResizeImages ?? true,
			resize: test.options?.model?.inputLimits?.images?.resize ?? null,
			vision: test.options?.model ? test.options.model.input.includes("image") : null,
		};
		try {
			const reply = await images.attach(test.detail ? { path, detail: test.detail } : { path });
			const image = images.images[0];
			const shown = getImageDimensions(image.data, image.mimeType);
			const bytes = Buffer.from(image.data, "base64");
			out.push({ ...record, reply, mime: image.mimeType, width: shown?.widthPx ?? null, height: shown?.heightPx ?? null,
				same_bytes: test.bytes.equals(bytes), content: images.content("out") .map((c) => c.type === "text" ? c.text : c.mimeType) });
		} catch (error) {
			out.push({ ...record, error: error.message.split(dir).join("<dir>") });
		}
	}
	// The per-cell limit: the ninth image is refused.
	const images = new CellImages({});
	const path = join(dir, "limit");
	writeFileSync(path, png(2, 2));
	const replies = [];
	for (let i = 0; i < 9; i++) {
		try { replies.push((await images.attach({ path })).description); } catch (error) { replies.push(`error: ${error.message}`); }
	}
	out.push({ name: "per cell limit", input: png(2, 2).toString("base64"), replies });
} finally {
	rmSync(dir, { recursive: true, force: true });
}
writeFileSync(join(here, "images.json"), `${JSON.stringify({ cases: out }, null, "\t")}\n`);
console.log(createHash("sha256").update(JSON.stringify(out)).digest("hex"));
