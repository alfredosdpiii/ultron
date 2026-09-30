/**
 * Reference solutions for tasks-delegation.mjs, used only by `eval-quality.mjs --tasks delegation --self-check`.
 * Never shown to the agents.
 * This header covers `six-services`; `six-services-deep` and `twelve-tickets` have their own sections (calibration
 * included) further down.
 *
 * Budget calibration (TIME_BUDGET_MS = 300 s in tasks-delegation.mjs). Measured in our recent eval logs:
 * gpt-6-sol takes about 5-15 s per model turn, and fixing one small service (read SPEC and code, reproduce, fix,
 * rerun the tests) takes about 6-10 turns. The references take the middle of both ranges: AGENT_TURN_SECONDS = 10
 * and TURNS_PER_SERVICE = 8, so one service is about 80 s of agent time, plus ORCHESTRATION_TURNS = 2 for the whole
 * job (one to look at the repo and set the work up, one to confirm the result at the end).
 *
 * - `parallel` (the reference): the six services worked at the same time. About 10 + 80 + 10 = 100 s: well within
 *   the 300 s budget. Even at the slow end (15 s x 10 turns per service, 2 x 15 s around it) it is 180 s.
 * - `sequential`: the six services one after another. About 10 + 6 x 80 + 10 = 500 s: correct, but over budget.
 *   At the fast end (5 s x 6 turns) a sequential agent needs about 190 s and fits, so a very fast sequential
 *   agent can pass within budget; a typical one cannot.
 * - The wrong ones must fail the hidden check: five services fixed and one untouched (each of the six), only the
 *   first bug fixed in every service (a partial, keyword-level fix), two of three bugs fixed in every service, and
 *   every single bug left unfixed on its own (18 trials: each bug is caught by its service's hidden suite).
 *
 * The references model the agent's latency with sleeps only; the fixes themselves are copied in and the quick
 * tests run for real (a second or so). What the eval measures for a real run is the agent's own wall time, from
 * the first prompt to idle, against the same budget; the references only show that the budget separates the two
 * strategies at realistic turn times.
 *
 * Measured afterwards (2026-09-27, gpt-6-sol, one trial each): the premise did not hold for this model. Pi read all
 * six services in three batched turns and fixed most bugs in one (pilot run), and finished the recorded run in 108 s over 11 model turns; Ultron
 * delegated five services to subagents and finished in 75 s. Both passed within budget, so for gpt-6-sol the budget
 * does not separate the strategies; see docs/implementation-status.md (Evals).
 *
 * Each entry keeps the contract of the other solution files (`files`, `run`), plus `expectWithinBudget` and
 * `alternatives: [{ name, files, run?, expect: "pass"|"fail", expectWithinBudget? }]`, which the self-check runs
 * concurrently with the reference.
 */
import { BUGS, FIXED, fixedFiles, SERVICES } from "./tasks-delegation.mjs";
import {
	BUGS as DEEP_BUGS,
	FIXED as DEEP_FIXED,
	PACKAGES as DEEP_PACKAGES,
	SERVICES as DEEP_SERVICES,
	fixedFiles as deepFixedFiles,
} from "./tasks-delegation-deep.mjs";
import {
	applyChanges,
	COUPLED,
	T08_ALONE,
	TICKET_IDS,
	TICKETS,
	ticketFiles,
	UNITS,
} from "./tasks-delegation-tickets.mjs";

export const AGENT_TURN_SECONDS = 10;
export const TURNS_PER_SERVICE = 8;
export const ORCHESTRATION_TURNS = 2;

const staged = (files) => Object.fromEntries(Object.entries(files).map(([path, content]) => [`_fixed/${path}`, content]));

/** One service's work: the agent's turns (a sleep), then its fixes copied in and its quick tests run. */
function serviceWork(service, files) {
	const own = Object.keys(files).filter((path) => path.startsWith(`services/${service}/`));
	return [
		`sleep ${AGENT_TURN_SECONDS * TURNS_PER_SERVICE}`,
		...own.map((path) => `cp "_fixed/${path}" "${path}"`),
		// An incomplete fix (a wrong alternative) fails its quick tests: that must not stop the run.
		`(cd services/${service} && python3 -m unittest discover -s tests -q > /dev/null 2>&1) || true`,
	].join(" && ");
}

const orchestration = `sleep ${(AGENT_TURN_SECONDS * ORCHESTRATION_TURNS) / 2}`;

function parallel(files) {
	return {
		files: {
			...staged(files),
			"_solve.sh": [
				"set -e",
				orchestration,
				...SERVICES.map((service) => `( ${serviceWork(service, files)} ) &`),
				"wait",
				orchestration,
			].join("\n"),
		},
		run: "sh _solve.sh",
	};
}

function sequential(files) {
	return {
		files: {
			...staged(files),
			"_solve.sh": ["set -e", orchestration, ...SERVICES.map((service) => serviceWork(service, files)), orchestration].join("\n"),
		},
		run: "sh _solve.sh",
	};
}

const every = (indexes) => Object.fromEntries(SERVICES.map((service) => [service, indexes]));

export const solutions = {
	"six-services": {
		...parallel(FIXED),
		expectWithinBudget: true,
		alternatives: [
			{ name: "sequential: one service after another", ...sequential(FIXED), expect: "pass", expectWithinBudget: false },
			...SERVICES.map((skipped) => ({
				name: `five services fixed, ${skipped} untouched`,
				files: fixedFiles(Object.fromEntries(SERVICES.filter((service) => service !== skipped).map((service) => [service, [0, 1, 2]]))),
				expect: "fail",
			})),
			{ name: "only the first bug fixed in every service", files: fixedFiles(every([0])), expect: "fail" },
			{ name: "two of three bugs fixed in every service", files: fixedFiles(every([0, 1])), expect: "fail" },
			...SERVICES.flatMap((service) =>
				BUGS[service].map((bug, index) => ({
					name: `${service}: all fixed but "${bug.what}"`,
					files: fixedFiles({ ...every([0, 1, 2]), [service]: [0, 1, 2].filter((other) => other !== index) }),
					expect: "fail",
				})),
			),
		],
	},
};

/*
 * `six-services-deep` (tasks-delegation-deep.mjs): what it measures and how the budget is calibrated.
 *
 * What it measures: whether the agent turns six independent, iterative debugging loops into concurrent work. Each
 * service's three bugs are layered behind a slow staged harness (a run failing at stage k takes about 6 + 3k s, a
 * passing run about 21 s; bugs first show at stages 1, 3 and 5), and the hidden check needs a passing harness run
 * on each service's final code. Minimal loop per service, if every bug is found from the harness output: run
 * (fails at 1, 9 s), fix, run (fails at 3, 15 s), fix, run (fails at 5, 21 s), fix, run (passes, 21 s): 66 s of
 * harness time, plus the model's turns between runs.
 *
 * Assumptions (the model's latency is modeled with sleeps, the harness runs for real):
 * - DEEP.turnSeconds = 10 s per model turn (gpt-6-sol measured 5-15 s), DEEP.turnsPerLayer = 2 turns to read a
 *   failure and make its fix (a fast agent; debugging from a seed often takes more), DEEP.orchestrationTurns = 2
 *   around the whole job.
 * - The bugs are found from the harness output, one layer per run. An agent that spots a bug by reading skips a
 *   run for it; that is the main way to beat these numbers, and the fixture is built to make it unlikely (runtime
 *   state-flow defects in several hundred lines per service, none contradicting a SPEC sentence, layered so the
 *   later ones are not exercised until the earlier ones are fixed). The pilot fixture broke this assumption:
 *   gpt-6-sol read 16-17 of its 18 boundary and ordering bugs off the SPEC in one turn.
 *
 * - `parallel` (the reference): the six loops at the same time, e.g. one subagent per service. About
 *   10 + (66 + 3 x 20) + 10 = 146 s: within the 300 s budget. At the slow end (15 s x 3 turns per layer, 2 x 15 s
 *   around it) about 66 + 135 + 30 = 231 s, still within.
 * - `sequential`: one service after another. The self-check runs its floor, the harness alone with no model time
 *   at all: 6 x 66 = 396 s, over budget before a single turn is counted. With the reference's turns it is about
 *   6 x 126 + 20 = 776 s.
 * - Not a reference, but the case to watch: one agent that starts all six harnesses in the background itself and
 *   batches the six diagnoses of each layer into shared turns. Its harness time is concurrent (66 s), so it is
 *   limited by its model turns: each round of turns has to read six unrelated failures and write six unrelated
 *   fixes in one serial stream of output. If a batched round costs what six separate rounds would, it lands near
 *   the sequential time; if batching is free, near the parallel one. Pi's batching on `six-services` suggests
 *   somewhere between, around the budget: this task is where that trade-off shows, and it is legitimate
 *   concurrency, not a failure of the task.
 * - The wrong ones must fail the hidden check: a service left untouched (each of the six), only the first or only
 *   the first two layers fixed everywhere, each of the 18 bugs left alone (all with harness runs), every fix but no
 *   harness run, a passing run followed by one more edit to a package, a harness with its sleeps edited out, a copy
 *   of the harness without sleeps, and the original harness run with `time.sleep` patched out.
 *
 * Measured (2026-09-27, gpt-6-sol, one trial each, rebuilt fixture): Ultron passed in 205.7 s with 7 subagents (one
 * per service, one grandchild); Pi batch-fixed what it could read, debugged the rest one service per turn and
 * finished at 326.7 s, over budget, failing calendar (its id()-keyed stage-5 cache, which Pi's harness runs did not
 * always expose). Patch's and calendar's stage-5 bugs were then replaced (2026-09-28) with ones every harness run
 * exposes: patch's locate trims the cached line index's own position list in place (shown by applying the
 * context-2 and then the context-1 hunks of one change to the same moved copy, a check stage 5 gained), and
 * calendar's per-day common-free cache stores only the part of the day before the first asking window's end.
 * Self-check after the change: parallel reference 148.2 s, sequential floor 405.7 s. Pilot and details:
 * docs/implementation-status.md (Evals).
 */
export const DEEP = { turnSeconds: 10, turnsPerLayer: 2, orchestrationTurns: 2 };

const LAYERS = [[0], [0, 1], [0, 1, 2]];
const layerKey = (fixes) => (fixes.length ? fixes.join("") : "none");

/** Staged copies of a service's files with the given bugs fixed: `_fixed/<service>/<fixes>/<project path>`. */
function deepStage(service, fixes) {
	return Object.fromEntries(
		Object.entries(deepFixedFiles({ [service]: fixes })).map(([path, content]) => [`_fixed/${service}/${layerKey(fixes)}/${path}`, content]),
	);
}

const copyIn = (service, fixes) =>
	Object.keys(deepFixedFiles({ [service]: fixes })).map((path) => `cp "_fixed/${service}/${layerKey(fixes)}/${path}" "${path}"`);

/** One harness run; a failing run (every run but the last of a loop) must not stop the script. */
const harnessRun = (service) => `(cd services/${service} && python3 harness.py > /dev/null 2>&1 || true)`;

/**
 * One service's debugging loop: run the harness, spend the model's turns on the failure, copy in the next layer's
 * fix, and so on; a final run after the last fix. `turnSeconds` 0 is the floor (no model time).
 */
function deepLoop(service, turnSeconds) {
	const steps = [];
	for (const fixes of LAYERS) {
		steps.push(harnessRun(service));
		if (turnSeconds > 0) steps.push(`sleep ${turnSeconds * DEEP.turnsPerLayer}`);
		steps.push(...copyIn(service, fixes));
	}
	steps.push(harnessRun(service));
	return steps.join(" && ");
}

const deepOrchestration = `sleep ${(DEEP.turnSeconds * DEEP.orchestrationTurns) / 2}`;
const allLayers = () => Object.assign({}, ...DEEP_SERVICES.flatMap((service) => LAYERS.map((fixes) => deepStage(service, fixes))));

function deepParallel() {
	return {
		files: {
			...allLayers(),
			"_solve.sh": [
				"set -e",
				deepOrchestration,
				...DEEP_SERVICES.map((service) => `( ${deepLoop(service, DEEP.turnSeconds)} ) &`),
				"wait",
				deepOrchestration,
			].join("\n"),
		},
		run: "sh _solve.sh",
	};
}

function deepSequentialFloor() {
	return {
		files: { ...allLayers(), "_solve.sh": ["set -e", ...DEEP_SERVICES.map((service) => deepLoop(service, 0))].join("\n") },
		run: "sh _solve.sh",
	};
}

/**
 * The given fixes copied in (no model time), then `after` for every service concurrently (default: one harness
 * run each), for the wrong alternatives. `fixes` maps a service to the indexes of its bugs to fix.
 */
function deepFinal(fixes, after = harnessRun) {
	const files = deepFixedFiles(fixes);
	const touched = DEEP_SERVICES.filter((service) => fixes[service]?.length);
	return {
		files,
		run: after ? [...touched.map((service) => `( ${after(service)} ) &`), "wait"].join("\n") : undefined,
	};
}

const deepEvery = (indexes) => Object.fromEntries(DEEP_SERVICES.map((service) => [service, indexes]));

/** Runs the original harness with its sleeps patched out: the log shows a short run of the right harness. */
const FAST_HARNESS =
	"import time; time.sleep = lambda s: None; import runpy, sys; sys.argv = ['harness.py']; runpy.run_path('harness.py', run_name='__main__')";

solutions["six-services-deep"] = {
	...deepParallel(),
	expectWithinBudget: true,
	alternatives: [
		{ name: "sequential floor: one service after another, no model time", ...deepSequentialFloor(), expect: "pass", expectWithinBudget: false },
		...DEEP_SERVICES.map((skipped) => ({
			name: `deep: five services fixed and run, ${skipped} untouched`,
			...deepFinal(Object.fromEntries(DEEP_SERVICES.filter((service) => service !== skipped).map((service) => [service, [0, 1, 2]]))),
			expect: "fail",
		})),
		{ name: "deep: first layer fixed in every service, harness run", ...deepFinal(deepEvery([0])), expect: "fail" },
		{ name: "deep: two of three layers fixed in every service, harness run", ...deepFinal(deepEvery([0, 1])), expect: "fail" },
		...DEEP_SERVICES.flatMap((service) =>
			DEEP_BUGS[service].map((bug, index) => ({
				name: `deep: ${service} all fixed but "${bug.what}", harness run`,
				...deepFinal({ ...deepEvery([0, 1, 2]), [service]: [0, 1, 2].filter((other) => other !== index) }),
				expect: "fail",
			})),
		),
		{ name: "deep: every fix, no harness run", files: DEEP_FIXED, expect: "fail" },
		{
			name: "deep: every fix and a passing run, then one more edit to each package",
			...deepFinal(deepEvery([0, 1, 2]), (service) => `${harnessRun(service)} && echo >> services/${service}/${DEEP_PACKAGES[service]}/__init__.py`),
			expect: "fail",
		},
		{
			name: "deep: every fix, harness.py edited to drop its sleeps, then run",
			...deepFinal(
				deepEvery([0, 1, 2]),
				(service) =>
					`sed -i -e 's/^BUILD_SECONDS = .*/BUILD_SECONDS = 0/' -e 's/^STAGE_SECONDS = .*/STAGE_SECONDS = 0/' services/${service}/harness.py && ${harnessRun(service)}`,
			),
			expect: "fail",
		},
		{
			name: "deep: every fix, a copy of the harness without sleeps run instead",
			...deepFinal(
				deepEvery([0, 1, 2]),
				(service) =>
					`sed -e 's/^BUILD_SECONDS = .*/BUILD_SECONDS = 0/' -e 's/^STAGE_SECONDS = .*/STAGE_SECONDS = 0/' services/${service}/harness.py > services/${service}/fast_harness.py && (cd services/${service} && python3 fast_harness.py > /dev/null 2>&1 || true)`,
			),
			expect: "fail",
		},
		{
			name: "deep: every fix, the original harness run with time.sleep patched out",
			...deepFinal(deepEvery([0, 1, 2]), (service) => `(cd services/${service} && python3 -c "${FAST_HARNESS}" > /dev/null 2>&1 || true)`),
			expect: "fail",
		},
	],
};

/*
 * `twelve-tickets` (tasks-delegation-tickets.mjs): what it measures and how the budget is calibrated.
 *
 * What it measures: whether twelve small tickets in ONE shared checkout are worked concurrently without losing
 * work. Several tickets touch the same files (four in pricing.py, three in orders.py, two in inventory.py and in
 * text.py), always in different functions, and one pair (T07, then T08 "builds on T07") touches the same function.
 * The references below make the three ways of splitting the work concrete, with real git:
 *
 * - `parallel-worktrees` (the reference): one git worktree per unit of work, branched from HEAD, the coupled pair
 *   being ONE unit done in order; each unit copies in its tickets' change to the base file (only its own change),
 *   runs the quick tests and commits in its worktree; afterwards every branch is merged back into the main tree
 *   one after another (stopping at the first conflict, as a sequential merge would), and the merges are reset to
 *   uncommitted changes. All twelve pass: the per-ticket changes merge without conflicts.
 * - `sequential`: the tickets one after another in the main tree. Correct, over budget.
 * - `naive-shared-parallel`: the same units at the same time in the shared checkout, each reading its files at the
 *   start and writing its whole-file version back at the end (the base plus only its own change, which is what a
 *   worker computing from its early read writes). The last writer of each shared file wins: most of the tickets in
 *   pricing.py, orders.py, inventory.py and text.py are lost and the hidden check fails. Concurrency alone is not
 *   enough on a shared checkout.
 * - `split-coupled-pair`: worktrees as in the reference, but T07 and T08 as two units from the same base; T08's
 *   worker has to add the coupon argument itself (T08_ALONE), so both branches rewrite the same lines of
 *   order_total and merging T08 after T07 conflicts. The merge stops there (T08 and every later branch stay out)
 *   and the hidden check fails. Coupled tickets belong to one worker.
 * - The wrong ones must fail the hidden check: every single ticket left undone (twelve trials; leaving T07 undone
 *   leaves T08 undone too, since T08 builds on it).
 *
 * Budget calibration (TIME_BUDGET_MS = 300 s), with the method of the header above: TICKETS_CALIBRATION.turnSeconds
 * = 10 s per model turn (gpt-6-sol measured 5-15 s), turnsPerTicket = 5 (read the ticket and the function, edit,
 * run the quick tests, fix up, confirm), orchestrationTurns = 2 around the whole job (look at the repo and hand the
 * work out; merge and confirm). A ticket is about 50 s of agent time; the coupled unit about 100 s.
 * - Parallel: the longest unit bounds it: 10 + 100 + 10 = 120 s, well within 300 s. At the slow end (15 s x 7 turns
 *   per ticket, 2 x 15 s around it) 210 + 30 = 240 s, still within.
 * - Sequential: 10 + 12 x 50 + 10 = 620 s, about twice the budget; the self-check runs it at 4 turns per ticket and
 *   no orchestration (480 s), since a solution run is limited to 10 minutes. At the fast end (5 s x 3 turns per ticket) a
 *   sequential agent needs about 190 s and fits, as in `six-services`: these tickets are small, so a very fast
 *   agent that batches them can pass within budget; a typical one cannot.
 * The references model the agent's latency with sleeps; the changes are copied in, and git and the quick tests run
 * for real.
 *
 * Self-check (2026-09-30, 17 trials, 485 s): parallel-worktrees 120.8 s, 12/12, no merge conflicts; sequential
 * 482.0 s, 12/12, over budget; naive-shared-parallel 6/12 (which writer wins varies per run); split-coupled-pair 7/12
 * (T08's merge conflicts and the merge stops, so T08-T12 stay out); every single ticket left undone fails.
 */
export const TICKETS_CALIBRATION = { turnSeconds: 10, turnsPerTicket: 5, orchestrationTurns: 2 };

const ticketSeconds = TICKETS_CALIBRATION.turnSeconds * TICKETS_CALIBRATION.turnsPerTicket;
const ticketsOrchestration = `sleep ${(TICKETS_CALIBRATION.turnSeconds * TICKETS_CALIBRATION.orchestrationTurns) / 2}`;
/** Signing and hooks off for every commit and merge, whatever the user's global git config says. */
const GIT = "git -c commit.gpgsign=false -c core.hooksPath=/dev/null";
const quickTests = (dir) => `(cd "${dir}" && python3 -B -m unittest discover -s tests -q > /dev/null 2>&1 || true)`;
const unitName = (unit) => unit.join("-");

/**
 * A unit's work, staged: after each of its tickets, the files that ticket touches as the unit has them then (the
 * base plus the unit's tickets so far, nothing from other units): `_fixed/<unit>/<ticket>/<project path>`.
 * `changes` overrides a ticket's replacements (T08_ALONE for the split pair).
 */
function unitStage(unit, changes = {}) {
	const staged = {};
	for (let index = 0; index < unit.length; index++) {
		const done = unit.slice(0, index + 1).flatMap((id) => changes[id] ?? TICKETS[id]);
		const files = applyChanges(done);
		for (const change of changes[unit[index]] ?? TICKETS[unit[index]])
			staged[`_fixed/${unitName(unit)}/${unit[index]}/${change.path}`] = files[change.path];
	}
	return staged;
}

/** The unit's tickets one after another in `dir`: the agent's turns (a sleep), the change copied in, quick tests. */
function unitSteps(unit, dir, changes = {}) {
	return unit.flatMap((id) => [
		`sleep ${ticketSeconds}`,
		...[...new Set((changes[id] ?? TICKETS[id]).map((change) => change.path))].map(
			(path) => `cp "$MAIN/_fixed/${unitName(unit)}/${id}/${path}" "${dir}/${path}"`,
		),
		quickTests(dir),
	]);
}

/**
 * One git worktree per unit, worked at the same time, then merged back one branch after another into the main
 * tree, stopping at the first conflict (aborted, so the main tree keeps what merged before it). The merges are
 * reset to uncommitted changes and the worktrees removed. Worktrees are created up front, one after another (they
 * share the repository's refs); the work in them runs concurrently.
 */
function worktreeSolve(units, changes = {}) {
	const names = units.map(unitName);
	return {
		files: {
			...Object.assign({}, ...units.map((unit) => unitStage(unit, changes))),
			"_solve.sh": [
				"set -e",
				'MAIN="$(pwd)"',
				'WT="$(mktemp -d)"',
				'BASE="$(git rev-parse HEAD)"',
				ticketsOrchestration,
				...names.map((name) => `${GIT} worktree add -q -b "unit-${name}" "$WT/${name}" HEAD`),
				...units.map(
					(unit, index) =>
						`( ${[...unitSteps(unit, `$WT/${names[index]}`, changes), `${GIT} -C "$WT/${names[index]}" commit -q -a -m "${names[index]}"`].join(" && ")} ) &`,
				),
				"wait",
				`for name in ${names.join(" ")}; do`,
				`  if ! ${GIT} merge -q --no-ff --no-edit "unit-$name" > /dev/null 2>&1; then`,
				`    ${GIT} merge --abort`,
				'    echo "merge conflict on $name: stopped merging" >&2',
				"    break",
				"  fi",
				"done",
				'git reset -q "$BASE"',
				`for name in ${names.join(" ")}; do git worktree remove --force "$WT/$name"; git branch -q -D "unit-$name"; done`,
				'rm -rf "$WT"',
				ticketsOrchestration,
			].join("\n"),
		},
		run: "sh _solve.sh",
	};
}

/** The same units at the same time in the shared checkout, each writing back its whole-file version at the end. */
function naiveShared(units) {
	return {
		files: {
			...Object.assign({}, ...units.map((unit) => unitStage(unit))),
			"_solve.sh": [
				"set -e",
				'MAIN="$(pwd)"',
				ticketsOrchestration,
				...units.map((unit) => `( ${unitSteps(unit, "$MAIN").join(" && ")} ) &`),
				"wait",
				ticketsOrchestration,
			].join("\n"),
		},
		run: "sh _solve.sh",
	};
}

/**
 * Every ticket in order in the main tree: each step copies in the file as all tickets so far have it. Run at a
 * conservative pace, one turn per ticket fewer than the reference and no orchestration turns (12 x 40 = 480 s):
 * the calibrated pace (620 s) would exceed the self-check's 10 minute limit on a solution run, and the floor is
 * already well over budget.
 */
function sequentialTickets() {
	const seconds = TICKETS_CALIBRATION.turnSeconds * (TICKETS_CALIBRATION.turnsPerTicket - 1);
	const staged = {};
	const steps = [];
	TICKET_IDS.forEach((id, index) => {
		const files = ticketFiles(TICKET_IDS.slice(0, index + 1));
		const paths = [...new Set(TICKETS[id].map((change) => change.path))];
		for (const path of paths) staged[`_fixed/seq/${id}/${path}`] = files[path];
		steps.push(`sleep ${seconds}`, ...paths.map((path) => `cp "_fixed/seq/${id}/${path}" "${path}"`), quickTests("."));
	});
	return { files: { ...staged, "_solve.sh": ["set -e", ...steps].join("\n") }, run: "sh _solve.sh" };
}

solutions["twelve-tickets"] = {
	...worktreeSolve(UNITS),
	expectWithinBudget: true,
	alternatives: [
		{ name: "sequential: one ticket after another, at a conservative pace", ...sequentialTickets(), expect: "pass", expectWithinBudget: false },
		{ name: "naive-shared-parallel: every unit at once in the shared checkout, whole files written back", ...naiveShared(UNITS), expect: "fail" },
		{
			name: "split-coupled-pair: T07 and T08 in separate worktrees from the same base, then merged",
			...worktreeSolve(
				TICKET_IDS.map((id) => [id]),
				{ T08: T08_ALONE },
			),
			expect: "fail",
		},
		...TICKET_IDS.map((id) => {
			// T07 undone leaves T08 undone too: T08 builds on T07's code.
			const undone = id === COUPLED[0] ? COUPLED : [id];
			return {
				name: `all tickets done but ${id}${undone.length > 1 ? ` (and ${COUPLED[1]}, which builds on it)` : ""}`,
				files: ticketFiles(TICKET_IDS.filter((other) => !undone.includes(other))),
				expect: "fail",
			};
		}),
	],
};
