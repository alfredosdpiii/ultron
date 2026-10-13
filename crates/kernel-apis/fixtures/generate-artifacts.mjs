// Generates crates/kernel-apis/fixtures/artifacts.json from NativeLocalServices (packages/coding-agent/src/ultron/
// local-services.ts) over in-memory documents (a Map, as the TS tests use): scripted sessions of artifacts.put / read
// (byte ranges, including ones that split multibyte characters) / list / usage / pin / unpin / delete / reclaim, with
// size limits and an optional quota, recording each result or error and the documents after every step; and a
// reopened service over the same documents. Dates are fixed. Usage: node crates/kernel-apis/fixtures/generate-artifacts.mjs
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const { NativeLocalServices } = await import(join(here, "../../../packages/coding-agent/src/ultron/local-services.ts"));

const RealDate = Date;
globalThis.Date = class extends RealDate {
	constructor(...args) {
		super(...(args.length === 0 ? [1_700_000_000_000] : args));
	}
	static now() {
		return 1_700_000_000_000;
	}
};

function documents() {
	const values = new Map();
	return {
		values,
		get: async (key) => structuredClone(values.get(key)),
		set: async (key, value) => void values.set(key, structuredClone(value)),
		list: async (prefix) =>
			[...values.entries()].filter(([key]) => key.startsWith(prefix)).map(([key, value]) => ({ key, value: structuredClone(value) })),
	};
}

const multibyte = `${"é☃𝄞".repeat(200)}tail`;
const sessions = [
	{
		name: "limits and dedupe",
		options: { maxArtifactBytes: 1024, maxArtifactStorageBytes: 1500 },
		steps: [
			["artifacts.put", { text: "a".repeat(1025) }],
			["artifacts.put", { text: "é".repeat(513) }],
			["artifacts.put", { text: "b".repeat(1000) }],
			["artifacts.put", { text: "b".repeat(1000) }],
			["artifacts.put", { text: "c".repeat(600) }],
			["artifacts.list", {}],
			["artifacts.usage", {}],
			["artifacts.put", { text: "x", options: { mediaType: "application/json", label: "small" } }],
			["artifacts.put", { text: 5 }],
			["artifacts.put", { text: "y", options: { mediaType: 1 } }],
			["artifacts.put", { text: "y", options: { other: 1 } }],
			["artifacts.put", { text: "y", extra: 1 }],
			["artifacts.put", {}],
			["artifacts.list", { x: 1 }],
			["artifacts.nope", {}],
		],
	},
	{
		name: "range reads",
		options: {},
		steps: [
			["artifacts.put", { text: multibyte }],
			["range", 0, 1],
			["range", 1, 3],
			["range", 2, 5],
			["range", 3, 7],
			["range", 1798, 6],
			["range", 1802, 100],
			["range", 1804, 10],
			["range", 1809, 10],
			["range", null, null],
			["range", 0, 1_048_576],
			["range", 0, 1_048_577],
			["range", -1, 1],
			["range", 1.5, 1],
			["range", 0, "1"],
			["artifacts.read", { id: "nope" }],
			["artifacts.read", { id: "0".repeat(64) }],
			["artifacts.read", { id: "0".repeat(64), options: { start: 1 } }],
			["artifacts.read", { id: "0".repeat(64), holder: "x" }],
		],
	},
	{
		name: "retention, deletion and reclaim",
		options: {},
		steps: [
			["artifacts.put", { text: "checkpoint state", options: { label: "checkpoint" } }],
			["artifacts.put", { text: "x".repeat(500) }],
			["artifacts.put", { text: "test log: 12 passed" }],
			["artifacts.put", { text: "y".repeat(300) }],
			["pin", 0, "checkpoint:worker@1"],
			["pin", 0, "checkpoint:worker@1"],
			["pin", 2, "evidence:task-9"],
			["pin", 2, " "],
			["pin", 2, "h".repeat(201)],
			["delete", 0, null],
			["artifacts.reclaim", { bytes: 400 }],
			["artifacts.reclaim", { bytes: 1_000_000, reason: "make room" }],
			["artifacts.reclaim", { bytes: -1 }],
			["artifacts.reclaim", { bytes: 1, reason: 5 }],
			["read", 1, 0],
			["artifacts.list", {}],
			["artifacts.usage", {}],
			["reopen"],
			["delete", 2, null],
			["unpin", 0, "checkpoint:worker@1"],
			["unpin", 0, "never-held"],
			["delete", 0, "checkpoint superseded"],
			["delete", 0, null],
			["read", 0, 10],
			["pin", 0, "late"],
			["artifacts.put", { text: "checkpoint state", options: { label: "restored" } }],
			["read", 0, 100],
			["artifacts.delete", { id: "nope" }],
			["delete", 3, 7],
		],
	},
	{
		name: "quota never evicts",
		options: { artifactQuotaBytes: 1000 },
		steps: [
			["artifacts.put", { text: "a".repeat(600) }],
			["pin", 0, "task:live"],
			["artifacts.put", { text: "b".repeat(300) }],
			["artifacts.put", { text: "c".repeat(200) }],
			["artifacts.put", { text: "a".repeat(600) }],
			["artifacts.usage", {}],
			["artifacts.reclaim", { bytes: 300 }],
			["artifacts.put", { text: "c".repeat(200) }],
			["artifacts.usage", {}],
		],
	},
];

const results = [];
for (const session of sessions) {
	const store = documents();
	let services = new NativeLocalServices(store, session.options);
	const ids = [];
	const steps = [];
	for (const step of session.steps) {
		let request;
		if (step[0] === "range") {
			// A range read of the first artifact: offset and length when not null.
			const options = {};
			if (step[1] !== null) options.offset = step[1];
			if (step[2] !== null) options.length = step[2];
			request = ["artifacts.read", { id: ids[0], ...(Object.keys(options).length === 0 ? {} : { options }) }];
		} else if (step[0] === "read") {
			request = ["artifacts.read", { id: ids[step[1]], options: { length: step[2] } }];
		} else if (step[0] === "pin" || step[0] === "unpin")
			request = [`artifacts.${step[0]}`, { id: ids[step[1]], holder: step[2] }];
		else if (step[0] === "delete")
			request = ["artifacts.delete", { id: ids[step[1]], ...(step[2] === null ? {} : { reason: step[2] }) }];
		else if (step[0] === "reopen") {
			services = new NativeLocalServices(store, session.options);
			steps.push({ step, request: null, result: null, documents: Object.fromEntries(store.values) });
			continue;
		} else request = step;
		let outcome;
		try {
			const result = await services.handle(request[0], request[1], {});
			if (request[0] === "artifacts.put" && result && !ids.includes(result.id)) ids.push(result.id);
			outcome = { result };
		} catch (error) {
			outcome = { error: error.message };
		}
		steps.push({ step, request, ...outcome, documents: Object.fromEntries(store.values) });
	}
	results.push({ name: session.name, options: session.options, steps });
}
writeFileSync(join(here, "artifacts.json"), `${JSON.stringify(results, null, 1)}\n`);
