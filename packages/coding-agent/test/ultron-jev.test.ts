import { describe, expect, test, vi } from "vitest";
import { createNativeJevClient, NativeJevClient } from "../src/ultron/jev.ts";

const maxResponseBytes = 1_048_576;

function client(fetch: typeof globalThis.fetch, timeoutMs = 100): NativeJevClient {
	return new NativeJevClient({
		apiKey: "test-key",
		baseUrl: "https://jev.example.test",
		fetch,
		timeoutMs,
	});
}

function jsonResponse(value: unknown): Response {
	return new Response(JSON.stringify(value), {
		status: 200,
		headers: { "content-type": "application/json" },
	});
}

function streamedResponse(chunks: Uint8Array[], onCancel: () => void): Response {
	let index = 0;
	return new Response(
		new ReadableStream<Uint8Array>({
			pull(controller) {
				if (index === chunks.length) controller.close();
				else controller.enqueue(chunks[index++]);
			},
			cancel() {
				onCancel();
			},
		}),
		{ status: 200, headers: { "content-type": "application/json" } },
	);
}

describe("NativeJevClient", () => {
	test("uses numeric noul probabilities and the .65 recall threshold", async () => {
		const values = [0.64, 0.65, 0.9];
		const fetcher: typeof fetch = async () => jsonResponse({ answers: { retrieve: { noul: values.shift() } } });
		const value = client(fetcher);

		await expect(value.memoryGate("first")).resolves.toEqual({ retrieve: false, probability: 0.64 });
		await expect(value.memoryGate("second")).resolves.toEqual({ retrieve: true, probability: 0.65 });
		await expect(value.memoryRecall("third")).resolves.toEqual({ retrieve: true, probability: 0.9 });
	});

	test("rejects invalid probabilities instead of clamping or defaulting", async () => {
		const responses = [-0.01, 1.01, "0.9", null, undefined];
		const fetcher: typeof fetch = async () => jsonResponse({ answers: { retrieve: { noul: responses.shift() } } });
		const value = client(fetcher);

		for (let index = 0; index < 5; index += 1) {
			await expect(value.memoryGate(`invalid-${index}`)).rejects.toThrow("Invalid Jev probability");
		}
	});

	test("bounds a streamed response before retaining more than the protocol limit", async () => {
		let cancelled = false;
		const fetcher: typeof fetch = async () =>
			streamedResponse([new Uint8Array(maxResponseBytes), new Uint8Array([0])], () => {
				cancelled = true;
			});

		await expect(client(fetcher).memoryGate("large")).rejects.toMatchObject({
			name: "UNAVAILABLE",
			message: "Jev UNAVAILABLE",
		});
		expect(cancelled).toBe(false);
	});

	test("combines caller cancellation and timeout without leaking abort reasons", async () => {
		let calls = 0;
		let requestSignal: AbortSignal | undefined;
		const fetcher: typeof fetch = async (_input, init) => {
			calls += 1;
			requestSignal = init?.signal ?? undefined;
			return await new Promise<Response>(() => {});
		};
		const controller = new AbortController();
		const pending = client(fetcher, 1_000).memoryGate("cancel", controller.signal);
		await new Promise<void>((resolve) => setImmediate(resolve));
		controller.abort(new Error("private cancellation reason"));
		await expect(pending).rejects.toMatchObject({ name: "ABORTED", message: "Jev ABORTED" });
		expect(calls).toBe(1);
		expect(requestSignal?.aborted).toBe(true);

		const timeoutFetcher: typeof fetch = async () => await new Promise<Response>(() => {});
		await expect(client(timeoutFetcher, 1).memoryGate("timeout")).rejects.toMatchObject({
			name: "ABORTED",
			message: "Jev ABORTED",
		});
	});

	test("does not dispatch when already aborted and redacts fetch failures", async () => {
		let calls = 0;
		const fetcher: typeof fetch = async () => {
			calls += 1;
			throw new Error("secret=do-not-return");
		};
		const controller = new AbortController();
		controller.abort(new Error("bearer private-reason"));
		await expect(client(fetcher).memoryGate("already stopped", controller.signal)).rejects.toMatchObject({
			name: "ABORTED",
			message: "Jev ABORTED",
		});
		expect(calls).toBe(0);

		await expect(client(fetcher).memoryGate("fetch failed")).rejects.toMatchObject({
			name: "UNAVAILABLE",
			message: "Jev UNAVAILABLE",
		});
	});

	test("prefers the native model environment variable over the legacy one", async () => {
		const fetcher = vi.fn<typeof fetch>(async () => jsonResponse({ answers: { retrieve: { noul: 0.8 } } }));
		vi.stubEnv("TYPESAFE_API_KEY", "test-key");
		vi.stubEnv("TYPESAFE_BASE_URL", "https://jev.example.test");
		vi.stubEnv("ULTRON_JEV_MODEL", "native-model");
		vi.stubEnv("PI_JEV_MODEL", "legacy-model");
		vi.stubGlobal("fetch", fetcher);

		const value = createNativeJevClient();
		expect(value).toBeDefined();
		await value!.memoryGate("model selection");
		const body = JSON.parse(String(fetcher.mock.calls[0][1]?.body)) as { model: string };
		expect(body.model).toBe("native-model");
	});
});
