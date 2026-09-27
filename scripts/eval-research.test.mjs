import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import {
	FULL,
	incidentMeta,
	incidentReports,
	RESEARCH_THRESHOLD,
	tasks,
	trueIncidentIds,
} from "../evals/quality/tasks-research.mjs";
import { isGatedComparison, verifyMetrics } from "./eval-quality.mjs";

const fullComparison = (taskSet) => ({ taskSet, summary: { gate: [{ check: "pass rate", ok: true }] } });

test("the release gate reads only full comparisons of the default set", () => {
	assert.equal(isGatedComparison(fullComparison(undefined)), true);
	assert.equal(isGatedComparison(fullComparison("default")), true);
	for (const taskSet of ["hard", "judged", "parallel", "research"]) assert.equal(isGatedComparison(fullComparison(taskSet)), false, taskSet);
	assert.equal(isGatedComparison({ taskSet: "default", summary: { gate: [] } }), false);
	assert.equal(isGatedComparison({ taskSet: "research", passed: true, results: [] }), false);
});

test("self-check metrics come from the hidden check's last JSON line", () => {
	assert.deepEqual(verifyMetrics('missed: a\nextra:\n{"precision": 0.5, "recall": 1}\n'), { precision: 0.5, recall: 1 });
	assert.equal(verifyMetrics("ok\n"), null);
	assert.equal(verifyMetrics("[1, 2]"), null);
	assert.equal(verifyMetrics(undefined), null);
});

test("the research corpus is frozen with the promised shape", () => {
	const reports = incidentReports();
	const { labels } = incidentMeta();
	// The frozen corpus is the 157-report pilot (generation of the full ~400 was interrupted); a full corpus
	// would also pass these bounds.
	assert.ok(reports.length >= 150 && reports.length <= 420, `${reports.length} reports`);
	const bytes = reports.reduce((sum, report) => sum + Buffer.byteLength(report.text), 0);
	assert.ok(bytes > 7e5 && bytes < 3e6, `${bytes} bytes`);
	assert.deepEqual(reports.map((report) => report.id).sort(), Object.keys(labels).sort());
	for (const report of reports) {
		assert.match(report.id, /^INC-\d{4}$/);
		assert.equal(report.path, `incidents/${report.id}.md`);
		assert.ok(report.text.includes(report.id), report.id);
		assert.ok(report.text.includes(labels[report.id].service), report.id);
	}
	const truth = trueIncidentIds();
	const noKeyword = truth.filter((id) => labels[id].no_keyword);
	const decoys = Object.values(labels).filter((entry) => entry.label === "decoy");
	assert.ok(truth.length >= 20 && truth.length <= 30, `${truth.length} true positives`);
	assert.ok(noKeyword.length * 3 >= truth.length, `${noKeyword.length} of ${truth.length} without "cert"`);
	assert.ok(decoys.length >= 35, `${decoys.length} decoys`);
	const text = Object.fromEntries(reports.map((report) => [report.id, report.text]));
	for (const id of noKeyword) assert.doesNotMatch(text[id], /cert/i, id);
});

test("the full research corpus holds the pilot plus 243 more reports, with the same labels for shared ids", () => {
	const reports = FULL.reports();
	const { labels, corpus } = FULL.meta();
	assert.equal(reports.length, 400);
	assert.deepEqual(reports.map((report) => report.id).sort(), Object.keys(labels).sort());
	assert.deepEqual(corpus, { reports: 400, bytes: corpus.bytes, truePositives: 25, noKeywordTruePositives: 10, decoys: 40, other: 335 });
	const pilot = incidentMeta().labels;
	for (const [id, entry] of Object.entries(pilot)) assert.equal(labels[id]?.label, entry.label, id);
	assert.deepEqual(FULL.truth(), trueIncidentIds());
	const text = Object.fromEntries(reports.map((report) => [report.id, report.text]));
	for (const id of FULL.truth().filter((id) => labels[id].no_keyword)) assert.doesNotMatch(text[id], /cert/i, id);
});

test("the research task prompt is agent-neutral and its hidden check scores precision and recall", () => {
	const [task] = tasks();
	assert.equal(task.id, "incident-root-causes");
	assert.doesNotMatch(task.prompts.join(" "), /\brlm\b|ultron|sub-?model|infer/i);
	const { files, hidden } = task.build();
	const work = mkdtempSync(join(tmpdir(), "ultron-research-check-"));
	try {
		for (const [path, content] of Object.entries({ ...files, ...hidden })) {
			mkdirSync(dirname(join(work, path)), { recursive: true });
			writeFileSync(join(work, path), content);
		}
		const truth = trueIncidentIds();
		const score = (ids) => {
			writeFileSync(join(work, "answer.json"), JSON.stringify({ incident_ids: ids }));
			const run = spawnSync("sh", ["-c", task.verify], { cwd: work, encoding: "utf8" });
			return { status: run.status, metrics: verifyMetrics(run.stdout) };
		};
		assert.equal(score(truth).status, 0);
		assert.deepEqual([score(truth).metrics.precision, score(truth).metrics.recall], [1, 1]);
		// One miss keeps recall above the threshold; dropping a sixth of the answers does not.
		assert.equal(score(truth.slice(1).map((id) => id.toLowerCase())).status, 0);
		const partial = score(truth.slice(Math.ceil(truth.length / 6)));
		assert.equal(partial.status, 1);
		assert.ok(partial.metrics.recall < RESEARCH_THRESHOLD);
		const decoys = Object.entries(incidentMeta().labels)
			.filter(([, entry]) => entry.label === "decoy")
			.map(([id]) => id);
		const padded = score([...truth, ...decoys.slice(0, 5)]);
		assert.equal(padded.status, 1);
		assert.ok(padded.metrics.precision < RESEARCH_THRESHOLD);
		assert.equal(score([]).status, 1);
	} finally {
		rmSync(work, { recursive: true, force: true });
	}
});
