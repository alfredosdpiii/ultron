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
 *   fixed at stage 3 (bug 1); with bugs 0 and 1 fixed at stage 5 (bug 2). The bugs are semantic interactions
 *   (boundaries, feature combinations) in code that looks deliberate, spread over two or three files, so the
 *   harness output is how they are found in practice. The layering is the fixture's contract, checked by
 *   scripts/eval-delegation.test.mjs.
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
const FIXTURE_SHA256 = "14beaf05358d29505fa9fa4322b6117b98d4cf46e84b3211000b751a1b219023";

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
			"what": "a write that rolls over to a new segment is indexed at the old segment's end offset instead of offset 0",
			"buggy": "        segment.append(raw)\n        return Location(segment.id, offset)\n",
			"fixed": "        return Location(segment.id, segment.append(raw))\n"
		},
		{
			"path": "segstore/compaction.py",
			"what": "compaction keeps a key's oldest record within the newest closed segment holding it, not its newest",
			"buggy": "        for offset, record in closed[position].records():\n",
			"fixed": "        for offset, record in reversed(list(closed[position].records())):\n"
		},
		{
			"path": "segstore/segment.py",
			"what": "compacted segments get fresh ids above the active segment, so recovery replays old data after newer writes",
			"buggy": "        start = self._next_id\n        self._next_id += len(self._segments)\n        return iter(range(start, self._next_id))\n",
			"fixed": "        return iter(sorted(segment.id for segment in self.closed()))\n"
		}
	],
	"scheduler": [
		{
			"path": "jobqueue/pool.py",
			"what": "attempts ending at the same tick complete in submission order instead of the order they started",
			"buggy": "heapq.heappush(self._running, (end, job.seq, job.id))",
			"fixed": "heapq.heappush(self._running, (end, self._starts, job.id))"
		},
		{
			"path": "jobqueue/queue.py",
			"what": "a stale heap entry of a cancelled job is taken for the resubmitted job with the same id",
			"buggy": "return job is not None and job.state == QUEUED",
			"fixed": "return job is not None and job.seq == entry[-2] and job.state == QUEUED"
		},
		{
			"path": "jobqueue/backoff.py",
			"what": "the cap applies one doubling too early when the cap is not base times a power of two",
			"buggy": "self._capped_from = (self.cap // self.base).bit_length() - 1",
			"fixed": "self._capped_from = (self.cap // self.base).bit_length()"
		}
	],
	"patch": [
		{
			"path": "linediff/unified.py",
			"what": "a hunk body line that starts with '--- ' or '+++ ' (a removed '-- ...' or added '++ ...' line) is skipped as a file header",
			"buggy": "        if line.startswith((\"--- \", \"+++ \")):\n",
			"fixed": "        if not (old_left or new_left) and line.startswith((\"--- \", \"+++ \")):\n"
		},
		{
			"path": "linediff/hunks.py",
			"what": "change regions exactly 2*context lines apart are split into two touching hunks instead of merged",
			"buggy": "block[0] - groups[-1][-1][1] < 2 * context",
			"fixed": "block[0] - groups[-1][-1][1] <= 2 * context"
		},
		{
			"path": "linediff/apply.py",
			"what": "offset search prefers the later of two equally near positions",
			"buggy": "(expected + step, expected - step) if step",
			"fixed": "(expected - step, expected + step) if step"
		}
	],
	"calendar": [
		{
			"path": "slots/intervals.py",
			"what": "union does not merge intervals that touch ([a, b) and [b, c) stay two intervals)",
			"buggy": "            if out and start < out[-1][1]:\n",
			"fixed": "            if out and start <= out[-1][1]:\n"
		},
		{
			"path": "slots/attendee.py",
			"what": "busy blocks just outside the window are dropped before their buffer is applied",
			"buggy": "        nearby = self._busy.clip(start, end)\n",
			"fixed": "        nearby = self._busy.clip(start - self.buffer, end + self.buffer)\n"
		},
		{
			"path": "slots/search.py",
			"what": "slot starts are aligned to a grid anchored at `earliest` instead of multiples of granularity in UTC",
			"buggy": "    grid = earliest\n",
			"fixed": "    grid = align_up(earliest, granularity)\n"
		}
	],
	"wire": [
		{
			"path": "frames/record.py",
			"what": "a sint field rejects -2**63 (the range check uses bit_length, which is 64 for the most negative value)",
			"buggy": "        if kind == \"sint\" and value.bit_length() > 63:\n",
			"fixed": "        if kind == \"sint\" and not -(1 << 63) <= value < (1 << 63):\n"
		},
		{
			"path": "frames/varint.py",
			"what": "zigzag uses the 64-bit formula, wrong for packed-series differences beyond 64 bits",
			"buggy": "    return (value << 1) ^ (value >> 63)\n",
			"fixed": "    return value << 1 if value >= 0 else ((-value) << 1) - 1\n"
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
			"path": "depgraph/graph.py",
			"what": "a dependency listed twice is kept twice, so ordering releases its dependent too early",
			"buggy": "        self._declared[name] = declared\n",
			"fixed": "        self._declared[name] = list(dict.fromkeys(declared))\n"
		},
		{
			"path": "depgraph/rebuild.py",
			"what": "dirtied dependents are queued first-in first-out instead of in build order, so a target can be built before a dependency that changes later",
			"buggy": "                pending.append(dependent)\n",
			"fixed": "                pending.append(dependent)\n                pending.sort(key=position.__getitem__)\n"
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
