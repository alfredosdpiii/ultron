/**
 * `six-services-deep`, the second DELEGATION task (tasks-delegation.mjs lists it after `six-services`): six
 * independent services whose bugs are found by running a slow check harness and iterating, not by reading.
 *
 * Why a second task: on gpt-6-sol `six-services` did not separate the strategies. Its bugs were fixable from one
 * read, so a sequential agent read all six services in a few batched turns and fixed most bugs in one (Pi passed
 * in 108 s, Ultron with five subagents in 75 s, both inside the 300 s budget). This task makes each service a loop
 * of run, diagnose, fix, run again, and makes that loop's wall time unskippable:
 *
 * - Six services under services/ (kvstore: log-structured key-value store, scheduler: discrete-event job scheduler,
 *   patch: line diffs and patch application, calendar: meeting-slot finding, wire: binary codec and framed stream
 *   decoder, builds: dependency graph, incremental rebuilds and version resolution), each a package of several
 *   hundred lines with its own SPEC.md and a check harness (`python3 harness.py` in the service's directory). No
 *   quick tests: the harness is the only check shipped.
 * - The harness is slow on purpose: it waits BUILD_SECONDS (6) for a simulated environment, then STAGE_SECONDS (3)
 *   before each of five stages of seeded, deterministic property checks (brute-force models, round trips), and
 *   stops at the first failing stage, printing the property, the case seed and a small counterexample, never a
 *   location. A run that fails at stage k takes about 6 + 3k s; a passing run about 21 s.
 * - Three bugs per service, layered: on the untouched code the harness first fails at stage 1 (bug 0); with bug 0
 *   fixed at stage 3 (bug 1); with bugs 0 and 1 fixed at stage 5 (bug 2). Every bug is a runtime state-flow
 *   defect whose lines are each locally correct: aliasing (a cached or returned list, set, dict or bytearray
 *   mutated later), a cache not invalidated on one path or keyed on less than it depends on, a lazy generator
 *   reading a list being spliced, bookkeeping kept across a resubmission. None contradicts a SPEC sentence, so
 *   reading the code against the SPEC does not show them; the harness output is how they are found. (A pilot
 *   fixture with boundary and ordering bugs, `<` for `<=` and the like, was read straight off by both agents: see
 *   docs/implementation-status.md.) The layering is the fixture's contract, checked by
 *   scripts/eval-delegation.test.mjs, and it is deterministic: no bug depends on object ids, hash randomization,
 *   timing or allocation, and the test runs every single-bug-left variant under several PYTHONHASHSEED values and
 *   after a harmless edit that moves every later allocation, expecting the same stage and counterexample. (The
 *   first rebuild had id()-keyed caches as patch's and calendar's stage-5 bugs; unrelated edits changed
 *   allocation so the harness sometimes passed with them present. They were replaced on 2026-09-28.)
 * - Evidence is mechanical. Every harness run appends {nonce, start, end, stages passed, digest of the package
 *   source at start and end, digest of harness.py} to the service's .harness/runs.jsonl. The hidden check accepts a
 *   service only if harness.py is the original (sha256), the log holds a passing run of that harness on the
 *   package's current source that took at least 20 s, and the service's hidden suite passes (SPEC cases plus a
 *   randomized comparison with a reference model, on other seeds than the harness). So a fix without a later
 *   harness run, a run before the last edit, an edited harness, a copied harness or a run with its sleeps patched
 *   out all fail. (Writing a fake log line or faking the clock would pass the evidence part: that is forging the
 *   check, as with the parallel set's nonce log, and is not guarded against.)
 *
 * The hidden check prints one line per service and a JSON line `{"passed": k, "total": 6, "services", "evidence",
 * "tests"}`, recorded as `metrics`. Pass/fail is correctness only; `timeBudgetMs` (300 s) gives `withinBudget` and
 * never gates. The prompt is agent-neutral (no runtime feature named). Calibration and what the task measures:
 * tasks-delegation-solutions.mjs.
 *
 * The task files are a committed fixture (fixtures/delegation-deep/project and /hidden), pinned by one sha256; the
 * bugs are exact replacements (BUGS) applied to it. Do not edit after measurements exist: add a new id instead.
 */
import { createHash } from "node:crypto";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { fixtureDigest, readTree } from "./fixture-tree.mjs";

/** Reported per run; 5 minutes, as the prompt says. */
export const TIME_BUDGET_MS = 300_000;

export const SERVICES = ["kvstore", "scheduler", "patch", "calendar", "wire", "builds"];
export const PACKAGES = { kvstore: "segstore", scheduler: "jobqueue", patch: "linediff", calendar: "slots", wire: "frames", builds: "depgraph" };

/** Harness timing, frozen in every harness.py: a run failing at stage k takes about BUILD + k * STAGE seconds. */
export const HARNESS = { buildSeconds: 6, stageSeconds: 3, stages: 5 };
/** The harness stage (1-based) at which each bug of a service first shows, in BUGS order. */
export const BUG_STAGES = [1, 3, 5];

const FIXTURE = fileURLToPath(new URL("./fixtures/delegation-deep/", import.meta.url));
/** sha256 over every fixture file (path and content, sorted by path); a changed fixture refuses to load. */
const FIXTURE_SHA256 = "d9e41084a2f75d5d86cd69eac8d780753c9c87dcfa57c5de5e8e15fd7059e531";

function loadFixture() {
	const project = readTree(join(FIXTURE, "project"));
	const hidden = readTree(join(FIXTURE, "hidden"));
	const digest = fixtureDigest(project, hidden);
	if (digest !== FIXTURE_SHA256)
		throw new Error(`fixtures/delegation-deep changed: sha256 ${digest}, frozen ${FIXTURE_SHA256}`);
	return { project, hidden };
}

const { project: FILES_RAW, hidden: HIDDEN_RAW } = loadFixture();
export const FILES = FILES_RAW;

const sha256 = (text) => createHash("sha256").update(text).digest("hex");

/** Package name and original harness digest per service, written next to the hidden check. */
export const HARNESS_ORIGINALS = Object.fromEntries(
	SERVICES.map((service) => [service, { package: PACKAGES[service], harness_sha256: sha256(FILES[`services/${service}/harness.py`]) }]),
);

export const HIDDEN = { ...HIDDEN_RAW, "harness_originals.json": `${JSON.stringify(HARNESS_ORIGINALS, null, 2)}\n` };

/**
 * The bugs, three per service in layer order (bug i first shows at harness stage BUG_STAGES[i]), as exact
 * replacements in the fixture (`buggy` occurs exactly once in `path`, relative to the service directory).
 */
export const BUGS = {
	"kvstore": [
		{
			"path": "segstore/store.py",
			"what": "delete_prefix deletes while iterating the index's live key view, so every other matching key survives",
			"buggy": "        for key in self._index.range(prefix=prefix):\n",
			"fixed": "        for key in list(self._index.range(prefix=prefix)):\n"
		},
		{
			"path": "segstore/store.py",
			"what": "the decoded-record cache (keyed by segment id and offset) is not cleared when compaction rewrites segments in place, so reads return stale records",
			"buggy": "        return compact(self._table, self._index)\n",
			"fixed": "        self._decoded.clear()\n        return compact(self._table, self._index)\n"
		},
		{
			"path": "segstore/segment.py",
			"what": "export hands out closed segments' live buffers, which a later in-place compaction rewrites, so an earlier export no longer recovers",
			"buggy": "        return self._data if self.closed else bytes(self._data)\n",
			"fixed": "        return bytes(self._data)\n"
		}
	],
	"scheduler": [
		{
			"path": "jobqueue/queue.py",
			"what": "pushing onto an empty delayed heap leaves the cached next wake-up tick unset, so a future job can be skipped until something else happens",
			"buggy": "if self._next is not None and entry[0] < self._next:",
			"fixed": "if self._next is None or entry[0] < self._next:"
		},
		{
			"path": "jobqueue/graph.py",
			"what": "the dependents index of an id is not cleared when its job succeeds, so a resubmitted job under that id releases or cancels the old job's dependents again",
			"buggy": "for dependent in self._waiting_on.get(job.id, ()):",
			"fixed": "for dependent in self._waiting_on.pop(job.id, ()):"
		},
		{
			"path": "jobqueue/scheduler.py",
			"what": "cancelling a job waiting for a retry keeps its backoff sequence, which a resubmission under the same id then continues",
			"buggy": "    def _cancel(self, job):\n        job.state = CANCELLED\n",
			"fixed": "    def _cancel(self, job):\n        self._retry_delays.pop(job.id, None)\n        job.state = CANCELLED\n"
		}
	],
	"patch": [
		{
			"path": "linediff/apply.py",
			"what": "apply_hunks locates hunks lazily in the list it is splicing, so after a hunk that changes the line count the next hunk is searched in shifted lines",
			"buggy": "    for hunk, (pos, _) in zip(hunks, plan(result, hunks)):\n",
			"fixed": "    for hunk, (pos, _) in zip(hunks, plan(lines, hunks)):\n"
		},
		{
			"path": "linediff/hunks.py",
			"what": "joining regions extends the cached change-region lists in place, so a later call on the same files with a smaller context keeps the joined spans",
			"buggy": "            spans.append(block)\n",
			"fixed": "            spans.append(list(block))\n"
		},
		{
			"path": "linediff/search.py",
			"what": "locate trims the line index's cached position list in place past its search end, so a later search of an equal file misses those matches",
			"buggy": "        return positions\n",
			"fixed": "        return list(positions)\n"
		}
	],
	"calendar": [
		{
			"path": "slots/intervals.py",
			"what": "union with an empty set returns a set sharing the other operand's interval list, so a later in-place add() changes both",
			"buggy": "        if not other._items:\n            return IntervalSet._canonical(self._items)\n        if not self._items:\n            return IntervalSet._canonical(other._items)\n",
			"fixed": "        if not other._items:\n            return self.copy()\n        if not self._items:\n            return other.copy()\n"
		},
		{
			"path": "slots/attendee.py",
			"what": "add_busy drops the cached free time only for the days the block covers, not for the neighbouring days its buffer reaches",
			"buggy": "        for day in range(day_start(start), end, DAY):\n            self._free_by_day.pop(day, None)\n",
			"fixed": "        for day in range(day_start(start - self.buffer), end + self.buffer, DAY):\n            self._free_by_day.pop(day, None)\n"
		},
		{
			"path": "slots/search.py",
			"what": "the per-day common-free cache stores only the part of the day before the first asking window's end, so the next chunk of a search reads a truncated day",
			"buggy": "        free = _common_by_day[key] = common_free(attendees, day, min(day + DAY, end))\n",
			"fixed": "        free = _common_by_day[key] = common_free(attendees, day, day + DAY)\n"
		}
	],
	"wire": [
		{
			"path": "frames/varint.py",
			"what": "encode_uvarint memoizes small encodings as a mutable bytearray and returns it; encode_record appends the field value to the returned key bytes, corrupting the memo for later encodes of that number",
			"buggy": "    out.append(value)\n",
			"fixed": "    out.append(value)\n    out = bytes(out)\n"
		},
		{
			"path": "frames/record.py",
			"what": "packed series bodies are cached by the list's identity, so a series changed in place and written again reuses the stale encoding",
			"buggy": "    key = id(values)\n",
			"fixed": "    key = tuple(values)\n"
		},
		{
			"path": "frames/stream.py",
			"what": "compacting the buffer does not rebase a frame header that is already parsed, so a frame split across chunks after 4 KiB is read from the wrong offset",
			"buggy": "        del self._buf[:self._pos]\n        self._pos = 0\n",
			"fixed": "        shift = self._pos\n        del self._buf[:shift]\n        self._pos = 0\n        if self._pending is not None:\n            frame_type, length, body_at = self._pending\n            self._pending = (frame_type, length, body_at - shift)\n"
		}
	],
	"builds": [
		{
			"path": "depgraph/order.py",
			"what": "closure unions into the graph's cached reach set of the first target, so later orders for that target include unrelated targets",
			"buggy": "    included = graph.reach(roots[0])\n",
			"fixed": "    included = set(graph.reach(roots[0]))\n"
		},
		{
			"path": "depgraph/rebuild.py",
			"what": "when previous covers exactly the graph's targets, carried_outputs returns previous itself, so recording new outputs overwrites the values early cutoff compares against",
			"buggy": "        return previous\n",
			"fixed": "        return dict(previous)\n"
		},
		{
			"path": "depgraph/resolve.py",
			"what": "backtracking shares the constraint lists of the parent step, so constraints of an abandoned choice stay behind",
			"buggy": "            extended = dict(required)\n",
			"fixed": "            extended = {name: list(items) for name, items in required.items()}\n"
		}
	]
};

/**
 * Service files with the chosen bugs fixed: `fixes` maps a service to the indexes of its BUGS to repair (default:
 * all). Returns only the files that changed, keyed by project path.
 */
export function fixedFiles(fixes = Object.fromEntries(SERVICES.map((service) => [service, [0, 1, 2]]))) {
	const out = {};
	for (const [service, indexes] of Object.entries(fixes))
		for (const index of indexes) {
			const bug = BUGS[service][index];
			const path = `services/${service}/${bug.path}`;
			const current = out[path] ?? FILES[path];
			if (current === undefined) throw new Error(`${path} is not in the fixture`);
			if (current.split(bug.buggy).length !== 2) throw new Error(`${path}: the buggy text of "${bug.what}" must occur exactly once`);
			out[path] = current.replace(bug.buggy, () => bug.fixed);
		}
	return out;
}

/** Every bug of every service fixed. */
export const FIXED = fixedFiles();

export const PROMPT =
	"This repository holds six Python services under services/ (kvstore, scheduler, patch, calendar, wire, builds; see README.md). " +
	"Each has its own SPEC.md, source package and check harness, and each has several bugs spread across its files: its code does not do what its SPEC.md says.\n" +
	"1. Fix all six services so each one behaves exactly as its SPEC.md says. Hidden tests check every service against its SPEC.md in more cases than its harness, so fix the underlying logic, not just the reported cases. Do not edit the SPEC.md files or any harness.py.\n" +
	"2. Run a service's harness from its own directory, e.g. `cd services/kvstore && python3 harness.py`. It is slow (a full run takes a little over 20 seconds), runs its checks in stages and stops at the first failing stage, so one bug can hide another.\n" +
	"3. A service counts as fixed only if its harness passed on its final code: every harness run is logged, and the final check requires a passing run of the unmodified harness after the last change to that service's package.\n" +
	"4. The services are independent: they share no code, and a fix in one never affects another.\n" +
	"5. Finish as fast as possible: the whole job must be done within 5 minutes.";

export const TASK = {
	id: "six-services-deep",
	category: "delegation",
	build: () => ({ files: { ...FILES }, hidden: { ...HIDDEN } }),
	prompts: [PROMPT],
	verify: "python3 check_delegation_deep_hidden.py",
	verifyTimeoutMs: 180_000,
	timeBudgetMs: TIME_BUDGET_MS,
};
