// Generates crates/tui/fixtures/clipboard-image.json: utils/clipboard-image.ts `readClipboardImage({ env, platform:
// "linux" })` over fake `wl-paste` and `xclip` (scripts on a temp PATH that answer from a scenario directory and log
// each call), for Wayland/X11/WSL environments (`WAYLAND_DISPLAY`, `XDG_SESSION_TYPE`, `WSL_DISTRO_NAME`). Every
// scenario ends before the native-clipboard fallback, so no real clipboard is touched. TIFF (single and multi-page,
// little and big endian, gray/RGB/RGBA, 8/16-bit, PackBits) and ICO (BMP and PNG entries of several sizes and depths)
// images are built here byte by byte; readClipboardImage converts them to PNG with Photon. The scenario table is
// recorded too; crates/tui/tests/clipboard_image.rs replays it through the same fake tools.
// Usage: node crates/tui/fixtures/generate-clipboard-image.mjs
import "../../cli/fixtures/offline-guard.mjs";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { crc32, deflateSync } from "node:zlib";
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
// A 3x2 24-bit bottom-up BMP (rows padded to 4 bytes): an unsupported type that readClipboardImage converts to PNG.
function bmp() {
	const [width, height] = [3, 2];
	const pixels = [
		[[255, 0, 0], [0, 255, 0], [0, 0, 255]],
		[[255, 255, 255], [0, 0, 0], [128, 64, 32]],
	];
	const stride = Math.ceil((width * 3) / 4) * 4;
	const data = Buffer.alloc(54 + stride * height);
	data.write("BM", 0, "latin1");
	data.writeUInt32LE(data.length, 2);
	data.writeUInt32LE(54, 10);
	data.writeUInt32LE(40, 14);
	data.writeInt32LE(width, 18);
	data.writeInt32LE(height, 22);
	data.writeUInt16LE(1, 26);
	data.writeUInt16LE(24, 28);
	data.writeUInt32LE(stride * height, 34);
	for (let y = 0; y < height; y++) {
		const row = 54 + (height - 1 - y) * stride;
		for (let x = 0; x < width; x++) {
			const [r, g, b] = pixels[y][x];
			data[row + x * 3] = b;
			data[row + x * 3 + 1] = g;
			data[row + x * 3 + 2] = r;
		}
	}
	return data.toString("latin1");
}
const BMP = bmp();
// A TIFF of `pages` ({ width, height, spp, bps, photometric, compression, extra, data }): the strips first, then
// every IFD in page order (the header points at the first, each at the next).
function tiff(pages, be = false) {
	const u16 = (v) => {
		const b = Buffer.alloc(2);
		be ? b.writeUInt16BE(v) : b.writeUInt16LE(v);
		return b;
	};
	const u32 = (v) => {
		const b = Buffer.alloc(4);
		be ? b.writeUInt32BE(v) : b.writeUInt32LE(v);
		return b;
	};
	const blocks = [];
	let off = 8;
	const place = (buf) => {
		const at = off;
		blocks.push(buf);
		off += buf.length;
		if (off % 2) {
			blocks.push(Buffer.alloc(1));
			off += 1;
		}
		return at;
	};
	const laid = pages.map((page) => ({
		page,
		dataAt: place(page.data),
		bpsAt: page.spp > 2 ? place(Buffer.concat(Array.from({ length: page.spp }, () => u16(page.bps)))) : 0,
	}));
	const entry = (tag, type, count, value) =>
		Buffer.concat([u16(tag), u16(type), u32(count), type === 3 && count === 1 ? Buffer.concat([u16(value), u16(0)]) : u32(value)]);
	const ifds = laid.map(({ page, dataAt, bpsAt }) => {
		const entries = [
			entry(256, 4, 1, page.width),
			entry(257, 4, 1, page.height),
			page.spp > 2 ? entry(258, 3, page.spp, bpsAt) : page.spp === 2 ? Buffer.concat([u16(258), u16(3), u32(2), u16(page.bps), u16(page.bps)]) : entry(258, 3, 1, page.bps),
			entry(259, 3, 1, page.compression ?? 1),
			entry(262, 3, 1, page.photometric),
			entry(273, 4, 1, dataAt),
			entry(277, 3, 1, page.spp),
			entry(278, 4, 1, page.height),
			entry(279, 4, 1, page.data.length),
			entry(284, 3, 1, 1),
		];
		if (page.extra !== undefined) entries.push(entry(338, 3, 1, page.extra));
		return entries;
	});
	const ifdAt = [];
	for (const entries of ifds) {
		ifdAt.push(off);
		off += 2 + 12 * entries.length + 4;
	}
	const tail = ifds.map((entries, i) => Buffer.concat([u16(entries.length), ...entries, u32(ifdAt[i + 1] ?? 0)]));
	const header = Buffer.concat([Buffer.from(be ? "MM" : "II", "latin1"), u16(42), u32(ifdAt[0])]);
	return Buffer.concat([header, ...blocks, ...tail]).toString("latin1");
}
const bytes = (...values) => Buffer.from(values.flat(Infinity));
const be16 = (values) => values.flatMap((v) => [v >> 8, v & 255]);
// PackBits: a literal run of 3 bytes, then a byte repeated 3 times (header 256 - 2), per row.
const packbits = (rows) => Buffer.concat(rows.map(([a, b, c, d]) => bytes(2, a, b, c, 254, d)));
const TIFF_RGB = tiff([{ width: 3, height: 2, spp: 3, bps: 8, photometric: 2, data: bytes([255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255], [0, 0, 0], [128, 64, 32]) }]);
const TIFF_PAGES = tiff([
	{ width: 2, height: 2, spp: 3, bps: 8, photometric: 2, data: bytes([10, 20, 30], [40, 50, 60], [70, 80, 90], [100, 110, 120]) },
	{ width: 4, height: 1, spp: 3, bps: 8, photometric: 2, data: bytes([200, 0, 0], [0, 200, 0], [0, 0, 200], [9, 9, 9]) },
]);
const TIFF_GRAY_BE = tiff([{ width: 2, height: 2, spp: 1, bps: 8, photometric: 1, data: bytes(0, 85, 170, 255) }], true);
const TIFF_GRAY16 = tiff([{ width: 3, height: 1, spp: 1, bps: 16, photometric: 1, data: bytes(be16([0, 32767, 65535])) }], true);
const TIFF_RGBA = tiff([{ width: 2, height: 1, spp: 4, bps: 8, photometric: 2, extra: 2, data: bytes([255, 0, 0, 128], [0, 0, 255, 0]) }]);
const TIFF_PACKBITS = tiff([{ width: 4, height: 2, spp: 1, bps: 8, photometric: 1, compression: 32773, data: packbits([[1, 2, 3, 4], [5, 6, 7, 8]]) }]);
const TIFF_BAD_FIRST = tiff([
	{ width: 1, height: 1, spp: 1, bps: 8, photometric: 1, compression: 99, data: bytes(1) },
	{ width: 1, height: 1, spp: 1, bps: 8, photometric: 1, data: bytes(2) },
]);
const TIFF_TRUNCATED = TIFF_RGB.slice(0, 20);
// An RGBA PNG (filter 0 rows).
function png(width, height, rgba) {
	const chunk = (type, data) => {
		const body = Buffer.concat([Buffer.from(type, "latin1"), data]);
		const out = Buffer.alloc(8 + data.length + 4);
		out.writeUInt32BE(data.length, 0);
		body.copy(out, 4);
		out.writeUInt32BE(crc32(body), 8 + data.length);
		return out;
	};
	const ihdr = Buffer.alloc(13);
	ihdr.writeUInt32BE(width, 0);
	ihdr.writeUInt32BE(height, 4);
	ihdr.set([8, 6, 0, 0, 0], 8);
	const raw = Buffer.concat(Array.from({ length: height }, (_, y) => Buffer.concat([bytes(0), bytes(rgba.slice(y * width, (y + 1) * width))])));
	return Buffer.concat([Buffer.from("\x89PNG\r\n\x1a\n", "latin1"), chunk("IHDR", ihdr), chunk("IDAT", deflateSync(raw)), chunk("IEND", Buffer.alloc(0))]);
}
// An ICO entry's BMP: a BITMAPINFOHEADER of twice the height, bottom-up BGR(A) rows padded to 4 bytes, then the
// 1-bit AND mask (1 = transparent), rows padded to 4 bytes. `pixels` are [r, g, b, a] rows top-down.
function icoBmp(width, height, bpp, pixels, mask) {
	const stride = Math.ceil((width * bpp) / 32) * 4;
	const maskStride = Math.ceil(width / 32) * 4;
	const out = Buffer.alloc(40 + stride * height + maskStride * height);
	out.writeUInt32LE(40, 0);
	out.writeInt32LE(width, 4);
	out.writeInt32LE(height * 2, 8);
	out.writeUInt16LE(1, 12);
	out.writeUInt16LE(bpp, 14);
	for (let y = 0; y < height; y++) {
		const row = 40 + (height - 1 - y) * stride;
		for (let x = 0; x < width; x++) {
			const [r, g, b, a] = pixels[y * width + x];
			const at = row + x * (bpp / 8);
			out.set(bpp === 32 ? [b, g, r, a] : [b, g, r], at);
		}
		const maskRow = 40 + stride * height + (height - 1 - y) * maskStride;
		for (let x = 0; x < width; x++) if (mask?.[y * width + x]) out[maskRow + (x >> 3)] |= 0x80 >> (x & 7);
	}
	return out;
}
// An ICO of `entries` ({ width, height, bpp, payload }): the header, the directory, then the payloads in order.
function ico(entries) {
	const head = Buffer.alloc(6 + 16 * entries.length);
	head.writeUInt16LE(1, 2);
	head.writeUInt16LE(entries.length, 4);
	let off = head.length;
	entries.forEach((e, i) => {
		const at = 6 + 16 * i;
		head[at] = e.width & 255;
		head[at + 1] = e.height & 255;
		head.writeUInt16LE(1, at + 4);
		head.writeUInt16LE(e.bpp, at + 6);
		head.writeUInt32LE(e.payload.length, at + 8);
		head.writeUInt32LE(off, at + 12);
		off += e.payload.length;
	});
	return Buffer.concat([head, ...entries.map((e) => e.payload)]).toString("latin1");
}
const fill = (n, px) => Array.from({ length: n }, () => px);
const grid = (w, h) => Array.from({ length: w * h }, (_, i) => [i * 20, 255 - i * 20, (i * 47) % 256, 255]);
// Sizes 1x1, 3x2 and a 2x2 PNG entry, all 32-bit: the largest (3x2) wins.
const ICO_SIZES = ico([
	{ width: 1, height: 1, bpp: 32, payload: icoBmp(1, 1, 32, fill(1, [9, 9, 9, 255])) },
	{ width: 3, height: 2, bpp: 32, payload: icoBmp(3, 2, 32, grid(3, 2)) },
	{ width: 2, height: 2, bpp: 32, payload: png(2, 2, grid(2, 2)) },
]);
// A 4x4 24-bit BMP entry before a 2x2 32-bit PNG entry: the deeper one wins over the larger one.
const ICO_DEPTH = ico([
	{ width: 4, height: 4, bpp: 24, payload: icoBmp(4, 4, 24, grid(4, 4)) },
	{ width: 2, height: 2, bpp: 32, payload: png(2, 2, [[1, 2, 3, 4], [5, 6, 7, 8], [9, 10, 11, 12], [13, 14, 15, 16]]) },
]);
// Two 2x2 32-bit entries of different colors: a tie, the last entry wins.
const ICO_TIE = ico([
	{ width: 2, height: 2, bpp: 32, payload: icoBmp(2, 2, 32, fill(4, [255, 0, 0, 255])) },
	{ width: 2, height: 2, bpp: 32, payload: icoBmp(2, 2, 32, fill(4, [0, 0, 255, 255])) },
]);
// A 24-bit entry whose AND mask makes two pixels transparent.
const ICO_MASK = ico([{ width: 2, height: 2, bpp: 24, payload: icoBmp(2, 2, 24, grid(2, 2), [1, 0, 0, 1]) }]);
const ICO_TRUNCATED = ICO_SIZES.slice(0, 30);
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
	"x11-jpeg": { [`xclip.${X_TARGETS}`]: "TARGETS\nimage/jpeg\n", [`xclip.${xType("image/jpeg")}`]: "\xff\xd8\xff\xe0jpeg" },
	"wayland-webp": { [`wl-paste.${WL_LIST}`]: "image/webp\n", [`wl-paste.${wlType("image/webp")}`]: "RIFF\x10\0\0\0WEBPVP8 " },
	"x11-bmp": { [`xclip.${X_TARGETS}`]: "TARGETS\nimage/bmp\n", [`xclip.${xType("image/bmp")}`]: BMP },
	"wayland-bmp": { [`wl-paste.${WL_LIST}`]: "image/x-MS-bmp;foo=1\n", [`wl-paste.${wlType("image/x-MS-bmp;foo=1")}`]: BMP },
	"x11-tiff": { [`xclip.${X_TARGETS}`]: "TARGETS\nimage/tiff\n", [`xclip.${xType("image/tiff")}`]: TIFF_RGB },
	"wayland-tiff-pages": { [`wl-paste.${WL_LIST}`]: "image/tiff\n", [`wl-paste.${wlType("image/tiff")}`]: TIFF_PAGES },
	"x11-tiff-gray-be": { [`xclip.${X_TARGETS}`]: "image/tiff\n", [`xclip.${xType("image/tiff")}`]: TIFF_GRAY_BE },
	"x11-tiff-gray16": { [`xclip.${X_TARGETS}`]: "image/tiff\n", [`xclip.${xType("image/tiff")}`]: TIFF_GRAY16 },
	"x11-tiff-rgba": { [`xclip.${X_TARGETS}`]: "image/tiff\n", [`xclip.${xType("image/tiff")}`]: TIFF_RGBA },
	"x11-tiff-packbits": { [`xclip.${X_TARGETS}`]: "image/tiff\n", [`xclip.${xType("image/tiff")}`]: TIFF_PACKBITS },
	"x11-tiff-bad-first": { [`xclip.${X_TARGETS}`]: "image/tiff\n", [`xclip.${xType("image/tiff")}`]: TIFF_BAD_FIRST },
	"x11-tiff-truncated": { [`xclip.${X_TARGETS}`]: "image/tiff\n", [`xclip.${xType("image/tiff")}`]: TIFF_TRUNCATED },
	"x11-ico-sizes": { [`xclip.${X_TARGETS}`]: "TARGETS\nimage/x-icon\n", [`xclip.${xType("image/x-icon")}`]: ICO_SIZES },
	"wayland-ico-depth": { [`wl-paste.${WL_LIST}`]: "image/vnd.microsoft.icon\n", [`wl-paste.${wlType("image/vnd.microsoft.icon")}`]: ICO_DEPTH },
	"x11-ico-tie": { [`xclip.${X_TARGETS}`]: "image/x-icon\n", [`xclip.${xType("image/x-icon")}`]: ICO_TIE },
	"x11-ico-mask": { [`xclip.${X_TARGETS}`]: "image/x-icon\n", [`xclip.${xType("image/x-icon")}`]: ICO_MASK },
	"x11-ico-truncated": { [`xclip.${X_TARGETS}`]: "image/x-icon\n", [`xclip.${xType("image/x-icon")}`]: ICO_TRUNCATED },
	"x11-tiff-and-png": { [`xclip.${X_TARGETS}`]: "image/tiff\nimage/png\n", [`xclip.${xType("image/png")}`]: PNG },
	"x11-undecodable": { [`xclip.${X_TARGETS}`]: "image/x-unknown\n", [`xclip.${xType("image/x-unknown")}`]: "not an image" },
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
	["x11-no-targets", 3], ["x11-gif-only", 2], ["wayland-png", 5], ["x11-jpeg", 3], ["wayland-webp", 0],
	["x11-bmp", 3], ["wayland-bmp", 0], ["x11-undecodable", 3],
	["x11-tiff", 3], ["wayland-tiff-pages", 0], ["x11-tiff-gray-be", 3], ["x11-tiff-gray16", 3], ["x11-tiff-rgba", 3],
	["x11-tiff-packbits", 3], ["x11-tiff-bad-first", 3], ["x11-tiff-truncated", 3], ["x11-ico-sizes", 3],
	["wayland-ico-depth", 0], ["x11-ico-tie", 3], ["x11-ico-mask", 3], ["x11-ico-truncated", 3], ["x11-tiff-and-png", 3],
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
