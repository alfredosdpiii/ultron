/**
 * What a repository teaches the reviewer (`autoreview/lessons.ts`): fates observed from re-reviews, threads and
 * merges; the ledger; the rules a sample supports; what is retained in Hindsight and how; and the pipeline's
 * use of a rule (a lowered finding is posted at low with the count, never dropped).
 */
import { execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, test } from "vitest";
import {
	FATE_DAYS,
	type Fate,
	fatesOf,
	HindsightLessons,
	hindsightUrlFrom,
	lessonsFor,
	MIN_SAMPLES,
	mergeFates,
} from "../src/ultron/autoreview/lessons.ts";
import { certainty } from "../src/ultron/autoreview/plan.ts";

const PYTHON =
	process.env.ULTRON_PYTHON ??
	(process.platform === "linux" && existsSync("/usr/bin/python3") ? "/usr/bin/python3" : "python3");
const here = dirname(fileURLToPath(import.meta.url));
const RLM_DIR = resolve(here, "../src/ultron/rlm");

const NOW = "2026-10-09T12:00:00.000Z";

function finding(id: string, extra: Partial<Parameters<typeof fatesOf>[0]["findings"][number]> = {}) {
	return {
		id,
		file: "src/a.py",
		line: 3,
		claim: `claim ${id}`,
		severity: "medium",
		category: "tests",
		kind: "deep:tests",
		status: "open",
		...extra,
	};
}

describe("fates", () => {
	test("a fixed finding was accepted; a thread a human resolved without a fix, a thumbs-down, or an open finding at merge was rejected; a reply alone is nothing", () => {
		const threads = new Map([
			[1, { id: "T1", resolved: true, thumbsUp: 0, thumbsDown: 0, replies: 1 }],
			[2, { id: "T2", resolved: false, thumbsUp: 0, thumbsDown: 1, replies: 0 }],
			[3, { id: "T3", resolved: false, thumbsUp: 1, thumbsDown: 0, replies: 0 }],
			[4, { id: "T4", resolved: true, thumbsUp: 0, thumbsDown: 0, replies: 0 }],
			[5, { id: "T5", resolved: false, thumbsUp: 0, thumbsDown: 0, replies: 2 }],
			[6, { id: "T6", resolved: false, thumbsUp: 1, thumbsDown: 1, replies: 0 }],
		]);
		const fates = fatesOf({
			pull: "github.com/o/r#1",
			now: NOW,
			findings: [
				finding("a-1", { commentId: 1 }),
				finding("a-2", { commentId: 2 }),
				finding("a-3", { commentId: 3 }),
				finding("a-4", { commentId: 4 }),
				finding("a-5", { commentId: 5 }),
				finding("a-6", { commentId: 6 }),
				finding("a-7"),
			],
			statuses: new Map([
				["a-1", "still_present"],
				["a-4", "fixed"],
				["a-5", "still_present"],
				["a-7", "fixed"],
			]),
			threads,
			// T4 was resolved by this account after the fix: not a dismissal.
			resolvedByUs: new Set(["T4"]),
		});
		expect(fates.map((fate) => [fate.id, fate.fate, fate.how])).toEqual([
			["a-1", "rejected", "the thread was resolved without a fix"],
			["a-2", "rejected", "thumbs down on the comment"],
			["a-3", "accepted", "thumbs up on the comment"],
			["a-4", "accepted", "fixed in a later commit"],
			["a-7", "accepted", "fixed in a later commit"],
		]);
		expect(fates[0]).toMatchObject({
			pull: "github.com/o/r#1",
			category: "tests",
			kind: "deep:tests",
			level: "medium",
			at: NOW,
		});

		// At merge: what is still open was passed over; a fixed one is nothing new; no threads needed.
		const atMerge = fatesOf({
			pull: "github.com/o/r#1",
			now: NOW,
			findings: [finding("b-1"), finding("b-2", { status: "fixed" }), finding("b-3", { status: "not_applicable" })],
			merged: true,
		});
		expect(atMerge.map((fate) => [fate.id, fate.fate, fate.how])).toEqual([
			["b-1", "rejected", "merged with the finding open"],
			["b-2", "accepted", "fixed in a later commit"],
		]);
		// Closed without merging: nothing is settled.
		expect(fatesOf({ pull: "p", now: NOW, findings: [finding("c-1")], merged: false })).toEqual([]);
	});

	test("the ledger keeps one fate per finding (the later wins), bounded; a rule needs a full recent sample", () => {
		const fate = (id: string, kind: Fate["fate"], daysAgo: number, category = "tests"): Fate => ({
			id,
			pull: "github.com/o/r#1",
			file: "f",
			line: 1,
			category,
			kind: "deep:tests",
			level: "medium",
			claim: id,
			fate: kind,
			how: "test",
			at: new Date(Date.parse(NOW) - daysAgo * 86_400_000).toISOString(),
		});
		const ledger = mergeFates([fate("x", "accepted", 10)], [fate("x", "rejected", 1), fate("y", "rejected", 2)]);
		expect(ledger.map((item) => [item.id, item.fate])).toEqual([
			["y", "rejected"],
			["x", "rejected"],
		]);
		// Four rejections: no rule yet. The fifth makes one. An old fate does not count.
		const four = [1, 2, 3, 4].map((n) => fate(`r${n}`, "rejected", n));
		expect(lessonsFor(four, NOW)).toEqual([]);
		expect(lessonsFor([...four, fate("old", "rejected", FATE_DAYS + 1)], NOW)).toEqual([]);
		expect(lessonsFor([...four, fate("r5", "rejected", 5)], NOW)).toEqual([
			{ category: "tests", kind: "deep:tests", accepted: 0, rejected: MIN_SAMPLES, lowered: true },
		]);
		// Half accepted is not lowered; another category is its own rule.
		const mixed = [
			...four,
			fate("a1", "accepted", 1),
			fate("a2", "accepted", 1),
			fate("a3", "accepted", 1),
			fate("a4", "accepted", 1),
		];
		expect(lessonsFor([...mixed, fate("s1", "rejected", 1, "security")], NOW)).toEqual([
			{ category: "tests", kind: "deep:tests", accepted: 4, rejected: 4, lowered: false },
		]);
	});

	test("the Hindsight URL comes from the environment, else the setting, else the default; off disables", () => {
		expect(hindsightUrlFrom(undefined, undefined, "http://localhost:8888")).toBe("http://localhost:8888");
		expect(hindsightUrlFrom("http://a:1", "http://b:2", "d")).toBe("http://a:1");
		expect(hindsightUrlFrom(" ", "http://b:2", "d")).toBe("http://b:2");
		expect(hindsightUrlFrom("off", "http://b:2", "d")).toBeUndefined();
		expect(hindsightUrlFrom(undefined, "none", "d")).toBeUndefined();
	});

	test("fates and rules are retained in Hindsight as documents tagged per repository, replaced by id", async () => {
		const calls: Array<{ url: string; method: string; body: unknown }> = [];
		const fetch: typeof globalThis.fetch = async (input, init) => {
			calls.push({
				url: String(input),
				method: init?.method ?? "GET",
				body: init?.body ? JSON.parse(String(init.body)) : undefined,
			});
			return new Response(JSON.stringify({ ok: true }), {
				status: 200,
				headers: { "content-type": "application/json" },
			});
		};
		const memory = new HindsightLessons({ baseUrl: "http://memory.test:8888", fetch, now: () => 1 });
		const fate: Fate = {
			id: "ab12345-2",
			pull: "github.com/o/r#7",
			file: "src/a.py",
			line: 3,
			category: "tests",
			kind: "deep:tests",
			level: "medium",
			claim: "No test pins it.",
			fate: "rejected",
			how: "thumbs down on the comment",
			at: NOW,
		};
		await memory.retain(
			"github.com/o/r",
			[fate],
			[{ category: "tests", kind: "deep:tests", accepted: 1, rejected: 6, lowered: true }],
		);
		const retains = calls.filter((call) => call.url.endsWith("/memories") && call.method === "POST");
		expect(calls[0]).toMatchObject({
			method: "PUT",
			url: "http://memory.test:8888/v1/default/banks/ultron-autoreview",
		});
		expect(retains).toHaveLength(2);
		const [first, second] = retains.map((call) => (call.body as { items: Array<Record<string, unknown>> }).items[0]!);
		expect(first).toMatchObject({
			document_id: expect.stringMatching(/^autoreview:[0-9a-f]{16}:github\.com\/o\/r#7:ab12345-2$/),
			tags: [expect.stringMatching(/^ultron:autoreview:project:[0-9a-f]{16}$/)],
			update_mode: "replace",
			metadata: { ultron_evidence_class: "tool_evidence" },
		});
		expect(first!.content).toBe(
			"In github.com/o/r, a medium tests finding of the deep:tests pass at src/a.py:3 was rejected (thumbs down on the comment): No test pins it.",
		);
		expect(second!.content).toBe(
			`In github.com/o/r, tests findings of the deep:tests pass were accepted 1 of 7 times in the last ${FATE_DAYS} days; the reviewer now posts such findings as low, non-blocking notes.`,
		);
		expect((first!.observation_scopes as string[][])[0]).toEqual(first!.tags);
	});
});

describe("the pipeline's use of a rule", () => {
	test("a lowered rule posts a matching confirmed finding at low with the count; a kind is fast, deep:<lens>, structure:<shape> or compiled", () => {
		const out = JSON.parse(
			execFileSync(
				PYTHON,
				[
					"-c",
					`
import sys, json
sys.path.insert(0, ${JSON.stringify(RLM_DIR)})
import autoreview_api as a
findings = [
    {"id": 1, "file": "f", "line": 1, "severity": "major", "level": "high", "category": "tests", "source": "deep:tests", "claim": "c1"},
    {"id": 2, "file": "f", "line": 2, "severity": "major", "level": "medium", "category": "tests", "source": "fast", "claim": "c2"},
    {"id": 3, "file": "f", "line": 3, "severity": "minor", "level": "nit", "category": "tests", "source": "deep:tests", "claim": "c3"},
    {"id": 4, "file": "f", "line": 4, "severity": "major", "level": "high", "category": "security", "source": "deep:structure", "shape": "permissions", "claim": "c4"},
]
lessons = [{"category": "tests", "kind": "deep:tests", "accepted": 1, "rejected": 6, "lowered": True},
           {"category": "security", "kind": "structure:permissions", "accepted": 4, "rejected": 4, "lowered": False}]
changed = a.apply_lessons(findings, lessons)
print(json.dumps({"changed": changed, "levels": [(f["level"], f.get("lesson")) for f in findings],
                  "kinds": [a.finding_kind(f) for f in findings + [{"source": "compiled:s5"}, {"source": "hybrid"}, {}]]}))
`,
				],
				{ encoding: "utf8" },
			),
		) as { changed: number; levels: Array<[string, string | null]>; kinds: string[] };
		expect(out.changed).toBe(1);
		expect(out.levels).toEqual([
			["low", "accepted 1 of 7 such findings in the last 180 days"],
			["medium", null],
			// Already below low: the rule names the count but never raises it.
			["nit", "accepted 1 of 7 such findings in the last 180 days"],
			["high", null],
		]);
		expect(out.kinds).toEqual([
			"deep:tests",
			"fast",
			"deep:tests",
			"structure:permissions",
			"compiled",
			"hybrid",
			"fast",
		]);
	});

	test("the comment says when the reviewer proved it, read it from the structure, or posts it as a note by the repository's history", () => {
		const base = {
			file: "f",
			line: 1,
			severity: "minor" as const,
			category: "tests",
			claim: "c",
			why: "w",
			verification: "confirmed" as const,
			confidence: 0.9,
		};
		expect(certainty({ ...base, strength: "test" })).toBe("Proven by a test run the reviewer made.");
		expect(certainty({ ...base, source: "deep:structure" })).toBe(
			"Stated by the reviewer from the files' structure; the verifier agreed it matters here.",
		);
		expect(
			certainty({ ...base, strength: "outside", lesson: "accepted 1 of 7 such findings in the last 180 days" }),
		).toBe("Posted as a note: this repository accepted 1 of 7 such findings in the last 180 days.");
		expect(certainty({ ...base })).toBe("");
	});
});
