import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { JsonValue } from "@earendil-works/chord";
import { BACKGROUND_CONTEXT, withAbortSignal } from "@earendil-works/chord/context";
import { NodeExecutionEnv } from "@earendil-works/pi-agent-core/node";
import { describe, expect, test } from "vitest";
import { createUltronRlmTool } from "../src/experimental/session-worker.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import type { NativeHostStore } from "../src/ultron/rlm/task-store.ts";
import { createWorkerServices } from "../src/ultron/worker-services.ts";

describe("Ultron native RLM tool", () => {
	test("routes refinements, artifacts, and experiments through the native worker services", async () => {
		const cwd = mkdtempSync(join(tmpdir(), "ultron-native-rlm-services-"));
		const env = new NodeExecutionEnv({ cwd });
		let taskDocument: JsonValue | undefined;
		const taskStore: NativeHostStore = {
			read: async () => taskDocument,
			write: async (document) => {
				taskDocument = structuredClone(document);
			},
		};
		const values = new Map<
			string,
			{ address: { namespace: string; key: string; kind: "value" }; value: JsonValue }
		>();
		const session = {
			getValue: async (address: { namespace: string; key: string }) =>
				values.get(`${address.namespace}\0${address.key}`),
			setValue: async (address: { namespace: string; key: string }, value: JsonValue) => {
				values.set(`${address.namespace}\0${address.key}`, { address: { ...address, kind: "value" }, value });
			},
			scanValues: async (prefix: { namespace: string; key: string }) =>
				[...values.entries()]
					.filter(([key]) => key.startsWith(`${prefix.namespace}\0${prefix.key}`))
					.map(([, entry]) => entry),
		};
		const services = createWorkerServices({ session: session as never, sessionId: "integration", cwd });
		const host = new NativeRlmHost({ lane: async () => ({}) } as never, {} as never, {
			store: taskStore,
			services,
		});
		const tool = createUltronRlmTool(cwd, (type, payload, signal) =>
			host.handle(type, payload, signal ? withAbortSignal(signal, BACKGROUND_CONTEXT) : BACKGROUND_CONTEXT),
		);
		const invocation = {
			invocationId: "rlm-services-test",
			operationId: "operation-services-test",
			turnId: "turn-services-test",
			getMemo: async () => undefined,
			setMemo: async () => undefined,
		};
		try {
			const result = await tool.execute(
				"call-services",
				{
					code: [
						'proposal = await refinements.propose("skill", "skill:review", 0, "---\\nname: review\\ndescription: Review boundaries\\n---\\nCheck boundaries.", [{"ref": "test"}])',
						'active = await refinements.activate(proposal["id"])',
						"refinement_records = await refinements.list()",
						'put = await rlm.host_request("artifacts.put", {"text": "native artifact", "options": {"label": "integration"}})',
						'read = await rlm.host_request("artifacts.read", {"id": put["id"]})',
						'artifact_records = await rlm.host_request("artifacts.list")',
						'baseline = await rlm.host_request("experiments.record", {"run": {"variant": "baseline", "fixtureHash": "fixture", "outcome": "failed"}})',
						'candidate = await rlm.host_request("experiments.record", {"run": {"variant": "candidate", "fixtureHash": "fixture", "outcome": "passed"}})',
						'comparison = await rlm.host_request("experiments.compare", {"baseline": baseline["id"], "candidate": candidate["id"]})',
						'experiment_records = await rlm.host_request("experiments.list")',
						'{"refinement": active["state"], "refinements": len(refinement_records), "artifact": read["text"], "artifacts": len(artifact_records), "experiments": len(experiment_records), "claim": comparison["claim"]}',
					].join("\n"),
				},
				() => {},
				{ env },
				invocation,
				BACKGROUND_CONTEXT,
			);
			const text = result.content[0];
			expect(text).toMatchObject({
				type: "text",
				text: expect.stringContaining("'refinement': 'active'"),
			});
			expect(text).toMatchObject({ text: expect.stringContaining("'artifact': 'native artifact'") });
			expect(text).toMatchObject({ text: expect.stringContaining("'experiments': 2") });
			expect(text).toMatchObject({
				text: expect.stringContaining("Observed runs only; no statistical superiority established"),
			});
		} finally {
			await tool.close();
			await host.close();
			await env.cleanup(BACKGROUND_CONTEXT);
			rmSync(cwd, { recursive: true, force: true });
		}
	});

	test("keeps Python state across tool invocations and exposes protocol errors", async () => {
		const cwd = mkdtempSync(join(tmpdir(), "ultron-native-rlm-"));
		const env = new NodeExecutionEnv({ cwd });
		const tool = createUltronRlmTool(cwd, async (type) => {
			if (type === "ping") return { ok: true };
			throw new Error(`Unknown test host request: ${type}`);
		});
		const invocation = {
			invocationId: "rlm-test",
			operationId: "operation-test",
			turnId: "turn-test",
			getMemo: async () => undefined,
			setMemo: async () => undefined,
		};
		try {
			const first = await tool.execute(
				"call-1",
				{ code: "answer = 40 + 2\nanswer" },
				() => {},
				{ env },
				invocation,
				BACKGROUND_CONTEXT,
			);
			expect(first.content[0]).toMatchObject({ type: "text", text: "42" });
			const second = await tool.execute(
				"call-2",
				{ code: "answer + 1" },
				() => {},
				{ env },
				invocation,
				BACKGROUND_CONTEXT,
			);
			expect(second.content[0]).toMatchObject({ type: "text", text: "43" });
			await expect(
				tool.execute(
					"call-3",
					{ code: "raise RuntimeError('bad')" },
					() => {},
					{ env },
					invocation,
					BACKGROUND_CONTEXT,
				),
			).rejects.toThrow("RuntimeError");
		} finally {
			await env.cleanup(BACKGROUND_CONTEXT);
			rmSync(cwd, { recursive: true, force: true });
		}
	});
});
