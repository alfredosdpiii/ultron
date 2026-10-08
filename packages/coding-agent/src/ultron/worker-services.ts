import { createHash } from "node:crypto";
import { homedir } from "node:os";
import { type Session, value } from "@ultron/agent-core";
import type { Context, JsonValue } from "@ultron/chord";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import Type from "typebox";
import { Check } from "typebox/value";
import { NativeLocalServices, type RefinementBranch } from "./local-services.ts";
import { createHindsightBackend, type MemoryBackend, NativeMemoryService } from "./memory.ts";
import { validateRefinementContent } from "./refinement-validation.ts";
import { containsSensitiveMemory } from "./sensitive.ts";

/** A stable, non-reversible id for a directory, used in Hindsight scope tags. */
function projectIdentity(path: string): string {
	return createHash("sha256").update(path).digest("hex").slice(0, 16);
}

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
	},
	{ additionalProperties: false },
);
const propose = Type.Object(
	{ text: Type.String({ minLength: 1 }), evidence, scope, evidenceClass },
	{ additionalProperties: false },
);
const correct = Type.Object(
	{ id: Type.String({ minLength: 1 }), text: Type.String({ minLength: 1 }), evidence, evidenceClass },
	{ additionalProperties: false },
);
const id = Type.Object({ id: Type.String({ minLength: 1 }) }, { additionalProperties: false });
const why = Type.Object({ taskId: Type.String({ minLength: 1 }) }, { additionalProperties: false });

/** Shared dispatch for Python and worker consumers. */
export function createWorkerServices(options: {
	session: SessionValues;
	sessionId: string;
	cwd: string;
	hindsightUrl?: string;
	bankId?: string;
	backend?: MemoryBackend;
	extensionCommands?: {
		list(): Promise<readonly { readonly name: string; readonly description?: string }[]>;
		run(name: string, args: string): Promise<unknown>;
	};
}) {
	const { session } = options;
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
				// Memory is used on purpose (nothing recalls or retains on its own), so every recall runs and every write
				// (a proposal or a correction) is kept unless it holds a secret.
				gate: async (request) => {
					if (request.action === "recall") return { retrieve: true, probability: 1 };
					return { action: containsSensitiveMemory(request.text) ? "sensitive" : "keep", confidence: 1 };
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
		/** The memory service, when Hindsight is configured. */
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
