/**
 * Deterministic synthetic codebase for the swarm set's `follow-ups` and `route` tasks (tasks-swarm.mjs): a large
 * Python package `app/` of filler modules plus one or more DENSE subsystems, each a staged rule pipeline whose
 * behaviour is spread over many files.
 *
 * A subsystem `app/<name>/` (domain vocabulary from DOMAINS) holds:
 * - `model.py`: the record it checks (a dataclass with `region` and four integer fields);
 * - `stages/<stage>.py`: one rule per module, registered with `@stage("<stage>")` (registry.py). A check stage
 *   rejects (`raise Reject(code, stage)`, errors.py) when a field is above (`max`) or below (`min`) its limit; an
 *   adjust stage adds its limit to a field for the regions it lists, so later checks see the adjusted value;
 * - `limits_base.py`: the base limit constant of every stage and `BASE = {stage: CONSTANT}`;
 * - `regions/<region>.py`: per-region constants and `OVERRIDES = {stage: CONSTANT}` (some regions override some
 *   stages), plus LEGACY constants that look like overrides but are not in OVERRIDES;
 * - `limits.py`: `limit_for(stage, region)` = the region's override, else the base;
 * - `pipeline.py`: `ORDER` (the stage sequence), `DISABLED` (stages in ORDER that are skipped), `active_order()` and
 *   `run(record)`. Stages registered but not in ORDER never run.
 *
 * The same model is simulated here (`simulate`), so the questions' answers are computed without Python; the
 * reference solution answers them by importing and running the generated Python, which checks the two agree.
 * Everything is a pure function of the seed: same seed, same files, same answers.
 */

/** mulberry32: a small seeded PRNG. */
function rng(seed) {
	let state = seed >>> 0;
	const next = () => {
		state = (state + 0x6d2b79f5) >>> 0;
		let t = state;
		t = Math.imul(t ^ (t >>> 15), t | 1);
		t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
		return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
	};
	const int = (lo, hi) => lo + Math.floor(next() * (hi - lo + 1));
	const pick = (items) => items[Math.floor(next() * items.length)];
	const shuffle = (items) => {
		const out = [...items];
		for (let i = out.length - 1; i > 0; i--) {
			const j = Math.floor(next() * (i + 1));
			[out[i], out[j]] = [out[j], out[i]];
		}
		return out;
	};
	return { next, int, pick, shuffle };
}

export const REGIONS = ["north", "south", "east", "west", "central"];

/** Domain vocabulary: the record, its fields (unique across domains) and the stage names (unique too). */
export const DOMAINS = {
	dispatch: {
		entity: "Shipment",
		noun: "shipment",
		prefix: "D",
		fields: ["weight_g", "value_cents", "length_cm", "distance_km"],
		stages: [
			"intake", "weigh", "measure", "value_cap", "route_plan", "dim_weight", "customs", "insure", "pack",
			"label", "sort", "hub_check", "lane_assign", "courier_fit", "last_mile", "dock", "seal", "manifest",
			"audit", "priority", "bulk_gate", "remote_zone", "tariff", "fragile", "oversize", "night_hold",
			"depot_cap", "returns",
		],
		about: "shipping: parcels are weighed, measured, valued and routed through depots and couriers",
	},
	billing: {
		entity: "Invoice",
		noun: "invoice",
		prefix: "B",
		fields: ["amount_cents", "days_late", "line_count", "discount_pct"],
		stages: [
			"ledger_open", "credit_line", "dunning", "late_fee", "write_off", "tax_band", "rebate", "proration",
			"collections", "net_terms", "volume_tier", "currency_lock", "dispute", "settlement", "reconcile",
			"chargeback", "grace", "statement", "escrow", "retainer", "deposit", "usage_cap", "true_up", "renewal",
		],
		about: "billing: invoices are priced, discounted, aged and sent to collections",
	},
	access: {
		entity: "AccessRequest",
		noun: "access request",
		prefix: "A",
		fields: ["token_age_s", "scope_count", "failed_logins", "session_min"],
		stages: [
			"handshake", "nonce_guard", "token_ttl", "scope_gate", "lockout", "mfa_step", "device_bind", "geo_fence",
			"session_cap", "replay_shield", "consent", "role_map", "elevation", "audit_trail", "throttle", "revocation",
			"refresh_window", "idle_timeout", "break_glass", "federation", "key_rotation", "posture",
		],
		about: "access control: sign-in requests are checked for token age, scopes, failed logins and session length",
	},
};

const WORDS = [
	"account", "batch", "cache", "cursor", "digest", "entry", "field", "frame", "group", "index", "journal", "ledger",
	"marker", "node", "offset", "packet", "queue", "record", "region", "sample", "segment", "signal", "slot", "token",
	"total", "value", "vector", "window", "bucket", "channel", "cluster", "column", "metric", "profile", "report",
];
const VERBS = ["build", "collect", "compute", "count", "derive", "encode", "filter", "format", "merge", "normalize", "parse", "rank", "render", "score", "split", "summarize", "validate"];
const AREAS = ["catalog", "customers", "reports", "search", "inventory", "notifications", "analytics", "storage", "scheduler", "exports", "pricing", "webhooks"];

const upper = (text) => text.toUpperCase();
const camel = (text) => text.split("_").map((part) => part[0].toUpperCase() + part.slice(1)).join("");

function sentence(r, words = 12) {
	const out = [];
	for (let i = 0; i < words; i++) out.push(r.pick(i % 3 === 1 ? VERBS : WORDS));
	const text = out.join(" ");
	return `${text[0].toUpperCase()}${text.slice(1)}.`;
}

/** A filler module: a dataclass, a class with methods and some functions, all plain valid Python. */
function fillerModule(r, area, index) {
	const name = `${r.pick(WORDS)}_${r.pick(WORDS)}_${index}`;
	const lines = [`"""${area}: ${sentence(r, 9)}`, "", sentence(r, 14), sentence(r, 11), '"""', "", "from dataclasses import dataclass, field", ""];
	const cls = camel(name);
	lines.push("", "@dataclass", `class ${cls}Row:`, `    """${sentence(r, 8)}"""`, "");
	const attrs = Array.from({ length: r.int(3, 6) }, (_, i) => `${r.pick(WORDS)}_${i}`);
	for (const attr of attrs) lines.push(`    ${attr}: int = ${r.int(0, 99)}`);
	lines.push("    tags: list = field(default_factory=list)", "");
	lines.push("", `class ${cls}:`, `    """${sentence(r, 10)}"""`, "", "    def __init__(self, rows=None):", "        self.rows = list(rows or [])", "        self.version = 0", "");
	const methods = r.int(3, 6);
	for (let m = 0; m < methods; m++) {
		const verb = r.pick(VERBS);
		const attr = r.pick(attrs);
		lines.push(
			`    def ${verb}_${attr}(self, threshold=${r.int(1, 50)}):`,
			`        """${sentence(r, 9)}"""`,
			"        total = 0",
			"        for row in self.rows:",
			`            if row.${attr} >= threshold:`,
			`                total += row.${attr} * ${r.int(2, 9)}`,
			"            else:",
			`                total -= ${r.int(1, 5)}`,
			"        self.version += 1",
			"        return total",
			"",
		);
	}
	const functions = r.int(4, 8);
	for (let f = 0; f < functions; f++) {
		const verb = r.pick(VERBS);
		lines.push(
			"",
			`def ${verb}_${r.pick(WORDS)}_${f}(values, scale=${r.int(1, 9)}):`,
			`    """${sentence(r, 10)}"""`,
			"    out = []",
			"    for position, value in enumerate(values):",
			`        if position % ${r.int(2, 5)} == 0:`,
			"            out.append(value * scale)",
			`        elif value > ${r.int(10, 90)}:`,
			`            out.append(value - ${r.int(1, 9)})`,
			"        else:",
			"            out.append(value)",
			"    return out",
		);
	}
	lines.push("");
	return { path: `app/${area}/${name}.py`, content: lines.join("\n") };
}

/** One subsystem's rules: stage kinds, fields, limits, overrides, order. Pure function of `r`. */
function subsystemModel(r, domainName, { stageCount }) {
	const domain = DOMAINS[domainName];
	const names = r.shuffle(domain.stages).slice(0, stageCount);
	const usedCodes = new Set();
	const code = () => {
		for (;;) {
			const value = `${domain.prefix}-${r.int(1000, 9999)}`;
			if (!usedCodes.has(value)) {
				usedCodes.add(value);
				return value;
			}
		}
	};
	const stages = names.map((name, index) => {
		const kind = index % 4 === 3 ? "adjust" : r.next() < 0.7 ? "max" : "min";
		const fieldName = r.pick(domain.fields);
		const base = kind === "adjust" ? r.int(150, 900) : kind === "max" ? r.int(8600, 9900) : r.int(150, 1300);
		const stage = { name, kind, field: fieldName, base, baseConst: `${upper(name)}_${kind === "adjust" ? "DELTA" : upper(kind)}` };
		if (kind === "adjust") stage.regions = r.shuffle(REGIONS).slice(0, r.int(2, 4)).sort();
		else stage.code = code();
		return stage;
	});
	// Region overrides and legacy decoys.
	const regions = {};
	for (const region of REGIONS) {
		const overrides = {};
		const legacy = {};
		for (const stage of stages) {
			const roll = r.next();
			const shift = stage.kind === "adjust" ? r.int(-120, 300) : stage.kind === "max" ? -r.int(600, 2400) : r.int(300, 1500);
			if (roll < 0.35) overrides[stage.name] = { const: `${upper(region)}_${stage.baseConst}`, value: stage.base + shift };
			if (roll < 0.15 || (roll > 0.8 && roll < 0.9))
				legacy[stage.name] = { const: `${upper(region)}_${stage.baseConst}_LEGACY`, value: stage.base + Math.round(shift / 2) + 7 };
		}
		regions[region] = { overrides, legacy };
	}
	// ORDER: most stages; a few are DISABLED; the rest are registered but never run.
	const shuffled = r.shuffle(stages.map((stage) => stage.name));
	const unused = shuffled.slice(0, 4);
	const order = names.filter((name) => !unused.includes(name));
	const disabled = r.shuffle(order).slice(0, 3);
	return { domain: domainName, stages, regions, order, disabled, unused };
}

export function limitFor(model, stageName, region) {
	const stage = model.stages.find((entry) => entry.name === stageName);
	return model.regions[region].overrides[stageName]?.value ?? stage.base;
}

export function limitConstFor(model, stageName, region) {
	const stage = model.stages.find((entry) => entry.name === stageName);
	return model.regions[region].overrides[stageName]?.const ?? stage.baseConst;
}

export function activeOrder(model, { without = [] } = {}) {
	return model.order.filter((name) => !model.disabled.includes(name) && !without.includes(name));
}

/** What `pipeline.run(record)` does: `{stage, code}` of the first rejection, or `{stage: null, code: "accepted"}`. */
export function simulate(model, record, { without = [] } = {}) {
	const item = { ...record };
	for (const name of activeOrder(model, { without })) {
		const stage = model.stages.find((entry) => entry.name === name);
		const limit = limitFor(model, name, item.region);
		if (stage.kind === "adjust") {
			if (stage.regions.includes(item.region)) item[stage.field] += limit;
		} else if (stage.kind === "max" ? item[stage.field] > limit : item[stage.field] < limit)
			return { stage: name, code: stage.code };
	}
	return { stage: null, code: "accepted" };
}

const py = (value) => JSON.stringify(value);

/** The Python files of one subsystem. */
function subsystemFiles(r, model) {
	const domain = DOMAINS[model.domain];
	const base = `app/${model.domain}`;
	const files = {};
	const doc = (count) => Array.from({ length: count }, () => sentence(r, r.int(9, 15)));
	files[`${base}/__init__.py`] = [`"""The ${model.domain} subsystem (${domain.about}).`, "", ...doc(4), "", "Entry point: pipeline.run(record).", '"""', ""].join("\n");
	files[`${base}/errors.py`] = [
		`"""Errors raised by the ${model.domain} pipeline."""`,
		"",
		"",
		"class Reject(Exception):",
		`    """A stage refused the ${domain.noun}: \`code\` is the stable error code, \`stage\` the stage that raised it."""`,
		"",
		"    def __init__(self, code, stage):",
		'        super().__init__(f"{stage}: {code}")',
		"        self.code = code",
		"        self.stage = stage",
		"",
	].join("\n");
	files[`${base}/model.py`] = [
		`"""The record the ${model.domain} pipeline checks."""`,
		"",
		"from dataclasses import dataclass",
		"",
		"",
		"@dataclass",
		`class ${domain.entity}:`,
		`    """One ${domain.noun}. Stages may adjust fields as it moves through the pipeline (run() works on a copy)."""`,
		"",
		"    region: str",
		...domain.fields.map((name) => `    ${name}: int = 0`),
		"",
	].join("\n");
	files[`${base}/registry.py`] = [
		'"""Stage registry: modules under stages/ register their rule with @stage(name)."""',
		"",
		"STAGES = {}",
		"",
		"",
		"def stage(name):",
		'    """Register the decorated function as the rule of stage `name`."""',
		"",
		"    def register(function):",
		"        if name in STAGES:",
		'            raise ValueError(f"stage {name} registered twice")',
		"        STAGES[name] = function",
		"        function.stage_name = name",
		"        return function",
		"",
		"    return register",
		"",
	].join("\n");
	const baseLines = ['"""Base limits of every stage. Regions may override them (regions/)."""', ""];
	for (const stage of model.stages) baseLines.push(`# ${stage.name}: ${sentence(r, 8)}`, `${stage.baseConst} = ${stage.base}`);
	baseLines.push("", "BASE = {", ...model.stages.map((stage) => `    ${py(stage.name)}: ${stage.baseConst},`), "}", "");
	files[`${base}/limits_base.py`] = baseLines.join("\n");
	for (const region of REGIONS) {
		const { overrides, legacy } = model.regions[region];
		const lines = [`"""Limits for the ${region} region: OVERRIDES replaces base limits for the stages it lists."""`, ""];
		const entries = [
			...Object.entries(overrides).map(([stage, entry]) => ({ stage, ...entry, kind: "override" })),
			...Object.entries(legacy).map(([stage, entry]) => ({ stage, ...entry, kind: "legacy" })),
		];
		for (const entry of r.shuffle(entries))
			lines.push(
				entry.kind === "legacy" ? `# ${entry.stage}: ${sentence(r, 7)} (kept for old reports)` : `# ${entry.stage}: ${sentence(r, 7)}`,
				`${entry.const} = ${entry.value}`,
			);
		lines.push("", "OVERRIDES = {", ...Object.entries(overrides).map(([stage, entry]) => `    ${py(stage)}: ${entry.const},`), "}", "");
		files[`${base}/regions/${region}.py`] = lines.join("\n");
	}
	files[`${base}/regions/__init__.py`] = [
		'"""Per-region override tables."""',
		"",
		`from . import ${REGIONS.join(", ")}`,
		"",
		"TABLES = {",
		...REGIONS.map((region) => `    ${py(region)}: ${region}.OVERRIDES,`),
		"}",
		"",
	].join("\n");
	files[`${base}/limits.py`] = [
		'"""The limit a stage applies to a record: the region override if there is one, else the base limit."""',
		"",
		"from .limits_base import BASE",
		"from .regions import TABLES",
		"",
		"",
		"def limit_for(stage, region):",
		"    return TABLES[region].get(stage, BASE[stage])",
		"",
	].join("\n");
	for (const stage of model.stages) {
		const lines = [`"""Stage ${stage.name}.`, "", ...doc(r.int(16, 28)), '"""', "", "from ..errors import Reject", "from ..limits import limit_for", "from ..registry import stage", ""];
		if (stage.kind === "adjust") lines.push(`ADJUST_REGIONS = ${py(stage.regions).replace(/\[/, "(").replace(/\]$/, ",)")}`, "");
		// Helpers that never change the outcome: bulk a real module would have.
		const helpers = r.int(10, 16);
		for (let h = 0; h < helpers; h++) {
			const verb = r.pick(VERBS);
			lines.push(
				"",
				`def _${verb}_${r.pick(WORDS)}_${h}(record, extra=${r.int(1, 9)}):`,
				`    """${sentence(r, 10)}"""`,
				"    summary = {}",
				`    summary["region"] = record.region`,
				`    summary["${r.pick(DOMAINS[model.domain].fields)}"] = getattr(record, "${r.pick(DOMAINS[model.domain].fields)}") + extra`,
				`    if summary["region"] == "${r.pick(REGIONS)}":`,
				`        summary["note"] = "${r.pick(WORDS)} ${r.pick(WORDS)}"`,
				"    else:",
				`        summary["note"] = "${r.pick(WORDS)}"`,
				`    for step in range(${r.int(2, 6)}):`,
				`        summary[f"step_{step}"] = step * ${r.int(2, 9)}`,
				"    return summary",
			);
		}
		lines.push("", "", `@stage(${py(stage.name)})`, `def check_${stage.name}(record):`, `    """${sentence(r, 11)}"""`);
		lines.push(`    limit = limit_for(${py(stage.name)}, record.region)`);
		if (stage.kind === "adjust")
			lines.push("    if record.region in ADJUST_REGIONS:", `        record.${stage.field} = record.${stage.field} + limit`);
		else
			lines.push(
				`    if record.${stage.field} ${stage.kind === "max" ? ">" : "<"} limit:`,
				`        raise Reject(${py(stage.code)}, ${py(stage.name)})`,
			);
		lines.push("    return record", "");
		files[`${base}/stages/${stage.name}.py`] = lines.join("\n");
	}
	files[`${base}/stages/__init__.py`] = [
		'"""Importing this package registers every stage."""',
		"",
		...[...model.stages].sort((a, b) => a.name.localeCompare(b.name)).map((stage) => `from . import ${stage.name}  # noqa: F401`),
		"",
	].join("\n");
	files[`${base}/pipeline.py`] = [
		`"""The ${model.domain} pipeline: runs the active stages in ORDER on a copy of the record."""`,
		"",
		"import dataclasses",
		"",
		"from . import stages  # noqa: F401  (registers every stage)",
		"from .registry import STAGES",
		"",
		"ORDER = [",
		...model.order.map((name) => `    ${py(name)},`),
		"]",
		"",
		`# ${sentence(r, 9)}`,
		`DISABLED = {${model.disabled.map(py).join(", ")}}`,
		"",
		"",
		"def active_order():",
		"    return [name for name in ORDER if name not in DISABLED]",
		"",
		"",
		"def run(record):",
		'    """Run the active stages in order. Returns "accepted", or raises errors.Reject from the first stage that refuses."""',
		"    record = dataclasses.replace(record)",
		"    for name in active_order():",
		"        STAGES[name](record)",
		'    return "accepted"',
		"",
	].join("\n");
	return files;
}

/** A check-stage question: a record rejected late in the pipeline, by an overridden limit, rejected again later without that stage. */
function pickRecord(r, model) {
	const domain = DOMAINS[model.domain];
	const order = activeOrder(model);
	for (let attempt = 0; attempt < 200_000; attempt++) {
		const record = { region: r.pick(REGIONS) };
		for (const name of domain.fields) record[name] = r.int(2900, 8400);
		// Push one or two fields toward a limit so rejections happen.
		const target = model.stages.filter((stage) => stage.kind !== "adjust" && order.includes(stage.name))[r.int(0, 10)];
		if (!target) continue;
		const limit = limitFor(model, target.name, record.region);
		record[target.field] = target.kind === "max" ? limit + r.int(1, 400) : Math.max(0, limit - r.int(1, 120));
		const first = simulate(model, record);
		if (!first.stage) continue;
		if (order.indexOf(first.stage) < 5) continue;
		const second = simulate(model, record, { without: [first.stage] });
		if (!second.stage || order.indexOf(second.stage) <= order.indexOf(first.stage)) continue;
		// The adjust stages matter: ignoring them gives another answer.
		const naive = simulate({ ...model, stages: model.stages.map((stage) => (stage.kind === "adjust" ? { ...stage, regions: [] } : stage)) }, record);
		// Prefer both traps (an overridden limit and an adjust stage that changes the answer); settle for one late on.
		const overridden = Boolean(model.regions[record.region].overrides[first.stage]);
		const adjusted = naive.stage !== first.stage;
		if (!(overridden && adjusted) && !(attempt > 100_000 && (overridden || adjusted))) continue;
		return { record, first, second, naive };
	}
	throw new Error(`no question record found for ${model.domain}`);
}

/** A constant question: a (stage, region) with an override and a legacy decoy, not the record's rejecting stage. */
function pickConstant(r, model, avoid) {
	const candidates = [];
	for (const region of REGIONS)
		for (const stage of model.stages)
			if (!avoid.includes(stage.name) && activeOrder(model).includes(stage.name) && stage.kind !== "adjust" && model.regions[region].overrides[stage.name] && model.regions[region].legacy[stage.name])
				candidates.push({ stage: stage.name, region });
	if (candidates.length === 0) throw new Error(`no constant question for ${model.domain}`);
	const chosen = r.pick(candidates);
	return { ...chosen, constant: limitConstFor(model, chosen.stage, chosen.region), decoy: model.regions[chosen.region].legacy[chosen.stage].const };
}

/**
 * Generate the codebase. `subsystems` maps a domain name to `{stageCount}`. Returns `{files, models, questions}`:
 * `questions[domain]` holds `stages` (active order), `record` / `first` / `second` / `naive` and `constant`.
 */
export function generateCodebase({ seed, fillerModules, subsystems }) {
	const r = rng(seed);
	const files = {};
	const models = {};
	const questions = {};
	for (const [domainName, options] of Object.entries(subsystems)) {
		const model = subsystemModel(r, domainName, options);
		models[domainName] = model;
		Object.assign(files, subsystemFiles(r, model));
		const recordQuestion = pickRecord(r, model);
		questions[domainName] = {
			stages: activeOrder(model),
			...recordQuestion,
			constant: pickConstant(r, model, [recordQuestion.first.stage, recordQuestion.second.stage]),
		};
	}
	const areas = AREAS.slice(0, Math.min(AREAS.length, Math.max(4, Math.round(fillerModules / 25))));
	for (const area of areas) files[`app/${area}/__init__.py`] = `"""${area}: ${sentence(r, 10)}"""\n`;
	for (let index = 0; index < fillerModules; index++) {
		const module = fillerModule(r, areas[index % areas.length], index);
		files[module.path] = module.content;
	}
	files["app/__init__.py"] = '"""The application package."""\n';
	return { files, models, questions };
}

export function lineCount(files, prefix = "") {
	return Object.entries(files)
		.filter(([path]) => path.startsWith(prefix))
		.reduce((total, [, content]) => total + content.split("\n").length, 0);
}
