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
import { FIXED as PIPELINE_FIXED, FILES as PIPELINE_FILES } from "./tasks-parallel-deep.mjs";

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

/**
 * `slow-pipeline-deep` (tasks-parallel-deep.mjs): the timing model and the references.
 *
 * Every reference runs the real ops steps (provision 35 s, snapshot 120 s, load test 150 s, replay 45 s: 350 s of
 * steps in all) and models the agent's own latency with sleeps of PIPELINE.turnSeconds per model turn: one turn to
 * read the task and start work, PIPELINE.fixTurns per bug (read the test and module, edit, run the unit tests), one
 * turn to put the provisioned tenant into client/config.py and start the load test, one to start the replay with the
 * snapshot's id, and one to write REPORT.md. Ten seconds per turn is the middle of what gpt-6-sol measured (5-15 s).
 *
 * - `overlapped` (the reference): provision and snapshot start in the first turn; the uploader is fixed while they
 *   run; the load test starts as soon as provision reports; the ledger is fixed while the load test runs; the replay
 *   starts as soon as the snapshot reports; the report is written when both end. About 10 + 35 + 10 + 150 + 10 =
 *   215 s: within the 270 s budget.
 * - `waitAll`: the same steps started the same way, but handled together: both first-stage steps are awaited
 *   before either result is used, so the load test starts after the snapshot. About 10 + 120 + 10 + 150 + 10 = 300 s:
 *   correct, but over budget. This is "starts everything with `&`, then waits for all of it".
 * - `sequential`: each step blocking, the fixes between them. About 450 s: correct, far over budget.
 * - The wrong ones must fail the hidden check: guessed report lines with no runs, a load test against a made-up
 *   tenant, a replay of a made-up snapshot id, the replay run before the ledger fix and not rerun, an edit to
 *   client/ after the passing load test, each fix missing, and ops scripts with their sleeps edited out.
 */
export const PIPELINE = { turnSeconds: 10, fixTurns: 3 };

const turns = (count, turnSeconds = PIPELINE.turnSeconds) => (turnSeconds > 0 ? `sleep ${count * turnSeconds}` : "true");

/** Reads the tenant and limit from provision's output into client/config.py, as the agent would. */
const CONFIGURE_PY = `import re

line = next(l for l in open("_provision.log") if l.startswith("provision:"))
tenant, batch = re.search(r"tenant (t-[0-9a-f]+) ready, max_batch=(\\d+)", line).groups()
config = open("client/config.py").read()
config = re.sub(r"(?m)^TENANT = .*$", f'TENANT = "{tenant}"', config)
config = re.sub(r"(?m)^MAX_BATCH = .*$", f"MAX_BATCH = {batch}", config)
open("client/config.py", "w").write(config)
`;

const STEP = {
	provision: "./ops/provision.sh > _provision.log 2>&1",
	snapshot: "./ops/snapshot.sh > _snapshot.log 2>&1",
	configure: "python3 _configure.py",
	loadtest: "./ops/loadtest.sh > _loadtest.log 2>&1",
	replay: `./ops/replay.sh "$(sed -n 's/^snapshot: \\(s-[0-9a-f]*\\) .*/\\1/p' _snapshot.log)" > _replay.log 2>&1`,
	report: "grep -h -e '^loadtest:' -e '^replay:' _loadtest.log _replay.log > REPORT.md || true",
};

/** Copy one staged fix in, if the variant has it (a wrong alternative may leave it out), and run the unit tests. */
const fix = (files, path) =>
	(path in files ? [`cp "_fixed/${path}" "${path}"`] : []).concat("python3 -m unittest discover -s tests/unit -q > /dev/null 2>&1 || true");

function pipelineFiles(files, extra = {}) {
	return { ...staged(files), "_configure.py": CONFIGURE_PY, ...extra };
}

/**
 * The overlapped plan. `ledgerAfterReplay` copies the ledger fix only after the replay ran (and never reruns it);
 * `afterwards` runs extra commands before the report (e.g. a late edit); `turnSeconds` 0 drops the model latency.
 */
function pipelineOverlapped(files, { ledgerAfterReplay = false, afterwards = [], turnSeconds = PIPELINE.turnSeconds, extra = {} } = {}) {
	const t = (count) => turns(count, turnSeconds);
	const ledgerFix = [t(PIPELINE.fixTurns), ...fix(files, "ledger/ledger.py")];
	return {
		files: pipelineFiles(files, {
			...extra,
			"_solve.sh": [
				"set -e",
				t(1),
				`${STEP.provision} & provision=$!`,
				`${STEP.snapshot} & snapshot=$!`,
				t(PIPELINE.fixTurns),
				...fix(files, "client/uploader.py"),
				"wait $provision",
				t(1),
				STEP.configure,
				`${STEP.loadtest} & loadtest=$!`,
				...(ledgerAfterReplay ? [] : ledgerFix),
				"wait $snapshot",
				t(1),
				`${STEP.replay} & replay=$!`,
				"wait $loadtest || true",
				"wait $replay || true",
				...(ledgerAfterReplay ? ledgerFix : []),
				...afterwards,
				t(1),
				STEP.report,
			].join("\n"),
		}),
		run: "sh _solve.sh",
	};
}

function pipelineWaitAll(files) {
	return {
		files: pipelineFiles(files, {
			"_solve.sh": [
				"set -e",
				turns(1),
				`${STEP.provision} & provision=$!`,
				`${STEP.snapshot} & snapshot=$!`,
				turns(PIPELINE.fixTurns),
				...fix(files, "client/uploader.py"),
				turns(PIPELINE.fixTurns),
				...fix(files, "ledger/ledger.py"),
				"wait $provision $snapshot",
				turns(1),
				STEP.configure,
				`${STEP.loadtest} & loadtest=$!`,
				`${STEP.replay} & replay=$!`,
				"wait $loadtest $replay",
				turns(1),
				STEP.report,
			].join("\n"),
		}),
		run: "sh _solve.sh",
	};
}

function pipelineSequential(files) {
	return {
		files: pipelineFiles(files, {
			"_solve.sh": [
				"set -e",
				turns(1),
				STEP.provision,
				turns(PIPELINE.fixTurns),
				...fix(files, "client/uploader.py"),
				turns(1),
				STEP.configure,
				STEP.loadtest,
				turns(PIPELINE.fixTurns),
				...fix(files, "ledger/ledger.py"),
				STEP.snapshot,
				turns(1),
				STEP.replay,
				turns(1),
				STEP.report,
			].join("\n"),
		}),
		run: "sh _solve.sh",
	};
}

/** The ops scripts with their waits cut to a second: a run is quick and passes, but the scripts are not the originals. */
const FAST_OPS = Object.fromEntries(
	Object.entries(PIPELINE_FILES)
		.filter(([path, content]) => path.startsWith("ops/lib/") && /^SECONDS = \d+$/m.test(content))
		.map(([path, content]) => [path, content.replace(/^SECONDS = \d+$/m, "SECONDS = 1")]),
);

const only = (name) => Object.fromEntries(Object.entries(PIPELINE_FIXED).filter(([path]) => path.startsWith(name)));

function pipelineSolutions() {
	const guessedConfig = PIPELINE_FILES["client/config.py"]
		.replace(/^TENANT = .*$/m, 'TENANT = "t-4f2a91"')
		.replace(/^MAX_BATCH = .*$/m, "MAX_BATCH = 32");
	return {
		...pipelineOverlapped(PIPELINE_FIXED),
		expectWithinBudget: true,
		alternatives: [
			{ name: "waits for both first steps before using either", ...pipelineWaitAll(PIPELINE_FIXED), expect: "pass", expectWithinBudget: false },
			{ name: "sequential: every step blocking", ...pipelineSequential(PIPELINE_FIXED), expect: "pass", expectWithinBudget: false },
			{
				name: "no runs, guessed report lines",
				files: {
					...PIPELINE_FIXED,
					"client/config.py": guessedConfig,
					"REPORT.md":
						"loadtest: tenant t-4f2a91, 50 rounds passed, 0 failed in 150.2s [run 0badc0de]\n" +
						"replay: snapshot s-9c1d7e, 600 events, 40 accounts, 0 mismatched in 45.1s [run 5eed1e55]\n",
				},
				expect: "fail",
			},
			{
				name: "load test against a made-up tenant, replay of a made-up snapshot",
				files: { ...PIPELINE_FIXED, "client/config.py": guessedConfig },
				run: [
					"./ops/loadtest.sh > _loadtest.log 2>&1 || true",
					"./ops/replay.sh s-9c1d7e > _replay.log 2>&1 || true",
					STEP.report,
				].join("\n"),
				expect: "fail",
			},
			{ name: "replay before the ledger fix, not rerun", ...pipelineOverlapped(PIPELINE_FIXED, { ledgerAfterReplay: true }), expect: "fail" },
			{
				name: "client edited after the passing load test",
				...pipelineOverlapped(PIPELINE_FIXED, { afterwards: ["printf '\\n# Reviewed for release.\\n' >> client/uploader.py"] }),
				expect: "fail",
			},
			{ name: "without the uploader fix", ...pipelineOverlapped(only("ledger/")), expect: "fail" },
			{ name: "without the ledger fix", ...pipelineOverlapped(only("client/")), expect: "fail" },
			{ name: "ops scripts with their waits cut", ...pipelineOverlapped(PIPELINE_FIXED, { turnSeconds: 0, extra: FAST_OPS }), expect: "fail" },
		],
	};
}

solutions["slow-pipeline-deep"] = pipelineSolutions();
