/**
 * Reference solutions for tasks-delegation.mjs, used only by `eval-quality.mjs --tasks delegation --self-check`.
 * Never shown to the agents.
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
