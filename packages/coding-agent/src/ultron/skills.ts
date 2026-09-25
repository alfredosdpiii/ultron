import { createHash, randomUUID } from "node:crypto";
import { dirname } from "node:path";
import { isJsonValue, type JsonValue } from "@earendil-works/chord";
import { parseFrontmatter } from "../utils/frontmatter.ts";
import type { HostModuleRequest, HostModuleStore, NativeHostApi, NativeHostModule } from "./rlm/host-module.ts";

/** A skill as discovered by Pi's existing loader; this module never discovers skills itself. */
export type SkillSource = {
	name: string;
	description: string;
	filePath: string;
	content: string;
	disableModelInvocation?: boolean;
};

export type SkillModuleOptions = {
	store: HostModuleStore;
	loadSkills: () => Promise<SkillSource[]>;
	now?: () => number;
};

type CatalogEntry = {
	name: string;
	description: string;
	file_path: string;
	version: string;
	invocable: boolean;
};

type SkillMatch = { term: string; fields: Array<"name" | "description" | "body"> };

type Candidate = { name: string; version: string; score: number; matches: SkillMatch[] };

type Decision = {
	id: string;
	at: number;
	query: string;
	terms: string[];
	caller_task_id: string | null;
	catalog_generation: number;
	considered: Record<string, string>;
	excluded: Array<{ name: string; version: string; reason: "not_invocable" }>;
	candidates: Candidate[];
	chosen: Array<{ name: string; version: string }>;
};

type LoadRecord = {
	id: string;
	at: number;
	name: string;
	version: string;
	caller_task_id: string | null;
	context_chars: number;
};

type Invocation = {
	id: string;
	at: number;
	name: string;
	version: string;
	caller_task_id: string | null;
	state: "admitting" | "spawned" | "failed";
	task_id?: string;
	error?: string;
};

type SkillDocument = {
	format: 1;
	generation: number;
	catalog: CatalogEntry[];
	changes: Array<{ at: number; generation: number; name: string; from: string | null; to: string | null }>;
	decisions: Decision[];
	loads: LoadRecord[];
	invocations: Invocation[];
};

type Skill = CatalogEntry & {
	content: string;
	body: string;
	ignoredCapabilityRequests: Record<string, JsonValue>;
};

/** Immutable once built. A refresh installs a new snapshot; readers keep the one they captured. */
type Snapshot = { generation: number; skills: ReadonlyMap<string, Skill> };

/**
 * Frontmatter keys that would grant tools, models, budgets, or permissions in other skill
 * formats. They are reported and otherwise ignored: a skill is instructions, never authority.
 */
const CAPABILITY_KEYS = [
	"allowed-tools",
	"allowed_tools",
	"allowedTools",
	"tools",
	"grants",
	"permissions",
	"capabilities",
	"model",
	"budget",
	"timeout",
];

const STOPWORDS = new Set([
	"a",
	"an",
	"and",
	"are",
	"for",
	"how",
	"i",
	"in",
	"is",
	"it",
	"me",
	"my",
	"of",
	"on",
	"or",
	"the",
	"this",
	"to",
	"with",
]);

const WEIGHTS = { name: 3, description: 2, body: 1 } as const;
const EXACT_NAME_BONUS = 5;
const MAX_RECORDS = 2000;
const MAX_QUERY_CHARS = 4000;

const sha256 = (value: string): string => createHash("sha256").update(value).digest("hex");

function fields(payload: Record<string, unknown>, allowed: string[]): void {
	for (const key of Object.keys(payload)) {
		if (!allowed.includes(key)) throw new Error(`Unknown payload field: ${key}`);
	}
}

function nonemptyString(value: unknown, name: string): string {
	if (typeof value !== "string" || !value.trim()) throw new Error(`${name} must be a nonempty string`);
	return value;
}

function versionString(value: unknown): string {
	if (typeof value !== "string" || !/^[a-f0-9]{64}$/.test(value))
		throw new Error("version must be a lowercase sha256 hex digest");
	return value;
}

function tokens(text: string): string[] {
	return text.toLowerCase().match(/[a-z0-9]+/g) ?? [];
}

function queryTerms(query: string): string[] {
	return [...new Set(tokens(query).filter((term) => term.length > 1 && !STOPWORDS.has(term)))];
}

function toJson(value: unknown): JsonValue {
	// YAML can produce dates and other non-JSON values; normalize before recording.
	try {
		const json = JSON.parse(JSON.stringify(value ?? null)) as unknown;
		return isJsonValue(json) ? json : String(value);
	} catch {
		return String(value);
	}
}

function parseSkill(source: SkillSource): Skill {
	let frontmatter: Record<string, unknown> = {};
	let body = source.content;
	try {
		({ frontmatter, body } = parseFrontmatter(source.content));
	} catch {
		// Pi's loader already rejected unparseable skills; tolerate them here as plain bodies.
	}
	const ignoredCapabilityRequests: Record<string, JsonValue> = {};
	for (const key of CAPABILITY_KEYS) {
		if (frontmatter && Object.hasOwn(frontmatter, key)) ignoredCapabilityRequests[key] = toJson(frontmatter[key]);
	}
	return {
		name: source.name,
		description: source.description,
		file_path: source.filePath,
		version: sha256(source.content),
		invocable: source.disableModelInvocation !== true,
		content: source.content,
		body,
		ignoredCapabilityRequests,
	};
}

function validateSources(value: unknown): SkillSource[] {
	if (!Array.isArray(value)) throw new Error("loadSkills must return an array");
	const seen = new Set<string>();
	for (const source of value as SkillSource[]) {
		if (
			!source ||
			typeof source.name !== "string" ||
			!source.name ||
			typeof source.description !== "string" ||
			typeof source.filePath !== "string" ||
			typeof source.content !== "string"
		)
			throw new Error("loadSkills returned an invalid skill");
		if (seen.has(source.name)) throw new Error(`Duplicate skill name: ${source.name}`);
		seen.add(source.name);
	}
	return value as SkillSource[];
}

function emptyDocument(): SkillDocument {
	return { format: 1, generation: 0, catalog: [], changes: [], decisions: [], loads: [], invocations: [] };
}

function readDocument(value: JsonValue | undefined): SkillDocument {
	if (value === undefined) return emptyDocument();
	const document = value as SkillDocument;
	if (
		!document ||
		typeof document !== "object" ||
		document.format !== 1 ||
		!Number.isSafeInteger(document.generation) ||
		!Array.isArray(document.catalog) ||
		!Array.isArray(document.changes) ||
		!Array.isArray(document.decisions) ||
		!Array.isArray(document.loads) ||
		!Array.isArray(document.invocations)
	)
		throw new Error("Skill module document is invalid");
	return document;
}

function capped<T>(records: T[]): T[] {
	return records.length > MAX_RECORDS ? records.slice(records.length - MAX_RECORDS) : records;
}

function score(skill: Skill, terms: string[], query: string): Candidate | undefined {
	const nameTokens = new Set(tokens(skill.name));
	const descriptionTokens = new Set(tokens(skill.description));
	const bodyTokens = new Set(tokens(skill.body));
	const matches: SkillMatch[] = [];
	let total = 0;
	for (const term of terms) {
		const matched: SkillMatch["fields"] = [];
		if (nameTokens.has(term)) matched.push("name");
		if (descriptionTokens.has(term)) matched.push("description");
		if (bodyTokens.has(term)) matched.push("body");
		if (!matched.length) continue;
		matches.push({ term, fields: matched });
		for (const field of matched) total += WEIGHTS[field];
	}
	if (skill.name.toLowerCase() === query.trim().toLowerCase()) total += EXACT_NAME_BONUS;
	return total > 0 ? { name: skill.name, version: skill.version, score: total, matches } : undefined;
}

function invocationPrompt(skill: Skill, input: JsonValue): string {
	// Same block shape as Pi's /skill expansion so existing renderers recognize it.
	return `<skill name="${skill.name}" location="${skill.file_path}">\nReferences are relative to ${dirname(skill.file_path)}.\n\n${skill.body}\n</skill>\n\nSkill version: ${skill.version}\n\nInput (JSON):\n${JSON.stringify(input)}`;
}

class SkillModule implements NativeHostModule {
	readonly prefixes = ["skills."] as const;
	private snapshot?: Snapshot;
	private queue: Promise<unknown> = Promise.resolve();
	private readonly store: HostModuleStore;
	private readonly loadSkills: () => Promise<SkillSource[]>;
	private readonly now: () => number;

	constructor(options: SkillModuleOptions) {
		if (!options?.store) throw new Error("createSkillModule requires options.store");
		if (typeof options.loadSkills !== "function") throw new Error("createSkillModule requires options.loadSkills");
		this.store = options.store;
		this.loadSkills = options.loadSkills;
		this.now = options.now ?? Date.now;
	}

	/** Durable mutations and snapshot installation run one at a time. */
	private exclusive<T>(operation: () => Promise<T>): Promise<T> {
		const next = this.queue.then(operation, operation);
		this.queue = next.catch(() => {});
		return next;
	}

	private async mutate<T>(change: (document: SkillDocument) => T): Promise<T> {
		const document = readDocument(await this.store.read());
		const result = change(document);
		await this.store.write(document as unknown as JsonValue);
		return result;
	}

	/** Must run inside {@link exclusive}. */
	private async refreshLocked(): Promise<{ generation: number; changes: SkillDocument["changes"] }> {
		const skills = validateSources(await this.loadSkills()).map(parseSkill);
		const at = this.now();
		const outcome = await this.mutate((document) => {
			const previous = new Map(document.catalog.map((entry) => [entry.name, entry.version]));
			const next = new Map(skills.map((skill) => [skill.name, skill.version]));
			const generation = document.generation + 1;
			const changes: SkillDocument["changes"] = [];
			for (const name of [...new Set([...previous.keys(), ...next.keys()])].sort()) {
				const from = previous.get(name) ?? null;
				const to = next.get(name) ?? null;
				if (from !== to) changes.push({ at, generation, name, from, to });
			}
			document.generation = generation;
			document.catalog = skills.map(({ name, description, file_path, version, invocable }) => ({
				name,
				description,
				file_path,
				version,
				invocable,
			}));
			document.changes = capped([...document.changes, ...changes]);
			return { generation, changes };
		});
		// Install only after the catalog record is durable.
		this.snapshot = { generation: outcome.generation, skills: new Map(skills.map((skill) => [skill.name, skill])) };
		return outcome;
	}

	/** One consistent catalog for the whole request, loaded on first use. */
	private async current(): Promise<Snapshot> {
		if (this.snapshot) return this.snapshot;
		return this.exclusive(async () => {
			if (!this.snapshot) await this.refreshLocked();
			return this.snapshot!;
		});
	}

	private pinned(snapshot: Snapshot, name: string, version: string | undefined): Skill {
		const skill = snapshot.skills.get(name);
		if (!skill) {
			if (version !== undefined)
				throw new Error(`Stale skill version: ${name}@${version} is no longer in the catalog (skill removed)`);
			throw new Error(`Unknown skill: ${name}`);
		}
		if (version !== undefined && skill.version !== version)
			throw new Error(
				`Stale skill version: ${name}@${version} was requested but the catalog has ${name}@${skill.version}; select or load again`,
			);
		return skill;
	}

	async start(): Promise<void> {
		await this.current();
	}

	async handle(request: HostModuleRequest, host: NativeHostApi): Promise<unknown> {
		const { type, payload } = request;
		const caller = host.callerTaskId(request.caller);
		switch (type) {
			case "skills.refresh": {
				fields(payload, []);
				return this.exclusive(() => this.refreshLocked());
			}
			case "skills.list": {
				fields(payload, []);
				const snapshot = await this.current();
				return {
					generation: snapshot.generation,
					skills: [...snapshot.skills.values()].map(({ name, description, version, invocable }) => ({
						name,
						description,
						version,
						invocable,
					})),
				};
			}
			case "skills.select":
				return this.select(payload, caller);
			case "skills.load":
				return this.load(payload, caller);
			case "skills.why": {
				fields(payload, ["decision_id"]);
				const id = nonemptyString(payload.decision_id, "decision_id");
				const decision = readDocument(await this.store.read()).decisions.find((item) => item.id === id);
				if (!decision) throw new Error(`Unknown skill decision: ${id}`);
				return decision;
			}
			case "skills.invoke":
				return this.invoke(request, payload, caller, host);
			default:
				throw new Error(`Unsupported skills request: ${type}`);
		}
	}

	private async select(payload: Record<string, unknown>, caller: string | null): Promise<Decision> {
		fields(payload, ["query", "limit"]);
		const query = nonemptyString(payload.query, "query");
		if (query.length > MAX_QUERY_CHARS) throw new Error(`query must be at most ${MAX_QUERY_CHARS} characters`);
		const limit = payload.limit ?? 5;
		if (typeof limit !== "number" || !Number.isSafeInteger(limit) || limit < 1 || limit > 50)
			throw new Error("limit must be an integer between 1 and 50");
		const snapshot = await this.current();
		const terms = queryTerms(query);
		const considered: Record<string, string> = {};
		const excluded: Decision["excluded"] = [];
		const scored: Candidate[] = [];
		for (const skill of [...snapshot.skills.values()].sort((a, b) => a.name.localeCompare(b.name))) {
			considered[skill.name] = skill.version;
			if (!skill.invocable) {
				excluded.push({ name: skill.name, version: skill.version, reason: "not_invocable" });
				continue;
			}
			const candidate = score(skill, terms, query);
			if (candidate) scored.push(candidate);
		}
		scored.sort((a, b) => b.score - a.score || (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
		const candidates = scored.slice(0, limit);
		const decision: Decision = {
			id: randomUUID(),
			at: this.now(),
			query,
			terms,
			caller_task_id: caller,
			catalog_generation: snapshot.generation,
			considered,
			excluded,
			candidates,
			chosen: candidates.map(({ name, version }) => ({ name, version })),
		};
		await this.exclusive(() =>
			this.mutate((document) => {
				document.decisions = capped([...document.decisions, decision]);
			}),
		);
		return structuredClone(decision);
	}

	private async load(payload: Record<string, unknown>, caller: string | null): Promise<unknown> {
		fields(payload, ["name", "version"]);
		const name = nonemptyString(payload.name, "name");
		const version = payload.version === undefined ? undefined : versionString(payload.version);
		const skill = this.pinned(await this.current(), name, version);
		const record: LoadRecord = {
			id: randomUUID(),
			at: this.now(),
			name,
			version: skill.version,
			caller_task_id: caller,
			context_chars: skill.content.length,
		};
		await this.exclusive(() =>
			this.mutate((document) => {
				document.loads = capped([...document.loads, record]);
			}),
		);
		return {
			load_id: record.id,
			name,
			version: skill.version,
			description: skill.description,
			invocable: skill.invocable,
			content: skill.content,
			context_chars: record.context_chars,
			ignored_capability_requests: structuredClone(skill.ignoredCapabilityRequests),
		};
	}

	private async invoke(
		request: HostModuleRequest,
		payload: Record<string, unknown>,
		caller: string | null,
		host: NativeHostApi,
	): Promise<unknown> {
		fields(payload, ["name", "version", "input"]);
		const name = nonemptyString(payload.name, "name");
		const version = versionString(payload.version);
		if (!Object.hasOwn(payload, "input") || !isJsonValue(payload.input)) throw new Error("input must be JSON");
		const input = payload.input;
		const skill = this.pinned(await this.current(), name, version);
		if (!skill.invocable) throw new Error(`Skill ${name} disables model invocation`);
		const invocation: Invocation = {
			id: randomUUID(),
			at: this.now(),
			name,
			version: skill.version,
			caller_task_id: caller,
			state: "admitting",
		};
		const record = (change: Partial<Invocation>) =>
			this.exclusive(() =>
				this.mutate((document) => {
					const existing = document.invocations.find((item) => item.id === invocation.id);
					if (existing) Object.assign(existing, change);
					else document.invocations = capped([...document.invocations, { ...invocation, ...change }]);
				}),
			);
		await record({});
		let taskId: string;
		try {
			// Only definition, input, and key: frontmatter never reaches model, tools, or budget.
			const task = await host.spawn(
				{
					definition: "rlm-child@1",
					input: { prompt: invocationPrompt(skill, input) },
					key: `skills.invoke:${invocation.id}`,
				},
				caller,
				request.context,
			);
			taskId = task.id;
		} catch (error) {
			await record({ state: "failed", error: String(error instanceof Error ? error.message : error) });
			throw error;
		}
		await record({ state: "spawned", task_id: taskId });
		return {
			invocation_id: invocation.id,
			task_id: taskId,
			name,
			version: skill.version,
			ignored_capability_requests: structuredClone(skill.ignoredCapabilityRequests),
		};
	}
}

/** Host module for `skills.*` requests: version-pinned, explainable, and capability-free. */
export function createSkillModule(options: SkillModuleOptions): NativeHostModule {
	return new SkillModule(options);
}
