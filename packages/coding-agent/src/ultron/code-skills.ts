import { createHash, randomUUID } from "node:crypto";
import {
	existsSync,
	mkdirSync,
	mkdtempSync,
	readdirSync,
	readFileSync,
	renameSync,
	rmSync,
	writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { type Context, isJsonValue, type JsonValue } from "@ultron/chord";
import { BACKGROUND_CONTEXT, withAbortSignal } from "@ultron/chord/context";
import lockfile from "proper-lockfile";
import { getAgentDir, getRlmRuntimePath } from "../config.ts";
import { containsCodeSecret, type NativeJevClient } from "./jev.ts";
import { RlmKernel } from "./rlm/kernel.ts";

/**
 * Procedural memory as tested code (supreme plan, Phase 4).
 *
 * A code skill is a Python module under `<agentDir>/skills/<name>/`. Every proposal becomes a new
 * numbered version in `.versions/<n>/` (skill.py, test_skill.py, SKILL.md); its test runs in a fresh,
 * isolated kernel, and only a passing version becomes active. The active version is copied to the skill
 * directory's top level, so Pi's skill loader lists its SKILL.md, and it is importable in every kernel as
 * `from code_skills import <name>` (the `code_skills` package is installed by `skills_api.py`).
 *
 * Versioning follows refinements (`local-services.ts`): states proposed → active | rejected, an activation
 * supersedes the previous active version and remembers it, and only the active version can be rolled back,
 * which restores the version it superseded (or leaves none active).
 */

export type CodeSkillKind = "code" | "policy";
export type CodeSkillState = "proposed" | "active" | "superseded" | "rejected" | "rolled_back";

export type CodeSkillFunction = { name: string; signature: string; doc: string | null };
export type CodeSkillTestReport = {
	passed: boolean;
	tests: { name: string; ok: boolean; error?: string }[];
	/** Failure outside any single test: import error, no tests found, timeout, kernel failure. */
	error?: string;
	durationMs: number;
	doc: string | null;
	functions: CodeSkillFunction[];
};
/** Jev's retention judgement of a proposal: relevance (keep/skip with confidence) and sensitivity. */
export type CodeSkillJevScore =
	| { status: "ok"; action: "keep" | "skip" | "sensitive"; confidence: number }
	| { status: "deterministic"; action: "sensitive"; reason: string }
	| { status: "unavailable" | "error"; reason: string };

export type CodeSkillVersion = {
	version: number;
	kind: CodeSkillKind;
	family: string | null;
	state: CodeSkillState;
	createdAt: string;
	sha256: { skill: string; test: string };
	evidence: JsonValue;
	/** Version this one superseded on activation; restored by its rollback. */
	previous: number | null;
	jev: CodeSkillJevScore;
	test: CodeSkillTestReport | null;
	history: { state: CodeSkillState; at: string; cause: string }[];
};

export type CodeSkillHistory = { format: 1; name: string; active: number | null; versions: CodeSkillVersion[] };

export type CodeSkillProposal = {
	status: "active" | "rejected" | "refused";
	name: string;
	kind: CodeSkillKind;
	version: number | null;
	previous: number | null;
	import?: string;
	reason?: string;
	test: CodeSkillTestReport | null;
	jev: CodeSkillJevScore;
};

export type ActiveCodeSkill = {
	name: string;
	kind: CodeSkillKind;
	family: string | null;
	version: number;
	doc: string | null;
	functions: CodeSkillFunction[];
	activatedAt: string;
};

/** The subset of UltronRlmKernel a test run needs. */
export interface CodeSkillTestKernel {
	execute(code: string, context: Context): Promise<string>;
	close(): Promise<void>;
}

export type CodeSkillOptions = {
	/** Where skills live; defaults to {@link codeSkillsDir}. */
	dir?: string;
	/** Jev scores relevance and sensitivity; absent or failing Jev never blocks a proposal. */
	jev?: Partial<Pick<NativeJevClient, "skillPolicy">>;
	/** Fresh kernel per test run. Defaults to a plain RLM kernel that refuses every host request. */
	createTestKernel?: (cwd: string, env: Record<string, string>) => CodeSkillTestKernel;
	/** Wall budget for one test run (ULTRON_CODE_SKILL_TEST_TIMEOUT_MS, default 120 s). */
	testTimeoutMs?: number;
	now?: () => number;
};

/** Prompt fragment for the system prompt's Runtime section (Phase 1) and the rlm tool description. */
export const CODE_SKILLS_PROMPT = [
	"Code skills are tested Python kept across sessions. When a procedure worked and will recur, save it: `await skills.propose_code(name, source, test_source, evidence)` (name is a lowercase identifier; `test_source` defines `test_*` functions and imports the skill with `from code_skills import <name>`). The test runs in a fresh kernel; only a passing version becomes active. `await skills.rollback(name, version)` undoes the active version; `await skills.code_list()` and `await skills.code_history(name)` inspect them.",
	"Use an active code skill instead of re-deriving it: `from code_skills import <name>` works in every kernel.",
].join("\n");

const NAME = /^[a-z][a-z0-9_]{0,63}$/;
const PYTHON_KEYWORDS = new Set(
	"false none true and as assert async await break class continue def del elif else except finally for from global if import in is lambda nonlocal not or pass raise return try while with yield match case type".split(
		" ",
	),
);
const MAX_SOURCE_BYTES = 64 * 1024;
const MAX_EVIDENCE_BYTES = 16 * 1024;
const MAX_FAMILY_CHARS = 200;
const DEFAULT_TEST_TIMEOUT_MS = 120_000;
const MAX_DESCRIPTION_ENTRY_CHARS = 240;
const MAX_DESCRIPTION_CHARS = 3000;
const MAX_DESCRIPTION_ENTRIES = 24;
const REPORT_MARKER = "ULTRON_CODE_SKILL_REPORT";

const sha256 = (value: string): string => createHash("sha256").update(value).digest("hex");

/** `ULTRON_CODE_SKILLS_DIR`, else `<agentDir>/skills` (shared with Pi's skill loader). */
export function codeSkillsDir(env: NodeJS.ProcessEnv = process.env): string {
	return env.ULTRON_CODE_SKILLS_DIR?.trim() || join(getAgentDir(), "skills");
}

export function validCodeSkillName(name: unknown): name is string {
	return typeof name === "string" && NAME.test(name) && !PYTHON_KEYWORDS.has(name);
}

function codeSkillName(value: unknown): string {
	if (!validCodeSkillName(value))
		throw new Error("name must be a lowercase Python identifier (a-z, 0-9, _; at most 64 characters)");
	return value;
}

function sourceText(value: unknown, label: string): string {
	if (typeof value !== "string" || !value.trim()) throw new Error(`${label} must be nonempty Python source`);
	if (Buffer.byteLength(value) > MAX_SOURCE_BYTES) throw new Error(`${label} exceeds 64 KiB`);
	return value;
}

function nonemptyJson(value: JsonValue): boolean {
	if (value === null) return false;
	if (typeof value === "string") return value.trim().length > 0;
	if (typeof value === "object") return Object.values(value).some(nonemptyJson);
	return true;
}

function evidenceValue(value: unknown): JsonValue {
	// As for refinement activation, a skill needs nonempty evidence of the decomposition that worked.
	if (value === undefined || !isJsonValue(value) || !nonemptyJson(value))
		throw new Error("evidence must be nonempty JSON (what worked, and where)");
	if (Buffer.byteLength(JSON.stringify(value)) > MAX_EVIDENCE_BYTES) throw new Error("evidence exceeds 16 KiB");
	return value;
}

function atomicWrite(path: string, content: string): void {
	const temporary = `${path}.${randomUUID()}.tmp`;
	writeFileSync(temporary, content);
	renameSync(temporary, path);
}

function readHistory(base: string, name: string): CodeSkillHistory {
	const path = join(base, ".history.json");
	if (!existsSync(path)) return { format: 1, name, active: null, versions: [] };
	const history = JSON.parse(readFileSync(path, "utf8")) as CodeSkillHistory;
	if (!history || history.format !== 1 || history.name !== name || !Array.isArray(history.versions))
		throw new Error(`Code skill history for ${name} is invalid`);
	return history;
}

function oneLine(text: string | null | undefined, limit: number): string {
	const line = (text ?? "").split(/\r?\n/).find((item) => item.trim()) ?? "";
	const trimmed = line.trim();
	return trimmed.length > limit ? `${trimmed.slice(0, limit - 1)}…` : trimmed;
}

function skillMarkdown(name: string, record: CodeSkillVersion): string {
	const doc = record.test?.doc ?? null;
	const description =
		oneLine(doc, 300) || `Python ${record.kind === "policy" ? "policy" : "code"} skill ${name} (tested).`;
	const functions = record.test?.functions ?? [];
	return [
		"---",
		`name: ${name.replace(/_/g, "-")}`,
		`description: ${JSON.stringify(description)}`,
		"---",
		`# ${name} (${record.kind} skill, version ${record.version})`,
		"",
		`Tested Python. In the rlm tool: \`from code_skills import ${name}\`.`,
		...(record.family ? ["", `Task family: ${record.family}`] : []),
		...(doc ? ["", doc.trim()] : []),
		...(functions.length
			? [
					"",
					"## Functions",
					"",
					...functions.map((fn) => `- \`${fn.signature}\`${fn.doc ? `: ${oneLine(fn.doc, 200)}` : ""}`),
				]
			: []),
		"",
	].join("\n");
}

/** Python run in the fresh test kernel. It writes its report to a nonce file, never trusting stdout framing. */
function testRunner(options: { name: string; kind: CodeSkillKind; dir: string; reportPath: string }): string {
	const literal = (value: unknown) => JSON.stringify(JSON.stringify(value));
	return `
import json as _j, ast as _ast, sys as _sys, time as _time, traceback as _tb, inspect as _inspect, os as _os
_cfg = _j.loads(${literal(options)})
_started = _time.monotonic()
_report = {"passed": False, "tests": [], "doc": None, "functions": []}
def _err(error):
    text = "".join(_tb.format_exception(type(error), error, error.__traceback__)[-6:])
    return text[-2000:]
try:
    _skill_path = _os.path.join(_cfg["dir"], "skill.py")
    _source = open(_skill_path, encoding="utf-8").read()
    _tree = _ast.parse(_source, filename="skill.py")
    _report["doc"] = _ast.get_docstring(_tree)
    for _node in _tree.body:
        if isinstance(_node, (_ast.FunctionDef, _ast.AsyncFunctionDef, _ast.ClassDef)) and not _node.name.startswith("_"):
            _sig = _node.name + ("(" + _ast.unparse(_node.args) + ")" if not isinstance(_node, _ast.ClassDef) else "")
            if not isinstance(_node, _ast.ClassDef) and _node.returns is not None:
                _sig += " -> " + _ast.unparse(_node.returns)
            _report["functions"].append({"name": _node.name, "signature": ("async " if isinstance(_node, _ast.AsyncFunctionDef) else "") + _sig, "doc": _ast.get_docstring(_node)})
    _report["functions"] = _report["functions"][:20]
    _sys.path.insert(0, _cfg["dir"])
    import code_skills as _cs
    _cs._use_candidate(_cfg["name"], _skill_path)
    _module = getattr(_cs, _cfg["name"])
    if _cfg["kind"] == "policy":
        try:
            assert callable(getattr(_module, "plan", None)), "a policy skill must define a callable plan(...)"
            _report["tests"].append({"name": "policy_defines_plan", "ok": True})
        except (Exception, SystemExit) as _error:
            _report["tests"].append({"name": "policy_defines_plan", "ok": False, "error": _err(_error)})
    _test_ns = {"__name__": "test_skill"}
    exec(compile(open(_os.path.join(_cfg["dir"], "test_skill.py"), encoding="utf-8").read(), "test_skill.py", "exec"), _test_ns)
    _tests = sorted(name for name, value in _test_ns.items() if name.startswith("test") and callable(value))
    if not _tests:
        _report["error"] = "test_source defines no test_* functions"
    for _name in _tests:
        try:
            _value = _test_ns[_name]()
            if _inspect.isawaitable(_value):
                await _value
            _report["tests"].append({"name": _name, "ok": True})
        except (Exception, SystemExit) as _error:
            _report["tests"].append({"name": _name, "ok": False, "error": _err(_error)})
    _report["tests"] = _report["tests"][:50]
    _report["passed"] = bool(_tests) and "error" not in _report and all(item["ok"] for item in _report["tests"])
except BaseException as _error:
    _report["error"] = _err(_error)
_report["durationMs"] = int((_time.monotonic() - _started) * 1000)
with open(_cfg["reportPath"], "w", encoding="utf-8") as _handle:
    _handle.write(${JSON.stringify(REPORT_MARKER)} + _j.dumps(_report))
None
`;
}

/** A plain RLM kernel that refuses every host request: a skill test gets no host capabilities. */
function defaultTestKernel(cwd: string, env: Record<string, string>): CodeSkillTestKernel {
	const kernel = new RlmKernel({ cwd, runtimePath: getRlmRuntimePath(), env }, () => {
		throw new Error("Host requests are unavailable while a code skill test runs");
	});
	return {
		async execute(code, context) {
			const result = await kernel.execute(code, context.abortSignal);
			if (result.status === "error")
				throw new Error(`${result.error?.ename ?? "PythonError"}: ${result.error?.evalue ?? "Execution failed"}`);
			return [result.stdout, result.stderr, result.result].filter(Boolean).join("\n");
		},
		close: () => kernel.shutdown(),
	};
}

function failedReport(error: string, durationMs: number): CodeSkillTestReport {
	return { passed: false, tests: [], error: error.slice(0, 2000), durationMs, doc: null, functions: [] };
}

function parseReport(text: string): CodeSkillTestReport {
	if (!text.startsWith(REPORT_MARKER)) throw new Error("malformed test report");
	const value = JSON.parse(text.slice(REPORT_MARKER.length)) as CodeSkillTestReport;
	if (typeof value?.passed !== "boolean" || !Array.isArray(value.tests)) throw new Error("malformed test report");
	return value;
}

export class CodeSkills {
	readonly dir: string;
	private readonly jev?: Partial<Pick<NativeJevClient, "skillPolicy">>;
	private readonly createTestKernel: (cwd: string, env: Record<string, string>) => CodeSkillTestKernel;
	private readonly testTimeoutMs: number;
	private readonly now: () => number;

	constructor(options: CodeSkillOptions = {}) {
		this.dir = options.dir ?? codeSkillsDir();
		this.jev = options.jev;
		this.createTestKernel = options.createTestKernel ?? defaultTestKernel;
		const configured = Number(process.env.ULTRON_CODE_SKILL_TEST_TIMEOUT_MS);
		this.testTimeoutMs =
			options.testTimeoutMs ??
			(Number.isSafeInteger(configured) && configured > 0 ? configured : DEFAULT_TEST_TIMEOUT_MS);
		this.now = options.now ?? Date.now;
	}

	private base(name: string): string {
		return join(this.dir, name);
	}

	/** Cross-process exclusion per skill (sessions share the profile's skills directory). */
	private async locked<T>(name: string, operation: () => T | Promise<T>): Promise<T> {
		const base = this.base(name);
		mkdirSync(base, { recursive: true });
		const release = await lockfile.lock(base, {
			lockfilePath: join(base, ".lock"),
			retries: { retries: 100, minTimeout: 20, maxTimeout: 500 },
			stale: 30_000,
		});
		try {
			return await operation();
		} finally {
			await release();
		}
	}

	private writeHistory(history: CodeSkillHistory): void {
		atomicWrite(join(this.base(history.name), ".history.json"), `${JSON.stringify(history, null, "\t")}\n`);
	}

	/** Top-level copy of the active version for Pi's loader and humans; removed when none is active. */
	private publish(history: CodeSkillHistory): void {
		const base = this.base(history.name);
		const record = history.versions.find((item) => item.version === history.active);
		for (const file of ["SKILL.md", "skill.py", "test_skill.py"]) {
			const target = join(base, file);
			if (record) atomicWrite(target, readFileSync(join(base, ".versions", String(record.version), file), "utf8"));
			else rmSync(target, { force: true });
		}
	}

	private async score(
		name: string,
		source: string,
		testSource: string,
		evidence: JsonValue,
		signal?: AbortSignal,
	): Promise<CodeSkillJevScore> {
		const evidenceText = typeof evidence === "string" ? evidence : JSON.stringify(evidence);
		if (containsCodeSecret(`${name}\n${source}\n${testSource}\n${evidenceText}`))
			return {
				status: "deterministic",
				action: "sensitive",
				reason: "the proposal contains a secret or credential",
			};
		if (!this.jev?.skillPolicy) return { status: "unavailable", reason: "Jev is not configured" };
		try {
			const policy = await this.jev.skillPolicy(
				name,
				evidenceText.slice(0, 2000),
				`${source.slice(0, 6000)}\n\n# test\n${testSource.slice(0, 2000)}`,
				signal,
			);
			return { status: "ok", action: policy.action, confidence: policy.confidence };
		} catch (error) {
			// An outage never blocks a deliberate proposal; the deterministic secret check above still ran.
			const message = error instanceof Error ? error.message : String(error);
			return {
				status: /UNAVAILABLE|ABORTED/.test(message) ? "unavailable" : "error",
				reason: message.slice(0, 200),
			};
		}
	}

	private async runTest(
		name: string,
		kind: CodeSkillKind,
		source: string,
		testSource: string,
		context: Context,
	): Promise<CodeSkillTestReport> {
		const started = this.now();
		const work = mkdtempSync(join(tmpdir(), "ultron-skill-test-"));
		const reportPath = join(work, `.report-${randomUUID()}.json`);
		// A copy, so the test can never alter the version it is judging.
		const dir = join(work, "candidate");
		mkdirSync(dir);
		writeFileSync(join(dir, "skill.py"), source);
		writeFileSync(join(dir, "test_skill.py"), testSource);
		const kernel = this.createTestKernel(work, { ULTRON_CODE_SKILLS_DIR: this.dir });
		const signals = [AbortSignal.timeout(this.testTimeoutMs)];
		if (context.abortSignal) signals.push(context.abortSignal);
		try {
			await kernel.execute(
				testRunner({ name, kind, dir, reportPath }),
				withAbortSignal(AbortSignal.any(signals), context),
			);
			return parseReport(readFileSync(reportPath, "utf8"));
		} catch (error) {
			context.abortSignal?.throwIfAborted();
			const message = error instanceof Error ? error.message : String(error);
			const timedOut = this.now() - started >= this.testTimeoutMs;
			return failedReport(
				timedOut ? `test timed out after ${this.testTimeoutMs} ms` : message,
				this.now() - started,
			);
		} finally {
			await kernel.close().catch(() => {});
			rmSync(work, { recursive: true, force: true });
		}
	}

	async propose(
		kind: CodeSkillKind,
		payload: Record<string, unknown>,
		context: Context = BACKGROUND_CONTEXT,
	): Promise<CodeSkillProposal> {
		const allowed = ["name", "source", "test_source", "evidence", ...(kind === "policy" ? ["family"] : [])];
		for (const key of Object.keys(payload))
			if (!allowed.includes(key)) throw new Error(`Unknown payload field: ${key}`);
		const name = codeSkillName(payload.name);
		const source = sourceText(payload.source, "source");
		const testSource = sourceText(payload.test_source, "test_source");
		const evidence = evidenceValue(payload.evidence);
		let family: string | null = null;
		if (payload.family !== undefined) {
			if (typeof payload.family !== "string" || !payload.family.trim() || payload.family.length > MAX_FAMILY_CHARS)
				throw new Error(`family must be a nonempty string of at most ${MAX_FAMILY_CHARS} characters`);
			family = payload.family.trim();
		}
		const base = this.base(name);
		if (existsSync(join(base, "SKILL.md")) && !existsSync(join(base, ".history.json")))
			throw new Error(`${name} is an instruction skill (SKILL.md without code history); choose another name`);
		const jev = await this.score(name, source, testSource, evidence, context.abortSignal);
		if ("action" in jev && jev.action === "sensitive")
			return {
				status: "refused",
				name,
				kind,
				version: null,
				previous: null,
				reason: "Jev judged the proposal sensitive; nothing was written",
				test: null,
				jev,
			};
		const at = () => new Date(this.now()).toISOString();
		const cause = randomUUID();
		const version = await this.locked(name, () => {
			const history = readHistory(base, name);
			const next = history.versions.reduce((max, item) => Math.max(max, item.version), 0) + 1;
			const directory = join(base, ".versions", String(next));
			mkdirSync(directory, { recursive: true });
			const record: CodeSkillVersion = {
				version: next,
				kind,
				family,
				state: "proposed",
				createdAt: at(),
				sha256: { skill: sha256(source), test: sha256(testSource) },
				evidence,
				previous: null,
				jev,
				test: null,
				history: [{ state: "proposed", at: at(), cause }],
			};
			writeFileSync(join(directory, "skill.py"), source);
			writeFileSync(join(directory, "test_skill.py"), testSource);
			writeFileSync(join(directory, "SKILL.md"), skillMarkdown(name, record));
			history.versions.push(record);
			this.writeHistory(history);
			return next;
		});
		const report = await this.runTest(name, kind, source, testSource, context);
		return this.locked(name, () => {
			const history = readHistory(base, name);
			const record = history.versions.find((item) => item.version === version);
			if (!record) throw new Error(`Code skill ${name} lost version ${version}`);
			record.test = report;
			const directory = join(base, ".versions", String(version));
			// Regenerate SKILL.md now that the docstring is known.
			writeFileSync(join(directory, "SKILL.md"), skillMarkdown(name, record));
			let previous: number | null = null;
			if (report.passed) {
				const current = history.versions.find((item) => item.version === history.active && item.state === "active");
				if (current) {
					current.state = "superseded";
					current.history.push({ state: "superseded", at: at(), cause });
					previous = current.version;
				}
				record.previous = previous;
				record.state = "active";
				record.history.push({ state: "active", at: at(), cause });
				history.active = version;
			} else {
				record.state = "rejected";
				record.history.push({ state: "rejected", at: at(), cause });
			}
			this.writeHistory(history);
			if (report.passed) this.publish(history);
			return {
				status: report.passed ? "active" : "rejected",
				name,
				kind,
				version,
				previous,
				...(report.passed ? { import: `from code_skills import ${name}` } : {}),
				test: report,
				jev,
			} satisfies CodeSkillProposal;
		});
	}

	/** Refinement semantics: only the active version can be rolled back; the version it superseded returns. */
	async rollback(
		payload: Record<string, unknown>,
	): Promise<{ name: string; rolledBack: number; active: number | null }> {
		for (const key of Object.keys(payload))
			if (key !== "name" && key !== "version") throw new Error(`Unknown payload field: ${key}`);
		const name = codeSkillName(payload.name);
		const version = payload.version;
		if (typeof version !== "number" || !Number.isSafeInteger(version) || version < 1)
			throw new Error("version must be a positive integer");
		if (!existsSync(join(this.base(name), ".history.json"))) throw new Error(`Unknown code skill: ${name}`);
		return this.locked(name, () => {
			const history = readHistory(this.base(name), name);
			const record = history.versions.find((item) => item.version === version);
			if (!record) throw new Error(`Unknown version ${version} of code skill ${name}`);
			if (history.active !== version || record.state !== "active")
				throw new Error(
					`Only the active version can be rolled back: ${name} has ${history.active === null ? "no active version" : `version ${history.active} active`}`,
				);
			const at = new Date(this.now()).toISOString();
			const cause = randomUUID();
			record.state = "rolled_back";
			record.history.push({ state: "rolled_back", at, cause });
			const previous = history.versions.find(
				(item) => item.version === record.previous && item.state === "superseded",
			);
			if (previous) {
				previous.state = "active";
				previous.history.push({ state: "active", at, cause });
			}
			history.active = previous?.version ?? null;
			this.writeHistory(history);
			this.publish(history);
			return { name, rolledBack: version, active: history.active };
		});
	}

	history(name: unknown): CodeSkillHistory {
		const valid = codeSkillName(name);
		if (!existsSync(join(this.base(valid), ".history.json"))) throw new Error(`Unknown code skill: ${valid}`);
		return readHistory(this.base(valid), valid);
	}

	/** Active code skills, most recently activated first. Unreadable entries are skipped. */
	list(filter: { kind?: CodeSkillKind; family?: string } = {}): ActiveCodeSkill[] {
		let entries: string[];
		try {
			entries = readdirSync(this.dir);
		} catch {
			return [];
		}
		const skills: ActiveCodeSkill[] = [];
		for (const name of entries) {
			if (!validCodeSkillName(name) || !existsSync(join(this.base(name), ".history.json"))) continue;
			try {
				const history = readHistory(this.base(name), name);
				const record = history.versions.find((item) => item.version === history.active && item.state === "active");
				if (!record) continue;
				if (filter.kind && record.kind !== filter.kind) continue;
				if (filter.family && record.family !== filter.family) continue;
				skills.push({
					name,
					kind: record.kind,
					family: record.family,
					version: record.version,
					doc: record.test?.doc ?? null,
					functions: record.test?.functions ?? [],
					activatedAt:
						[...record.history].reverse().find((item) => item.state === "active")?.at ?? record.createdAt,
				});
			} catch {}
		}
		return skills.sort((a, b) => b.activatedAt.localeCompare(a.activatedAt) || a.name.localeCompare(b.name));
	}

	/**
	 * Bounded listing for the rlm tool description: one line per active skill (docstring summary and
	 * signatures), at most {@link MAX_DESCRIPTION_ENTRIES} entries and {@link MAX_DESCRIPTION_CHARS} characters.
	 */
	describe(): string {
		const skills = this.list();
		if (!skills.length) return "";
		const lines: string[] = [];
		let size = 0;
		for (const [index, skill] of skills.entries()) {
			const signatures = skill.functions.map((fn) => fn.signature).join("; ");
			const summary = oneLine(skill.doc, 160);
			let line = `- ${skill.name} v${skill.version}${skill.kind === "policy" ? ` (policy${skill.family ? `, ${skill.family}` : ""})` : ""}: ${summary}${summary && signatures ? " " : ""}${signatures ? `[${signatures}]` : ""}`;
			if (line.length > MAX_DESCRIPTION_ENTRY_CHARS) line = `${line.slice(0, MAX_DESCRIPTION_ENTRY_CHARS - 1)}…`;
			if (index >= MAX_DESCRIPTION_ENTRIES || size + line.length > MAX_DESCRIPTION_CHARS) {
				lines.push(`- … ${skills.length - index} more: \`await skills.code_list()\``);
				break;
			}
			lines.push(line);
			size += line.length + 1;
		}
		return [`Active code skills (\`from code_skills import <name>\`):`, ...lines].join("\n");
	}
}

export function createCodeSkills(options: CodeSkillOptions = {}): CodeSkills {
	return new CodeSkills(options);
}

/** The rlm tool description's code-skills section: the prompt fragment plus the bounded active listing. */
export function codeSkillsToolSection(skills: Pick<CodeSkills, "describe"> = new CodeSkills()): string {
	let listing = "";
	try {
		listing = skills.describe();
	} catch {}
	return `\n${CODE_SKILLS_PROMPT}${listing ? `\n${listing}` : ""}`;
}
