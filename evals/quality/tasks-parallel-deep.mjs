/**
 * `slow-pipeline-deep`, the second PARALLEL task (tasks-parallel.mjs lists it after `slow-suite-and-fixes`): several
 * slow steps whose outputs feed other slow steps, so the fastest plan reacts to each result as it arrives.
 *
 * Why a second task: `slow-suite-and-fixes` tied on both models (gpt-6-sol: Pi 162 s, Ultron 162 s; glm-5.3-flash:
 * Pi 175 s, Ultron 196 s). It has one slow step with no inputs and no outputs anyone waits for, so starting it with
 * `&` and fixing two bugs meanwhile is already the optimal plan, and a strong model does the fixes in seconds.
 *
 * What this task measures: orchestration of long-running work with data dependencies. The project (an order-sync
 * client and a ledger, fixture fixtures/slow-pipeline/) has four slow ops steps, all simulated remote systems
 * behind fixed sleeps, each with real checks and a logged, nonce-signed run:
 *
 *   provision (35 s) --tenant id, limit--> config edit --> loadtest (150 s, on the final client/ code)
 *   snapshot (120 s) --snapshot id-------------------> replay (45 s, on the final ledger/ code)
 *   plus one unit-tested bug in client/uploader.py (before loadtest) and one in ledger/ledger.py (before replay).
 *
 * The provisioned tenant id and batch limit and the snapshot id are random per run, so no step can be skipped or
 * its output guessed: the load test refuses a tenant that was never provisioned (or the wrong limit), and the replay
 * a snapshot that was never taken. The winning plan starts provision and snapshot at once, fixes the uploader while
 * they run, configures the client and starts the load test as soon as provision reports (about 45 s in), fixes the
 * ledger, starts the replay as soon as the snapshot reports (about 130 s in), and writes REPORT.md when both end:
 * about 215 s with 10 s model turns. Why the other plans miss the 270 s budget:
 *
 * - Sequential (each step blocking, in any order): the steps alone are 350 s; about 450 s with the turns.
 * - Everything started with `&` but handled together ("wait for all of it, then continue"): the load test can only
 *   start after the slowest first-stage step (snapshot, 120 s), so about 300 s. A single agent that backgrounds
 *   everything still has to notice provision's result at 35 s while the snapshot is running, and act on it.
 * - Starting the load test or the replay before its fix, or editing afterwards: the run does not count (below), so
 *   it costs a full extra cycle (150 s for the load test).
 *
 * The prompt states each step's duration, the budget, and that doing the steps one after another takes more than
 * 7 minutes; it names no runtime feature or strategy (scripts/eval-parallel-deep.test.mjs forbids the words).
 *
 * Evidence (the six-services-deep pattern): every ops run appends {nonce, start, end, result, and for the load test
 * and the replay a digest of client/ or ledger/ at the run's start and end} to .ops/<step>.jsonl and prints the
 * nonce in its summary line. The hidden check accepts the work only if ops/ holds exactly its original files
 * (sha256 per file, nothing added: a module added next to the scripts could shadow one), the unit tests and hidden
 * cases pass, the log holds a passing load test of at least 140 s on the current client/ source against a tenant
 * that a provision run of at least 30 s created with that limit, and a passing replay of at least 40 s on the current
 * ledger/ source of a snapshot that a snapshot run of at least 110 s took (same data digest), and REPORT.md quotes
 * the summary line of both runs. The ops scripts bind the clock and sleep before importing project code, and import
 * project code only after their own modules, so client/ or ledger/ code cannot patch the sleeps out or shadow an ops
 * module. Residual gaps, as in the other nonce-logged tasks: forging log lines (and a snapshot file with a matching
 * digest), faking the system clock, or patching the interpreter (sitecustomize, PYTHONPATH) would pass the evidence
 * part; that is forging the check, not solving the task, and is not guarded against.
 *
 * Measured (2026-09-29, gpt-6-sol, one trial each): both passed within budget near the floor, Ultron 212 s, Pi
 * 220 s. Both started provision and snapshot at once and then blocked on the next result they needed (Pi with a
 * bash `while [ ! -f exit ]` loop, Ultron with `await job.result()`), so the task separates plans, not these agents.
 * See docs/implementation-status.md (Evals).
 *
 * The hidden check prints a JSON line {"ops", "tests", "loadtest", "replay", "report"} recorded as `metrics`. Pass
 * or fail is correctness only; `timeBudgetMs` (270 s) gives `withinBudget` and never gates. Reference solutions and
 * the timing model: tasks-parallel-solutions.mjs. The fixture is pinned by one sha256; the reference fixes are exact
 * replacements (BUGS). Do not edit after measurements exist: add a new id instead.
 */
import { createHash } from "node:crypto";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { fixtureDigest, readTree } from "./fixture-tree.mjs";

/** Reported per run; 4.5 minutes, as the prompt says. */
export const TIME_BUDGET_MS = 270_000;

/** Durations of the ops steps in seconds, frozen in ops/lib/*.py, and the minimum a logged run must last. */
export const STEPS = {
	provision: { seconds: 35, minSeconds: 30 },
	snapshot: { seconds: 120, minSeconds: 110 },
	loadtest: { seconds: 150, minSeconds: 140 },
	replay: { seconds: 45, minSeconds: 40 },
};

const FIXTURE = fileURLToPath(new URL("./fixtures/slow-pipeline/", import.meta.url));
/** sha256 over every fixture file (path and content, sorted by path); a changed fixture refuses to load. */
const FIXTURE_SHA256 = "e5b55ccfb8f22a83fb565d162845bc06e67891b41457536f4391c629af275318";

function loadFixture() {
	const project = readTree(join(FIXTURE, "project"));
	const hidden = readTree(join(FIXTURE, "hidden"));
	const digest = fixtureDigest(project, hidden);
	if (digest !== FIXTURE_SHA256) throw new Error(`fixtures/slow-pipeline changed: sha256 ${digest}, frozen ${FIXTURE_SHA256}`);
	return { project, hidden };
}

const { project: FILES_RAW, hidden: HIDDEN_RAW } = loadFixture();
export const FILES = FILES_RAW;

const sha256 = (text) => createHash("sha256").update(text).digest("hex");

/** Every file under ops/ with its sha256: the hidden check requires exactly these. */
export const OPS_ORIGINALS = Object.fromEntries(
	Object.keys(FILES)
		.filter((path) => path.startsWith("ops/"))
		.sort()
		.map((path) => [path, sha256(FILES[path])]),
);

export const HIDDEN = { ...HIDDEN_RAW, "ops_originals.json": `${JSON.stringify(OPS_ORIGINALS, null, 2)}\n` };

/** The two bugs, as exact replacements in the fixture (`buggy` occurs exactly once in `path`). */
export const BUGS = {
	uploader: {
		path: "client/uploader.py",
		what: "chunks stops before a final partial batch, so the records after the last full batch are never sent",
		buggy: "range(0, len(items) - size + 1, size)",
		fixed: "range(0, len(items), size)",
	},
	ledger: {
		path: "ledger/ledger.py",
		what: "a chargeback credits the fee instead of debiting it",
		buggy: "change = -amount + CHARGEBACK_FEE_CENTS",
		fixed: "change = -(amount + CHARGEBACK_FEE_CENTS)",
	},
};

/** The files with the named bugs fixed (default: both), keyed by project path. */
export function fixedFiles(names = Object.keys(BUGS)) {
	const out = {};
	for (const name of names) {
		const bug = BUGS[name];
		const current = out[bug.path] ?? FILES[bug.path];
		if (current.split(bug.buggy).length !== 2) throw new Error(`${bug.path}: the buggy text must occur exactly once`);
		out[bug.path] = current.replace(bug.buggy, () => bug.fixed);
	}
	return out;
}

export const FIXED = fixedFiles();

export const PROMPT =
	"This repository is an order-sync client and an account ledger (see README.md). Get it ready for release:\n" +
	"1. Provision a staging tenant with `./ops/provision.sh` (about 35 seconds). It prints the new tenant's id and batch limit; set `TENANT` and `MAX_BATCH` in client/config.py to those values.\n" +
	"2. There is one bug in client/uploader.py and one in ledger/ledger.py. Fix both so the unit tests pass (`python3 -m unittest discover -s tests/unit`). Fix the underlying logic, not just the tested cases; hidden tests check more cases. Do not edit the tests or anything under ops/.\n" +
	"3. Load-test the client against the provisioned tenant with `./ops/loadtest.sh` (about 2.5 minutes).\n" +
	"4. Take a production snapshot with `./ops/snapshot.sh` (about 2 minutes; it prints the snapshot's id), then replay it through the ledger with `./ops/replay.sh <snapshot-id>` (about 45 seconds).\n" +
	"5. Put the summary lines that the load test and the replay print at the end (starting with `loadtest:` and `replay:`) verbatim into REPORT.md. Every ops run is logged with a digest of the code it ran: only a passing load test on the final code in client/ and a passing replay on the final code in ledger/ count, so a change after a run needs a new run.\n" +
	"6. Finish as fast as possible: everything must be done within 4.5 minutes. Done strictly one step after another, these steps take more than 7 minutes, so plan the order.";

export const TASK = {
	id: "slow-pipeline-deep",
	category: "parallel",
	build: () => ({ files: { ...FILES }, hidden: { ...HIDDEN } }),
	prompts: [PROMPT],
	verify: "python3 check_pipeline_hidden.py",
	timeBudgetMs: TIME_BUDGET_MS,
};
