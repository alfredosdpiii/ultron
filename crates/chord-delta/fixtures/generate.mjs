// Generates crates/chord-delta/fixtures/delta.json from the pinned upstream Chord source.
// Usage: node crates/chord-delta/fixtures/generate.mjs <pi-reference-checkout>
// Chord is MIT licensed, (c) the upstream authors; see the reference checkout's LICENSE.
import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const reference = resolve(process.argv[2] ?? "../ultron-rust-reference");
const d = await import(pathToFileURL(join(reference, "packages/chord/src/delta/index.ts")).href);
const revision = execFileSync("git", ["-C", reference, "rev-parse", "HEAD"], { encoding: "utf8" }).trim();

const clone = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));
const outcome = (fn) => {
	try {
		return { ok: clone(fn()) };
	} catch (e) {
		return { error: { name: e.name, message: e.message } };
	}
};

// ─── Seeded random streams through the real tracker ─────────────────────────
function mulberry32(seed) {
	return () => {
		seed |= 0;
		seed = (seed + 0x6d2b79f5) | 0;
		let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
		t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
		return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
	};
}

function streams(count, steps) {
	const out = [];
	for (let seed = 1; seed <= count; seed++) {
		const rnd = mulberry32(seed);
		const int = (n) => Math.floor(rnd() * n);
		const pick = (xs) => xs[int(xs.length)];
		const KEYS = ["a", "b", "text", "items", "meta", "x_y", "émoji", "constructor", "prototype", "nested", "", "id"];
		const TEXT = ["", "x", "hello ", "wörld", "😀", "a😀b", "line\n", "日本"];
		const scalar = () => pick([null, true, false, 0, -3, 42, 0.5, 1e20, pick(TEXT)]);
		const value = (depth) => {
			const r = rnd();
			if (depth <= 0 || r < 0.5) return scalar();
			if (r < 0.75) return Array.from({ length: int(4) }, () => value(depth - 1));
			const o = {};
			for (let i = int(4); i > 0; i--) o[pick(KEYS)] = value(depth - 1);
			return o;
		};
		const containers = (node, acc = []) => {
			acc.push(node);
			for (const v of Array.isArray(node) ? node : Object.values(node))
				if (v !== null && typeof v === "object") containers(v, acc);
			return acc;
		};
		const mutate = (root) => {
			const target = pick(containers(root));
			if (Array.isArray(target)) {
				const n = target.length;
				switch (int(13)) {
					case 0: target.push(value(2)); break;
					case 1: target.pop(); break;
					case 2: target.shift(); break;
					case 3: target.unshift(value(1), value(1)); break;
					case 4: target.splice(int(n + 1), int(3), ...Array.from({ length: int(3) }, () => value(1))); break;
					case 5: target.reverse(); break;
					case 6: target.sort((p, q) => (JSON.stringify(p) < JSON.stringify(q) ? -1 : 1)); break;
					case 7: if (n > 0) target[int(n)] = value(2); break;
					case 8: target[n] = value(1); break;
					case 9: target.length = int(n + 3); break;
					case 10: if (n > 0) target.fill(scalar(), int(n)); break;
					case 11: if (n > 1) target.copyWithin(0, 1); break;
					case 12: {
						const i = target.findIndex((v) => typeof v === "string");
						if (i >= 0) target[i] += pick(TEXT);
						break;
					}
				}
			} else {
				const keys = Object.keys(target);
				const key = keys.length > 0 && rnd() < 0.6 ? pick(keys) : pick(KEYS);
				const current = target[key];
				switch (int(5)) {
					case 0: target[key] = value(2); break;
					case 1: delete target[key]; break;
					case 2: target[key] = typeof current === "string" ? current + pick(TEXT) + pick(TEXT) : pick(TEXT); break;
					case 3:
						if (typeof current === "string") {
							const cps = Array.from(current);
							target[key] = cps.slice(int(cps.length + 1)).join("") + (rnd() < 0.5 ? pick(TEXT) : "");
						}
						break;
					case 4: target[key] = clone(current ?? null); break;
				}
			}
		};

		const base = { text: "start😀", items: [1, "two", { three: 3 }], meta: { nested: { id: 1 } } };
		const tracker = d.track(clone(base));
		const enc = d.encoder();
		const batches = [];
		for (let step = 0; step < steps; step++) {
			let prepared;
			if (rnd() < 0.05) prepared = tracker.prepareReplace(rnd() < 0.5 ? [value(2), value(2)] : { root: value(3) });
			else {
				const change = tracker.beginChange();
				try {
					for (let i = 1 + int(4); i > 0; i--) mutate(change.state);
				} catch {
					change.abort();
					continue;
				}
				prepared = change.prepare();
			}
			tracker.adopt(prepared);
			const ops = clone(prepared.ops);
			batches.push({ ops, wire: clone(enc.encode(prepared.ops)), value: clone(tracker.value) });
		}
		out.push({ seed, base, batches });
	}
	return out;
}

// ─── Hand-written edge and error cases ──────────────────────────────────────
const APPLY = [
	[{ a: 1 }, [["r", [1, 2]]]],
	[null, [["r", { a: 1 }], ["s", ["b"], 2]]],
	[null, [["s", ["a"], 1]]],
	[{ a: 1 }, [["s", ["b", "c"], 1]]],
	[{ a: 1 }, [["s", ["a", "c"], 1]]],
	[{ a: { b: 1 } }, [["s", ["a", "b"], { c: [1] }]]],
	[{ a: 1, b: 2, c: 3 }, [["d", ["a"]], ["s", ["a"], 9]]],
	[{ a: 1, b: 2 }, [["s", ["a"], 5]]],
	[{ a: 1 }, [["d", ["missing"]]]],
	[{ l: [1, 2] }, [["s", ["l", 2], 3]]],
	[{ l: [1, 2] }, [["s", ["l", 3], 3]]],
	[{ l: [1, 2] }, [["s", ["l", "x"], 3]]],
	[{ l: [1, 2] }, [["s", ["l", "length"], 3]]],
	[{ l: [1, 2] }, [["s", ["l", "0"], 3]]],
	[{ l: [1, 2] }, [["s", ["l", "x", "y"], 3]]],
	[{ l: [1, 2] }, [["s", ["l", "length", "y"], 3]]],
	[{ l: [1, 2] }, [["s", ["l", 5, "y"], 3]]],
	[{ l: [1, 2] }, [["d", ["l", 0]]]],
	[{ l: [1, 2] }, [["d", ["l", 2]]]],
	[{ l: [1, 2] }, [["d", ["l", 3]]]],
	[{ o: {} }, [["s", ["o", 0], "zero"]]],
	[{ o: { 0: "z" } }, [["a", ["o", 0], "!"]]],
	[{ s: "ab" }, [["a", ["s"], "cd"]]],
	[{ s: 1 }, [["a", ["s"], "cd"]]],
	[{}, [["a", ["s"], "cd"]]],
	[{ s: "a😀b" }, [["t", ["s"], 3]]],
	[{ s: "a😀b" }, [["t", ["s"], 1]]],
	[{ s: "abc" }, [["t", ["s"], 10]]],
	[{ s: "abc" }, [["t", ["s"], 0]]],
	[{ l: ["x", "y"] }, [["a", ["l", 1], "z"], ["t", ["l", 0], 1]]],
	[{ l: ["x"] }, [["a", ["l", 1], "z"]]],
	[{ s: 5 }, [["t", ["s"], 1]]],
	[{ l: [1, 2, 3] }, [["p", ["l"], 1, 1, ["a", "b"]]]],
	[{ l: [1, 2, 3] }, [["p", ["l"], 10, 5, ["end"]]]],
	[{ l: [1, 2, 3] }, [["p", ["l"], 0, 100, []]]],
	[[1, 2, 3], [["p", [], 0, 1, [9]]]],
	[{ l: {} }, [["p", ["l"], 0, 0, [1]]]],
	[{ l: 1 }, [["p", ["l"], 0, 0, [1]]]],
	[{ l: [1] }, [["p", ["missing"], 0, 0, [1]]]],
	[{ l: [1, 2, 3] }, [["m", ["l"], [2, 0, 1]]]],
	[[1, 2, 3], [["m", [], [1, 2, 0]]]],
	[{ l: [1, 2] }, [["m", ["l"], [2, 0, 1]]]],
	[{ l: {} }, [["m", ["l"], []]]],
	[{ a: 1 }, [["s", ["__proto__", "x"], 1]]],
	[{ a: 1 }, [["s", ["constructor"], 1]]],
	[{ a: 1 }, [["s", [-1], 1]]],
	[{ a: 1 }, [["s", [1.5], 1]]],
	[{ a: 1 }, [["s", [null], 1]]],
	[{ a: 1 }, [["s", [], 1]]],
	[{ a: 1 }, [["x", ["a"]]]],
	[{ a: 1 }, [["s", ["a"]]]],
	[{ a: 1 }, [["s", "a", 1]]],
	[{ a: 1 }, [[]]],
	[{ a: 1 }, ["s"]],
	[{ a: 1 }, [["s", ["a"], 2], ["s", ["b", "c"], 3]]],
	[{ a: { b: { c: 1 } }, keep: { k: 1 } }, [["s", ["a", "b", "c"], 2], ["d", ["a", "b"]]]],
	[{ a: [{ b: [1] }] }, [["p", ["a", 0, "b"], 1, 0, [2, 3]], ["m", ["a", 0, "b"], [2, 1, 0]]]],
	[5, [["s", ["a"], 1]]],
	[5, [["p", [], 0, 0, []]]],
	[{ a: 1 }, [["d", ["a", "b"]]]],
];

const VALID = [
	["r", 1], ["r"], ["r", 1, 2], ["s", ["a"], 1], ["s", [], 1], ["s", ["a"]], ["s", "a", 1], ["s", [{}], 1],
	["s", [true], 1], ["s", [-1], 1], ["s", [1.5], 1], ["s", [2.0], 1], ["d", ["a"]], ["d", []], ["d", ["a"], 1],
	["a", ["a"], "x"], ["a", ["a"], 1], ["a", [], "x"], ["t", ["a"], 1], ["t", ["a"], -1], ["t", ["a"], 1.5], ["t", ["a"], "1"],
	["p", [], 0, 0, []], ["p", [], -1, 0, []], ["p", [], 0, 0.5, []], ["p", [], 0, 0, {}], ["p", [], 0, 0], ["p", "x", 0, 0, []],
	["m", [], [0]], ["m", [], [1]], ["m", [], [0, 0]], ["m", [], [0.5]], ["m", [], {}], ["m", []], ["m", ["prototype"], []],
	["#", 0, ["a"]], ["q"], [null], [[1, null, "x"]], [{}], [], "s", null, 7,
];
const WIRE = [
	["r", 1], ["r"], ["s", ["a"], 1], ["s", 0, 1], ["s", 1], ["s", -1, 1], ["s", 1.5, 1], ["s", "a", 1], ["s"], ["s", 1, 2, 3],
	["d", 0], ["d"], ["d", 0, 1], ["d", "x"], ["a", 0, "x"], ["a", "x"], ["a", 1], ["a", 0, 1], ["a"], ["t", 0, 1], ["t", 1],
	["t", -1], ["t", 0, "x"], ["t"], ["p", 0, 0, 0, []], ["p", 0, 0, []], ["p", 0, 0], ["p", 0, -1, []], ["p", 0, 0, {}],
	["p", ["a"], 0, 0, []], ["m", 0, [0]], ["m", [0]], ["m", [1]], ["m"], ["m", 0, [0], 1], ["#", 0, ["a"]], ["#", -1, ["a"]],
	["#", 0, "a"], ["#", 0, ["__proto__"]], ["#", 0], ["s", ["prototype"], 1], ["z"], [], {}, null,
];
const DECODE = [
	[[["s", ["a"], 1], ["s", 2], ["#", 0, ["b", 1]], ["a", 0, "x"], ["t", 1], ["d", ["c"]], ["d"]], [["s", 0, 3], ["p", 0, 0, 0, [1]]]],
	[[["r", { a: 1 }], ["s", ["a"], 2]], [["s", 0, 1]]],
	[[["#", 0, ["a"]], ["r", {}]], [["s", 0, 1]]],
	[[["s", 1]]],
	[[["s", ["a"], 1]], [["s", 2]]],
	[[["s", 7, 1]]],
	[[["s", [], 1]]],
	[[["p", [], 0, 0, [1]], ["p", 1, 0, [2]], ["m", [1, 0]], ["m", ["l"], [0]], ["m", [0]]]],
	[[["#", 3, []], ["p", 3, 0, 0, []], ["s", 3, 1]]],
	[[["#", 0, ["a"]], ["#", 0, ["b"]], ["s", 0, 1]]],
	[[["s", ["a"], 1], ["bogus"]]],
];

const apply = APPLY.map(([target, ops]) => ({
	target,
	ops,
	apply: outcome(() => d.apply(clone(target), clone(ops))),
	applyImmutable: outcome(() => {
		const input = clone(target);
		const before = JSON.stringify(input);
		const result = d.applyImmutable(input, clone(ops));
		if (JSON.stringify(input) !== before) throw new Error("applyImmutable mutated its input");
		return result;
	}),
}));
const validate = VALID.map((op) => ({ op, result: outcome(() => (d.assertValidOp(op), true)) }));
const validateWire = WIRE.map((op) => ({ op, result: outcome(() => (d.assertValidWireOp(op), true)) }));
const decode = DECODE.map((batches) => {
	const dec = d.decoder();
	const results = [];
	for (const wire of batches) {
		const r = outcome(() => dec.decode(clone(wire)));
		results.push(r);
		if (r.error) break;
	}
	return { batches, results };
});

const fixture = {
	source: { repository: "https://github.com/earendil-works/pi", revision, module: "packages/chord/src/delta/index.ts", license: "MIT" },
	generator: "crates/chord-delta/fixtures/generate.mjs",
	streams: streams(40, 60),
	apply,
	validate,
	validateWire,
	decode,
};
writeFileSync(join(dirname(fileURLToPath(import.meta.url)), "delta.json"), `${JSON.stringify(fixture)}\n`);
console.log(`streams=${fixture.streams.length} batches=${fixture.streams.reduce((n, s) => n + s.batches.length, 0)} apply=${apply.length} validate=${validate.length} wire=${validateWire.length} decode=${decode.length}`);
