// Generates crates/services/fixtures/kernel.json: Python cells calling the local-service kernel APIs
// (`refinements.*` of memory_api.py, `gates.*` of release_gate_api.py, and `rlm.host_request` for the requests
// without a wrapper) through the real runtime.py and the TypeScript kernel supervisor (kernel.ts), served by the TS
// host (NativeRlmHost with NativeLocalServices as its services and the release-gate module). Each scenario records
// every cell's outcome and the durable documents at the end. Ids and times are normalized (generate.mjs rules).
// Usage: node crates/services/fixtures/generate-kernel.mjs
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { attempt, FIXED_NOW, scenario, show } from "../../kernel-apis/fixtures/generate.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const pkg = resolve(here, "../../../packages/coding-agent/src/ultron");
const load = (path) => import(pathToFileURL(join(pkg, path)).href);
const { NativeLocalServices } = await load("local-services.ts");
const { validateRefinementContent } = await load("refinement-validation.ts");
const { createReleaseGateModule } = await load("release-gate.ts");
const { createMemoryModuleStore } = await load("rlm/host-module.ts");

function localHost(validate) {
	return async () => {
		const values = new Map();
		const documents = {
			get: async (key) => structuredClone(values.get(key)),
			set: async (key, value) => void values.set(key, structuredClone(value)),
			list: async (prefix) =>
				[...values.entries()].filter(([key]) => key.startsWith(prefix)).map(([key, value]) => ({ key, value })),
		};
		const gates = createMemoryModuleStore();
		return {
			services: new NativeLocalServices(documents, { validate }),
			modules: [createReleaseGateModule({ store: gates, now: () => FIXED_NOW })],
			state: async () => ({ local: Object.fromEntries(values), gates: (await gates.read()) ?? null }),
		};
	};
}

const attempts = (calls) => `import json as _json\n${calls.map(attempt).join("\n")}`;
const SKILL = "---\\nname: release-notes\\ndescription: Draft release notes\\n---\\nCollect merged PRs, then group them.";

const scenarios = [
	[
		"refinements through the kernel API",
		localHost(undefined),
		[
			show("await refinements.propose('instruction', 'instruction:rlm-child', 0, 'Check empty inputs.', [{'run': 'r1'}])"),
			`import json as _json\nfirst = (await refinements.list())[0]\nprint(_json.dumps(await refinements.activate(first['id']), sort_keys=True))`,
			show("await rlm.host_request('refinements.current', {'kind': 'instruction', 'target': 'instruction:rlm-child'})"),
			`import json as _json\nsecond = await refinements.propose('instruction', 'instruction:rlm-child', 1, 'Check empty and huge inputs.', {'failure': 'huge'}, scope='project')\nprint(_json.dumps(await refinements.activate(second['id']), sort_keys=True))\nprint(_json.dumps(await refinements.rollback(second['id']), sort_keys=True))`,
			show("[(r['state'], r['version'], r['approval'], [h['state'] for h in r['history']]) for r in await refinements.list()]"),
			`import json as _json\nthird = await refinements.propose('skill', 'skill:triage', 0, {'procedure': 'rerun'}, [{'run': 'r2'}])\nprint(_json.dumps(await rlm.host_request('refinements.approve', {'id': third['id']}), sort_keys=True))\nprint(_json.dumps(await refinements.reject(third['id']), sort_keys=True))\nprint(_json.dumps(await rlm.host_request('refinements.get', {'id': third['id']}), sort_keys=True))`,
			attempts([
				"refinements.propose('instruction', 'instruction:security-policy', 0, 'Relax.', [{'run': 'x'}])",
				"refinements.propose('instruction', 'acceptance/manifest.json', 0, 'Edit.', [{'run': 'x'}])",
				"refinements.propose('skill', 'skill:review', 0, {'instructions': 'Review.', 'allowed-tools': ['bash']}, [{'run': 'x'}])",
				"refinements.propose('skill', 'skill:review', 0, '---\\nname: r\\nmodel: big\\n---\\nbody', [{'run': 'x'}])",
				"refinements.propose('instruction', 'instruction:rlm-child', 0, 'Check empty inputs.', [{'run': 'r1'}])",
				"refinements.propose('instruction', 'instruction:rlm-child', -1, 'x', [])",
				"refinements.propose('instruction', 'instruction:rlm-child', 0, '   ', [])",
				"refinements.propose('instruction', 'instruction:rlm-child', 0, 'x', 5)",
				"refinements.activate('nope')",
				"refinements.rollback('')",
				"rlm.host_request('refinements.current', {'kind': 'tool', 'target': 'tool:x'})",
				"rlm.host_request('refinements.list', {'all': True})",
				"rlm.host_request('refinements.update', {'id': 'x'})",
			]),
			"print(type(refinements).__module__, type(refinements).__name__, sorted(n for n in dir(refinements) if not n.startswith('_')))",
		],
	],
	[
		"experiments through the kernel API",
		localHost(undefined),
		[
			`import json as _json\nruns = []\nfor variant, outcome in [('baseline', 'failed'), ('baseline', 'passed'), ('candidate', 'incomplete')]:\n    runs.append(await rlm.host_request('experiments.record', {'run': {'variant': variant, 'fixtureHash': 'heldout-v1', 'outcome': outcome, 'usage': {'root': 10, 'frames': 2, 'total': 12}}}))\nprint(_json.dumps(runs, sort_keys=True))`,
			show("[(r['variant'], r['outcome']) for r in await rlm.host_request('experiments.list', {})]"),
			show("(await rlm.host_request('experiments.compare', {'baseline': runs[0]['id'], 'candidate': runs[2]['id']}))['claim']"),
			attempts([
				"rlm.host_request('experiments.record', {'run': {'variant': 'v', 'fixtureHash': 'f', 'outcome': 'crashed'}})",
				"rlm.host_request('experiments.record', {'run': {'variant': 'v', 'fixtureHash': 'f', 'outcome': 'failed', 'usage': {'root': 10, 'total': 12}}})",
				"rlm.host_request('experiments.record', {'run': {'variant': 'v', 'fixtureHash': 'other', 'outcome': 'passed'}})",
				"rlm.host_request('experiments.compare', {'baseline': runs[0]['id'], 'candidate': 'nope'})",
				"rlm.host_request('experiments.delete', {'id': runs[0]['id']})",
			]),
			`import json as _json\nlisted = await rlm.host_request('experiments.list', {})\ntry:\n    await rlm.host_request('experiments.compare', {'baseline': listed[0]['id'], 'candidate': listed[-1]['id']})\nexcept Exception as e:\n    print(type(e).__name__, e)\nprint(len(listed))`,
		],
	],
	[
		"release gates through the kernel API",
		localHost(undefined),
		[
			show("await gates.define('demo-unit', [{'name': 'unit', 'required': True}, {'name': 'lint', 'required': False}], 'a' * 64)"),
			show("await gates.define('demo-unit', [{'name': 'lint', 'required': False}, {'name': 'unit', 'required': True}], 'a' * 64)"),
			show("await gates.compare('demo-unit', {'variant': 'baseline', 'fixture_hash': 'a' * 64, 'results': {'unit': 'failed', 'lint': 'passed'}}, {'variant': 'candidate', 'fixture_hash': 'a' * 64, 'results': {'unit': 'passed'}})"),
			show("await gates.compare('demo-unit', {'variant': 'base', 'fixture_hash': 'a' * 64, 'results': {'unit': 'passed', 'lint': 'passed'}}, {'variant': 'cand', 'fixture_hash': 'b' * 64, 'results': {'unit': 'passed'}})"),
			show("[(a['decision'], a['reasons']) for a in (await gates.history('demo-unit'))['attempts']]"),
			show("await gates.list()"),
			attempts([
				"gates.define('demo-unit', [{'name': 'unit', 'required': False}, {'name': 'x', 'required': True}], 'a' * 64)",
				"gates.define('Bad', [{'name': 'unit', 'required': True}], 'a' * 64)",
				"gates.define('opt', [{'name': 'unit', 'required': False}], 'a' * 64)",
				"gates.compare('demo-unit', {'variant': 'b', 'fixture_hash': 'a' * 64, 'results': {}}, {'variant': 'c', 'fixture_hash': 'a' * 64, 'results': {'bonus': 'passed'}})",
				"gates.compare('nope', {}, {})",
				"gates.history('nope')",
			]),
			"print(type(gates).__module__, type(gates).__name__, sorted(n for n in dir(gates) if not n.startswith('_')))",
		],
	],
	[
		"worker validation of skill proposals",
		localHost(validateRefinementContent),
		[
			attempts([
				"refinements.activate((await refinements.propose('skill', 'skill:bad-one', 0, 'just prose', [{'repeated': 3}]))['id'])",
				"refinements.activate((await refinements.propose('skill', 'skill:bad-two', 0, '---\\nname: Bad Name\\ndescription: x\\n---\\nbody', [{'repeated': 3}]))['id'])",
				`refinements.activate((await refinements.propose('skill', 'skill:release-notes', 0, '${SKILL}', [{'repeated': 3}]))['id'])`,
				"refinements.activate((await refinements.propose('instruction', 'instruction:rlm-child', 0, {'text': 'not text'}, [{'run': 1}]))['id'])",
			]),
			show("[(r['target'], r['state'], r['approval']) for r in await refinements.list()]"),
		],
	],
];

const recorded = [];
for (const [name, make, cells] of scenarios) recorded.push(await scenario(name, make, cells));
writeFileSync(join(here, "kernel.json"), `${JSON.stringify({ area: "services", scenarios: recorded }, null, "\t")}\n`);
console.log(`kernel: ${recorded.length} scenarios`);
