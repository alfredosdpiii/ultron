// Generates crates/telemetry/fixtures/telemetry.json from this checkout's TypeScript telemetry sources.
// Scenarios run against InMemoryTelemetryContext (packages/telemetry) and record getSpans() snapshots; install-telemetry
// cases run isInstallTelemetryEnabled (packages/coding-agent/src/core/telemetry.ts) with a real in-memory SettingsManager.
// Usage: node crates/telemetry/fixtures/generate.mjs
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const load = (path) => import(pathToFileURL(join(root, path)).href);
const { InMemoryTelemetryContext, NOOP_TELEMETRY_CONTEXT } = await load("packages/telemetry/src/index.ts");
const { isInstallTelemetryEnabled } = await load("packages/coding-agent/src/core/telemetry.ts");
const { SettingsManager } = await load("packages/coding-agent/src/core/settings-manager.ts");

// A scenario is one span: {name, attributes?, ops, end, late?}.
// ops: ["event", name, attrs?] | ["set", attrs] | ["status", status] | ["child", scenario] | ["snapshot"]
// end: "ok" | {error: {name, message}} (an Error instance) | "throwValue" (a non-Error value)
// late: ops applied to the retained span handle after it settled.
async function apply(context, span, op, snapshots) {
	switch (op[0]) {
		case "event": return op.length > 2 ? span.addEvent(op[1], op[2]) : span.addEvent(op[1]);
		case "set": return span.setAttributes(op[1]);
		case "status": return span.setStatus(op[1]);
		case "child": return run(context, span, op[1], snapshots);
		case "snapshot": return snapshots.push(context.getSpans());
		default: throw new Error(`unknown op ${op[0]}`);
	}
}

async function run(context, parent, scenario, snapshots) {
	let handle;
	const options = scenario.attributes === undefined ? { name: scenario.name } : { name: scenario.name, attributes: scenario.attributes };
	try {
		await parent.startSpan(options, async (span) => {
			handle = span;
			for (const op of scenario.ops ?? []) await apply(context, span, op, snapshots);
			if (scenario.end === "throwValue") throw { kind: "value" };
			if (scenario.end?.error) {
				const error = new Error(scenario.end.error.message);
				error.name = scenario.end.error.name;
				throw error;
			}
		});
	} catch {}
	for (const op of scenario.late ?? []) await apply(context, handle, op, snapshots);
}

const scenarios = [
	{ id: "attributes-and-events", roots: [{ name: "op", attributes: { kind: "read", n: 3, ok: true, tags: ["a", "b"], sizes: [1, 2.5], flags: [true, false] },
		ops: [["event", "start"], ["event", "progress", { pct: 50 }], ["set", { kind: "write", extra: "x" }], ["set", { n: 4 }]], end: "ok" }] },
	{ id: "nested-end-order", roots: [{ name: "root", ops: [
		["child", { name: "a", ops: [["child", { name: "a1", end: "ok" }]], end: "ok" }],
		["child", { name: "b", end: { error: { name: "TypeError", message: "bad" } } }],
	], end: "ok" }, { name: "second-root", end: "ok" }] },
	{ id: "automatic-error-status", roots: [
		{ name: "error-instance", end: { error: { name: "RangeError", message: "out of range" } } },
		{ name: "non-error-value", end: "throwValue" },
	] },
	{ id: "explicit-status-wins", roots: [
		{ name: "last-status", ops: [["status", { status: "error", error: { name: "Expected", message: "first" } }], ["status", { status: "ok" }]], end: "ok" },
		{ name: "explicit-before-throw", ops: [["status", { status: "ok" }]], end: { error: { name: "Error", message: "after explicit status" } } },
		{ name: "explicit-error-no-details", ops: [["status", { status: "error" }]], end: "ok" },
		{ name: "expected-failure", ops: [["status", { status: "error", error: { name: "Expected", message: "returned failure" } }]], end: "ok" },
	] },
	{ id: "settled-span-ignores-mutation", roots: [{ name: "done", attributes: { a: 1 }, end: "ok",
		late: [["event", "late"], ["set", { a: 2, b: 3 }], ["status", { status: "error" }], ["child", { name: "orphan", ops: [["event", "x"], ["child", { name: "orphan-child", end: "ok" }]], end: "ok" }]] },
		{ name: "after", end: "ok" }] },
	{ id: "open-snapshot", roots: [{ name: "snapshot", attributes: { tags: ["initial"] }, ops: [["event", "event", { value: 1 }], ["snapshot"]], end: "ok" }] },
	{ id: "empty", roots: [] },
];

const spanCases = [];
for (const scenario of scenarios) {
	const context = new InMemoryTelemetryContext();
	const snapshots = [];
	for (const root of scenario.roots) await run(context, context, root, snapshots);
	spanCases.push({ id: scenario.id, roots: scenario.roots, snapshots, spans: context.getSpans() });
}

// The noop context records nothing and runs callbacks; record that a child of a noop span is the same span.
let noopSame;
await NOOP_TELEMETRY_CONTEXT.startSpan({ name: "x" }, async (span) => {
	noopSame = (await span.startSpan({ name: "child" }, (child) => child)) === span;
});

const installCases = [];
for (const env of [undefined, "1", "true", "TRUE", "Yes", "yes", "0", "false", "", "on", " 1", "2"]) {
	for (const setting of [undefined, true, false]) {
		const settings = SettingsManager.inMemory(setting === undefined ? {} : { enableInstallTelemetry: setting });
		installCases.push({ env: env ?? null, setting: setting ?? null, enabled: isInstallTelemetryEnabled(settings, env) });
	}
}

writeFileSync(join(here, "telemetry.json"), `${JSON.stringify({ spanCases, noopChildIsSameSpan: noopSame, installCases }, null, "\t")}\n`);
