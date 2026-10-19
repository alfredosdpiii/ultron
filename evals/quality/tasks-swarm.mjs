/**
 * Frozen SWARM quality tasks: do Ultron's durable many-agent APIs pay off? They are served by the Rust host
 * (ultron-rs; the TypeScript host refuses them): persistent children (`rlm.spawn(..., keep=True)` and follow-ups with
 * `rlm.ask`), the session's agent registry and messages (`swarm.register/route/send/inbox`), its task board
 * (`board.add/claim/update/list`) and shared notes, and delegation briefs with contracts (role, contract, hop).
 * Each task is one where the feature should clearly help and one-shot `rlm.spawn` or solo work should be worse, with
 * a deterministic hidden check that scores correctness and prints a JSON metrics line (recorded as `metrics`). The
 * prompts stay agent-neutral: they describe the job and how it will continue (follow-up questions, follow-up
 * tickets), never a runtime feature.
 *
 * - `follow-ups`: a generated ~51k-line Python application (fixtures/swarm/codebase.mjs, seed FOLLOW_UPS_SEED) with
 *   one dense subsystem, app/dispatch/ (~6.3k lines: 28 stage modules, base limits, region overrides with legacy
 *   decoys, adjust stages that change a record before later checks, disabled and unregistered stages). prompts[0]
 *   asks for an investigation and the active stage order; prompts[1..3] are follow-up questions whose answers need
 *   details of that subsystem (the first rejecting stage and code for a record, the constant behind a region's limit,
 *   the rejection with one stage removed). A persistent child that read the subsystem answers them with `rlm.ask`
 *   while the root's context grows only by question and answer; a root that read the code itself carries it into
 *   every later prompt, and a one-shot child is gone, so the root re-reads. Check: answers.json (4 answers).
 *   Metrics from the driver: root input tokens per prompt, root tool calls in prompts[1..] that name app/dispatch
 *   (re-reads), and swarm API calls in the root's code (keep=True spawns, rlm.ask).
 * - `shared-board`: twelve small tickets in one git repository, several in the same files (fixtures/swarm/board):
 *   ten open at the start and two follow-ups opened only when their parent ticket is marked done
 *   (`python3 tools/done.py T03` opens T11, T07 opens T12). done.py appends to a log in the repository's shared
 *   git directory, so every checkout and worktree writes the same log; a ticket marked twice is duplicate work.
 *   Workers that claim tickets from a shared board (and add the follow-ups to it) neither duplicate nor miss work; a
 *   static split made up front misses the follow-ups or hands them out twice. Check: every ticket's hidden tests,
 *   every ticket marked exactly once. Metrics: `{passed, total, duplicates, unmarked, marks, tickets}` plus the
 *   driver's board calls in the root's code.
 * - `route`: the same generator with three subsystems (dispatch, billing, access; seed ROUTE_SEED, ~49k lines). prompts[0]
 *   asks for all three to be investigated (three readers); prompts[1] and [2] are questions that do not name their
 *   subsystem (a record given by its fields, a stage given by its name). `swarm.route` finds the child that holds
 *   the context and `rlm.ask` asks it; a root without that re-reads. Check: answers.json (3 answers). Metrics as for
 *   follow-ups, re-reads counted over all three subsystem directories, plus swarm.route calls.
 * - `contracts` (OPTIONAL, `optional: "contracts"`: delegation briefs are not exposed to Python yet, so summaries
 *   report it apart; see scripts/eval-quality.mjs): three independent modules, each with a bug whose tempting partial
 *   fix passes the quick test and the reported example but not the module's acceptance check (`check.py <module>`).
 *   A child whose contract requires a passing check cannot report early success. Check: hidden copies of the full
 *   suites plus extra cases.
 *
 * Calibration (tasks-swarm-solutions.mjs): every reference answers from the files and the Python alone, no model.
 * Time budgets follow the delegation set's turn model (about 10 s per model turn): follow-ups and route 15 minutes,
 * shared-board 10, contracts 7; `withinBudget` is reported and never gates.
 * The generated codebases are pinned by sha256 (a changed generator refuses to load); the board and contracts
 * projects are committed fixtures pinned the same way. Do not edit after measurements exist: add a new id instead.
 */
import { createHash } from "node:crypto";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { DOMAINS, generateCodebase, lineCount } from "./fixtures/swarm/codebase.mjs";
import { fixtureDigest, readTree } from "./fixture-tree.mjs";

export const FROZEN_AT = "2026-10-19-swarm";

export const FOLLOW_UPS_SEED = 1101;
export const ROUTE_SEED = 2202;
const FOLLOW_UPS_OPTIONS = { seed: FOLLOW_UPS_SEED, fillerModules: 300, subsystems: { dispatch: { stageCount: 28 } } };
const ROUTE_OPTIONS = {
	seed: ROUTE_SEED,
	fillerModules: 220,
	subsystems: { dispatch: { stageCount: 24 }, billing: { stageCount: 24 }, access: { stageCount: 22 } },
};
/** sha256 over the generated files (path and content, sorted by path). */
const FOLLOW_UPS_SHA256 = "9e1117a7f80899db631a68199a9d8e688c9898c6d51646ed7b7aa86ad6075b37";
const ROUTE_SHA256 = "7eb77e6979bd89c9676eb41eb7a339f636e02813290e4a0b8aa48e29011ea828";
const BOARD_SHA256 = "4b2946c289e35b3892b418fd7f585888e6e8d66ac5560125280636faf6587852";
const CONTRACTS_SHA256 = "70a57d74e848632c1517b69ceb2ec159b31db095c6bef771dd23cb24cf53da27";

export const TIME_BUDGETS_MS = { "follow-ups": 900_000, "shared-board": 600_000, route: 900_000, contracts: 420_000 };

export function treeDigest(files) {
	const hash = createHash("sha256");
	for (const path of Object.keys(files).sort()) hash.update(`${path}\0${files[path]}\0`);
	return hash.digest("hex");
}

const cache = new Map();
/** A generated codebase, pinned by its digest; memoized (generation takes tens of milliseconds). */
export function codebase(name) {
	if (!cache.has(name)) {
		const [options, frozen] = name === "follow-ups" ? [FOLLOW_UPS_OPTIONS, FOLLOW_UPS_SHA256] : [ROUTE_OPTIONS, ROUTE_SHA256];
		const generated = generateCodebase(options);
		const digest = treeDigest(generated.files);
		if (digest !== frozen) throw new Error(`swarm codebase ${name} changed: sha256 ${digest}, frozen ${frozen}`);
		cache.set(name, generated);
	}
	return cache.get(name);
}

function loadFixture(name, frozen) {
	const dir = fileURLToPath(new URL(`./fixtures/swarm/${name}/`, import.meta.url));
	const project = readTree(join(dir, "project"));
	const hidden = readTree(join(dir, "hidden"));
	const digest = fixtureDigest(project, hidden);
	if (digest !== frozen) throw new Error(`fixtures/swarm/${name} changed: sha256 ${digest}, frozen ${frozen}`);
	return { project, hidden };
}

const recordText = (record) =>
	`region ${record.region} with ${Object.entries(record)
		.filter(([key]) => key !== "region")
		.map(([key, value]) => `${key}=${value}`)
		.join(", ")}`;

/**
 * The hidden check of an answers task: compares answers.json with `expected` ({key: value}; values are a list of
 * stage names, a {"stage", "code"} pair or a {"constant"}), case-insensitively. Prints one line per answer, then
 * {"passed": k, "total": n, "answers": {key: bool}}.
 */
export function answersCheck(expected) {
	return `"""Hidden check: answers.json against the answers computed from the generated code."""
import json
import sys

EXPECTED = json.loads(${JSON.stringify(JSON.stringify(expected))})


def norm(value):
    if isinstance(value, str):
        return value.strip().strip("\`").lower()
    if isinstance(value, list):
        return [norm(item) for item in value]
    if isinstance(value, dict):
        return {str(key).strip().lower(): norm(item) for key, item in value.items()}
    return value


try:
    with open("answers.json", encoding="utf-8") as handle:
        answers = json.load(handle)
    if not isinstance(answers, dict):
        raise ValueError("answers.json is not an object")
except (OSError, ValueError) as error:
    print(f"answers.json unreadable: {error}")
    answers = {}

results = {}
for key, expected in EXPECTED.items():
    got = answers.get(key)
    if isinstance(expected, dict) and isinstance(got, dict):
        ok = all(norm(got.get(field)) == norm(value) for field, value in expected.items())
    else:
        ok = norm(got) == norm(expected)
    results[key] = ok
    print(f"{key}: {'pass' if ok else 'FAIL'} (got {json.dumps(got)[:200]}, expected {json.dumps(expected)[:200]})")
passed = sum(results.values())
print(json.dumps({"passed": passed, "total": len(EXPECTED), "answers": results}, separators=(",", ":")))
sys.exit(0 if passed == len(EXPECTED) else 1)
`;
}

/** follow-ups: the expected answers and the prompts, from the generated model. */
export function followUps() {
	const { files, questions } = codebase("follow-ups");
	const q = questions.dispatch;
	const expected = {
		stages: q.stages,
		q1: q.first,
		q2: { constant: q.constant.constant },
		q3: q.second,
	};
	const lines = Math.round(lineCount(files) / 1000);
	const prompts = [
		`This repository is a large Python application, app/ (about ${lines},000 lines). Its dispatch subsystem, app/dispatch/, decides which shipments are accepted: a pipeline of stages (pipeline.py) with base limits, per-region overrides, and stages that adjust a shipment before later checks see it.\n` +
			"1. Investigate that subsystem thoroughly: how the pipeline runs, every stage's rule, the limits and the region overrides.\n" +
			'2. Write answers.json at the repository root with one key, "stages": the names of the stages that actually run, in the order they run.\n' +
			"3. I will ask you several detailed follow-up questions about this subsystem afterwards; each answer goes into answers.json as well. Do not modify anything under app/.",
		`Follow-up 1: a shipment in ${recordText(q.record)} goes through the dispatch pipeline. Which stage rejects it first, and with which error code? Add "q1": {"stage": "<stage name>", "code": "<error code>"} to answers.json and keep the other keys.`,
		`Follow-up 2: which named constant (as written in the code, e.g. "SOME_NAME") sets the limit the ${q.constant.stage} stage applies to a shipment in region ${q.constant.region}? Add "q2": {"constant": "<NAME>"} to answers.json and keep the other keys.`,
		`Follow-up 3: if the ${q.first.stage} stage were removed from the pipeline, which stage would reject the shipment from follow-up 1, and with which error code? Add "q3": {"stage": "<stage name>", "code": "<error code>"} to answers.json and keep the other keys. Do not change the code.`,
	];
	return { files, expected, prompts };
}

/** route: three subsystems; follow-ups that do not name theirs. */
export function route() {
	const { files, questions } = codebase("route");
	const access = questions.access;
	const billing = questions.billing;
	const expected = {
		stages: Object.fromEntries(Object.entries(questions).map(([domain, q]) => [domain, q.stages])),
		q1: access.first,
		q2: { constant: billing.constant.constant },
	};
	const lines = Math.round(lineCount(files) / 1000);
	const prompts = [
		`This repository is a large Python application, app/ (about ${lines},000 lines), with three rule subsystems: app/dispatch/ (${DOMAINS.dispatch.about}), app/billing/ (${DOMAINS.billing.about}) and app/access/ (${DOMAINS.access.about}). Each is a pipeline of stages (pipeline.py) with base limits, per-region overrides, and stages that adjust a record before later checks see it.\n` +
			"1. Investigate all three subsystems thoroughly: how each pipeline runs, every stage's rule, the limits and the region overrides.\n" +
			'2. Write answers.json at the repository root with one key, "stages": {"dispatch": [...], "billing": [...], "access": [...]}, each the names of the stages that actually run in that subsystem, in the order they run.\n' +
			"3. I will ask you detailed follow-up questions afterwards, without saying which subsystem a question is about; each answer goes into answers.json as well. Do not modify anything under app/.",
		`Follow-up 1: a record in ${recordText(access.record)} goes through the pipeline of the subsystem those fields belong to. Which stage rejects it first, and with which error code? Add "q1": {"stage": "<stage name>", "code": "<error code>"} to answers.json and keep the other keys.`,
		`Follow-up 2: which named constant (as written in the code, e.g. "SOME_NAME") sets the limit the ${billing.constant.stage} stage applies in region ${billing.constant.region}? Add "q2": {"constant": "<NAME>"} to answers.json and keep the other keys.`,
	];
	return { files, expected, prompts };
}

export const BOARD_TICKETS = [...Array.from({ length: 10 }, (_, i) => `T${String(i + 1).padStart(2, "0")}`), "T11", "T12"];
/** Follow-up tickets: opened when the parent is marked done. */
export const BOARD_FOLLOW_UPS = { T03: "T11", T07: "T12" };

const BOARD_PROMPT =
	"This repository is a small Python package, ledger/ (see README.md), with ten tickets under tickets/ (T01.md ... T10.md), each a small feature or bug fix with acceptance criteria.\n" +
	"1. Implement every ticket, including the follow-up tickets that finishing some tickets opens. Hidden tests check every ticket's acceptance criteria in more cases than the quick tests, so implement the behaviour, not just the examples. Do not edit the tickets, the tests or tools/.\n" +
	"2. When a ticket is finished, mark it done from the repository root with `python3 tools/done.py <id>`, exactly once per ticket: a ticket marked twice counts as duplicate work and fails the job, and so does a ticket never marked. done.py prints any follow-up ticket it opens.\n" +
	"3. The tickets are small and mostly independent, though several touch the same files. Run the quick tests with `python3 -m unittest discover -s tests`. Finish as fast as possible: the whole job must be done within 10 minutes.";

const CONTRACTS_PROMPT =
	"This repository holds three small, independent Python modules under lib/ (see README.md), each with a bug report under issues/.\n" +
	"1. Fix all three modules so each behaves exactly as its issue describes. Hidden tests check more cases than the quick tests and the examples in the issues. Do not edit the issues, the tests or check.py.\n" +
	'2. A module is fixed only when `python3 check.py <module>` reports "passed": true for it.\n' +
	"3. Finish as fast as possible: the whole job must be done within 7 minutes.";

export function tasks() {
	return [
		{
			id: "follow-ups",
			category: "swarm",
			build: () => {
				const { files, expected } = followUps();
				return { files: { ...files }, hidden: { "check_answers_hidden.py": answersCheck(expected) } };
			},
			get prompts() {
				return followUps().prompts;
			},
			/** Root tool calls in prompts[1..] whose arguments name these paths count as re-reads. */
			rereadPaths: ["app/dispatch"],
			verify: "python3 check_answers_hidden.py",
			timeBudgetMs: TIME_BUDGETS_MS["follow-ups"],
		},
		{
			id: "shared-board",
			category: "swarm",
			build: () => {
				const { project, hidden } = loadFixture("board", BOARD_SHA256);
				return { files: { ...project }, hidden: { ...hidden } };
			},
			git: true,
			prompts: [BOARD_PROMPT],
			verify: "python3 check_board_hidden.py",
			verifyTimeoutMs: 180_000,
			timeBudgetMs: TIME_BUDGETS_MS["shared-board"],
		},
		{
			id: "route",
			category: "swarm",
			build: () => {
				const { files, expected } = route();
				return { files: { ...files }, hidden: { "check_answers_hidden.py": answersCheck(expected) } };
			},
			get prompts() {
				return route().prompts;
			},
			rereadPaths: ["app/dispatch", "app/billing", "app/access"],
			verify: "python3 check_answers_hidden.py",
			timeBudgetMs: TIME_BUDGETS_MS.route,
		},
		{
			id: "contracts",
			category: "swarm",
			/** Delegation briefs (role, contract, hop) are not exposed to Python yet: summaries report this task apart. */
			optional: "contracts",
			build: () => {
				const { project, hidden } = loadFixture("contracts", CONTRACTS_SHA256);
				return { files: { ...project }, hidden: { ...hidden } };
			},
			prompts: [CONTRACTS_PROMPT],
			verify: "python3 check_contracts_hidden.py",
			verifyTimeoutMs: 120_000,
			timeBudgetMs: TIME_BUDGETS_MS.contracts,
		},
	];
}
