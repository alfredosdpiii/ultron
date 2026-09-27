/**
 * Reference solutions for tasks-delegation.mjs, used only by `eval-quality.mjs --tasks delegation --self-check`.
 * Never shown to the agents.
 * This header covers `six-services`; `six-services-deep` has its own section (calibration included) further down.
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
