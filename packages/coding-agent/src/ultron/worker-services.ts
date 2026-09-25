import { homedir } from "node:os";
import { type Session, value } from "@ultron/agent-core";
import type { Context, JsonValue } from "@ultron/chord";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import Type from "typebox";
import { Check } from "typebox/value";
import { type NativeJevClient, projectIdentity } from "./jev.ts";
import { NativeLocalServices, type RefinementBranch } from "./local-services.ts";
import { createHindsightBackend, type MemoryBackend, NativeMemoryService } from "./memory.ts";
import { validateRefinementContent } from "./refinement-validation.ts";

/** Minimum Jev confidence for an automatic (non-explicit) retention to be kept. */
export const AUTOMATIC_KEEP_THRESHOLD = 0.65;

type SessionValues = Pick<Session, "getValue" | "setValue" | "scanValues">;
const evidence = Type.Array(
	Type.Object(
		{ ref: Type.String({ minLength: 1 }), sha256: Type.Optional(Type.String({ pattern: "^[a-fA-F0-9]{64}$" })) },
		{ additionalProperties: false },
	),
	{ minItems: 1 },
);
const scope = Type.Optional(Type.Union([Type.Literal("session"), Type.Literal("project"), Type.Literal("global")]));
const evidenceClass = Type.Optional(
	Type.Union([
		Type.Literal("hypothesis"),
		Type.Literal("user_statement"),
		Type.Literal("tool_evidence"),
		Type.Literal("verified"),
	]),
);
const prepare = Type.Object(
	{
		query: Type.String({ minLength: 1 }),
		taskId: Type.String({ minLength: 1 }),
		scope,
		refresh: Type.Optional(Type.Boolean()),
		explicit: Type.Optional(Type.Boolean()),
	},
	{ additionalProperties: false },
);
const propose = Type.Object(
	{ text: Type.String({ minLength: 1 }), evidence, scope, evidenceClass, explicit: Type.Optional(Type.Boolean()) },
	{ additionalProperties: false },
);
const correct = Type.Object(
	{ id: Type.String({ minLength: 1 }), text: Type.String({ minLength: 1 }), evidence, evidenceClass },
	{ additionalProperties: false },
);
const id = Type.Object({ id: Type.String({ minLength: 1 }) }, { additionalProperties: false });
const why = Type.Object({ taskId: Type.String({ minLength: 1 }) }, { additionalProperties: false });

/** Shared dispatch for Python and worker consumers. A missing judge never permits retention. */
export function createWorkerServices(options: {
	session: SessionValues;
	sessionId: string;
	cwd: string;
	jev?: Pick<NativeJevClient, "triage" | "memoryGate" | "memoryPolicy">;
	hindsightUrl?: string;
	bankId?: string;
	backend?: MemoryBackend;
	extensionCommands?: {
		list(): Promise<readonly { readonly name: string; readonly description?: string }[]>;
		run(name: string, args: string): Promise<unknown>;
	};
}) {
	const { session, jev } = options;
	const backend =
		options.backend ??
		(options.hindsightUrl
			? createHindsightBackend({
					baseUrl: options.hindsightUrl,
					bankId: options.bankId ?? "ultron",
					ensureBank: true,
					scopeTags: {
						session: [`ultron:session:${options.sessionId}`],
						project: [`ultron:project:${projectIdentity(options.cwd)}`],
						global: [`ultron:global:${projectIdentity(homedir())}`],
					},
				})
			: undefined);
	const memoryAddress = value<JsonValue>("ultron.memory.state", "root");
	const memory = backend
		? new NativeMemoryService({
				backend,
				store: {
					read: async () => (await session.getValue(memoryAddress, BACKGROUND_CONTEXT))?.value,
					write: (next) => session.setValue(memoryAddress, next, BACKGROUND_CONTEXT),
				},
				gate: async (request, signal) => {
					// Deliberate recall by agent code needs no relevance judgement, even without Jev.
					if (request.action === "recall" && request.explicit) return { retrieve: true, probability: 1 };
					if (!jev) return request.action === "recall" ? { retrieve: false } : { action: "skip" };
					if (request.action === "recall") return jev.memoryGate(request.query, signal);
					const policy = await jev.memoryPolicy(
						request.source?.prompt ?? request.text,
						request.source?.response ?? "",
						signal,
					);
					// A deliberate write is kept unless Jev judges it sensitive.
					if (request.explicit) return policy.action === "skip" ? { ...policy, action: "keep" } : policy;
					// Automatic writes follow Jev fully, and a keep needs the same confidence the Pi Jev extension used.
					return policy.action === "keep" && policy.confidence < AUTOMATIC_KEEP_THRESHOLD
						? { ...policy, action: "skip" }
						: policy;
				},
			})
		: undefined;
	const local = new NativeLocalServices(
		{
			get: async (key, context) => (await session.getValue(value<JsonValue>("ultron.local", key), context))?.value,
			set: (key, next, context) => session.setValue(value<JsonValue>("ultron.local", key), next, context),
			list: async (prefix, context) =>
				(await session.scanValues(value<JsonValue>("ultron.local", prefix), context)).map((entry) => ({
					key: entry.address.key,
					value: entry.value,
				})),
		},
		{ validate: validateRefinementContent },
	);
	return {
		/** The memory service, when Hindsight is configured; automatic per-turn memory drives it directly. */
		memory,
		async handle(
			type: string,
			payload: Record<string, unknown>,
			context: Context,
			branch?: RefinementBranch,
		): Promise<unknown> {
			context.abortSignal?.throwIfAborted();
			if (type === "extensions.list") return options.extensionCommands?.list() ?? [];
			if (type === "extensions.run") {
				if (!options.extensionCommands || typeof payload.name !== "string" || typeof payload.args !== "string")
					throw new Error("Extension command service is unavailable");
				return options.extensionCommands.run(payload.name, payload.args);
			}
			if (type === "jev.triage" || type === "jev.recall") {
				if (typeof payload.prompt !== "string" || !payload.prompt.trim())
					throw new Error("Jev prompt must be nonempty");
				if (!jev) return { available: false, reason: "Jev is not configured" };
				if (type === "jev.triage")
					return { available: true, ...(await jev.triage(payload.prompt, context.abortSignal)) };
				if (!memory) return { available: false, reason: "Hindsight is not configured" };
				const recalled = await memory.prepare({ query: payload.prompt, taskId: "jev.recall" }, context.abortSignal);
				return { available: true, gate: recalled.operation.gate, ...recalled };
			}
			if (!type.startsWith("memory.")) return local.handle(type, payload, context, branch);
			if (!memory) throw new Error("Hindsight is not configured. Set ULTRON_HINDSIGHT_URL.");
			if (type === "memory.prepare" && Check(prepare, payload)) return memory.prepare(payload, context.abortSignal);
			if (type === "memory.propose" && Check(propose, payload)) return memory.propose(payload, context.abortSignal);
			if (type === "memory.correct" && Check(correct, payload))
				return memory.correct(payload.id, payload, context.abortSignal);
			if (type === "memory.get" && Check(id, payload)) return memory.get(payload.id, context.abortSignal);
			if (type === "memory.forget" && Check(id, payload)) return memory.forget(payload.id, context.abortSignal);
			if (type === "memory.why" && Check(why, payload)) return memory.why(payload.taskId);
			if (type === "memory.list" && Object.keys(payload).length === 0) return memory.list();
			throw new Error(`Invalid memory request: ${type}`);
		},
	};
}
