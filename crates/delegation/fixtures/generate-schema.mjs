// Generates crates/delegation/fixtures/schema.json from the TypeScript definition registry
// (packages/coding-agent/src/ultron/rlm/definition-registry.ts) and typebox 1.3 `Value.Check` / `Value.Errors`:
// - `validate`: validateJsonSchema(value, "$") over valid and invalid schemas (ok or the error text);
// - `check`: Check(schema, value) and Errors(schema, value) (keyword, instancePath, params, message) over a corpus;
// - `registry`: a NativeDefinitionRegistry over an in-memory store: register (ok, idempotent, conflicts, invalid
//   descriptors, missing adapters), list(), stored hashes, reload of the stored document, and validationError texts,
//   including the agent-class schema detail.
// Deterministic: no clock, no randomness. Usage: node crates/delegation/fixtures/generate-schema.mjs
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../../packages/coding-agent");
const { validateJsonSchema, NativeDefinitionRegistry, nativeDefinitionHash } = await import(
	join(root, "src/ultron/rlm/definition-registry.ts")
);
const { Check, Errors } = await import("typebox/value");

const outcome = (fn) => {
	try {
		return { ok: fn() ?? null };
	} catch (error) {
		return { error: error.message };
	}
};

const schemas = [
	{},
	{ type: "string" },
	{ type: ["string", "null"] },
	{ type: "strin" },
	{ type: [] },
	{ type: ["string", "string"] },
	{ type: 3 },
	{ required: ["a", "a"] },
	{ required: ["a", ""] },
	{ enum: [] },
	{ enum: [1, "a"] },
	{ const: { a: 1 } },
	{ items: false },
	{ items: [{ type: "string" }] },
	{ items: [] },
	{ prefixItems: [{ type: "integer" }] },
	{ prefixItems: {} },
	{ additionalProperties: false },
	{ additionalProperties: { type: "integer" } },
	{ propertyNames: false },
	{ not: true },
	{ allOf: [] },
	{ anyOf: [{ type: "string" }, { bogus: 1 }] },
	{ oneOf: "x" },
	{ dependentRequired: { a: ["b"] } },
	{ dependentRequired: [] },
	{ dependencies: { a: ["b"], c: { required: ["d"] } } },
	{ minLength: -1 },
	{ maxLength: 1.5 },
	{ minimum: "1" },
	{ multipleOf: 0 },
	{ uniqueItems: "yes" },
	{ examples: {} },
	{ pattern: "(" },
	{ pattern: "^a+$" },
	{ title: 1 },
	{ $ref: "#/x" },
	{ $defs: { A: { type: "string" } } },
	{ "x-ultron-agent-class": "Triage", type: "object" },
	{ default: 1 },
	{ properties: { a: { type: "string", nullable: true } } },
	{ properties: [] },
	[],
	[{}],
	true,
	null,
	"string",
];

const corpus = [
	[{ type: "object", properties: { kind: { enum: ["bug", "feature"] } }, required: ["kind"], additionalProperties: false }, { kind: "x", extra: 1 }],
	[{ type: "object", properties: { kind: { enum: ["bug", "feature"] } }, required: ["kind"], additionalProperties: false }, { kind: "bug" }],
	[{ type: "object", properties: { n: { type: "integer" } }, required: ["n"], additionalProperties: false }, { n: "x" }],
	[{ type: "object", properties: { n: { type: "integer" } }, required: ["n"], additionalProperties: false }, { n: 1.5 }],
	[{ type: "object", properties: { n: { type: "integer" } }, required: ["n"], additionalProperties: false }, { n: 2 }],
	[{ type: "object", required: ["a", "b"] }, {}],
	[{ type: "object", required: ["a", "b"] }, []],
	[{ type: ["string", "null"] }, 3],
	[{ type: ["string", "null"] }, null],
	[{ type: "integer", minimum: 0 }, -4],
	[{ type: "number", exclusiveMaximum: 10, exclusiveMinimum: 0, maximum: 9, multipleOf: 0.5 }, 9.75],
	[{ type: "number", multipleOf: 0.1 }, 0.3],
	[{ type: "number", multipleOf: 3 }, 7],
	[{ type: "string", minLength: 2, maxLength: 3 }, "😀"],
	[{ type: "string", minLength: 2, maxLength: 3 }, "éééé"],
	[{ type: "string", minLength: 2, maxLength: 3 }, "🇫🇷🇩🇪"],
	[{ type: "string", maxLength: 1 }, "👨‍👩‍👧"],
	[{ type: "string", pattern: "^a+$" }, "b"],
	[{ type: "string", pattern: "^a+$" }, "aaa"],
	[{ type: "string", format: "email" }, "a@b.co"],
	[{ type: "string", format: "email" }, "nope"],
	[{ type: "string", format: "date" }, "2024-02-29"],
	[{ type: "string", format: "date" }, "2023-02-29"],
	[{ type: "string", format: "date-time" }, "2024-02-29T12:00:00Z"],
	[{ type: "string", format: "date-time" }, "2024-02-29T12:00:00"],
	[{ type: "string", format: "time" }, "23:59:60Z"],
	[{ type: "string", format: "time" }, "12:59:60+01:00"],
	[{ type: "string", format: "uuid" }, "123e4567-e89b-12d3-a456-426614174000"],
	[{ type: "string", format: "uuid" }, "x"],
	[{ type: "string", format: "ipv4" }, "256.1.1.1"],
	[{ type: "string", format: "duration" }, "P1DT2H"],
	[{ type: "string", format: "json-pointer" }, "/a/~2"],
	[{ type: "string", format: "unknown-format" }, "anything"],
	[{ type: "array", items: { type: "integer" }, minItems: 3, uniqueItems: true }, [1, 1.0, "x"]],
	[{ type: "array", prefixItems: [{ type: "integer" }, { type: "string" }], items: false, minItems: 2, maxItems: 2 }, [1, 2, 3]],
	[{ type: "array", prefixItems: [{ type: "integer" }], items: { type: "string" } }, [1, "a", 2]],
	[{ type: "array", items: [{ type: "integer" }], additionalItems: false }, [1, 2]],
	[{ type: "array", contains: { type: "string" } }, [1, 2]],
	[{ type: "array", contains: { type: "string" }, minContains: 2, maxContains: 3 }, ["a", 1]],
	[{ type: "array", contains: { type: "string" }, maxContains: 1 }, ["a", "b"]],
	[{ type: "array", contains: { type: "string" }, minContains: 0 }, []],
	[{ anyOf: [{ type: "integer" }, { type: "string", minLength: 3 }] }, "ab"],
	[{ anyOf: [{ type: "integer" }, { type: "null" }] }, null],
	[{ oneOf: [{ type: "integer" }, { type: "number" }] }, 1],
	[{ oneOf: [{ type: "integer" }, { type: "string" }] }, true],
	[{ allOf: [{ type: "integer" }, { minimum: 5 }] }, 3],
	[{ not: { type: "string" } }, "x"],
	[{ if: { type: "string" }, then: { minLength: 2 }, else: { type: "number" } }, "a"],
	[{ if: { type: "string" }, then: { minLength: 2 }, else: { type: "number" } }, true],
	[{ if: { type: "string" } }, 1],
	[{ const: { a: [1, 2] } }, { a: [1, 2.0] }],
	[{ const: "x" }, "y"],
	[{ enum: [{ a: 1 }, 2] }, { a: 1 }],
	[{ type: "object", dependentRequired: { a: ["b", "c"] } }, { a: 1 }],
	[{ type: "object", dependencies: { a: ["b", "c"], d: { required: ["e"] } } }, { a: 1, d: 2 }],
	[{ type: "object", dependentSchemas: { a: { required: ["z"] } } }, { a: 1 }],
	[{ type: "object", patternProperties: { "^x-": { type: "integer" } }, additionalProperties: false }, { "x-a": "s", y: 1 }],
	[{ type: "object", propertyNames: { pattern: "^[a-z]+$" } }, { ok: 1, BAD: 2, "x y": 3 }],
	[{ type: "object", minProperties: 2, maxProperties: 3 }, { a: 1 }],
	[{ type: "object", minProperties: 2, maxProperties: 3 }, { a: 1, b: 2, c: 3, d: 4 }],
	[{ type: "object", properties: { a: { type: "object", properties: { b: { type: "array", items: { type: "boolean" } } } } } }, { a: { b: [true, "no", 1] } }],
	[{ type: "object", additionalProperties: { type: "string" } }, { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6, g: 7, h: 8, i: 9, j: 10 }],
	[{ type: "object", properties: { a: { anyOf: [{ type: "string" }, { type: "integer" }] } } }, { a: true }],
	[false, 1],
	[true, 1],
	[{ minimum: 3 }, "not a number"],
	[{ type: "integer" }, 1e21],
	[{ items: { type: "integer" } }, { 0: "x" }],
];

const errorView = (error) => ({
	keyword: error.keyword,
	instancePath: error.instancePath,
	params: error.params,
	message: error.message,
});

const store = () => {
	let value;
	return { read: async () => structuredClone(value), write: async (next) => (value = structuredClone(next)), get: () => value };
};

const base = (id, strategy, overrides = {}) => ({
	id,
	version: "1",
	strategy,
	instructions: `Run ${id}.`,
	inputSchema: { type: "object", properties: { n: { type: "integer" } }, required: ["n"], additionalProperties: false },
	outputSchema: { type: "object", properties: { doubled: { type: "integer" } }, required: ["doubled"], additionalProperties: false },
	maxRepairs: 0,
	inputDescription: "{n:integer}",
	outputDescription: "{doubled:integer}",
	...overrides,
});
const agentClass = base("triage--classify", "rlm", {
	outputSchema: {
		"x-ultron-agent-class": "Triage",
		type: "object",
		properties: { kind: { enum: ["bug", "feature"] }, score: { type: "integer", minimum: 0 } },
		required: ["kind", "score"],
		additionalProperties: false,
	},
});

const registryOps = async () => {
	const backing = store();
	const adapter = async () => null;
	const registry = new NativeDefinitionRegistry(backing, { deterministic: adapter, predict: adapter });
	const ops = [];
	const record = async (name, args, fn) => {
		try {
			ops.push({ op: name, args, ok: (await fn()) ?? null });
		} catch (error) {
			ops.push({ op: name, args, error: error.message });
		}
	};
	const candidates = [
		base("doubler", "rlm"),
		base("doubler", "rlm"),
		base("doubler", "rlm", { instructions: "Changed." }),
		base("det", "deterministic"),
		base("pred", "predict", { maxRepairs: 2 }),
		base("greedy", "predict", { maxRepairs: 3 }),
		base("Bad", "rlm"),
		base("v", "rlm", { version: "1a" }),
		base("x", "jev"),
		base("x", "rlm", { instructions: "" }),
		base("x", "rlm", { extra: 1 }),
		base("x", "rlm", { model: "no-slash" }),
		base("x", "rlm", { model: "a b/c" }),
		base("modeled", "rlm", { model: "openai/gpt-5/mini" }),
		base("x", "rlm", { inputSchema: { $ref: "#/x" } }),
		base("x", "rlm", { outputSchema: { type: "object", properties: { a: { type: "nope" } } } }),
		{ ...base("x", "rlm"), inputSchema: undefined },
		agentClass,
		null,
		"text",
	];
	for (const candidate of candidates) await record("register", candidate, () => registry.register(candidate));
	await record("list", null, () => registry.list());
	await record("stored", null, () => backing.get());
	for (const key of ["doubler@1", "missing@1", "identity@1", "security-reviewer@1"])
		await record("get", key, () => {
			const item = registry.get(key);
			return { hash: item.hash, strategy: item.strategy };
		});
	const checks = [
		["doubler@1", { n: "x" }, "input"],
		["doubler@1", { doubled: "x" }, "output"],
		["security-reviewer@1", { request: "  " }, "input"],
		["security-reviewer@1", { other: 1 }, "input"],
		["security-reviewer@1", { request: "go" }, "input"],
		["triage--classify@1", { kind: "x", extra: true }, "output"],
		["triage--classify@1", { kind: "bug", score: -1 }, "output"],
		["triage--classify@1", "not an object", "output"],
		["triage--classify@1", { kind: "x", score: "y", a: 1, b: 2 }, "output"],
	];
	for (const [key, value, direction] of checks)
		await record("validation", [key, value, direction], () => {
			const item = registry.get(key);
			const valid = direction === "input" ? registry.isValidInput(item, value) : registry.isValidOutput(item, value);
			return { valid, error: registry.validationError(item, value, direction).message };
		});
	// A second owner reloads the stored document; tampering with a hash is refused.
	const reopened = new NativeDefinitionRegistry(backing, { deterministic: adapter, predict: adapter });
	await record("reload", null, async () => {
		await reopened.ready();
		return reopened.list().map((item) => `${item.id}@${item.version}`);
	});
	const tampered = structuredClone(backing.get());
	tampered.definitions.at(-1).hash = "0".repeat(64);
	const bad = { read: async () => tampered, write: async () => {} };
	await record("tampered", null, () => new NativeDefinitionRegistry(bad, {}).ready());
	const conflicting = structuredClone(backing.get());
	const identity = conflicting.definitions.find((item) => item.id === "identity");
	identity.instructions = "Something else.";
	identity.hash = nativeDefinitionHash((({ hash, ...rest }) => rest)(identity));
	await record("conflict", null, () => new NativeDefinitionRegistry({ read: async () => conflicting, write: async () => {} }, {}).ready());
	await record("version", null, () =>
		new NativeDefinitionRegistry({ read: async () => ({ version: 2, definitions: [] }), write: async () => {} }, {}).ready(),
	);
	await record("unreadable", null, () =>
		new NativeDefinitionRegistry({ read: async () => { throw new Error("disk"); }, write: async () => {} }, {}).ready(),
	);
	// No adapters: predict and deterministic definitions cannot be registered; no store: nothing new registers.
	const bare = new NativeDefinitionRegistry(store(), {});
	await record("no predict adapter", null, () => bare.register(base("p", "predict")));
	await record("no deterministic adapter", null, () => bare.register(base("d", "deterministic")));
	await record("no store", null, () => new NativeDefinitionRegistry(undefined, {}).register(base("r", "rlm")));
	await record("no store identical builtin", null, () =>
		new NativeDefinitionRegistry(undefined, {}).register(registry.list().find((item) => item.id === "identity")),
	);
	// A failed write poisons the registry.
	const failing = new NativeDefinitionRegistry({ read: async () => undefined, write: async () => { throw new Error("full"); } }, {});
	await record("failed write", null, () => failing.register(base("r", "rlm")));
	await record("after failed write", null, () => failing.register(base("s", "rlm")));
	return ops;
};

const fixture = {
	validate: schemas.map((schema) => ({ schema: schema === undefined ? null : schema, ...outcome(() => validateJsonSchema(schema, "$")) })),
	check: corpus.map(([schema, value]) => ({
		schema,
		value,
		check: Check(schema, value),
		errors: [...Errors(schema, value)].map(errorView),
	})),
	registry: await registryOps(),
};
writeFileSync(join(here, "schema.json"), `${JSON.stringify(fixture, null, 1)}\n`);
