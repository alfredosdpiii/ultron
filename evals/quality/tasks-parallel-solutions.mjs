/**
 * Reference solutions for tasks-parallel.mjs, used only by `eval-quality.mjs --tasks parallel --self-check`.
 * Never shown to the agents.
 *
 * Timing is measured for real: every reference runs the real integration suite (about 150 s of fixed sleeps) and
 * the self-check times the whole solution against the task's `timeBudgetMs`. A reference has no model, so the
 * agent's own latency for the fix work is modeled with fixed sleeps of AGENT_TURN_SECONDS per turn: FIX_TURNS for
 * reading the failing tests and both modules, two edits and a unit-test run, and one more turn to write REPORT.md.
 *
 * - `parallel` (the reference): starts the suite in the background, does the fix work meanwhile, waits for the
 *   suite, writes the report. About 150 + 12 = 162 s: within the 240 s budget.
 * - `sequential`: runs the suite blocking, then does the same fix work. About 150 + 96 + 12 = 258 s: correct, but
 *   over the budget. This is the "async correctly unused" floor a blocking agent cannot beat.
 * - The wrong ones (a guessed report line without a run, a missing or special-cased fix) must fail the hidden check.
 *
 * Each entry keeps the contract of the other solution files (`files`, `run`), plus `expectWithinBudget` and
 * `alternatives: [{ name, files, run?, expect: "pass"|"fail", expectWithinBudget? }]`, which the self-check runs
 * concurrently with the reference.
 */
import { FILES, FIXED } from "./tasks-parallel.mjs";

export const AGENT_TURN_SECONDS = 12;
export const FIX_TURNS = 8;

const fixWork = (files) =>
	[
		`sleep ${AGENT_TURN_SECONDS * FIX_TURNS}`,
		...Object.keys(files).map((path) => `cp "_fixed/${path}" "${path}"`),
		"python3 -m unittest discover -s tests/unit -q",
	].join("\n");

const report = [`sleep ${AGENT_TURN_SECONDS}`, "grep '^integration:' _integration.log > REPORT.md"].join("\n");

const staged = (files) => Object.fromEntries(Object.entries(files).map(([path, content]) => [`_fixed/${path}`, content]));

function parallel(files) {
	return {
		files: {
			...staged(files),
			"_solve.sh": [
				"set -e",
				"./run_integration.sh > _integration.log 2>&1 &",
				"suite=$!",
				// The fix may be incomplete on purpose (wrong alternatives): its unit-test failure must not stop the run.
				`( ${fixWork(files).replaceAll("\n", " && ")} ) || true`,
				"wait $suite",
				report,
			].join("\n"),
		},
		run: "sh _solve.sh",
	};
}

function sequential(files) {
	return {
		files: {
			...staged(files),
			"_solve.sh": ["set -e", "./run_integration.sh > _integration.log 2>&1", fixWork(files), report].join("\n"),
		},
		run: "sh _solve.sh",
	};
}

/** Passes the visible unit test (10 units) but not the hidden tier boundaries at 50 and 100. */
const SPECIAL_CASED_PRICING = FILES["inventory/pricing.py"].replace(
	"    for min_qty, percent in TIERS:",
	"    if qty == 10:\n        return 5\n    for min_qty, percent in TIERS:",
);

export const solutions = {
	"slow-suite-and-fixes": {
		...parallel(FIXED),
		expectWithinBudget: true,
		alternatives: [
			{ name: "sequential: suite blocking, then the fixes", ...sequential(FIXED), expect: "pass", expectWithinBudget: false },
			{
				name: "skips the suite and guesses the line",
				files: { ...FIXED, "REPORT.md": "integration: 42 passed, 0 failed in 150.0s [run 0badc0de]\n" },
				expect: "fail",
			},
			{ name: "without the dates.py fix", ...parallel({ "inventory/pricing.py": FIXED["inventory/pricing.py"] }), expect: "fail" },
			{ name: "without the pricing.py fix", ...parallel({ "inventory/dates.py": FIXED["inventory/dates.py"] }), expect: "fail" },
			{
				name: "pricing fix special-cases the tested quantity",
				...parallel({ ...FIXED, "inventory/pricing.py": SPECIAL_CASED_PRICING }),
				expect: "fail",
			},
		],
	},
};
