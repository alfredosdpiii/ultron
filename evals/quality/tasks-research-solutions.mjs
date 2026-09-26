/**
 * Reference solutions for tasks-research.mjs, used only by `eval-quality.mjs --tasks research --self-check`.
 * Never shown to the agents.
 *
 * `files`/`run` as in tasks-hard-solutions.mjs. `negatives` are extra trials whose hidden check must FAIL; the
 * self-check reports their metrics (the check's last output line) so their precision and recall are on record:
 *   - keyword baseline: every report matching /cert|x509|tls/i and /expir/i. It must fail, which shows the task is
 *     not solvable with a regex;
 *   - empty answer: no ids.
 */
import { trueIncidentIds } from "./tasks-research.mjs";

const KEYWORD_BASELINE = `import json, os, re
ids = []
for name in sorted(os.listdir("incidents")):
    text = open(os.path.join("incidents", name), encoding="utf-8").read()
    if re.search(r"cert|x509|tls", text, re.I) and re.search(r"expir", text, re.I):
        ids.append(name.removesuffix(".md"))
json.dump({"incident_ids": ids}, open("answer.json", "w"))
`;

export const solutions = {
	"incident-root-causes": {
		files: { "answer.json": `${JSON.stringify({ incident_ids: trueIncidentIds() })}\n` },
		negatives: [
			{ name: "keyword baseline", files: { "_keyword_baseline.py": KEYWORD_BASELINE }, run: "python3 _keyword_baseline.py" },
			{ name: "empty answer", files: { "answer.json": `${JSON.stringify({ incident_ids: [] })}\n` } },
		],
	},
};
