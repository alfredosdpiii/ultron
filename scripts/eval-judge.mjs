/**
 * LLM-judge tier for scripts/eval-quality.mjs (waku's judge next to deterministic checks).
 *
 * A task may declare `judge: { rubric, inputs, passAt, model? }` (see evals/quality/tasks-judged.mjs). After the run,
 * a fixed judge model scores the files named in `inputs` against the frozen rubric: one integer per criterion,
 * 0..points, with a one-sentence reason each. The prompt, the raw replies, the parsed scores and reasons, and the
 * normalized score are recorded next to the deterministic check. A malformed reply is re-asked once with the parse
 * error (at most JUDGE_MAX_ATTEMPTS calls per run).
 *
 * The judge never decides pass/fail of record and never gates a release: `record.passed` stays the deterministic
 * check, and scripts/gate.mjs reads only the default set's deterministic pass rates.
 */
import { createHash } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

/** Bump when the judge prompt or parsing changes; recorded with every judgement. */
export const JUDGE_PROMPT_VERSION = 1;
export const DEFAULT_JUDGE_MODEL = "cliproxyapi/glm-5.3-flash";
export const JUDGE_MAX_ATTEMPTS = 2;
/** Bounds on what the judge reads, so a runaway output cannot blow the judge's context. */
export const JUDGE_MAX_FILE_BYTES = 24_000;
export const JUDGE_MAX_TOTAL_BYTES = 80_000;

export const JUDGE_SYSTEM_PROMPT =
	"You are a strict, consistent grader of software work. Score only against the rubric you are given, using the " +
	"evidence in the files shown. Do not reward length or confidence. Reply with a single JSON object and nothing else.";

/** Structural problems with a task's judge declaration; empty when it is well formed. */
export function validateJudgeSpec(judge) {
	const problems = [];
	if (!judge || typeof judge !== "object") return ["judge must be an object"];
	if (!Array.isArray(judge.rubric) || judge.rubric.length === 0) problems.push("rubric must be a nonempty array");
	const ids = new Set();
	for (const [index, criterion] of (Array.isArray(judge.rubric) ? judge.rubric : []).entries()) {
		if (typeof criterion?.id !== "string" || !/^[a-z][a-z0-9_]*$/.test(criterion.id))
			problems.push(`rubric[${index}].id must be a snake_case identifier`);
		else if (ids.has(criterion.id)) problems.push(`rubric id ${criterion.id} is duplicated`);
		else ids.add(criterion.id);
		if (!Number.isSafeInteger(criterion?.points) || criterion.points < 1)
			problems.push(`rubric[${index}].points must be a positive integer`);
		if (typeof criterion?.description !== "string" || criterion.description.trim().length < 10)
			problems.push(`rubric[${index}].description must describe the criterion`);
	}
	if (!Array.isArray(judge.inputs) || judge.inputs.length === 0 || judge.inputs.some((path) => typeof path !== "string"))
		problems.push("inputs must be a nonempty array of paths");
	if (typeof judge.passAt !== "number" || judge.passAt <= 0 || judge.passAt > 1) problems.push("passAt must be in (0, 1]");
	if (judge.model !== undefined && (typeof judge.model !== "string" || !judge.model.includes("/")))
		problems.push("model must be provider/model");
	return problems;
}

export function rubricDigest(rubric) {
	return createHash("sha256").update(JSON.stringify(rubric)).digest("hex");
}

const maxScore = (rubric) => rubric.reduce((total, criterion) => total + criterion.points, 0);

function bounded(text, limit) {
	const bytes = Buffer.from(text);
	if (bytes.length <= limit) return text;
	const half = Math.floor(limit / 2);
	return `${bytes.subarray(0, half).toString("utf8")}\n[... ${bytes.length - limit} bytes elided ...]\n${bytes.subarray(bytes.length - half).toString("utf8")}`;
}

/**
 * The files the judge reads: each input as the run left it ("after"), plus its seed version ("before") when the task
 * provided one and the file changed. Bounded per file and in total.
 */
export function collectJudgeFiles(dir, judge, seedFiles = {}) {
	const files = [];
	let budget = JUDGE_MAX_TOTAL_BYTES;
	for (const path of judge.inputs) {
		const full = join(dir, path);
		const after = existsSync(full) ? readFileSync(full, "utf8") : null;
		const seed = seedFiles[path];
		const before = typeof seed === "string" && seed !== after ? seed : null;
		const take = (text) => {
			if (text === null) return null;
			const slice = bounded(text, Math.max(0, Math.min(JUDGE_MAX_FILE_BYTES, budget)));
			budget -= Buffer.byteLength(slice);
			return slice;
		};
		files.push({ path, before: take(before), after: take(after) });
	}
	return files;
}

/** The judge's user message: the task, the frozen rubric, the bounded files, and the reply format. */
export function buildJudgePrompt(task, files) {
	const { rubric } = task.judge;
	const sections = [
		"Grade the work an AI coding agent did for the task below.",
		`## Task given to the agent\n\n${task.prompts.join("\n\n")}`,
		`## Rubric\n\nScore each criterion with an integer from 0 to its maximum. Half-done earns partial credit; missing or wrong earns 0.\n\n${rubric
			.map((criterion) => `- ${criterion.id} (0-${criterion.points}): ${criterion.description}`)
			.join("\n")}`,
		`## Files\n\n${files
			.map((file) =>
				[
					file.before === null ? null : `=== ${file.path} (before the agent) ===\n${file.before}`,
					`=== ${file.path}${file.before === null ? "" : " (after the agent)"} ===\n${file.after ?? "(file does not exist)"}`,
				]
					.filter(Boolean)
					.join("\n\n"),
			)
			.join("\n\n")}`,
		`## Reply\n\nReply with JSON only, exactly this shape:\n{"scores": {${rubric.map((criterion) => `"${criterion.id}": <0-${criterion.points}>`).join(", ")}}, "reasons": {${rubric.map((criterion) => `"${criterion.id}": "<one sentence>"`).join(", ")}}}`,
	];
	return sections.join("\n\n");
}

/** Parses and validates a judge reply; throws an Error naming what is wrong. */
export function parseJudgeReply(text, rubric) {
	if (typeof text !== "string" || !text.trim()) throw new Error("empty reply");
	const unfenced = text.replace(/```(?:json)?/gi, "");
	const start = unfenced.indexOf("{");
	const end = unfenced.lastIndexOf("}");
	if (start === -1 || end <= start) throw new Error("no JSON object in the reply");
	let parsed;
	try {
		parsed = JSON.parse(unfenced.slice(start, end + 1));
	} catch (error) {
		throw new Error(`reply is not valid JSON: ${error.message}`);
	}
	const scores = parsed?.scores;
	if (!scores || typeof scores !== "object") throw new Error('reply has no "scores" object');
	const reasons = parsed.reasons && typeof parsed.reasons === "object" ? parsed.reasons : {};
	const result = { scores: {}, reasons: {} };
	for (const criterion of rubric) {
		const score = scores[criterion.id];
		if (!Number.isInteger(score) || score < 0 || score > criterion.points)
			throw new Error(`score for ${criterion.id} must be an integer 0-${criterion.points}, got ${JSON.stringify(score)}`);
		result.scores[criterion.id] = score;
		result.reasons[criterion.id] = typeof reasons[criterion.id] === "string" ? reasons[criterion.id].slice(0, 500) : "";
	}
	const unknown = Object.keys(scores).filter((id) => !rubric.some((criterion) => criterion.id === id));
	if (unknown.length) throw new Error(`reply scores unknown criteria: ${unknown.join(", ")}`);
	const total = Object.values(result.scores).reduce((sum, score) => sum + score, 0);
	const max = maxScore(rubric);
	return { ...result, total, max, normalized: total / max };
}

/**
 * Judges one run. `call(system, prompt)` performs one judge model call and resolves to `{ text, usage? }`. Returns
 * the judgement record; a judge that fails or never produces a valid reply yields `{ error }` with every attempt
 * kept, never a score.
 */
export async function judgeRun({ task, files, call, model, thinking }) {
	const prompt = buildJudgePrompt(task, files);
	const record = {
		model,
		thinking: thinking ?? null,
		promptVersion: JUDGE_PROMPT_VERSION,
		rubricDigest: rubricDigest(task.judge.rubric),
		passAt: task.judge.passAt,
		system: JUDGE_SYSTEM_PROMPT,
		prompt,
		attempts: [],
	};
	const started = Date.now();
	let message = prompt;
	for (let attempt = 1; attempt <= JUDGE_MAX_ATTEMPTS; attempt += 1) {
		let reply;
		try {
			reply = await call(JUDGE_SYSTEM_PROMPT, message);
		} catch (error) {
			record.attempts.push({ error: `judge call failed: ${error instanceof Error ? error.message : String(error)}` });
			break;
		}
		const entry = { raw: String(reply?.text ?? "").slice(0, 8000), ...(reply?.usage ? { usage: reply.usage } : {}) };
		record.attempts.push(entry);
		try {
			const parsed = parseJudgeReply(reply?.text, task.judge.rubric);
			Object.assign(record, parsed, { passed: parsed.normalized >= task.judge.passAt });
			break;
		} catch (error) {
			entry.error = error.message;
			message = `${prompt}\n\nYour previous reply could not be used (${error.message}). Reply again with the JSON object only.`;
		}
	}
	record.durationMs = Date.now() - started;
	if (record.normalized === undefined) record.error = record.attempts.at(-1)?.error ?? "no judgement";
	return record;
}

/**
 * Model-free stand-in for self-checks: returns a well-formed reply giving every criterion `fraction` of its points
 * (rounded down), or a malformed reply for `mode: "malformed"` / an out-of-range one for `mode: "out-of-range"`.
 */
export function fakeJudge(rubric, { fraction = 1, mode = "valid" } = {}) {
	return async () => {
		if (mode === "malformed") return { text: "I think this is pretty good overall." };
		const scores = Object.fromEntries(
			rubric.map((criterion) => [
				criterion.id,
				mode === "out-of-range" ? criterion.points + 1 : Math.floor(criterion.points * fraction),
			]),
		);
		const reasons = Object.fromEntries(rubric.map((criterion) => [criterion.id, "fake judge"]));
		return { text: `\`\`\`json\n${JSON.stringify({ scores, reasons })}\n\`\`\`` };
	};
}

/**
 * Model-free validation of a judged task against its reference solution tree: the rubric is well formed, every input
 * exists in the solved tree, the prompt names every criterion and input, and the parser accepts a full-marks reply,
 * scores a partial reply below full marks, and rejects malformed and out-of-range replies after the bounded re-ask.
 */
export async function checkJudgeWiring(task, solvedDir, seedFiles) {
	const outcomes = [];
	const add = (trial, ok, detail) => outcomes.push({ trial, expect: "pass", ok, ...(ok ? {} : { detail }) });
	const problems = validateJudgeSpec(task.judge);
	add("judge rubric", problems.length === 0, problems.join("; "));
	if (problems.length) return outcomes;
	const files = collectJudgeFiles(solvedDir, task.judge, seedFiles);
	const missing = files.filter((file) => file.after === null).map((file) => file.path);
	add("judge inputs exist in the solved tree", missing.length === 0, `missing: ${missing.join(", ")}`);
	const prompt = buildJudgePrompt(task, files);
	const absent = [...task.judge.rubric.map((criterion) => criterion.id), ...task.judge.inputs].filter(
		(name) => !prompt.includes(name),
	);
	add("judge prompt names every criterion and input", absent.length === 0, `absent: ${absent.join(", ")}`);
	const full = await judgeRun({ task, files, call: fakeJudge(task.judge.rubric), model: "fake" });
	add("fake judge: full marks parse", full.normalized === 1 && full.passed === true, JSON.stringify(full.error ?? full.scores));
	const partial = await judgeRun({ task, files, call: fakeJudge(task.judge.rubric, { fraction: 0.5 }), model: "fake" });
	add(
		"fake judge: partial marks score lower",
		partial.normalized !== undefined && partial.normalized < 1,
		JSON.stringify(partial.error ?? partial.scores),
	);
	for (const mode of ["malformed", "out-of-range"]) {
		const bad = await judgeRun({ task, files, call: fakeJudge(task.judge.rubric, { mode }), model: "fake" });
		add(
			`fake judge: ${mode} reply rejected after ${JUDGE_MAX_ATTEMPTS} attempts`,
			bad.normalized === undefined && typeof bad.error === "string" && bad.attempts.length === JUDGE_MAX_ATTEMPTS,
			JSON.stringify(bad.attempts),
		);
	}
	return outcomes;
}

/** Judge results of one variant's runs, reported next to its deterministic pass rate. */
export function summarizeJudged(records) {
	const judged = records.filter((record) => record.judge);
	if (judged.length === 0) return null;
	const scored = judged.filter((record) => typeof record.judge.normalized === "number");
	const byTask = {};
	for (const record of scored) {
		byTask[record.task] ??= { runs: 0, meanScore: 0, deterministicPassed: 0 };
		byTask[record.task].runs += 1;
		byTask[record.task].meanScore += record.judge.normalized;
		if (record.passed) byTask[record.task].deterministicPassed += 1;
	}
	for (const entry of Object.values(byTask)) entry.meanScore /= entry.runs;
	return {
		runs: judged.length,
		scored: scored.length,
		errors: judged.length - scored.length,
		meanScore: scored.length ? scored.reduce((sum, record) => sum + record.judge.normalized, 0) / scored.length : null,
		judgePassRate: scored.length ? scored.filter((record) => record.judge.passed).length / scored.length : null,
		byTask,
	};
}
