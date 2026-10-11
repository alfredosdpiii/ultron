// Generates crates/protocol/fixtures/protocol.json from the TypeScript packages/protocol in this checkout.
// Records CBOR encodings/decodings, frame decoder scripts, and message validation/encoding outcomes; the Rust tests
// replay each case and require identical bytes, values and error messages.
// Usage: node crates/protocol/fixtures/generate.mjs [output.json]
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "../../..");
const p = await import(pathToFileURL(join(root, "packages/protocol/src/index.ts")).href);

const hex = (b) => Buffer.from(b).toString("hex");
const unhex = (h) => new Uint8Array(Buffer.from(h, "hex"));
// Tagged JSON <-> JS values: {"$bytes": hex}, {"$num": "-0" | "NaN" | "Infinity" | "-Infinity"}.
function fromSpec(s) {
	if (Array.isArray(s)) return s.map(fromSpec);
	if (s && typeof s === "object") {
		if ("$bytes" in s) return unhex(s.$bytes);
		if ("$num" in s) return Number(s.$num);
		return Object.fromEntries(Object.entries(s).map(([k, v]) => [k, fromSpec(v)]));
	}
	return s;
}
function toSpec(v) {
	if (v instanceof Uint8Array) return { $bytes: hex(v) };
	if (typeof v === "number" && (Object.is(v, -0) || !Number.isFinite(v))) return { $num: Object.is(v, -0) ? "-0" : String(v) };
	if (Array.isArray(v)) return v.map(toSpec);
	if (v && typeof v === "object") return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, toSpec(x)]));
	return v;
}
const attempt = (f) => {
	try {
		return { ok: f() };
	} catch (e) {
		return { error: `${e.name}: ${e.message}` };
	}
};
const opts = (o) => (o ? { maxByteLength: o.max_byte_length, maxContainerLength: o.max_container_length, maxDepth: o.max_depth } : undefined);

const nest = (n) => {
	let v = null;
	for (let i = 0; i < n; i++) v = [v];
	return v;
};
const encodeValues = [
	[null], [false], [true], [0], [1], [10], [23], [24], [25], [100], [1000], [1_000_000], [1_000_000_000_000],
	[Number.MAX_SAFE_INTEGER], [-1], [-10], [-24], [-25], [-100], [-1000], [-1_000_000], [Number.MIN_SAFE_INTEGER],
	[1.1], [{ $num: "-0" }], [{ $bytes: "01020304" }], [""], ["IETF"], ["ü"], ["水"], ["𐅑"], [[]], [[1, 2, 3]],
	[[1, [2, 3], [4, 5]]], [{ a: 1, b: [2, 3] }], [{ zero: 0, empty: "", no: false, nil: null }], [{ __proto__x: "safe" }],
	[0.5], [-2.75], [1e300], [1.5e-300], [2 ** 32], [2 ** 32 - 1], [65535], [65536], [255], [256], [-256], [-257],
	[{ $num: "NaN" }], [{ $num: "Infinity" }], [{ $num: "-Infinity" }], [Number.MAX_SAFE_INTEGER + 1], [Number.MIN_SAFE_INTEGER - 1],
	[nest(64)], [nest(65)], [[1, 2, 3], { max_byte_length: 16777216, max_container_length: 2, max_depth: 64 }],
	[{ a: 1, b: 2, c: 3 }, { max_byte_length: 16777216, max_container_length: 2, max_depth: 64 }],
	["ab", { max_byte_length: 2, max_container_length: 1000000, max_depth: 64 }],
	["a", { max_byte_length: 2, max_container_length: 1000000, max_depth: 64 }],
	[[1, 2], { max_byte_length: 2, max_container_length: 1000000, max_depth: 64 }],
	[{ $bytes: "0102" }, { max_byte_length: 2, max_container_length: 1000000, max_depth: 64 }],
	[[[[1]]], { max_byte_length: 100, max_container_length: 100, max_depth: 2 }],
	[[[1]], { max_byte_length: 100, max_container_length: 100, max_depth: 2 }],
	["x".repeat(300)], [{ $bytes: "ab".repeat(300) }],
];
const cborEncode = encodeValues.map(([value, o]) => {
	const r = attempt(() => hex(p.encodeCbor(fromSpec(value), opts(o))));
	return { value, options: o ?? null, ...r };
});

const decodeHex = [
	"f6", "f4", "f5", "00", "01", "0a", "17", "1818", "1819", "1864", "1903e8", "1a000f4240", "1b000000e8d4a51000",
	"1b001fffffffffffff", "20", "29", "37", "3818", "3863", "3903e7", "3a000f423f", "3b001ffffffffffffe",
	"fb3ff199999999999a", "fb8000000000000000", "4401020304", "60", "6449455446", "62c3bc", "63e6b0b4", "64f0908591",
	"80", "83010203", "8301820203820405", "a26161016162820203", "63efbbbf", "fb3ff0000000000000", "fbc000000000000000",
	"1800", "190001", "1a00000001", "1b0000000000000001", "",
	"18", "1c", "1d", "1e", "5f", "7f", "9f", "bf", "c000", "f7", "e0", "ff", "f93c00", "fa3f800000", "fb7ff0000000000000",
	"fb7ff8000000000000", "fbfff0000000000000", "fb3ff00000", "44010203", "636162", "8201", "a16161", "0000", "a10102", "a2616101616102",
	"61ff", "62c080", "63eda080", "1b0020000000000000", "3b001fffffffffffff", "fb4340000000000000", "fbc340000000000000",
	"81".repeat(65) + "f6", "81".repeat(64) + "f6",
	"5a01000001", "7a01000001", "9a000f4241", "ba000f4241", "1f", "3f", "5c", "f8", "f818",
];
const cborDecode = decodeHex.map((h) => ({ hex: h, options: null, ...attempt(() => toSpec(p.decodeCbor(unhex(h)))) }));
const small = [
	["83010203", { max_byte_length: 16777216, max_container_length: 2, max_depth: 64 }],
	["626162", { max_byte_length: 2, max_container_length: 1000000, max_depth: 64 }],
	["6161", { max_byte_length: 2, max_container_length: 1000000, max_depth: 64 }],
	["420102", { max_byte_length: 3, max_container_length: 1000000, max_depth: 64 }],
	["818101", { max_byte_length: 100, max_container_length: 100, max_depth: 1 }],
	["8101", { max_byte_length: 100, max_container_length: 100, max_depth: 1 }],
];
for (const [h, o] of small) cborDecode.push({ hex: h, options: o, ...attempt(() => toSpec(p.decodeCbor(unhex(h), opts(o)))) });

function runFrames(max, ops) {
	let decoder;
	const init = attempt(() => {
		decoder = new p.FrameDecoder(max === null ? undefined : { maxFrameLength: max });
		return null;
	});
	if (init.error) return [{ op: "new", ...init }];
	return ops.map((op) =>
		op === "end"
			? { op, ...attempt(() => (decoder.end(), null)) }
			: { op, ...attempt(() => decoder.push(unhex(op)).map(hex)) },
	);
}
const big = Array.from({ length: 1000 }, (_, i) => i % 251);
const bigWire = hex(p.encodeFrame(new Uint8Array(big)));
const frameScripts = [
	[null, ["00000003aabbcc", "00000000", "0000000104", "end"]],
	[null, ["00", "00", "00", "03", "aa", "bb", "cc", "00", "00", "00", "00", "00", "00", "00", "01", "04", "end"]],
	[null, [bigWire.slice(0, 202), bigWire.slice(202, 1082), bigWire.slice(1082), "end"]],
	[null, ["", "end"]],
	[null, ["000000", "end", "00"]],
	[null, ["0000000201", "end", "end"]],
	[3, ["00000004", "01"]],
	[3, ["00000003010203", "end"]],
	[0, ["00000000", "00000001"]],
	[null, ["end", "00"]],
	[null, ["0000000410", "203040", "end"]],
	[2 ** 32, []],
];
const frames = frameScripts.map(([max, ops]) => ({ max, ops, results: runFrames(max, ops) }));
frames.push({ max: null, ops: ["encode:aabbcc"], results: [{ op: "encode:aabbcc", ok: [hex(p.encodeFrame(unhex("aabbcc")))] }] });

const SID = "00000000-0000-4000-8000-000000000001";
const sessionTarget = { serverId: SID, sessionId: "session-1", attachmentId: "attachment-1" };
const listCall = { serviceId: "pi.models", member: "list", args: [] };
const clientValues = [
	{ type: "hello", version: 8 }, { type: "hello", version: 0 }, { type: "hello", version: 9 },
	{ type: "hello", version: "8" }, { type: "hello", version: 8.5 }, { type: "hello", version: -1 }, { type: "hello", version: 8, extra: true },
	{ type: "hello" },
	...["", "server-1", "00000000-0000-7000-8000-000000000001", "00000000-0000-4000-7000-000000000001",
		"00000000-0000-4000-8000-00000000000A", "00000000-0000-4000-c000-000000000001", "00000000-0000-4000-b000-00000000000f", SID + "0"].map(
		(serverId) => ({ type: "request", id: "request-1", target: { serverId }, call: listCall }),
	),
	{ type: "request", id: "request-1", target: sessionTarget,
		call: { serviceId: "application.custom", instance: { key: "instance-1", generation: 2 }, member: "invoke", args: [{ arbitrary: true }, ["opaque"]] } },
	{ type: "request", id: "request-1", target: { serverId: SID }, call: { arbitrary: "strict JSON whose service meaning belongs to Chord" } },
	{ type: "request", id: "request-1", target: { serverId: SID }, call: { ...listCall, args: [{ $bytes: "01" }] } },
	{ type: "request", id: "request-1", target: { serverId: SID }, call: { ...listCall, args: [{ $num: "NaN" }] } },
	{ type: "request", id: "request-1", target: { serverId: SID }, call: { ...listCall, args: [{ $num: "-0" }, 1.5] } },
	{ type: "request", id: "", target: { serverId: SID }, call: listCall },
	{ type: "request", id: "request-1", target: { serverId: SID }, call: listCall, extra: true },
	{ type: "request", id: "request-1", target: { serverId: SID } },
	{ type: "request", id: "request-1", target: { serverId: SID, sessionId: "s" }, call: listCall },
	{ type: "request", id: "request-1", target: { ...sessionTarget, attachmentId: "" }, call: listCall },
	{ type: "request", id: "request-1", target: { serverId: SID, extra: 1 }, call: listCall },
	{ type: "request", id: 5, target: { serverId: SID }, call: listCall },
	{ type: "request", id: "request-1", target: { serverId: SID }, call: null },
	{ type: "cancel", id: "request-1", target: { serverId: SID } },
	{ type: "cancel", id: "request-1", target: sessionTarget },
	{ type: "cancel", id: "", target: { serverId: SID } },
	{ type: "cancel", id: "request-1", target: { serverId: SID }, extra: true },
	{ type: "unknown" }, "{\"type\":\"hello\",\"version\":8}", [], null,
];
const serverValues = [
	{ type: "hello", version: 8, serverId: SID }, { type: "hello", version: 7, serverId: SID },
	{ type: "hello", version: 8, serverId: "server-1" }, { type: "hello", version: 8, serverId: SID, snapshot: {} },
	{ type: "hello_error", error: { code: "version", message: "Unsupported protocol version 7; expected 8" } },
	{ type: "hello_error", error: { code: "", message: "x" } },
	{ type: "hello_error", error: { code: "x" } },
	{ type: "response", id: "request-1", ok: true }, { type: "response", id: "request-1", ok: true, result: [] },
	{ type: "response", id: "request-1", ok: true, result: null },
	{ type: "response", id: "request-1", ok: true, result: { $bytes: "01" } },
	{ type: "response", id: "request-1", ok: true, result: [], extra: true },
	{ type: "response", id: "request-1", ok: false, error: { code: "", message: "bad" } },
	...["wrong_server", "cancelled", "service_not_found", "application_error"].map((code) => ({ type: "response", id: "request-1", ok: false, error: { code, message: "safe" } })),
	{ type: "response", id: "request-1", ok: false, error: { code: "x", message: "y" }, result: 1 },
	{ type: "response", id: "request-1", ok: false },
	{ type: "response", id: "request-1", ok: true, error: { code: "x", message: "y" } },
	{ type: "response", id: "request-1", ok: 1, result: 1 },
	{ type: "service_update", subscriptionId: "subscription-1", update: { applicationDefined: true } },
	{ type: "service_update", subscriptionId: "", update: 1 },
	{ type: "attachment", attachment: sessionTarget }, { type: "attachment", attachment: null },
	{ type: "attachment", attachment: { sessionId: "session-1" } }, { type: "attachment" },
	{ type: "unknown", event: {} },
];
const messages = [
	...clientValues.map((value) => ({ side: "client", value, max: null, ...attempt(() => hex(p.encodeClientMessage(fromSpec(value)))) })),
	...serverValues.map((value) => ({ side: "server", value, max: null, ...attempt(() => hex(p.encodeServerMessage(fromSpec(value)))) })),
	{ side: "client", value: { type: "hello", version: 8 }, max: 8, ...attempt(() => hex(p.encodeClientMessage({ type: "hello", version: 8 }, { maxFrameLength: 8 }))) },
	{ side: "server", value: serverValues[0], max: 8, ...attempt(() => hex(p.encodeServerMessage(fromSpec(serverValues[0]), { maxFrameLength: 8 }))) },
	{ side: "server", value: { type: "response", id: "r", ok: true, result: "x".repeat(600) }, max: 100,
		...attempt(() => hex(p.encodeServerMessage({ type: "response", id: "r", ok: true, result: "x".repeat(600) }, { maxFrameLength: 100 }))) },
];

const helloFrame = hex(p.encodeClientMessage({ type: "hello", version: 8 }));
const reqFrame = hex(p.encodeClientMessage({ type: "request", id: "request-1", target: { serverId: SID }, call: { serviceId: "pi.session-directory", member: "list", args: [] } }));
function runDecoder(side, max, ops) {
	const D = side === "client" ? p.ClientMessageDecoder : p.ServerMessageDecoder;
	const decoder = new D(max === null ? undefined : { maxFrameLength: max });
	return ops.map((op) =>
		op === "end"
			? { op, ...attempt(() => (decoder.end(), null)) }
			: { op, ...attempt(() => decoder.push(unhex(op)).map(toSpec)) },
	);
}
const wire = helloFrame + reqFrame;
const decoderScripts = [
	["client", null, [wire, "end"]],
	["client", null, [wire.slice(0, 14), wire.slice(14), "end"]],
	["client", null, [wire.slice(0, 40), wire.slice(40, 42), wire.slice(42), "end"]],
	["client", null, ["00000000", helloFrame]],
	["client", null, ["00000001ff", helloFrame]],
	["client", null, [hex(p.encodeFrame(p.encodeCbor({ type: "hello", version: 1, extra: true }))), helloFrame]],
	["client", null, [helloFrame + hex(p.encodeFrame(p.encodeCbor({ type: "hello", version: 1, extra: true })))]],
	["server", null, ["0000000201", "end", "end"]],
	["client", 3, ["00000004"]],
	["client", null, [hex(p.encodeFrame(p.encodeCbor({ type: "hello", version: 8 }))) + "00", "end"]],
	["server", null, [hex(p.encodeServerMessage({ type: "hello", version: 8, serverId: SID })) + hex(p.encodeServerMessage({ type: "response", id: "request-1", ok: true, result: [] })), "end"]],
	["server", null, [hex(p.encodeFrame(p.encodeCbor({ type: "response", id: "r", ok: true, result: new Uint8Array([1]) })))]],
];
const decoders = decoderScripts.map(([side, max, ops]) => ({ side, max, ops, results: runDecoder(side, max, ops) }));

const text = `${JSON.stringify({ source: "packages/protocol/src", cborEncode, cborDecode, frames, messages, decoders }, null, 1)}\n`;
writeFileSync(process.argv[2] ?? join(root, "crates/protocol/fixtures/protocol.json"), text);
