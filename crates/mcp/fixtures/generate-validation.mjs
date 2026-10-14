// Generates crates/mcp/fixtures/validation.json from TypeScript: prepareToolArguments (extension-tools.ts, the
// validation and coercion every REPL tool call goes through) against the `mcp` gateway's schema, and the V8 JSON.parse
// messages the gateway reports for an invalid `args` string ("Error: <message>").
// Usage: node crates/mcp/fixtures/generate-validation.mjs   (from the repository root)
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const base = mkdtempSync(join(tmpdir(), "mcp-validation-fixture-"));
process.env.ULTRON_CODING_AGENT_DIR = join(base, "agent");
process.env.ULTRON_SERVER_DIR = join(base, "server");
const imp = (p) => import(pathToFileURL(join(root, p)).href);
const { prepareToolArguments } = await imp("packages/coding-agent/src/ultron/rlm/extension-tools.ts");
const { createMcpExtension } = await imp("packages/coding-agent/src/extensions/mcp/index.ts");

let tool;
createMcpExtension({ servers: [{ name: "s", config: { command: "x" }, source: "test" }], cwd: base, agentDir: base, projectTrusted: false })({
	registerTool: (definition) => (tool = definition),
	registerCommand: () => {},
	on: () => {},
});

const PARAMS = [
	{},
	{ tool: "t" },
	{ limit: "5", search: "x" },
	{ limit: " 7 ", search: "x" },
	{ limit: "abc", search: "x" },
	{ limit: "", search: "x" },
	{ limit: true, search: "x" },
	{ limit: null, search: "x" },
	{ limit: [1], search: "x" },
	{ limit: "1e2", search: "x" },
	{ limit: "0x10", search: "x" },
	{ limit: "Infinity", search: "x" },
	{ tool: 5 },
	{ tool: true },
	{ tool: null },
	{ tool: ["a"] },
	{ tool: { a: 1 } },
	{ tool: 1.5 },
	{ tool: "t", args: { a: 1 } },
	{ tool: "t", args: "{}" },
	{ tool: "t", args: 5 },
	{ tool: "t", args: true },
	{ tool: "t", args: null },
	{ tool: "t", args: [1, 2] },
	{ server: 3, describe: false },
	{ resources: "x" },
	{ resources: 0 },
	{ read: null, server: "s" },
	{ connect: null, extra: 1 },
	{ search: null },
	{ search: 12 },
	{ instructions: 1, connect: 2 },
	{ limit: "3", tool: null, server: null },
	{ limit: {} },
	{ resources: null },
	{ limit: "abc", tool: ["a"], args: [1] },
	{ tool: ["a"], limit: "abc" },
	{ limit: 1e21, tool: 1e21 },
	{ tool: -0 },
	{ tool: 0.1 },
	{ tool: 123456789.125 },
	{ limit: "-5", search: "x" },
	{ limit: "+5", search: "x" },
	{ limit: ".5", search: "x" },
	{ limit: "5.", search: "x" },
	{ limit: "0b11", search: "x" },
	{ limit: "0o17", search: "x" },
	{ limit: "1_000", search: "x" },
	{ limit: "\t8\n", search: "x" },
	{ limit: false, search: "x" },
	{ args: false, tool: "t" },
	{ args: 0, tool: "t" },
	{ describe: 1e-7 },
	{ describe: 123e-20 },
	{ describe: 2 ** 53 },
];
const prepared = PARAMS.map((params) => {
	try {
		return { params, ok: prepareToolArguments(tool, structuredClone(params)) };
	} catch (error) {
		return { params, err: error.message };
	}
});

// Schemas of other tools (`required`, `enum`, nested objects and arrays), as TypeBox checks them.
const SCHEMAS = [
	{
		type: "object",
		properties: {
			mode: { type: "string", enum: ["fast", "slow"] },
			level: { type: "number", enum: [1, 2, 3] },
			name: { type: "string" },
			nested: {
				type: "object",
				properties: { id: { type: "string" }, kind: { enum: ["a", "b"] } },
				required: ["id"],
			},
			list: { type: "array", items: { type: "object", properties: { x: { type: "number" } }, required: ["x"] } },
			mixed: { enum: ["on", 1, true, null] },
		},
		required: ["name", "mode"],
	},
	{ type: "object", properties: { a: { type: "string" }, b: { type: "string" } }, required: ["a", "b"] },
];
const SCHEMA_PARAMS = [
	[0, { name: "n", mode: "fast" }],
	[0, {}],
	[0, { mode: "fast" }],
	[0, { name: "n", mode: "medium" }],
	[0, { name: "n", mode: "fast", level: "2" }],
	[0, { name: "n", mode: "fast", level: 4 }],
	[0, { name: "n", mode: "fast", level: "9" }],
	[0, { name: 5, mode: "slow" }],
	[0, { name: "n", mode: "fast", nested: {} }],
	[0, { name: "n", mode: "fast", nested: { id: "i", kind: "c" } }],
	[0, { name: "n", mode: "fast", nested: { id: 3, kind: "a" } }],
	[0, { name: "n", mode: "fast", list: [{ x: 1 }, {}, { x: "2" }] }],
	[0, { name: "n", mode: "fast", list: [{ x: "nope" }] }],
	[0, { name: "n", mode: "fast", mixed: "on" }],
	[0, { name: "n", mode: "fast", mixed: false }],
	[0, { name: "n", mode: "fast", mixed: null }],
	[0, { name: null, mode: "fast" }],
	[0, { name: "n", mode: null }],
	[0, { name: "n", mode: 5 }],
	[1, {}],
	[1, { b: "x" }],
	[1, { a: null, b: null }],
	[0, { mode: "bad" }],
	[0, { name: "n", mode: ["x"] }],
	[0, { mode: "bad", name: ["q"], level: 7, nested: { kind: "z" } }],
	[0, { name: "n", mode: "fast", mixed: 1 }],
	[0, { name: "n", mode: "fast", mixed: "1" }],
];
// The bounds, length, pattern, format, item-count and closed-object keywords.
SCHEMAS.push(
	{
		type: "object",
		properties: {
			n: { type: "number", minimum: 1, maximum: 10 },
			x: { type: "number", exclusiveMinimum: 0, exclusiveMaximum: 5 },
			i: { type: "integer", minimum: 0, multipleOf: 2 },
			s: { type: "string", minLength: 2, maxLength: 4 },
			p: { type: "string", pattern: "^[a-z]+-\\d+$" },
			e: { type: "string", format: "email" },
			u: { type: "string", format: "uri" },
			d: { type: "string", format: "date-time" },
			l: { type: "array", minItems: 1, maxItems: 2, items: { type: "string" } },
			q: { type: "array", uniqueItems: true },
			o: { type: "object", properties: { k: { type: "string" } }, additionalProperties: false },
			m: { type: "object", minProperties: 1, maxProperties: 2 },
			c: { const: "fixed" },
		},
		additionalProperties: false,
	},
	{ type: "object", properties: { a: { type: "string" } }, additionalProperties: { type: "number" } },
);
SCHEMA_PARAMS.push(
	[2, { n: 5, x: 1, i: 4, s: "abc", p: "ab-12", l: ["a"], o: { k: "v" } }],
	[2, { n: 0 }],
	[2, { n: 11 }],
	[2, { n: 1, x: 5 }],
	[2, { x: 0 }],
	[2, { n: "0" }],
	[2, { i: 3 }],
	[2, { i: 2.5 }],
	[2, { i: -2 }],
	[2, { s: "a" }],
	[2, { s: "abcde" }],
	[2, { s: "\u{1F600}\u{1F600}" }],
	[2, { p: "AB-1" }],
	[2, { p: "ab-x" }],
	[2, { e: "nobody" }],
	[2, { e: "a@b.co" }],
	[2, { u: "not a uri" }],
	[2, { u: "https://x.y/z" }],
	[2, { d: "yesterday" }],
	[2, { d: "2024-01-02T03:04:05Z" }],
	[2, { l: [] }],
	[2, { l: ["a", "b", "c"] }],
	[2, { l: [1] }],
	[2, { q: [1, 1] }],
	[2, { q: [1, 2] }],
	[2, { o: { k: "v", z: 1 } }],
	[2, { extra: 1 }],
	[2, { extra: 1, other: 2, n: 0 }],
	[2, { m: {} }],
	[2, { m: { a: 1, b: 2, c: 3 } }],
	[2, { c: "fixed" }],
	[2, { c: "other" }],
	[2, { s: 12345 }],
	[2, { n: 0, s: "a", l: [], extra: true }],
	[3, { a: "x", b: 1 }],
	[3, { a: "x", b: "2" }],
	[3, { a: "x", b: "nope" }],
);
SCHEMAS.push(
	{
		type: "object",
		properties: { a: { type: "string" }, t: { type: ["string", "number"] }, f: false, ok: true },
		patternProperties: { "^x-": { type: "number" } },
		required: ["a"],
		additionalProperties: false,
		minProperties: 2,
	},
	{
		type: "object",
		properties: {
			s: { type: "string", minLength: 3, maxLength: 3 },
			q: { type: "array", uniqueItems: true },
			c: { const: { k: [1, 2] } },
			n: { type: "number", multipleOf: 0.1 },
			b: { type: "number", minimum: 1.5, exclusiveMaximum: 1e21 },
		},
	},
);
SCHEMA_PARAMS.push(
	[4, {}],
	[4, { a: "x", z: 1 }],
	[4, { a: "x", "x-1": 2 }],
	[4, { a: "x", "x-1": "two" }],
	[4, { a: "x", f: 1 }],
	[4, { a: "x", t: true }],
	[4, { a: "x", t: [1] }],
	[4, { a: "x", ok: [1] }],
	[4, { a: 1 }],
	[4, { 2: "n", 1: "m", a: "x" }],
	[5, { s: "\u{1F600}\u{1F600}\u{1F600}" }],
	[5, { s: "e\u0301e\u0301" }],
	[5, { s: "e\u0301e\u0301e\u0301" }],
	[5, { s: "\u{1F468}\u200D\u{1F469}\u200D\u{1F467}ab" }],
	[5, { s: "\u{1F1FA}\u{1F1F8}ab" }],
	[5, { s: "ab" }],
	[5, { s: "abcd" }],
	[5, { q: [{ a: 1, b: 2 }, { b: 2, a: 1 }] }],
	[5, { q: [1, "1", true, null, [1], { a: 1 }] }],
	[5, { q: [1, 1.0] }],
	[5, { c: { k: [1, 2] } }],
	[5, { c: { k: [2, 1] } }],
	[5, { n: 0.3 }],
	[5, { n: 0.35 }],
	[5, { b: 1 }],
	[5, { b: 1e22 }],
);
// Every format TypeBox registers, with values it accepts and rejects.
const FORMAT_VALUES = {
	"date-time": ["2024-01-02T03:04:05Z", "2024-01-02t03:04:05.5+01:00", "2024-02-30T00:00:00Z", "2024-01-02T03:04:05", "2016-12-31T23:59:60Z", "2016-12-31T22:59:60-01:00", "2024-01-02 03:04:05Z"],
	date: ["2024-02-29", "2023-02-29", "2024-13-01", "24-01-01"],
	time: ["03:04:05Z", "03:04:05", "24:00:00Z", "23:59:59.123-05:30", "12:00:00+24:00"],
	duration: ["P1Y2M3DT4H5M6S", "PT1H", "P1W", "P", "P1H", "PT"],
	email: ["a@b.co", "A.B+c@ex-ample.com", "\"q d\"@x.org", "a@[127.0.0.1]", "a@b", "a@-b.com", "nobody", "a@b..c"],
	hostname: ["example.com", "a-b.c", "-a.com", "a.com.", "xn--bcher-kva.de", "ab--c.com", "a_b.com", "a".repeat(64) + ".com"],
	"idn-hostname": ["b\u00fccher.de", "\u00e9xample.com", "a\u00b7l.com", "l\u00b7l.com", "\u05d0\u05d1.com", "-x.com", "\u0640x.com", "\u30fbx", "\u30a2\u30fb\u30a2"],
	"idn-email": ["\u00fc@b\u00fccher.de", "a@b", "a@-b.de", "x@y-.de"],
	ipv4: ["127.0.0.1", "256.1.1.1", "1.2.3", "01.2.3.4"],
	ipv6: ["::1", "2001:db8::8a2e:370:7334", "::ffff:1.2.3.4", "1:2:3:4:5:6:7:8:9", "gggg::1"],
	iri: ["https://ex.com/p\u00e4th", "http://[v1.fe]/x", "no scheme", "http://x/a b", "http://x/%zz"],
	"iri-reference": ["/relative/path", "//host/p", "a://b", "http//bad", "x y"],
	"json-pointer": ["/a/b", "", "/a~1b", "/a~2", "a"],
	"json-pointer-uri-fragment": ["#/a/b", "#", "#/a%20b", "/a", "#/%zz"],
	regex: ["^a+$", "(", "[a-z]", "\\p{L}", "a{2,1}"],
	"relative-json-pointer": ["0", "1/a", "2#", "-1", "01"],
	uri: ["https://x.y/z", "urn:isbn:123", "mailto:a@b", "not a uri", "/relative", "http://x/%zz"],
	"uri-reference": ["/relative", "https://x.y", "#frag", "a b", "%zz"],
	"uri-template": ["https://x/{id}", "/a/{+path}/b", "{?q,r}", "/a/{", "a b"],
	url: ["https://x.y/z", "file:///tmp/a", "not a url", "x:", "http://[::1]:80/"],
	uuid: ["123e4567-e89b-12d3-a456-426614174000", "123E4567-E89B-12D3-A456-426614174000", "123e4567e89b12d3a456426614174000", "123e4567-e89b-12d3-a456-42661417400g"],
	unknown: ["anything"],
};
const formatCases = Object.entries(FORMAT_VALUES).flatMap(([format, values]) =>
	values.map((value) => {
		const tool = { name: "probe", description: "", parameters: { type: "object", properties: { v: { type: "string", format } } } };
		try {
			prepareToolArguments(tool, { v: value });
			return { format, value, ok: true };
		} catch (error) {
			return { format, value, ok: false, err: error.message.split("\n")[1] };
		}
	}),
);
// `pattern` is an ECMAScript regular expression with the `u` flag.
const PATTERNS = [
	["^\\d+$", ["123", "\u0663", "12a"]],
	["^\\w+$", ["a_1", "\u00e9", "a b"]],
	["^\\s$", [" ", "\u00a0", "\u2028", "x"]],
	["\\bcat\\b", ["a cat!", "concat", "\u00e9cat"]],
	["^(?=.*x)(?!.*y).+$", ["ax", "axy", "a"]],
	["^(a)\\1$", ["aa", "ab"]],
	["^\\p{Lu}+$", ["ABC", "Abc", "\u00c9"]],
	["^[^a-c]$", ["d", "a", "\u{1F600}"]],
	["^.$", ["\u{1F600}", "ab"]],
	["colou?r", ["my color", "colr"]],
	["^\\u{1F600}$", ["\u{1F600}", "x"]],
];
const patternCases = PATTERNS.flatMap(([pattern, values]) =>
	values.map((value) => {
		const tool = { name: "probe", description: "", parameters: { type: "object", properties: { v: { type: "string", pattern } } } };
		try {
			prepareToolArguments(tool, { v: value });
			return { pattern, value, ok: true };
		} catch (error) {
			return { pattern, value, ok: false, err: error.message.split("\n")[1] };
		}
	}),
);
const schemaCases = SCHEMA_PARAMS.map(([index, params]) => {
	const tool = { name: "probe", description: "", parameters: SCHEMAS[index] };
	try {
		return { schema: index, params, ok: prepareToolArguments(tool, structuredClone(params)) };
	} catch (error) {
		return { schema: index, params, err: error.message };
	}
});

const JSON_INPUTS = [
	"x", "{", "}", "[", "{a}", "{\"a\"}", "{\"a\":}", "{\"a\":1,}", "{\"a\":1 \"b\":2}", "[1,]", "[1 2]", "\"abc",
	"\"a\\qb\"", "\"a\u0001b\"", "1 2", "{}x", "tru", "nul", "-", "1.", "1e", "1e+", "01", "{\"a\":1}}",
	"   ", "\n{\n\"a\": [1,\n2,,]}", "{'a':1}", "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz", "[\"x\", undefined]",
	"{\"k\": NaN}", "\"\\u12\"", "{\"a\":1,\"b\":", "\u00e9", "[-]", ".5", "+1", "{\"a\"  :  tru}",
	"undefined", "NaN", "Infinity", "[object Object]", "\r\n{x", "{\n\n  \"a\" 1}", "\r\n\r\n  [1,,]",
	"{\"aaaaaaaaaaaaaaaaaaaa\": 1, \"bbbbbbbbbbbbbbbbbbbbbbbbbbbb\": x}", "[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, x",
	"[\"\ud83d\ude00\", @]", "\t[\t1\t]\tz", "{\"a\":\"b\" , }", "[1, 2, 3, 4, 5, 6, 7, x, 8, 9, 10, 11, 12, 13]",
	"{\"a\":-x}", "\"\\u00zz\"", "[1e5, 2E-3, -0.0, 1.5e+300]", "{\"a\":1,\"a\":2,\"b\":3}", "[true, false, null]",
	"123456789012345678901234567890", "[\"\\/\\b\\f\\n\\r\\t\\\"\\\\\"]",
	"{\"a\": {\"b\": [1, {\"c\": tx}]}}", "\u2028", "[1]\u00a0", "\"abc\u0000\"", "\"\u007f\"", "{\"a\":1}\r\n\t ",
	"[0.5e-3, -12, 1E2]", "{\"__proto__\": 1}", "{\"a\" : [ ] , \"b\" : { } }", "\"\\u0041\\uD83D\\uDE00\"",
	"[\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\" x]", "00", "-01", "1.5.5", "[1}", "{\"a\":1]",
	"{\"a\":1", "[1", "{\"a\"", "{\"a\":1,", "[1,", "\"a\" \"b\"", "{\"a\":\"x\" 5}", "[\"x\" \"y\"]", "", "[\n1,\n2,\n]",
];
// Where the context of an unexpected token starts and ends: sources of 20/21 units, the token near each end.
for (const [n, positions] of [[20, [0, 19]], [21, [0, 9, 10, 11, 20]], [30, [0, 9, 10, 11, 18, 19, 20, 21, 29]]])
	for (const p of positions) JSON_INPUTS.push(`${" ".repeat(p)}x${" ".repeat(n - p - 1)}`);
JSON_INPUTS.push("\"\\u12", "\"\\", "\"\\");
const parse = JSON_INPUTS.map((input) => {
	try {
		return { input, ok: JSON.parse(input) };
	} catch (error) {
		return { input, err: error.message };
	}
});

const gateway = [];
for (const args of ["x", "{\"a\":", "[1]", "null", "\"s\"", "5"]) {
	const result = await tool.execute("c", { tool: "t", args }, undefined);
	gateway.push({ args, text: result.content.map((c) => c.text).join("\n"), details: result.details });
}
rmSync(base, { recursive: true, force: true });
writeFileSync(
	join(here, "validation.json"),
	`${JSON.stringify({ parameters: tool.parameters, prepared, parse, gateway, schemas: SCHEMAS, schemaCases, formatCases, patternCases }, null, "\t")}\n`,
);
process.exit(0);
