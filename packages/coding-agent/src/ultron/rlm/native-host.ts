import { randomUUID } from "node:crypto";
import { isJsonValue, type JsonValue } from "@earendil-works/chord";
import { BACKGROUND_CONTEXT, withAbortSignal } from "@earendil-works/chord/context";
import type { AgentHarness, AgentLane, Context, Entry } from "@earendil-works/pi-agent-core";
import type { NativeUsageCallStatus, NativeUsageLedgerLike, NativeUsageReservation } from "../usage.ts";
import {
	type NativeDefinition,
	type NativeDefinitionAdapter,
	NativeDefinitionRegistry,
	type NativeDefinitionStore,
} from "./definition-registry.ts";
import {
	type NativeHostStore,
	type NativeTask,
	NativeTaskJournal,
	type NativeResult as StoredTaskResult,
	taskFingerprint,
} from "./task-store.ts";

type Payload = Record<string, unknown>;
export type NativeResult = StoredTaskResult;

type TaskRecord = NativeTask & {
	promise?: Promise<NativeResult>;
	resolve?: (result: NativeResult) => void;
	reject?: (error: unknown) => void;
	finishing?: Promise<NativeResult>;
	lane?: AgentLane;
	controller?: AbortController;
	cleanup?: () => void;
	usageReservation?: NativeUsageReservation;
	usageSettled?: boolean;
};

type TaskRequest = {
	definition: string;
	input: JsonValue;
	model?: string;
	key?: string;
	timeoutMs: number;
};

type RlmChildHandle = {
	rlm_child_id: string;
	name: string;
	session_dir: string;
	model: string;
	timeout_ms: number;
	parent_branch_anchor: string;
};

type WorkflowNode = Omit<TaskRequest, "input"> & {
	id: string;
	input?: JsonValue;
	dependsOn: string[];
	inputFrom?: string;
};

function objectInput(value: unknown): Record<string, unknown> {
	if (value === null || typeof value !== "object" || Array.isArray(value))
		throw new Error("agent input must be a JSON object");
	return value as Record<string, unknown>;
}

function textOf(entry: Entry): string {
	if (entry.type !== "message" || entry.message.role !== "assistant") return "";
	return entry.message.content
		.filter((part) => part.type === "text")
		.map((part) => part.text)
		.join("\n");
}

function jsonFrom(text: string): unknown {
	const value = text.trim().replace(/^```(?:json)?\s*\n([\s\S]*?)\n```$/, "$1");
	return JSON.parse(value);
}

function definitionKey(value: unknown): string {
	if (typeof value !== "string" || !/^[a-z][a-z0-9-]*@[0-9]+$/.test(value))
		throw new Error("definition must be id@version");
	return value;
}

function publicTask(task: TaskRecord): Record<string, unknown> {
	return {
		id: task.id,
		definition: task.definition,
		state: task.state,
		...(task.result === undefined ? {} : { result: task.result }),
	};
}

function fields(payload: Payload, allowed: string[]): void {
	for (const key of Object.keys(payload)) {
		if (!allowed.includes(key)) throw new Error(`Unknown payload field: ${key}`);
	}
}

function nonemptyString(value: unknown, name: string): string {
	if (typeof value !== "string" || !value.trim()) throw new Error(`${name} must be a nonempty string`);
	return value;
}

function taskOptions(payload: Payload): Pick<TaskRequest, "model" | "key" | "timeoutMs"> {
	let model: string | undefined;
	if (payload.model !== undefined && payload.model !== null) {
		if (typeof payload.model !== "string" || !/^[^/\s]+\/[^/\s]+(?:\/[^/\s]+)*$/.test(payload.model))
			throw new Error("model must be provider/model");
		model = payload.model;
	}
	const key = payload.key == null ? undefined : nonemptyString(payload.key, "key");
	const timeoutMs = payload.timeout_ms === undefined ? 30 * 60 * 1000 : payload.timeout_ms;
	if (typeof timeoutMs !== "number" || !Number.isSafeInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 60 * 60 * 1000)
		throw new Error("timeout_ms must be an integer between 1 and 3600000");
	return { model, key, timeoutMs };
}

export type NativeHostService = {
	handle(type: string, payload: Record<string, unknown>, context: Context): Promise<unknown>;
};
export type NativeHostOptions = {
	store: NativeHostStore;
	definitionStore?: NativeDefinitionStore;
	services?: NativeHostService;
	usage?: NativeUsageLedgerLike;
	deterministic?: NativeDefinitionAdapter;
	predict?: NativeDefinitionAdapter;
};

export class NativeRlmHost {
	private readonly tasks = new Map<string, TaskRecord>();
	private loading?: Promise<void>;
	private admissions: Promise<void> = Promise.resolve();
	private closing?: Promise<void>;
	private closed = false;
	private readonly harness: AgentHarness;
	private readonly services: NativeHostService | undefined;
	private readonly usage: NativeUsageLedgerLike | undefined;
	private readonly journal: NativeTaskJournal;
	private readonly registry: NativeDefinitionRegistry;

	constructor(harness: AgentHarness, _rootLane: AgentLane, options: NativeHostOptions) {
		if (!options?.store) throw new Error("NativeRlmHost requires options.store");
		this.harness = harness;
		this.services = options.services;
		this.usage = options.usage;
		this.journal = new NativeTaskJournal(options.store);
		this.registry = new NativeDefinitionRegistry(options.definitionStore, {
			deterministic: options.deterministic,
			predict: options.predict,
		});
	}

	private definition(key: string): NativeDefinition {
		return this.registry.get(key);
	}

	private async loadTasks(): Promise<void> {
		this.loading ??= (async () => {
			await this.registry.ready();
			for (const stored of await this.journal.list()) this.tasks.set(stored.id, stored);
			await this.usage?.ready?.();
		})();
		await this.loading;
		await this.journal.ready();
	}

	list(): ReturnType<NativeDefinitionRegistry["list"]> {
		return this.registry.list();
	}

	private request(payload: Payload): TaskRequest {
		fields(payload, ["definition", "input", "model", "key", "timeout_ms"]);
		const definition = definitionKey(payload.definition);
		if (!isJsonValue(payload.input)) throw new Error("Agent input is not JSON");
		const item = this.definition(definition);
		if (!this.registry.isValidInput(item, payload.input))
			throw this.registry.validationError(item, payload.input, "input");
		return { definition, input: payload.input, ...taskOptions(payload) };
	}

	private async execute(task: TaskRecord, request: TaskRequest, context: Context): Promise<NativeResult> {
		const definition = this.definition(task.definition);
		const signal = task.controller!.signal;
		const taskContext = withAbortSignal(signal, context);
		let modelReservation: NativeUsageReservation | undefined;
		let modelStatus: NativeUsageCallStatus = "unknown";
		try {
			signal.throwIfAborted();
			if (definition.strategy === "deterministic") {
				const value = await this.registry.deterministicValue(definition, request.input, taskContext, signal);
				if (!isJsonValue(value)) throw new Error("Deterministic adapter returned a non-JSON result");
				if (!this.registry.isValidOutput(definition, value))
					throw this.registry.validationError(definition, value, "output");
				return { status: "succeeded", value, verification: "unverified" };
			}
			if (definition.strategy === "predict") {
				let value: unknown;
				let repair: Parameters<NativeDefinitionAdapter>[0]["repair"];
				for (let attempt = 0; attempt <= definition.maxRepairs; attempt += 1) {
					signal.throwIfAborted();
					value = await this.registry.predictValue(definition, request.input, taskContext, signal, repair);
					if (isJsonValue(value) && this.registry.isValidOutput(definition, value))
						return { status: "succeeded", value, verification: "unverified" };
					repair = {
						attempt: attempt + 1,
						previous: value,
						error: this.registry.validationError(definition, value, "output").message,
					};
				}
				throw new Error(repair?.error ?? "Predict adapter returned an invalid output");
			}
			const laneName = `ultron.${definition.id}.${task.id}`;
			const lane = await this.harness.lane(laneName, taskContext);
			task.lane = lane;
			if (signal.aborted) this.abortLane(task);
			signal.throwIfAborted();
			await lane.getActiveTools(taskContext);
			signal.throwIfAborted();
			// Each lane resolves to its own Python kernel. Keep ipython enabled so
			// recursive RLM work can continue in the child lane without sharing state.
			const model = request.model ?? definition.model;
			if (model) {
				const split = model.indexOf("/");
				await lane.setModel({ provider: model.slice(0, split), modelId: model.slice(split + 1) }, taskContext);
				signal.throwIfAborted();
			}
			const prompt =
				definition.id === "rlm-child"
					? String(objectInput(request.input).prompt)
					: `${definition.instructions}\n\nInput data:\n${JSON.stringify(request.input)}\n\nOutput contract:\n${definition.outputDescription}`;
			modelReservation = await this.usage?.reserve({
				kind: "model",
				parentTaskId: task.id,
				taskId: task.id,
				requestKey: `${task.id}:model`,
				timeoutMs: request.timeoutMs,
				signal,
			});
			const response = await lane.prompt(prompt, undefined, taskContext);
			signal.throwIfAborted();
			if (!response.ok) {
				modelStatus = "failed";
				throw new Error(JSON.stringify(response.error));
			}
			modelStatus = "succeeded";
			if (response.value.status !== "completed")
				throw new Error(`Agent run did not complete: ${response.value.status}`);
			const tipId = response.value.tipId;
			if (!tipId) throw new Error("Agent produced no assistant result");
			const entries = await lane.findEntries({ start: tipId, order: "newestFirst", limit: 1 }, taskContext);
			signal.throwIfAborted();
			const entry = entries.find((candidate) => candidate.id === tipId);
			const text = entry ? textOf(entry) : "";
			if (!text.trim()) throw new Error("Agent produced no assistant result at the completed tip");
			const value = definition.id === "rlm-child" || definition.id === "background-job" ? text : jsonFrom(text);
			if (!isJsonValue(value)) throw new Error("Agent produced a non-JSON result");
			if (!this.registry.isValidOutput(definition, value))
				throw this.registry.validationError(definition, value, "output");
			return { status: "succeeded", value, verification: "unverified" };
		} catch (error) {
			modelStatus = signal.aborted ? "cancelled" : modelStatus === "unknown" ? "failed" : modelStatus;
			return {
				status: signal.aborted ? "cancelled" : "failed",
				error: String(error instanceof Error ? error.message : error),
				verification: "unverified",
			};
		} finally {
			if (modelReservation) await this.usage?.settle(modelReservation, { status: modelStatus });
		}
	}

	/** The first terminal request wins, but nothing is published until its write commits. */
	private finish(task: TaskRecord, result: NativeResult): Promise<NativeResult> {
		if (task.finishing) return task.finishing;
		task.finishing = (async () => {
			try {
				const state = result.status === "succeeded" ? "completed" : result.status;
				const committed = await this.journal.transition(task.id, state, result);
				Object.assign(task, committed);
				if (!committed.result) throw new Error("Terminal task has no durable result");
				if (task.usageReservation && !task.usageSettled) {
					await this.usage?.settle(task.usageReservation, {
						status:
							result.status === "succeeded"
								? "succeeded"
								: result.status === "cancelled"
									? "cancelled"
									: result.status === "interrupted"
										? "unknown"
										: "failed",
					});
					task.usageSettled = true;
				}
				task.resolve?.(committed.result);
				return committed.result;
			} catch (error) {
				task.reject?.(error);
				task.controller?.abort(error);
				this.abortLane(task);
				throw error;
			} finally {
				task.cleanup?.();
			}
		})();
		return task.finishing;
	}

	private abortLane(task: TaskRecord): void {
		// Lane cleanup is best effort. An uncooperative lane must not hold a durable result hostage.
		void Promise.resolve()
			.then(() => task.lane?.abort(BACKGROUND_CONTEXT))
			.catch(() => {});
	}

	private cancel(task: TaskRecord, reason: string): Promise<NativeResult> {
		if (task.result) return Promise.resolve(task.result);
		const pending = this.finish(task, { status: "cancelled", error: reason, verification: "unverified" });
		task.controller?.abort(new Error(reason));
		this.abortLane(task);
		return pending;
	}

	private async run(task: TaskRecord, request: TaskRequest, context: Context): Promise<void> {
		try {
			const committed = await this.journal.transition(task.id, "running");
			Object.assign(task, committed);
			if (task.finishing) return;
			const result = await this.execute(task, request, context);
			await this.finish(task, result);
		} catch (error) {
			task.reject?.(error);
			task.controller?.abort(error);
			if (task.usageReservation && !task.usageSettled) {
				await this.usage?.settle(task.usageReservation, { status: "unknown" }).catch(() => {});
				task.usageSettled = true;
			}
			task.cleanup?.();
			this.abortLane(task);
		}
	}

	private spawnTask(request: TaskRequest, context: Context): Promise<TaskRecord> {
		// Serialize through installation of the live promise, not through execution.
		const pending = this.admissions.then(async () => {
			if (this.closed) throw new Error("Ultron task host is closed");
			context.abortSignal?.throwIfAborted();
			const fingerprint = taskFingerprint({
				definition: request.definition,
				input: request.input,
				model: request.model ?? null,
				timeout_ms: request.timeoutMs,
			});
			const key = request.key ?? randomUUID();
			const usageReservation = await this.usage?.reserve({
				kind: "task",
				requestKey: key,
				timeoutMs: request.timeoutMs,
				signal: context.abortSignal,
			});
			let admitted: Awaited<ReturnType<NativeTaskJournal["admit"]>>;
			try {
				admitted = await this.journal.admit(request.definition, fingerprint, key, context.abortSignal);
			} catch (error) {
				if (usageReservation) await this.usage?.settle(usageReservation, { status: "unknown" }).catch(() => {});
				throw error;
			}
			if (!admitted.created) {
				if (usageReservation) await this.usage?.settle(usageReservation, { status: "succeeded" });
				const existing = this.tasks.get(admitted.task.id);
				if (existing) return existing;
				this.tasks.set(admitted.task.id, admitted.task);
				return admitted.task;
			}
			const task: TaskRecord = { ...admitted.task, controller: new AbortController(), usageReservation };
			task.promise = new Promise<NativeResult>((resolve, reject) => {
				task.resolve = resolve;
				task.reject = reject;
			});
			// Spawn callers need not observe the result immediately, including store errors.
			void task.promise.catch(() => {});
			this.tasks.set(task.id, task);
			const onAbort = () => {
				void this.cancel(task, "Parent task aborted").catch(() => {});
			};
			const timeoutDelay =
				usageReservation?.deadlineAt === null || usageReservation?.deadlineAt === undefined
					? request.timeoutMs
					: Math.max(1, Math.min(request.timeoutMs, usageReservation.deadlineAt - Date.now()));
			const deadlineTimeout = timeoutDelay < request.timeoutMs;
			const timer = setTimeout(() => {
				void this.cancel(
					task,
					deadlineTimeout
						? "Ultron root wall deadline exceeded"
						: `Ultron task exceeded ${request.timeoutMs}ms timeout`,
				).catch(() => {});
			}, timeoutDelay);
			timer.unref();
			context.abortSignal?.addEventListener("abort", onAbort, { once: true });
			task.cleanup = () => {
				clearTimeout(timer);
				context.abortSignal?.removeEventListener("abort", onAbort);
			};
			if (this.closed) void this.cancel(task, "Ultron task host closed").catch(() => {});
			else if (context.abortSignal?.aborted) onAbort();
			else void this.run(task, request, context);
			return task;
		});
		this.admissions = pending.then(
			() => {},
			() => {},
		);
		return pending;
	}

	close(): Promise<void> {
		this.closed = true;
		this.closing ??= (async () => {
			await this.admissions;
			const results = await Promise.allSettled(
				[...this.tasks.values()]
					.filter((task) => !task.result)
					.map((task) => this.cancel(task, "Ultron task host closed")),
			);
			const failed = results.find((result) => result.status === "rejected");
			if (failed?.status === "rejected") throw failed.reason;
		})();
		return this.closing;
	}

	private workflow(payload: Payload): WorkflowNode[] {
		fields(payload, ["nodes"]);
		if (!Array.isArray(payload.nodes)) throw new Error("nodes must be an array");
		const nodes: WorkflowNode[] = payload.nodes.map((value) => {
			const node = objectInput(value);
			fields(node, ["id", "definition", "input", "model", "timeout_ms", "dependsOn", "inputFrom"]);
			const id = nonemptyString(node.id, "Workflow node ID");
			const definition = definitionKey(node.definition);
			const item = this.definition(definition);
			const dependsOn = node.dependsOn === undefined ? [] : node.dependsOn;
			if (
				!Array.isArray(dependsOn) ||
				dependsOn.some((dependency) => typeof dependency !== "string" || !dependency.trim()) ||
				new Set(dependsOn).size !== dependsOn.length
			)
				throw new Error("dependsOn must be an array of unique nonempty strings");
			const inputFrom = node.inputFrom === undefined ? undefined : nonemptyString(node.inputFrom, "inputFrom");
			if (inputFrom !== undefined) {
				if (!dependsOn.includes(inputFrom)) throw new Error("inputFrom must name a dependency");
				if (Object.hasOwn(node, "input")) throw new Error("Specify input or inputFrom, not both");
			} else {
				if (!isJsonValue(node.input)) throw new Error("Agent input is not JSON");
				if (!this.registry.isValidInput(item, node.input))
					throw this.registry.validationError(item, node.input, "input");
			}
			return {
				id,
				definition,
				input: node.input as JsonValue | undefined,
				dependsOn,
				inputFrom,
				...taskOptions(node),
			};
		});
		const ids = new Set(nodes.map((node) => node.id));
		if (ids.size !== nodes.length) throw new Error("Duplicate workflow node ID");
		for (const node of nodes) {
			if (node.dependsOn.some((dependency) => !ids.has(dependency))) throw new Error("Unknown workflow dependency");
		}
		const pending = new Set(ids);
		while (pending.size) {
			const ready = nodes.filter(
				(node) => pending.has(node.id) && node.dependsOn.every((dependency) => !pending.has(dependency)),
			);
			if (!ready.length) throw new Error("Workflow contains a cycle");
			for (const node of ready) pending.delete(node.id);
		}
		return nodes;
	}

	async handle(type: string, payload: Payload, context: Context): Promise<unknown> {
		if (this.closed) throw new Error("Ultron task host is closed");
		if (!isJsonValue(payload) || payload === null || Array.isArray(payload) || typeof payload !== "object")
			throw new Error("Host payload must be a JSON object");
		payload = structuredClone(payload);
		await this.loadTasks();
		if (this.closed) throw new Error("Ultron task host is closed");
		if (["ping", "agents.list", "agents.status", "agents.tasks"].includes(type)) fields(payload, []);
		if (type === "ping") return { ok: true };
		if (type === "agents.list") return this.list();
		if (type === "agents.status" || type === "agents.tasks") {
			const usage = this.usage ? await this.usage.status() : null;
			return {
				definitions: this.list(),
				tasks: (await this.journal.list()).map(publicTask),
				usage,
				limits: usage?.limits ?? null,
				controls: Object.fromEntries(
					[
						"permissionPrompts",
						"riskBlocking",
						"capabilityEnforcement",
						"budgetEnforcement",
						"completionGates",
						"refinementApproval",
						"sandboxRequired",
					].map((key) => [key, false]),
				),
			};
		}
		if (type === "agents.register") {
			fields(payload, ["definition"]);
			return this.registry.register(payload.definition);
		}
		if (type === "background.start") {
			fields(payload, ["prompt", "model", "key", "timeout_ms"]);
			const prompt = nonemptyString(payload.prompt, "prompt");
			if (prompt.length > 65_536) throw new Error("Background prompt exceeds 65536 characters");
			const request: TaskRequest = {
				definition: "background-job@1",
				input: { prompt },
				model: typeof payload.model === "string" ? payload.model : undefined,
				key: typeof payload.key === "string" ? `background:${payload.key}` : undefined,
				timeoutMs: typeof payload.timeout_ms === "number" ? payload.timeout_ms : 30 * 60 * 1000,
			};
			if (request.key !== undefined && request.key.length > 264) throw new Error("Background key is too long");
			const task = await this.spawnTask(request, context);
			return publicTask(task);
		}
		if (type === "background.list") {
			fields(payload, []);
			return (await this.journal.list()).filter((task) => task.definition === "background-job@1").map(publicTask);
		}
		if (type === "background.inspect" || type === "background.result" || type === "background.stop") {
			fields(payload, ["id"]);
			const id = nonemptyString(payload.id, "id");
			const stored = (await this.journal.list()).find(
				(task) => task.id === id && task.definition === "background-job@1",
			);
			if (!stored) throw new Error("Unknown background job");
			const task = this.tasks.get(id) ?? stored;
			if (type === "background.inspect") return publicTask(task);
			if (type === "background.stop")
				return { cancelled: (await this.cancel(task, "Background job stopped")).status === "cancelled" };
			const liveTask = this.tasks.get(id);
			return structuredClone(await (liveTask?.promise ?? task.result));
		}
		if (type === "rlm.spawn") {
			fields(payload, ["prompt", "kwargs"]);
			const prompt = nonemptyString(payload.prompt, "prompt");
			const kwargs = payload.kwargs === undefined ? {} : objectInput(payload.kwargs);
			fields(kwargs, ["name", "model", "timeout_ms"]);
			const name = nonemptyString(kwargs.name, "name");
			const request: TaskRequest = {
				definition: "rlm-child@1",
				input: { prompt },
				model: typeof kwargs.model === "string" ? kwargs.model : undefined,
				timeoutMs: typeof kwargs.timeout_ms === "number" ? kwargs.timeout_ms : 30 * 60 * 1000,
			};
			if (!Number.isSafeInteger(request.timeoutMs) || request.timeoutMs < 1 || request.timeoutMs > 60 * 60 * 1000)
				throw new Error("timeout_ms must be an integer between 1 and 3600000");
			const task = await this.spawnTask(request, context);
			return {
				rlm_child_id: task.id,
				name,
				session_dir: "",
				model: request.model ?? "",
				timeout_ms: request.timeoutMs,
				parent_branch_anchor: "",
			} satisfies RlmChildHandle;
		}
		if (type === "rlm.list_subagents") {
			fields(payload, []);
			return {
				subagents: [...this.tasks.values()].filter((task) => task.definition === "rlm-child@1").map(publicTask),
			};
		}
		if (type === "rlm.collect") {
			fields(payload, ["selectors", "timeout_ms"]);
			const selectors = Array.isArray(payload.selectors)
				? payload.selectors.map((selector) => nonemptyString(selector, "selector"))
				: [];
			const tasks = [...this.tasks.values()].filter(
				(task) => task.definition === "rlm-child@1" && (selectors.length === 0 || selectors.includes(task.id)),
			);
			const results = await Promise.all(
				tasks.map(async (task) => ({ id: task.id, result: await (task.promise ?? task.result) })),
			);
			return { results };
		}
		if (type === "rlm.delete_subagent") {
			fields(payload, ["selector"]);
			const selector = nonemptyString(payload.selector, "selector");
			const task = this.tasks.get(selector);
			if (!task || task.definition !== "rlm-child@1") throw new Error("Unknown RLM child");
			return { deleted: (await this.cancel(task, "RLM child deleted")).status === "cancelled" };
		}
		if (type === "agents.spawn" || type === "agents.invoke") {
			const task = await this.spawnTask(this.request(payload), context);
			if (type === "agents.spawn") return { id: task.id, state: task.state };
			return structuredClone(await (task.promise ?? task.result));
		}
		if (["agents.inspect", "agents.result", "agents.cancel"].includes(type)) {
			fields(payload, ["id"]);
			const id = nonemptyString(payload.id, "id");
			const stored = (await this.journal.list()).find((task) => task.id === id);
			if (!stored) throw new Error("Unknown Ultron task");
			if (type === "agents.inspect") return publicTask(stored);
			const task = this.tasks.get(id)!;
			if (type === "agents.cancel") {
				const result = stored.result ?? (await this.cancel(task, "Ultron task cancelled"));
				return { cancelled: result.status === "cancelled" };
			}
			return structuredClone(await (task.promise ?? stored.result));
		}
		if (
			type.startsWith("memory.") ||
			type.startsWith("refinements.") ||
			type.startsWith("artifacts.") ||
			type.startsWith("experiments.") ||
			type.startsWith("jev.")
		) {
			if (!this.services) throw new Error("Ultron local services are not connected to this session worker");
			const reservation = type.startsWith("jev.")
				? await this.usage?.reserve({ kind: "jev", requestKey: `jev:${randomUUID()}`, signal: context.abortSignal })
				: undefined;
			try {
				const result = await this.services.handle(type, payload, context);
				if (reservation) await this.usage?.settle(reservation, { status: "succeeded" });
				return result;
			} catch (error) {
				if (reservation)
					await this.usage?.settle(reservation, {
						status: context.abortSignal?.aborted ? "cancelled" : "failed",
					});
				throw error;
			}
		}
		if (type === "workflows.run") {
			const nodes = this.workflow(payload);
			const output = new Map<string, NativeResult | { status: "skipped"; reason: string }>();
			while (output.size < nodes.length) {
				const ready = nodes.filter(
					(node) => !output.has(node.id) && node.dependsOn.every((dependency) => output.has(dependency)),
				);
				const results = await Promise.all(
					ready.map(async (node) => {
						if (node.dependsOn.some((dependency) => output.get(dependency)!.status !== "succeeded"))
							return [node.id, { status: "skipped", reason: "Dependency did not succeed" }] as const;
						const input =
							node.inputFrom === undefined ? node.input : (output.get(node.inputFrom) as NativeResult).value;
						if (!isJsonValue(input)) throw new Error("Agent input is not JSON");
						const definition = this.definition(node.definition);
						if (!this.registry.isValidInput(definition, input))
							throw this.registry.validationError(definition, input, "input");
						const task = await this.spawnTask({ ...node, input }, context);
						const result = await (task.promise ?? task.result!);
						return [node.id, result] as const;
					}),
				);
				for (const [id, result] of results) output.set(id, result);
			}
			return structuredClone(Object.fromEntries(output));
		}
		throw new Error(`Ultron RLM host request is not wired: ${type}`);
	}
}
