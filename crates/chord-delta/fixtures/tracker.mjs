// Generates crates/chord-delta/fixtures/tracker.json from the pinned upstream Chord tracker.
// Each case is a base value and batches of recorded draft mutations; the upstream tracker's prepared ops and value
// for every batch are the expected output of the Rust Draft running the same mutations.
// Usage: node crates/chord-delta/fixtures/tracker.mjs <pi-reference-checkout>
// Chord is MIT licensed, (c) the upstream authors; see the reference checkout's LICENSE.
import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const reference = resolve(process.argv[2] ?? "../ultron-rust-reference");
const d = await import(pathToFileURL(join(reference, "packages/chord/src/delta/index.ts")).href);
const revision = execFileSync("git", ["-C", reference, "rev-parse", "HEAD"], { encoding: "utf8" }).trim();
const clone = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));

/** Run one recorded mutation against a draft. */
function run(state, step) {
	let target = state;
	for (const segment of step.path) target = target[segment];
	switch (step.m) {
		case "set": target[step.key] = clone(step.value); break;
		case "delete": delete target[step.key]; break;
		case "push": target.push(...clone(step.items)); break;
		case "pop": target.pop(); break;
		case "shift": target.shift(); break;
		case "unshift": target.unshift(...clone(step.items)); break;
		case "splice": target.splice(step.start, step.remove, ...clone(step.items)); break;
		case "length": target.length = step.len; break;
		default: throw new Error(`unknown step ${step.m}`);
	}
}

/** Record every batch of a case through the real tracker. */
function record(base, batches) {
	const tracker = d.track(clone(base));
	const out = [];
	for (const steps of batches) {
		const change = tracker.beginChange();
		for (const step of steps) run(change.state, step);
		const prepared = change.prepare();
		tracker.adopt(prepared);
		out.push({ steps, ops: clone(prepared.ops), value: clone(tracker.value) });
	}
	return { base, batches: out };
}

// ─── Hand-written cases ──────────────────────────────────────────────────────
const live = { items: ["a", "b"], nested: { count: 0 }, other: { label: "x" } };
const wide = {};
for (let i = 0; i < 4_100; i++) wide[`field${i}`] = 0;
const s = (path, key, value) => ({ m: "set", path, key, value });
const del = (path, key) => ({ m: "delete", path, key });
const CASES = {
	"object writes and nested counter": [live, [[s([], "message", "hello"), s(["nested"], "count", 1)]]],
	"same-value write and push+pop are empty": [live, [[s(["nested"], "count", 0), { m: "push", path: ["items"], items: ["z"] }, { m: "pop", path: ["items"] }]]],
	"shift then unshift is a structural no-op": [live, [[{ m: "shift", path: ["items"] }, { m: "unshift", path: ["items"], items: ["a"] }]]],
	"placement then push": [live, [[s([], "other", { label: "y" }), { m: "push", path: ["items"], items: ["c"] }]]],
	"edit inside a placed value": [live, [[s([], "other", { label: "y" }), s(["other"], "label", "x")], [s([], "fresh", { list: [1] }), { m: "push", path: ["fresh", "list"], items: [2] }]]],
	"delete then readd keeps key order": [{ a: 1, b: 2, c: 3 }, [[del([], "a"), s([], "a", 9)], [del([], "b"), del([], "missing"), s([], "z", 1), del([], "z")]]],
	"string append and trim": [{ s: "hello", t: "abcdef", u: "a😀b", v: "same" }, [[s([], "s", "hello world"), s([], "t", "defxyz"), s([], "u", "😀bc"), s([], "v", "other")]]],
	"array index writes": [{ l: [1, { k: 1 }, "x", 4] }, [[s(["l"], 0, 5), s(["l", 1], "k", 2), s(["l"], 2, "x"), s(["l"], 3, 4)], [s(["l"], 0, 1), s(["l"], 4, "end")]]],
	"nested element edits after structural changes": [{ l: [{ id: 1 }, { id: 2 }, { id: 3 }] }, [[{ m: "shift", path: ["l"] }, s(["l", 1], "id", 30), { m: "splice", path: ["l"], start: 1, remove: 0, items: [{ id: 9 }] }, s(["l", 0], "id", 20)]]],
	"splice runs and overrides": [{ l: [0, 1, 2, 3, 4, 5, 6] }, [[{ m: "splice", path: ["l"], start: 1, remove: 2, items: ["a"] }, { m: "splice", path: ["l"], start: 4, remove: 1, items: [] }, s(["l"], 0, 10), { m: "push", path: ["l"], items: ["p", "q"] }]]],
	"length truncates and pads": [{ l: [1, 2, 3] }, [[{ m: "length", path: ["l"], len: 1 }], [{ m: "length", path: ["l"], len: 3 }]]],
	"reserved key writes fold the container": [{ meta: { a: 1 }, keep: 1 }, [[s(["meta"], "constructor", 1)], [s(["meta"], "b", 2), del(["meta"], "constructor")]]],
	"reserved base key folds its parent": [{ outer: { prototype: { x: 1 }, y: 1 } }, [[s(["outer", "prototype"], "x", 2)]]],
	"oversized batch becomes a root replacement": [wide, [Array.from({ length: 4_100 }, (_, i) => s([], `field${i}`, 1))]],
	"4096 ops stay a delta": [wide, [Array.from({ length: 4_096 }, (_, i) => s([], `field${i}`, 2))]],
	"deep dirty order is shallowest first": [{ a: { b: { c: 1 } }, d: 1 }, [[s(["a", "b"], "c", 2), s(["a"], "e", 1), s([], "d", 2)]]],
};

// ─── Seeded random mutation scripts ──────────────────────────────────────────
function mulberry32(seed) {
	return () => {
		seed |= 0;
		seed = (seed + 0x6d2b79f5) | 0;
		let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
		t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
		return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
	};
}

function randomCase(seed, batchCount) {
	const rnd = mulberry32(seed);
	const int = (n) => Math.floor(rnd() * n);
	const pick = (xs) => xs[int(xs.length)];
	const KEYS = ["a", "b", "text", "items", "meta", "x_y", "émoji", "constructor", "prototype", "nested", "", "id", "0"];
	const TEXT = ["", "x", "hello ", "wörld", "😀", "a😀b", "line\n", "日本"];
	const scalar = () => pick([null, true, false, 0, -3, 42, 0.5, pick(TEXT)]);
	const value = (depth) => {
		const r = rnd();
		if (depth <= 0 || r < 0.5) return scalar();
		if (r < 0.75) return Array.from({ length: int(4) }, () => value(depth - 1));
		const o = {};
		for (let i = int(4); i > 0; i--) o[pick(KEYS)] = value(depth - 1);
		return o;
	};
	// Paths of every container in the current (plain) value.
	const containers = (node, path = [], acc = []) => {
		acc.push(path);
		for (const [key, child] of Array.isArray(node) ? node.entries() : Object.entries(node))
			if (child !== null && typeof child === "object") containers(child, [...path, key], acc);
		return acc;
	};
	const at = (root, path) => path.reduce((node, segment) => node[segment], root);
	const step = (shadow) => {
		const path = pick(containers(shadow));
		const target = at(shadow, path);
		let recorded;
		if (Array.isArray(target)) {
			const n = target.length;
			switch (int(9)) {
				case 0: recorded = { m: "push", path, items: [value(2)] }; break;
				case 1: recorded = { m: "pop", path }; break;
				case 2: recorded = { m: "shift", path }; break;
				case 3: recorded = { m: "unshift", path, items: [value(1), value(1)] }; break;
				case 4: recorded = { m: "splice", path, start: int(n + 1), remove: int(3), items: Array.from({ length: int(3) }, () => value(1)) }; break;
				case 5: recorded = n > 0 ? s(path, int(n), value(2)) : { m: "pop", path }; break;
				case 6: recorded = s(path, n, value(1)); break;
				case 7: recorded = { m: "length", path, len: int(n + 3) }; break;
				case 8: {
					const i = target.findIndex((v) => typeof v === "string");
					recorded = i >= 0 ? s(path, i, target[i] + pick(TEXT)) : { m: "pop", path };
					break;
				}
			}
		} else {
			const keys = Object.keys(target);
			const key = keys.length > 0 && rnd() < 0.6 ? pick(keys) : pick(KEYS);
			const current = Object.hasOwn(target, key) ? target[key] : undefined;
			switch (int(5)) {
				case 0: recorded = s(path, key, value(2)); break;
				case 1: recorded = del(path, key); break;
				case 2: recorded = s(path, key, typeof current === "string" ? current + pick(TEXT) + pick(TEXT) : pick(TEXT)); break;
				case 3: {
					const cps = typeof current === "string" ? Array.from(current) : [];
					recorded = s(path, key, cps.slice(int(cps.length + 1)).join("") + (rnd() < 0.5 ? pick(TEXT) : ""));
					break;
				}
				case 4: recorded = s(path, key, clone(current ?? null)); break;
			}
		}
		run(shadow, recorded);
		return recorded;
	};
	const base = { text: "start😀", items: [1, "two", { three: 3 }], meta: { nested: { id: 1 } } };
	let shadow = clone(base);
	const batches = [];
	for (let b = 0; b < batchCount; b++) {
		const steps = [];
		for (let i = 1 + int(5); i > 0; i--) steps.push(step(shadow));
		batches.push(steps);
	}
	return record(base, batches);
}

const cases = Object.entries(CASES).map(([name, [base, batches]]) => ({ name, ...record(base, batches) }));
for (let seed = 1; seed <= 60; seed++) cases.push({ name: `seed ${seed}`, ...randomCase(seed, 25) });

const out = join(dirname(fileURLToPath(import.meta.url)), "tracker.json");
writeFileSync(out, `${JSON.stringify({ source: { repository: "earendil-works/pi", revision }, cases })}\n`);
console.log(`wrote ${cases.length} cases to ${out}`);
