// Records crates/daemon/fixtures/daemon.json from the TypeScript sources: Chord's diffRevisions
// (packages/chord/src/state/diff.ts), the delta wire encoder applied to each batch, fallbackServerId
// (experimental/server.ts) and encodeControlLine (experimental/process.ts). The project instance directory names are
// computed with the formula of the user requirement (no TypeScript equivalent) from node's own sha256.
// Usage: node --import packages/coding-agent/src/experimental/source-resolver.ts crates/daemon/fixtures/generate.mjs
import { createHash } from "node:crypto";
import { writeFileSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const load = (path) => import(pathToFileURL(join(root, path)).href);
const { diffRevisions } = await load("packages/chord/src/state/diff.ts");
const { encoder } = await load("packages/chord/src/delta/index.ts");
const { fallbackServerId } = await load("packages/coding-agent/src/experimental/server.ts");
const { encodeControlLine } = await load("packages/coding-agent/src/experimental/process.ts");

const DIFFS = [
	["append", { text: "Hi" }, { text: "Hi there" }],
	["trim-append", { text: "abcdef" }, { text: "defgh" }],
	["unrelated", { text: "abc" }, { text: "xyz" }],
	["number", { n: 1, f: 0.1 }, { n: 2, f: 0.30000000000000004 }],
	["add-delete", { a: 1, b: 2 }, { b: 2, c: 3 }],
	["nested", { s: { o: { a: [1, 2, 3] } } }, { s: { o: { a: [1, 2, 3, 4] } } }],
	["splice-middle", { a: [1, 2, 3, 4, 5] }, { a: [1, 9, 9, 5] }],
	["splice-empty", { a: [] }, { a: [{ x: 1 }] }],
	["array-shrink", { a: [{ x: 1 }, { x: 2 }] }, { a: [{ x: 2 }] }],
	["permutation", { a: [1, 2, 3] }, { a: [3, 1, 2] }],
	["shift-remove", { a: [1, 2, 3, 4] }, { a: [3, 4, 5, 6] }],
	["shift-insert", { a: [1, 2, 3, 4] }, { a: [8, 9, 1, 2] }],
	["single-mismatch", { a: [1, { y: 1 }, 3] }, { a: [1, { y: 2 }, 3] }],
	["containers-same-length", { a: [{ y: 1 }, { y: 2 }] }, { a: [{ y: 3 }, { y: 4 }] }],
	["type-change", { a: "x" }, { a: [1] }],
	["null", { a: null }, { a: { b: null } }],
	["root-replace", [1], { a: 1 }],
	["root-string", "a", "ab"],
	["reserved", { a: { constructor: 1 } }, { a: { constructor: 2 } }],
	["unicode", { t: "héllo 😀" }, { t: "héllo 😀 wörld" }],
	["surrogate-overlap", { t: "ab😀cd" }, { t: "😀cdef" }],
	["equal", { a: [1, { b: 2 }] }, { a: [1, { b: 2 }] }],
	["transcript", { snapshot: { transcript: [], tipId: null, stats: { messageCount: 0 } }, event: null }, { snapshot: { transcript: [{ id: "e1", message: { role: "user" } }], tipId: "e1", stats: { messageCount: 1 } }, event: { type: "entry_added", lane: "main", entry: { id: "e1" } } }],
	["directory", { revision: 1, sessions: [] }, { revision: 2, sessions: [{ serverId: "s", sessionId: "a", createdAt: 1, modifiedAt: 2.5 }] }],
	["too-many-ops", { a: new Array(4097).fill(0) }, { a: new Array(4097).fill(1) }],
];

const diffs = DIFFS.map(([name, before, after]) => {
	const ops = diffRevisions(before, after);
	return { name, before, after, ops, wire: encoder().encode(ops) };
});

const SERVER_IDS = ["00000000-0000-4000-8000-000000000001", "6f1c9a52-6d0b-4b1e-9e7a-2c3d4e5f6a7b", "d6bebd31-69a5-4080-8e1d-8e8345d078bb"];
const fallbacks = SERVER_IDS.map((id) => ({ id, fallback: fallbackServerId(id) }));

const CONTROL = [
	{ type: "register_server", protocol: 4, serverConnectionId: "c", endpoint: "/tmp/e.sock" },
	{ type: "message", from: "worker-1", payload: { n: 1.5, big: 1e21, small: 1e-7, neg: -0, text: "é \n\"" } },
	{ type: "send", to: "server", payload: { list: [1, null, true], "1": "integer-like key", a: {} } },
];
const control = CONTROL.map((message) => ({ message, line: encodeControlLine(message) }));

const projectDir = (home, rootPath) => {
	const hex = createHash("sha256").update(rootPath).digest("hex").slice(0, 12);
	const name = basename(rootPath).replace(/[^A-Za-z0-9._-]/g, "-").slice(0, 32).replace(/^\.+|\.+$/g, "") || "root";
	return join(home, ".ultron", "projects", `${name}-${hex}`);
};
const PROJECTS = ["/home/u/code/ultron", "/srv/my project (2)", "/", "/tmp/.hidden.", "/a/b/abcdefghijklmnopqrstuvwxyz0123456789"];
const projects = PROJECTS.map((rootPath) => ({ root: rootPath, dir: projectDir("/home/u", rootPath) }));

writeFileSync(join(here, "daemon.json"), `${JSON.stringify({ diffs, fallbacks, control, projects }, null, 1)}\n`);
