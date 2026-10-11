// Generates crates/durable/fixtures/storage-conformance.json from the pinned upstream Pi Durable source.
// Runs every upstream storage conformance case against upstream MemoryStorage, SqliteStorage, and SqliteStorage
// reopened after every commit, through a recording proxy; the Rust tests replay each backend's recorded calls against
// the matching Rust backend and require identical results and errors.
// Usage: node crates/durable/fixtures/generate.mjs <pi-reference-checkout>
// Pi Durable is MIT licensed, (c) the upstream authors; see the reference checkout's LICENSE.
import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { mkdtemp, readdir, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { DatabaseSync } from "node:sqlite";
import { registerHooks } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const reference = resolve(process.argv[2] ?? "../ultron-rust-reference");
const here = dirname(fileURLToPath(import.meta.url));
// The reference checkout has no node_modules: resolve @earendil-works/* to its TypeScript sources.
const PACKAGES = { chord: "chord", "pi-ai": "ai", "pi-durable": "durable" };
registerHooks({
	resolve(specifier, context, next) {
		const match = /^@earendil-works\/(chord|pi-ai|pi-durable)(\/.*)?$/.exec(specifier);
		if (!match) return next(specifier, context);
		const path = `src${match[2] ?? ""}/index.ts`;
		return { url: pathToFileURL(join(reference, "packages", PACKAGES[match[1]], path)).href, shortCircuit: true };
	},
});
const src = (path) => import(pathToFileURL(join(reference, "packages/durable/src", path)).href);
const { MemoryStorage } = await src("storage/memory.ts");
const { openNodeSqliteStorage } = await src("storage/sqlite/node.ts");
const { openNodeJsonlStorage } = await src("storage/jsonl/node.ts");
const { createStorageConformance } = await src("testing/storage-conformance.ts");
const { BACKGROUND_CONTEXT } = await import("@earendil-works/chord/context");
const revision = execFileSync("git", ["-C", reference, "rev-parse", "HEAD"], { encoding: "utf8" }).trim();
const context = BACKGROUND_CONTEXT;

const clone = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));
const METHODS = new Set([
	"commit", "mintId", "conversation", "scanConversations", "entry", "findLatestHeadMarker", "scanEntries", "task",
	"scanTasks", "submission", "scanSubmissions", "submissionByRequest", "findDocument", "document", "scanDocuments",
	"close",
]);

function recording(storage, calls) {
	return new Proxy(storage, {
		get(target, key) {
			const value = Reflect.get(target, key);
			if (!METHODS.has(key)) return value;
			return async (...args) => {
				const call = { method: key, args: clone(args.filter((a) => a !== context)) };
				calls.push(call);
				try {
					const result = await value.apply(target, args);
					call.ok = clone(result);
					return result;
				} catch (e) {
					call.error = { name: e.name, message: e.message };
					throw e;
				}
			};
		},
	});
}

/** Every file of a closed JSONL directory, each as its parsed lines. */
async function dumpJsonl(directory) {
	const files = {};
	for (const name of (await readdir(directory)).sort()) {
		const text = await readFile(join(directory, name), "utf8");
		files[name] = text.split("\n").filter((line) => line !== "").map((line) => JSON.parse(line));
	}
	return files;
}

/** Upstream's ReopeningStorage (test/{sqlite,jsonl}-storage.test.ts): every commit closes and reopens. */
function reopening(initial, reopen) {
	let current = initial;
	let closed = false;
	return new Proxy(
		{},
		{
			get(_, key) {
				if (key === "commit") {
					return async (writes, ctx) => {
						if (closed) return current.commit(writes, ctx);
						try {
							return await current.commit(writes, ctx);
						} finally {
							await current.close(context);
							current = await reopen();
						}
					};
				}
				if (key === "close") {
					return async (ctx) => {
						if (closed) return;
						closed = true;
						await current.close(ctx);
					};
				}
				const value = current[key];
				return typeof value === "function" ? value.bind(current) : value;
			},
		},
	);
}

// Columns holding JSON text; dumps carry them parsed, since JSON key order and escapes are not part of the format.
const JSON_COLUMNS = {
	conversations: ["record"],
	entries: ["record"],
	tasks: ["kind", "record"],
	submissions: ["request_id", "record"],
	documents: ["kind", "key_value", "record"],
	document_revisions: ["content"],
};
const dumps = [];
const DUMP_QUERIES = {
	conversations: "SELECT * FROM conversations ORDER BY 1, 2",
	document_revisions: "SELECT * FROM document_revisions ORDER BY 1, 2",
	documents: "SELECT * FROM documents ORDER BY 1, 2",
	durable_metadata: "SELECT * FROM durable_metadata ORDER BY 1, 2",
	durable_schema: "SELECT * FROM durable_schema ORDER BY 1, 2",
	entries: "SELECT * FROM entries ORDER BY 1, 2",
	record_ids: "SELECT * FROM record_ids ORDER BY 1, 2",
	submissions: "SELECT * FROM submissions ORDER BY 1, 2",
	tasks: "SELECT * FROM tasks ORDER BY 1, 2",
};

/** Schema text and every row of a closed SQLite storage file. */
function dumpSqlite(path) {
	const db = new DatabaseSync(path, { readOnly: true });
	try {
		const schema = db.prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY name").all();
		const tables = {};
		const names = schema.filter((row) => row.type === "table").map((row) => row.name);
		if (JSON.stringify(names) !== JSON.stringify(Object.keys(DUMP_QUERIES).sort())) throw new Error(`tables ${names}`);
		for (const [name, query] of Object.entries(DUMP_QUERIES)) {
			tables[name] = db.prepare(query).all().map((row) => {
				const out = {};
				for (const [column, value] of Object.entries(row)) {
					out[column] =
						JSON_COLUMNS[name]?.includes(column) && typeof value === "string"
							? JSON.parse(value)
							: value;
				}
				return out;
			});
		}
		return { schema: schema.map((row) => ({ ...row })), tables };
	} finally {
		db.close();
	}
}

const BACKENDS = {
	memory: async (use) => use(new MemoryStorage()),
	sqlite: async (use) => {
		const directory = await mkdtemp(join(tmpdir(), "durable-fixture-"));
		const path = join(directory, "storage.sqlite");
		const storage = await openNodeSqliteStorage(path);
		try {
			await use(storage);
			await storage.close(context);
			dumps.push(dumpSqlite(path));
		} finally {
			await storage.close(context);
			await rm(directory, { recursive: true, force: true });
		}
	},
	sqliteReopen: async (use) => {
		const directory = await mkdtemp(join(tmpdir(), "durable-fixture-"));
		const path = join(directory, "storage.sqlite");
		await (await openNodeSqliteStorage(path)).close(context);
		const storage = reopening(await openNodeSqliteStorage(path), () => openNodeSqliteStorage(path));
		try {
			await use(storage);
		} finally {
			await storage.close(context);
			await rm(directory, { recursive: true, force: true });
		}
	},
};

BACKENDS.jsonl = async (use) => {
	const directory = await mkdtemp(join(tmpdir(), "durable-fixture-"));
	const storage = await openNodeJsonlStorage(directory, context);
	try {
		await use(storage);
		await storage.close(context);
		dumps.push(await dumpJsonl(directory));
	} finally {
		await storage.close(context);
		await rm(directory, { recursive: true, force: true });
	}
};
BACKENDS.jsonlReopen = async (use) => {
	const directory = await mkdtemp(join(tmpdir(), "durable-fixture-"));
	const storage = reopening(await openNodeJsonlStorage(directory, context), () =>
		openNodeJsonlStorage(directory, context),
	);
	try {
		await use(storage);
	} finally {
		await storage.close(context);
		await rm(directory, { recursive: true, force: true });
	}
};

// Extra scenarios for behavior the upstream suite leaves unexercised, recorded the same way.
const EXTRA = [
	[
		"extra: a submission whose request ID changes leaves the old request ID unindexed",
		async (storage) => {
			await storage.commit([{ type: "conversation", value: { id: 1 } }], context);
			const id = await storage.mintId();
			const submission = { id, conversationId: 1, type: "input", status: "queued" };
			await storage.commit([{ type: "submission", value: { ...submission, requestId: "a" } }], context);
			await storage.commit([{ type: "submission", value: { ...submission, requestId: "b" } }], context);
			await storage.submissionByRequest(1, "a", context);
			await storage.submissionByRequest(1, "b", context);
			await storage.commit([{ type: "submission", value: submission }], context);
			await storage.submissionByRequest(1, "b", context);
		},
	],
];

const fail = (message) => {
	throw new Error(message);
};
const assertions = {
	ok: (v, m) => v || fail(m ?? "not ok"),
	strictEqual: (a, b) => Object.is(a, b) || fail(`${JSON.stringify(a)} !== ${JSON.stringify(b)}`),
	deepEqual: (a, b) => JSON.stringify(a) === JSON.stringify(b) || fail("deepEqual"),
	partialDeepEqual: () => {},
	greaterThan: (a, b) => a > b || fail(`${a} <= ${b}`),
	async rejects(p, includes) {
		try {
			await p;
		} catch (e) {
			if (!String(e.message).includes(includes)) fail(`rejected with ${e.message}, wanted ${includes}`);
			return;
		}
		fail(`did not reject (${includes})`);
	},
};

const cases = [];
for (const [backend, withBackend] of Object.entries(BACKENDS)) {
	let calls;
	const withStorage = (use) => withBackend((storage) => use(recording(storage, calls)));
	const all = [
		...createStorageConformance({ assertions, withStorage }).map((c) => [c.name, () => c.run()]),
		...EXTRA.map(([name, run]) => [name, () => withStorage(run)]),
	];
	for (const [index, [name, run]] of all.entries()) {
		calls = [];
		try {
			await run();
		} catch (error) {
			throw new Error(`${backend}: ${name}: ${error.message}`);
		}
		cases[index] ??= { name };
		cases[index][backend] = calls;
		if (backend === "sqlite") cases[index].sqliteDump = dumps.pop();
		if (backend === "jsonl") cases[index].jsonlDump = dumps.pop();
	}
}

// A Rust string cannot hold a lone UTF-16 surrogate (upstream's "lossless string identities" case uses them). Map each
// lone surrogate U+D800+k to the distinct code point U+F0000+k, so the case still checks that distinct keys never merge.
const LONE = /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/g;
const scalar = (s) => s.replace(LONE, (c) => String.fromCodePoint(0xf0000 + c.charCodeAt(0) - 0xd800));
const scalars = (v) =>
	typeof v === "string"
		? scalar(v)
		: Array.isArray(v)
			? v.map(scalars)
			: v !== null && typeof v === "object"
				? Object.fromEntries(Object.entries(v).map(([k, x]) => [scalar(k), scalars(x)]))
				: v;

const out = join(here, "storage-conformance.json");
const fixture = scalars({ source: { repository: "earendil-works/pi", revision }, cases });
// One case per line keeps the file compact and its diffs per case.
const caseLines = fixture.cases.map((c) => JSON.stringify(c)).join(",\n");
writeFileSync(out, `{"source":${JSON.stringify(fixture.source)},"cases":[\n${caseLines}\n]}\n`);
console.log(`${cases.length} cases, ${cases.reduce((n, c) => n + c.memory.length + c.sqlite.length + c.sqliteReopen.length + c.jsonl.length + c.jsonlReopen.length, 0)} calls -> ${out}`);
