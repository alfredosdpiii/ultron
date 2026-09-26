/**
 * Frozen RESEARCH quality tasks: semantic judgement over far more text than is practical to read into context.
 *
 * Every task in the other sets is solvable with exact code. Here the answer needs reading comprehension at scale:
 * `incident-root-causes` ships about 400 fictional incident reports (~2 MB, one file each, many writing styles) and
 * asks for every incident whose root cause was an expired TLS/SSL certificate. Some true positives never use the
 * substring "cert"; dozens of decoys discuss certificates prominently but had another root cause, so a keyword
 * filter fails (see the keyword-baseline trial in `--self-check`).
 *
 * The corpus was written once by a model and validated by a second model (scripts/generate-research-corpus.mjs,
 * never run by the eval), then frozen as a gzipped fixture pinned by sha256 below. `build()` only unpacks it; no
 * model is called at eval time. Labels and generation stats live in fixtures/incident-root-causes.meta.json (also
 * pinned); the hidden check receives only the true-positive ids.
 *
 * Same contract as tasks-hard.mjs: `build()` returns `{ files, hidden }`. Reference solutions and the negative
 * self-check trials live in tasks-research-solutions.mjs. Prompts are agent-neutral.
 * Do not edit a task after measurements exist: add a new id instead (see FROZEN_AT).
 */
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { gunzipSync } from "node:zlib";

export const FROZEN_AT = "2026-09-26-research";

/** Pass threshold for both precision and recall. */
export const RESEARCH_THRESHOLD = 0.9;

const FIXTURE = new URL("./fixtures/incident-root-causes.jsonl.gz", import.meta.url);
const META = new URL("./fixtures/incident-root-causes.meta.json", import.meta.url);
const FIXTURE_SHA256 = "PENDING";
const META_SHA256 = "PENDING";

function pinned(url, expected) {
	const bytes = readFileSync(url);
	const actual = createHash("sha256").update(bytes).digest("hex");
	if (actual !== expected) throw new Error(`${url.pathname} changed: sha256 ${actual}, frozen ${expected}`);
	return bytes;
}

/** Reports of the frozen corpus: [{id, path, text}]. */
export function incidentReports() {
	return gunzipSync(pinned(FIXTURE, FIXTURE_SHA256))
		.toString("utf8")
		.split("\n")
		.filter(Boolean)
		.map((line) => JSON.parse(line));
}

/** Labels per incident id ({label: "tp" | "decoy" | "other", ...}) and generation stats. */
export function incidentMeta() {
	return JSON.parse(pinned(META, META_SHA256).toString("utf8"));
}

export function trueIncidentIds() {
	return Object.entries(incidentMeta().labels)
		.filter(([, entry]) => entry.label === "tp")
		.map(([id]) => id)
		.sort();
}

/**
 * Hidden checker: precision and recall of answer.json's incident_ids against the frozen labels. Prints the (capped)
 * missed and extra ids, writes the full lists to verify_detail_hidden.json, and ends with one JSON line of metrics
 * (the eval keeps only the tail of the output).
 */
const CHECK = `import json, sys

THRESHOLD = ${RESEARCH_THRESHOLD}
truth = set(json.load(open("expected_hidden.json"))["incident_ids"])
try:
    answer = json.load(open("answer.json"))
    ids = answer["incident_ids"]
    if not isinstance(ids, list):
        raise ValueError("incident_ids must be a list")
except Exception as error:
    print(json.dumps({"precision": 0, "recall": 0, "passed": False, "error": f"answer.json unusable: {error}"[:200]}))
    sys.exit(1)
got = {str(value).strip().upper().removesuffix(".MD") for value in ids}
hits = got & truth
precision = len(hits) / len(got) if got else 0.0
recall = len(hits) / len(truth)
missed = sorted(truth - got)
extra = sorted(got - truth)
passed = precision >= THRESHOLD and recall >= THRESHOLD
json.dump({"precision": precision, "recall": recall, "missed": missed, "extra": extra}, open("verify_detail_hidden.json", "w"))
cap = lambda values: " ".join(values[:12]) + (f" (+{len(values) - 12} more)" if len(values) > 12 else "")
print("missed:", cap(missed))
print("extra:", cap(extra))
print(json.dumps({"precision": round(precision, 4), "recall": round(recall, 4), "passed": passed, "answered": len(got), "missed": len(missed), "extra": len(extra)}))
sys.exit(0 if passed else 1)
`;

function buildIncidentRootCauses() {
	const files = {};
	for (const report of incidentReports()) files[report.path] = report.text;
	return {
		files,
		hidden: {
			"check_incidents_hidden.py": CHECK,
			"expected_hidden.json": `${JSON.stringify({ incident_ids: trueIncidentIds() })}\n`,
		},
	};
}

export function tasks() {
	return [
		{
			id: "incident-root-causes",
			category: "semantic-research",
			build: buildIncidentRootCauses,
			prompts: [
				'incidents/ holds about 400 incident reports from our engineering teams (one file per incident, about 2 MB in total), written by different people in very different styles: formal postmortems, on-call notes, chat-log dumps, tickets, emails. There is no index of root causes. Find every incident whose ROOT CAUSE was an expired TLS/SSL certificate, meaning a certificate (leaf, intermediate or client) that genuinely reached the end of its validity period while still in use. Reports describe causes in their own words, so the same cause can be phrased in many ways. Be careful: many other incidents mention certificates (suspected and ruled out, revoked, misconfigured, expired somewhere irrelevant, and so on) without a certificate expiry being the root cause; those must not be listed. Write answer.json as {"incident_ids": ["INC-....", ...]} using the ids from the reports. Scoring uses precision and recall over the ids; both must be at least 0.9.',
			],
			verify: "python3 check_incidents_hidden.py",
			verifyTimeoutMs: 120_000,
		},
	];
}
