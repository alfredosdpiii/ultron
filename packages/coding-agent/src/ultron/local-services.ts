import { createHash, randomUUID } from "node:crypto";
import type { Context, JsonValue } from "@earendil-works/chord";

export type { JsonValue } from "@earendil-works/chord";

export interface LocalServiceDocument {
	key: string;
	value: JsonValue;
}

/**
 * Adapter over authoritative session values, scoped by the caller to one owner.
 * get returns undefined only for a missing key. set atomically replaces one whole
 * document and resolves only after durable commit. list returns all matching keys
 * in insertion order; replacing a key must not move it. Reads must see completed
 * writes. Errors must propagate, never masquerade as missing documents.
 *
 * No transaction or compare-and-set is assumed. Use one service instance per
 * owner, or provide external serialization across all instances and processes.
 */
export interface DurableDocumentStorage {
	get(key: string, context: Context): Promise<JsonValue | undefined>;
	set(key: string, value: JsonValue, context: Context): Promise<void>;
	list(prefix: string, context: Context): Promise<LocalServiceDocument[]>;
}

export type RefinementKind = "observation" | "instruction" | "skill" | "agent";
export type RefinementState = "proposed" | "active" | "superseded" | "rejected" | "rolled_back";
export type RefinementProposal = {
	kind: RefinementKind;
	target: string;
	baseVersion: number;
	content: JsonValue;
	evidence: JsonValue;
	scope: string;
};
export type RefinementRecord = RefinementProposal & {
	id: string;
	version: number | null;
	previousId: string | null;
	state: RefinementState;
	history: { state: RefinementState; at: string; cause: string }[];
};
export type ArtifactRecord = { id: string; bytes: number; mediaType: string; label: string };
export type ExperimentRecord = {
	[key: string]: JsonValue;
	id: string;
	recordedAt: string;
	variant: string;
	fixtureHash: string;
	outcome: "passed" | "failed" | "incomplete";
};

export interface NativeLocalServicesOptions {
	/**
	 * Optional domain schema check on activation only, never during replay.
	 * Receives a defensive copy and must synchronously return true or undefined.
	 * false, other values, promises and exceptions reject activation. It must not
	 * call handle. Built-in JSON, evidence and version checks are always active.
	 */
	validate?: (kind: RefinementKind, content: JsonValue) => unknown;
	/**
	 * Total bytes of live artifacts this service may hold. A put beyond it fails with
	 * ArtifactQuotaError; nothing is evicted to make room. Unset means no service quota.
	 */
	artifactQuotaBytes?: number;
}

/** Explicit quota failure. The artifact was not stored and no other artifact was removed. */
export class ArtifactQuotaError extends Error {
	readonly code = "artifact_quota_exceeded";
	readonly usedBytes: number;
	readonly requestedBytes: number;
	readonly quotaBytes: number;
	constructor(usedBytes: number, requestedBytes: number, quotaBytes: number) {
		super(
			`Artifact quota exceeded: ${usedBytes} bytes used + ${requestedBytes} requested > ${quotaBytes} byte quota. ` +
				"Delete or reclaim unpinned artifacts first.",
		);
		this.name = "ArtifactQuotaError";
		this.usedBytes = usedBytes;
		this.requestedBytes = requestedBytes;
		this.quotaBytes = quotaBytes;
	}
}

type JsonObject = { [key: string]: JsonValue };
const refinementKey = "ultron.refinements";
const experimentKey = "ultron.experiments";
const artifactPrefix = "ultron.artifacts/";
// Deliberately outside artifactPrefix so listings never see it.
const artifactRefsKey = "ultron.artifact-refs";
const kinds: readonly string[] = ["observation", "instruction", "skill", "agent"];
const protectedNames = new Set(["agents.md", "security", "policy"]);

// Copy descriptors rather than stringify user objects: getters and toJSON must
// never run, and undefined, sparse arrays, cycles and nonfinite numbers must fail.
function jsonCopy(value: unknown): JsonValue {
	const ancestors = new Set<object>();
	function copy(item: unknown): JsonValue {
		if (item === null || typeof item === "string" || typeof item === "boolean") return item;
		if (typeof item === "number" && Number.isFinite(item)) return item;
		if (typeof item !== "object" || ancestors.has(item)) throw new TypeError("Expected finite, acyclic JSON data");
		const array = Array.isArray(item);
		const prototype: unknown = Object.getPrototypeOf(item);
		if (array ? prototype !== Array.prototype : prototype !== Object.prototype && prototype !== null) {
			throw new TypeError("Expected plain JSON data");
		}
		ancestors.add(item);
		const keys = Reflect.ownKeys(item).filter((key) => !(array && key === "length"));
		if (array && keys.length !== item.length) throw new TypeError("Sparse arrays are not JSON data");
		const result: JsonValue[] | JsonObject = array ? [] : {};
		for (const key of keys) {
			const descriptor = Object.getOwnPropertyDescriptor(item, key)!;
			if (
				typeof key !== "string" ||
				!descriptor.enumerable ||
				!Object.hasOwn(descriptor, "value") ||
				(array && (!/^(0|[1-9][0-9]*)$/.test(key) || Number(key) >= item.length))
			) {
				throw new TypeError("Expected plain JSON properties");
			}
			Object.defineProperty(result, key, {
				value: copy(descriptor.value),
				enumerable: true,
				writable: true,
				configurable: true,
			});
		}
		ancestors.delete(item);
		return result;
	}
	return copy(value);
}

function fields(value: JsonValue, required: string[], optional: string[] = []): asserts value is JsonObject {
	if (
		value === null ||
		typeof value !== "object" ||
		Array.isArray(value) ||
		required.some((key) => !Object.hasOwn(value, key)) ||
		Object.keys(value).some((key) => !required.includes(key) && !optional.includes(key))
	)
		throw new TypeError("Malformed local service fields");
}

function identity(kind: JsonValue, target: JsonValue): asserts kind is RefinementKind {
	if (typeof kind !== "string" || !kinds.includes(kind)) throw new TypeError("Invalid refinement kind");
	if (typeof target !== "string" || !/^[a-z]+:[a-zA-Z0-9_-]+(?:\.[a-zA-Z0-9_-]+)*$/.test(target)) {
		throw new TypeError("Target must be an explicit namespace:name ID, not a path");
	}
	const [namespace, name] = target.split(":");
	if (namespace !== kind || protectedNames.has(name.toLowerCase())) throw new Error("Forbidden refinement target");
}

function nonempty(value: JsonValue): boolean {
	if (value === null) return false;
	if (typeof value === "string") return value.trim().length > 0;
	if (typeof value === "object") return Object.values(value).some(nonempty);
	return true;
}

function proposal(value: JsonValue): RefinementProposal {
	fields(value, ["kind", "target", "baseVersion", "content"], ["evidence", "scope"]);
	const { kind, target, baseVersion, content, evidence = [], scope = "session" } = value;
	identity(kind, target);
	if (typeof baseVersion !== "number" || !Number.isSafeInteger(baseVersion) || baseVersion < 0) {
		throw new TypeError("baseVersion must be a nonnegative safe integer");
	}
	if (!(typeof content === "string" || (content !== null && typeof content === "object")) || !nonempty(content)) {
		throw new TypeError("Content must be nonempty JSON text, an object, or an array");
	}
	if (!(evidence === null || typeof evidence === "string" || typeof evidence === "object")) {
		throw new TypeError("Evidence must be JSON text, an object, or an array");
	}
	if (typeof scope !== "string" || !scope.trim()) throw new TypeError("Scope must be a nonempty string");
	return { kind, target: target as string, baseVersion, content, evidence, scope };
}

function lookup(records: RefinementRecord[], id: JsonValue): RefinementRecord {
	if (typeof id !== "string" || !id) throw new TypeError("Invalid refinement ID");
	const record = records.find((record) => record.id === id);
	if (!record) throw new Error(`Unknown refinement: ${id}`);
	return record;
}

function active(records: RefinementRecord[], kind: JsonValue, target: JsonValue): RefinementRecord | null {
	return (
		records.find((record) => record.kind === kind && record.target === target && record.state === "active") ?? null
	);
}

function apply(records: RefinementRecord[], event: JsonValue): RefinementRecord {
	if (event === null || typeof event !== "object" || Array.isArray(event))
		throw new TypeError("Malformed refinement event");
	fields(event, ["action", "id", "at"], event.action === "propose" ? ["proposal"] : []);
	const { id, at } = event;
	if (typeof id !== "string" || !id || typeof at !== "string" || !Number.isFinite(Date.parse(at))) {
		throw new TypeError("Malformed refinement event");
	}
	const transition = (record: RefinementRecord, state: RefinementState): void => {
		record.state = state;
		record.history.push({ state, at, cause: id });
	};
	if (event.action === "propose") {
		const data = proposal(event.proposal);
		if (records.some((record) => record.id === id)) throw new Error("Duplicate refinement ID");
		const record: RefinementRecord = { id, ...data, version: null, previousId: null, state: "proposed", history: [] };
		transition(record, "proposed");
		records.push(record);
		return record;
	}
	const record = lookup(records, id);
	if (event.action === "activate") {
		if (record.state !== "proposed") throw new Error("Only a proposed refinement can be activated");
		identity(record.kind, record.target);
		if (!nonempty(record.evidence)) throw new Error("Activation requires nonempty evidence");
		const previous = active(records, record.kind, record.target);
		if (record.baseVersion !== (previous?.version ?? 0)) throw new Error("Stale base version");
		let version = 0;
		for (const item of records) {
			if (item.kind === record.kind && item.target === record.target) version = Math.max(version, item.version ?? 0);
		}
		if (!Number.isSafeInteger(version + 1)) throw new Error("Version exhausted");
		record.version = version + 1;
		record.previousId = previous?.id ?? null;
		if (previous) transition(previous, "superseded");
		transition(record, "active");
	} else if (event.action === "reject") {
		if (record.state !== "proposed") throw new Error("Only a proposed refinement can be rejected");
		transition(record, "rejected");
	} else if (event.action === "rollback") {
		if (active(records, record.kind, record.target)?.id !== record.id)
			throw new Error("Only the latest active refinement can be rolled back");
		transition(record, "rolled_back");
		if (record.previousId !== null) transition(lookup(records, record.previousId), "active");
	} else throw new Error("Unknown refinement action");
	return record;
}

function experiment(value: JsonValue): asserts value is ExperimentRecord {
	if (
		value === null ||
		typeof value !== "object" ||
		Array.isArray(value) ||
		typeof value.variant !== "string" ||
		typeof value.fixtureHash !== "string" ||
		!["passed", "failed", "incomplete"].includes(String(value.outcome)) ||
		typeof value.id !== "string" ||
		!value.id ||
		typeof value.recordedAt !== "string" ||
		!Number.isFinite(Date.parse(value.recordedAt))
	)
		throw new TypeError("Experiment requires variant, fixtureHash, outcome and valid record metadata");
}

type ArtifactTombstone = ArtifactRecord & { deleted: { at: string; reason: string } };

function artifactDocument(
	value: JsonValue,
	id: string,
): { record: ArtifactRecord; bytes: Buffer } | { record: ArtifactRecord; tombstone: ArtifactTombstone } {
	if (value !== null && typeof value === "object" && !Array.isArray(value) && Object.hasOwn(value, "deleted")) {
		fields(value, ["formatVersion", "id", "bytes", "mediaType", "label", "deleted"]);
		const { deleted } = value;
		fields(deleted, ["at", "reason"]);
		if (
			value.formatVersion !== 1 ||
			value.id !== id ||
			!/^[a-f0-9]{64}$/.test(id) ||
			typeof value.bytes !== "number" ||
			typeof value.mediaType !== "string" ||
			typeof value.label !== "string" ||
			typeof deleted.at !== "string" ||
			typeof deleted.reason !== "string"
		)
			throw new Error("Invalid artifact document");
		const record = { id, bytes: value.bytes, mediaType: value.mediaType, label: value.label };
		return { record, tombstone: { ...record, deleted: { at: deleted.at, reason: deleted.reason } } };
	}
	return artifact(value, id);
}

type ArtifactRefs = Record<string, string[]>;

function artifactRefs(value: JsonValue | undefined): ArtifactRefs {
	if (value === undefined) return {};
	fields(value, ["formatVersion", "refs"]);
	const { refs } = value;
	if (value.formatVersion !== 1 || refs === null || typeof refs !== "object" || Array.isArray(refs))
		throw new Error("Invalid artifact reference document");
	const result: ArtifactRefs = {};
	for (const [id, holders] of Object.entries(refs)) {
		if (!Array.isArray(holders) || holders.some((holder) => typeof holder !== "string"))
			throw new Error("Invalid artifact reference document");
		result[id] = holders as string[];
	}
	return result;
}

function holderName(value: JsonValue): string {
	if (typeof value !== "string" || !value.trim() || value.length > 200)
		throw new TypeError("Artifact holder must be a nonempty string of at most 200 characters");
	return value;
}

function artifact(value: JsonValue, id: string): { record: ArtifactRecord; bytes: Buffer } {
	fields(value, ["formatVersion", "id", "bytes", "mediaType", "label", "text"]);
	if (
		value.formatVersion !== 1 ||
		value.id !== id ||
		!/^[a-f0-9]{64}$/.test(id) ||
		typeof value.text !== "string" ||
		typeof value.mediaType !== "string" ||
		typeof value.label !== "string"
	)
		throw new Error("Invalid artifact document");
	const bytes = Buffer.from(value.text);
	if (createHash("sha256").update(bytes).digest("hex") !== id || value.bytes !== bytes.length) {
		throw new Error("Artifact integrity failure");
	}
	return { record: { id, bytes: bytes.length, mediaType: value.mediaType, label: value.label }, bytes };
}

/**
 * Native, data-only services. No approval prompt, filesystem ledger, compat
 * import, definition execution, schema verification claim or global activation.
 * Scope is refinement metadata, not a separate version namespace or sandbox.
 * Evidence presence does not establish correctness.
 *
 * handle accepts the host request payloads:
 * - refinements.propose: {kind,target,baseVersion,content,evidence?,scope?}
 * - refinements.get/activate/reject/rollback: {id}; current: {kind,target}; list: {}
 * - artifacts.put: {text,options?:{mediaType?,label?}}
 * - artifacts.read: {id,options?:{offset?,length?}} -> {text,base64,offset,length,total}; list: {}
 * - artifacts.pin/unpin: {id,holder}; delete: {id,reason?} refuses while any holder pins it
 * - artifacts.usage: {}; reclaim: {bytes,reason?} tombstones unpinned artifacts oldest first
 * - experiments.record: {run}; compare: {baseline,candidate}; list: {}
 *
 * All calls serialize within this instance and reload authoritative documents.
 * One document replacement commits each mutation. Any failed set makes this
 * instance unusable: reopen with a reconciled adapter before continuing, since
 * a rejected commit can have an ambiguous durability outcome. Callbacks must
 * be supplied again after restart. No automatic retries or effect replay.
 * Documents are versioned. Artifacts obey artifactQuotaBytes when set; deletion
 * leaves a tombstone so later reads fail explicitly. Other size limits are external.
 */
export class NativeLocalServices {
	private readonly documents: DurableDocumentStorage;
	private readonly validate: NativeLocalServicesOptions["validate"];
	private readonly artifactQuotaBytes: number | undefined;
	private tail: Promise<void> = Promise.resolve();
	private validating = false;
	private uncertain = false;

	constructor(documents: DurableDocumentStorage, options: NativeLocalServicesOptions = {}) {
		if (options.validate !== undefined && typeof options.validate !== "function")
			throw new TypeError("validate must be a function");
		this.documents = documents;
		this.validate = options.validate;
		const quota = options.artifactQuotaBytes;
		if (quota !== undefined && (!Number.isSafeInteger(quota) || quota < 0))
			throw new TypeError("artifactQuotaBytes must be a nonnegative safe integer");
		this.artifactQuotaBytes = quota;
	}

	handle(type: string, payload: unknown, context: Context): Promise<JsonValue> {
		// Synchronous rejection lets a validator's nested call reject its activation.
		if (this.validating) throw new Error("Reentrant local service request is not supported");
		let data: JsonValue;
		try {
			data = jsonCopy(payload);
		} catch (error) {
			return Promise.reject(error);
		}
		const result = this.tail.then(async () => {
			if (this.uncertain) throw new Error("Document durability is uncertain; reopen the service before use");
			context.abortSignal?.throwIfAborted();
			return jsonCopy(await this.dispatch(type, data, context));
		});
		this.tail = result.then(
			() => {},
			() => {},
		);
		return result;
	}

	private async persist(key: string, value: JsonValue, context: Context): Promise<void> {
		context.abortSignal?.throwIfAborted();
		const document = jsonCopy(value);
		try {
			await this.documents.set(key, document, context);
		} catch (error) {
			this.uncertain = true;
			throw error;
		}
	}

	private async refinements(type: string, payload: JsonValue, context: Context): Promise<JsonValue> {
		const stored = await this.documents.get(refinementKey, context);
		const records: RefinementRecord[] = [];
		let events: JsonValue[] = [];
		if (stored !== undefined) {
			try {
				const document = jsonCopy(stored);
				fields(document, ["formatVersion", "events"]);
				if (document.formatVersion !== 1 || !Array.isArray(document.events))
					throw new Error("Unsupported document format");
				events = document.events;
				for (const event of events) apply(records, event);
			} catch (error) {
				throw new Error("Invalid refinement document", { cause: error });
			}
		}
		if (type === "refinements.list") {
			fields(payload, []);
			return records;
		}
		if (type === "refinements.current") {
			fields(payload, ["kind", "target"]);
			identity(payload.kind, payload.target);
			return active(records, payload.kind, payload.target);
		}
		let event: JsonObject;
		if (type === "refinements.propose") {
			event = { action: "propose", id: randomUUID(), proposal: proposal(payload), at: new Date().toISOString() };
		} else {
			fields(payload, ["id"]);
			if (type === "refinements.get") return lookup(records, payload.id);
			event = { action: type.slice("refinements.".length), id: payload.id, at: new Date().toISOString() };
		}
		const result = apply(records, event);
		if (event.action === "activate" && this.validate) {
			this.validating = true;
			try {
				const valid = this.validate(result.kind, jsonCopy(result.content));
				if (
					valid !== null &&
					(typeof valid === "object" || typeof valid === "function") &&
					"then" in valid &&
					typeof valid.then === "function"
				) {
					Promise.resolve(valid).catch(() => {});
					throw new TypeError("validate must be synchronous");
				}
				if (valid !== undefined && valid !== true) throw new Error("Refinement content validation failed");
			} finally {
				this.validating = false;
			}
		}
		await this.persist(refinementKey, { formatVersion: 1, events: [...events, event] }, context);
		return result;
	}

	private async artifactIndex(context: Context): Promise<{
		live: ArtifactRecord[];
		deleted: ArtifactTombstone[];
		refs: ArtifactRefs;
	}> {
		const documents = await this.documents.list(artifactPrefix, context);
		const seen = new Set<string>();
		const live: ArtifactRecord[] = [];
		const deleted: ArtifactTombstone[] = [];
		for (const { key, value } of documents) {
			if (!key.startsWith(artifactPrefix) || seen.has(key)) throw new Error("Invalid artifact listing");
			seen.add(key);
			const parsed = artifactDocument(jsonCopy(value), key.slice(artifactPrefix.length));
			if ("tombstone" in parsed) deleted.push(parsed.tombstone);
			else live.push(parsed.record);
		}
		const refs = artifactRefs(await this.documents.get(artifactRefsKey, context));
		return { live, deleted, refs };
	}

	private async tombstone(record: ArtifactRecord, reason: string, context: Context): Promise<void> {
		await this.persist(
			artifactPrefix + record.id,
			{ formatVersion: 1, ...record, deleted: { at: new Date().toISOString(), reason } },
			context,
		);
	}

	private async artifacts(type: string, payload: JsonValue, context: Context): Promise<JsonValue> {
		if (type === "artifacts.list") {
			fields(payload, []);
			return (await this.artifactIndex(context)).live;
		}
		if (type === "artifacts.usage") {
			fields(payload, []);
			const { live, refs } = await this.artifactIndex(context);
			return {
				usedBytes: live.reduce((total, record) => total + record.bytes, 0),
				quotaBytes: this.artifactQuotaBytes ?? null,
				count: live.length,
				pinned: live.filter((record) => (refs[record.id]?.length ?? 0) > 0).map((record) => record.id),
			};
		}
		if (type === "artifacts.reclaim") {
			// Deletion policy: only unpinned artifacts, oldest first, until enough bytes are free.
			fields(payload, ["bytes"], ["reason"]);
			const { bytes: wanted, reason = "reclaimed" } = payload;
			if (typeof wanted !== "number" || !Number.isSafeInteger(wanted) || wanted < 0)
				throw new TypeError("reclaim bytes must be a nonnegative safe integer");
			if (typeof reason !== "string") throw new TypeError("reclaim reason must be a string");
			const { live, refs } = await this.artifactIndex(context);
			const deleted: string[] = [];
			let freedBytes = 0;
			for (const record of live) {
				if (freedBytes >= wanted) break;
				if ((refs[record.id]?.length ?? 0) > 0) continue;
				await this.tombstone(record, reason, context);
				deleted.push(record.id);
				freedBytes += record.bytes;
			}
			const retained = live.filter((record) => (refs[record.id]?.length ?? 0) > 0).map((record) => record.id);
			return { deleted, freedBytes, satisfied: freedBytes >= wanted, retained };
		}
		if (type === "artifacts.put") {
			fields(payload, ["text"], ["options"]);
			if (typeof payload.text !== "string") throw new TypeError("artifact content must be a string");
			const options = payload.options === undefined ? {} : payload.options;
			fields(options, [], ["mediaType", "label"]);
			const { mediaType = "text/plain", label = "" } = options;
			if (typeof mediaType !== "string" || typeof label !== "string")
				throw new TypeError("Invalid artifact metadata");
			const bytes = Buffer.from(payload.text);
			const id = createHash("sha256").update(bytes).digest("hex");
			const key = artifactPrefix + id;
			const previous = await this.documents.get(key, context);
			const existing = previous === undefined ? undefined : artifactDocument(jsonCopy(previous), id);
			if (this.artifactQuotaBytes !== undefined && (existing === undefined || "tombstone" in existing)) {
				const { live } = await this.artifactIndex(context);
				const used = live.reduce((total, record) => total + record.bytes, 0);
				if (used + bytes.length > this.artifactQuotaBytes)
					throw new ArtifactQuotaError(used, bytes.length, this.artifactQuotaBytes);
			}
			const record: ArtifactRecord = { id, bytes: bytes.length, mediaType, label };
			await this.persist(key, { formatVersion: 1, ...record, text: bytes.toString("utf8") }, context);
			return { ...record, preview: bytes.subarray(0, 2048).toString("utf8") };
		}
		fields(payload, ["id"], type === "artifacts.read" ? ["options"] : ["holder", "reason"]);
		const { id } = payload;
		if (typeof id !== "string" || !/^[a-f0-9]{64}$/.test(id)) throw new Error("Unknown artifact");
		const stored = await this.documents.get(artifactPrefix + id, context);
		if (stored === undefined) throw new Error("Unknown artifact");
		const document = artifactDocument(jsonCopy(stored), id);
		if ("tombstone" in document) {
			// An explicit invalidation, never a silently dangling handle.
			throw new Error(`Artifact ${id} was deleted (${document.tombstone.deleted.reason})`);
		}
		if (type === "artifacts.pin" || type === "artifacts.unpin") {
			const holder = holderName(payload.holder ?? null);
			const refs = artifactRefs(await this.documents.get(artifactRefsKey, context));
			const holders = new Set(refs[id] ?? []);
			if (type === "artifacts.pin") holders.add(holder);
			else holders.delete(holder);
			if (holders.size > 0) refs[id] = [...holders];
			else delete refs[id];
			await this.persist(artifactRefsKey, { formatVersion: 1, refs }, context);
			return { id, holders: [...holders] };
		}
		if (type === "artifacts.delete") {
			const { reason = "deleted" } = payload;
			if (typeof reason !== "string") throw new TypeError("delete reason must be a string");
			const holders = artifactRefs(await this.documents.get(artifactRefsKey, context))[id] ?? [];
			if (holders.length > 0) throw new Error(`Artifact ${id} is retained by ${holders.join(", ")}; unpin it first`);
			await this.tombstone(document.record, reason, context);
			return { id, deleted: true };
		}
		const options = payload.options === undefined ? {} : payload.options;
		fields(options, [], ["offset", "length"]);
		const { offset = 0, length = 8192 } = options;
		if (
			typeof offset !== "number" ||
			!Number.isSafeInteger(offset) ||
			offset < 0 ||
			typeof length !== "number" ||
			!Number.isSafeInteger(length) ||
			length < 0 ||
			length > 1048576
		)
			throw new Error("Invalid artifact range");
		const { bytes } = document;
		const range = bytes.subarray(offset, offset + length);
		// text is a lossy UTF-8 decoding when the range splits a character; base64 is exact.
		return {
			id,
			text: range.toString("utf8"),
			base64: range.toString("base64"),
			offset,
			length: range.length,
			total: bytes.length,
		};
	}

	private async experiments(type: string, payload: JsonValue, context: Context): Promise<JsonValue> {
		const stored = await this.documents.get(experimentKey, context);
		let runs: ExperimentRecord[] = [];
		if (stored !== undefined) {
			const document = jsonCopy(stored);
			fields(document, ["formatVersion", "runs"]);
			if (document.formatVersion !== 1 || !Array.isArray(document.runs))
				throw new Error("Invalid experiment document");
			const seen = new Set<string>();
			runs = document.runs.map((run) => {
				experiment(run);
				if (seen.has(run.id)) throw new Error("Duplicate experiment ID");
				seen.add(run.id);
				return run;
			});
		}
		if (type === "experiments.list") {
			fields(payload, []);
			return runs;
		}
		if (type === "experiments.record") {
			fields(payload, ["run"]);
			const { run } = payload;
			if (run === null || typeof run !== "object" || Array.isArray(run))
				throw new TypeError("Invalid experiment run");
			const value = { ...run, id: randomUUID(), recordedAt: new Date().toISOString() };
			experiment(value);
			await this.persist(experimentKey, { formatVersion: 1, runs: [...runs, value] }, context);
			return value;
		}
		fields(payload, ["baseline", "candidate"]);
		const baseline = runs.find((run) => run.id === payload.baseline);
		const candidate = runs.find((run) => run.id === payload.candidate);
		if (!baseline || !candidate) throw new Error("Unknown experiment");
		if (baseline.fixtureHash !== candidate.fixtureHash)
			throw new Error("Different fixtures cannot form a matched comparison");
		return { baseline, candidate, claim: "Observed runs only; no statistical superiority established" };
	}

	private dispatch(type: string, payload: JsonValue, context: Context): Promise<JsonValue> {
		if (
			[
				"refinements.propose",
				"refinements.list",
				"refinements.get",
				"refinements.current",
				"refinements.activate",
				"refinements.reject",
				"refinements.rollback",
			].includes(type)
		) {
			return this.refinements(type, payload, context);
		}
		if (
			[
				"artifacts.put",
				"artifacts.read",
				"artifacts.list",
				"artifacts.pin",
				"artifacts.unpin",
				"artifacts.delete",
				"artifacts.usage",
				"artifacts.reclaim",
			].includes(type)
		)
			return this.artifacts(type, payload, context);
		if (["experiments.record", "experiments.list", "experiments.compare"].includes(type))
			return this.experiments(type, payload, context);
		throw new Error(`Unknown local service request: ${type}`);
	}
}
