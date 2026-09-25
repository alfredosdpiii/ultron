import type { Context, JsonValue } from "@earendil-works/chord";
import { BACKGROUND_CONTEXT } from "@earendil-works/chord/context";
import { AgentHarness, MemorySessionRepo } from "@earendil-works/pi-agent-core";
import { createModels, fauxAssistantMessage, fauxProvider } from "@earendil-works/pi-ai";
import { describe, expect, test } from "vitest";
import {
	type DurableDocumentStorage,
	NativeLocalServices,
	type RefinementBranch,
} from "../src/ultron/local-services.ts";

function documents(): DurableDocumentStorage {
	const values = new Map<string, JsonValue>();
	return {
		get: async (key) => structuredClone(values.get(key)),
		set: async (key, value) => {
			values.set(key, structuredClone(value));
		},
		list: async (prefix) =>
			[...values].filter(([key]) => key.startsWith(prefix)).map(([key, value]) => ({ key, value })),
	};
}

describe("A07 lessons follow the conversation branch", () => {
	test("a lesson from an abandoned branch never applies after navigating away, and each branch keeps its own", async () => {
		const repo = new MemorySessionRepo();
		const session = await repo.create({ id: "a07" }, BACKGROUND_CONTEXT);
		const faux = fauxProvider();
		faux.setResponses(Array.from({ length: 10 }, () => fauxAssistantMessage("ok")));
		const models = createModels();
		models.setProvider(faux.provider);
		const { harness } = await AgentHarness.create(
			{ session, models, model: faux.getModel(), activeToolNames: [] },
			BACKGROUND_CONTEXT,
		);
		const lane = await harness.lane("main", BACKGROUND_CONTEXT);
		// Same branch computation as the session worker.
		const branch = async (): Promise<RefinementBranch> => {
			const [tip, entries] = await Promise.all([
				lane.getTipId(BACKGROUND_CONTEXT),
				lane.findEntries(undefined, BACKGROUND_CONTEXT),
			]);
			const onPath = new Set(entries.map((entry) => entry.id));
			return { anchor: tip, onBranch: (anchor) => onPath.has(anchor) };
		};
		const store = documents();
		const services = new NativeLocalServices(store);
		const call = async (type: string, payload: object, on = services) =>
			on.handle(type, payload, BACKGROUND_CONTEXT as Context, await branch());
		const current = async (on = services) =>
			(
				(await call(
					"refinements.current",
					{ kind: "instruction", target: "instruction:correctness-reviewer" },
					on,
				)) as {
					content: string;
				} | null
			)?.content ?? null;
		const lesson = async (content: string, baseVersion: number) => {
			const proposed = (await call("refinements.propose", {
				kind: "instruction",
				target: "instruction:correctness-reviewer",
				baseVersion,
				content,
				evidence: [{ run: content }],
			})) as { id: string };
			await call("refinements.activate", { id: proposed.id });
			return proposed.id;
		};

		await lane.prompt("first question", undefined, BACKGROUND_CONTEXT);
		const forkPoint = await lane.getTipId(BACKGROUND_CONTEXT);
		await lane.prompt("second question", undefined, BACKGROUND_CONTEXT);
		await lesson("Lesson learned on branch A.", 0);
		expect(await current()).toBe("Lesson learned on branch A.");
		const branchATip = await lane.getTipId(BACKGROUND_CONTEXT);

		// Navigate back to before the lesson: branch B must not see it.
		await lane.navigateTree(forkPoint, undefined, BACKGROUND_CONTEXT);
		expect(await current()).toBeNull();
		await lane.prompt("a different second question", undefined, BACKGROUND_CONTEXT);
		expect(await current()).toBeNull();
		await lesson("Lesson learned on branch B.", 0);
		expect(await current()).toBe("Lesson learned on branch B.");

		// Returning to branch A restores its own lesson, untouched by B, including after a restart.
		await lane.navigateTree(branchATip, undefined, BACKGROUND_CONTEXT);
		expect(await current()).toBe("Lesson learned on branch A.");
		const restarted = new NativeLocalServices(store);
		expect(await current(restarted)).toBe("Lesson learned on branch A.");

		// A rollback on branch A holds on A and does not touch B.
		const listed = (await call("refinements.list", {})) as Array<{ id: string; content: string; state: string }>;
		const lessonA = listed.find((record) => record.content === "Lesson learned on branch A.")!;
		await call("refinements.rollback", { id: lessonA.id });
		expect(await current()).toBeNull();
		await lane.navigateTree(forkPoint, undefined, BACKGROUND_CONTEXT);
		// forkPoint is before both lessons, so nothing applies there either.
		expect(await current()).toBeNull();
		await harness.close(BACKGROUND_CONTEXT);
		await session.close(BACKGROUND_CONTEXT);
		await repo.close(BACKGROUND_CONTEXT);
	});
});
