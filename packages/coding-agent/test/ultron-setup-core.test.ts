import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Input } from "@ultron/tui";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
	checkEnvironment,
	effectiveHindsightUrl,
	formatCommand,
	hindsightDockerArgs,
	hindsightManualInstructions,
	hindsightPort,
	listEndpointModels,
	mergeCustomEndpoint,
	parseModelIds,
	probeHindsight,
	shouldOfferSetup,
	validateBaseUrl,
	validateProviderId,
	writePrivateFile,
} from "../src/cli/setup/core.ts";
import { InMemorySettingsStorage, SettingsManager } from "../src/core/settings-manager.ts";
import { hindsightUrl } from "../src/experimental/session-worker.ts";

const dirs: string[] = [];
function tempDir(): string {
	const dir = mkdtempSync(join(tmpdir(), "ultron-setup-core-"));
	dirs.push(dir);
	return dir;
}

afterEach(() => {
	for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

describe("custom endpoint in models.json", () => {
	const endpoint = {
		providerId: "proxy",
		baseUrl: "http://127.0.0.1:8317/v1/",
		apiKey: "sk-secret",
		api: "openai-completions" as const,
		modelIds: ["kimi-k3", "grok-4.5"],
	};

	it("creates the file with the provider in the loader's format", () => {
		const merged = mergeCustomEndpoint(undefined, endpoint);
		expect(merged.replaced).toBe(false);
		expect(JSON.parse(merged.text)).toEqual({
			providers: {
				proxy: {
					baseUrl: "http://127.0.0.1:8317/v1",
					api: "openai-completions",
					apiKey: "sk-secret",
					models: [{ id: "kimi-k3" }, { id: "grok-4.5" }],
				},
			},
		});
	});

	it("keeps other providers and top-level keys, and reports a replaced provider", () => {
		const existing = `// comments are allowed in models.json
{
  "providers": {
    "ollama": { "baseUrl": "http://localhost:11434/v1", "api": "openai-completions", "apiKey": "ollama", "models": [{ "id": "qwen" }] },
    "proxy": { "baseUrl": "http://old", "apiKey": "old", "models": [] }
  }
}`;
		const merged = mergeCustomEndpoint(existing, endpoint);
		const parsed = JSON.parse(merged.text);
		expect(merged.replaced).toBe(true);
		expect(parsed.providers.ollama.models).toEqual([{ id: "qwen" }]);
		expect(parsed.providers.proxy.baseUrl).toBe("http://127.0.0.1:8317/v1");
	});

	it("writes a placeholder key for endpoints without authentication", () => {
		const merged = mergeCustomEndpoint(undefined, { ...endpoint, apiKey: "  " });
		expect(JSON.parse(merged.text).providers.proxy.apiKey).toBe("none");
	});

	it("refuses to rewrite a file it cannot parse", () => {
		expect(() => mergeCustomEndpoint("{ not json", endpoint)).toThrow(/not valid JSON/);
		expect(() => mergeCustomEndpoint("[]", endpoint)).toThrow(/JSON object/);
	});

	it("loads through Pi's models.json loader", async () => {
		const dir = tempDir();
		const path = join(dir, "models.json");
		writePrivateFile(path, mergeCustomEndpoint(undefined, endpoint).text);
		const { ModelConfig } = await import("../src/core/model-config.ts");
		const config = await ModelConfig.load(path);
		expect(config.getError()).toBeUndefined();
		expect(config.getProvider("proxy")?.models?.map((model) => model.id)).toEqual(["kimi-k3", "grok-4.5"]);
	});

	it("validates names, URLs and model lists", () => {
		expect(validateProviderId("my-proxy")).toBeUndefined();
		expect(validateProviderId("My Proxy")).toBeDefined();
		expect(validateBaseUrl("http://localhost:11434/v1")).toBeUndefined();
		expect(validateBaseUrl("ftp://x")).toBeDefined();
		expect(validateBaseUrl("nope")).toBeDefined();
		expect(parseModelIds(" a, b\nc a ")).toEqual(["a", "b", "c"]);
	});

	it("lists the endpoint's models without leaking the key into the URL", async () => {
		const fetcher = vi.fn(async (url: string | URL | Request, init?: RequestInit) => {
			expect(String(url)).toBe("http://proxy/v1/models");
			expect((init?.headers as Record<string, string>).authorization).toBe("Bearer k");
			return new Response(JSON.stringify({ data: [{ id: "b" }, { id: "a" }, { nope: 1 }] }));
		});
		expect(await listEndpointModels("http://proxy/v1/", "k", fetcher as typeof fetch)).toEqual(["a", "b"]);
		const failing = vi.fn(async () => new Response("no", { status: 404 }));
		expect(await listEndpointModels("http://proxy/v1", undefined, failing as typeof fetch)).toBeUndefined();
	});
});

describe("Hindsight", () => {
	it("builds the documented docker run with the key passed by name only", () => {
		const args = hindsightDockerArgs({ port: 8888, provider: "openai", model: "gpt-5-mini", hasApiKey: true });
		expect(args).toEqual([
			"run",
			"-d",
			"--name",
			"hindsight",
			"--restart",
			"unless-stopped",
			"-p",
			"8888:8888",
			"-e",
			"HINDSIGHT_API_LLM_PROVIDER=openai",
			"-e",
			"HINDSIGHT_API_LLM_API_KEY",
			"-e",
			"HINDSIGHT_API_LLM_MODEL=gpt-5-mini",
			"-v",
			"hindsight-data:/home/hindsight/.pg0",
			"ghcr.io/vectorize-io/hindsight:latest",
		]);
		expect(formatCommand("docker", args)).not.toMatch(/sk-/);
	});

	it("maps a custom port and reaches a server on the host", () => {
		const args = hindsightDockerArgs({
			port: 9000,
			provider: "ollama",
			baseUrl: "http://host.docker.internal:11434/v1",
			hasApiKey: false,
		});
		expect(args).toContain("9000:8888");
		expect(args).not.toContain("HINDSIGHT_API_LLM_API_KEY");
		expect(args).toContain("host.docker.internal:host-gateway");
		expect(hindsightPort("http://localhost:9000")).toBe(9000);
		expect(hindsightManualInstructions("http://localhost:9000").join("\n")).toContain("hindsight-api --port 9000");
	});

	it("probes /health", async () => {
		const up = vi.fn(async (url: string | URL | Request) => {
			expect(String(url)).toBe("http://localhost:8888/health");
			return new Response("{}");
		});
		expect(await probeHindsight("http://localhost:8888/", up as typeof fetch)).toEqual({ ok: true });
		const down = vi.fn(async () => {
			throw new TypeError("fetch failed", { cause: new Error("connect ECONNREFUSED 127.0.0.1:8888") });
		});
		expect(await probeHindsight("http://localhost:8888", down as typeof fetch)).toEqual({
			ok: false,
			reason: "connect ECONNREFUSED 127.0.0.1:8888",
		});
		// Node's fetch to "localhost" fails with an AggregateError that has no message of its own.
		const refused = Object.assign(new Error(""), { code: "ECONNREFUSED" });
		const aggregate = vi.fn(async () => {
			throw new TypeError("fetch failed", { cause: new AggregateError([refused], "") });
		});
		expect(await probeHindsight("http://localhost:8888", aggregate as typeof fetch)).toEqual({
			ok: false,
			reason: "connection refused",
		});
	});

	it("uses the environment, then the saved setting, then the default", () => {
		expect(effectiveHindsightUrl({}, undefined)).toEqual({ url: "http://localhost:8888", source: "default" });
		expect(effectiveHindsightUrl({}, "http://mem:9000")).toEqual({ url: "http://mem:9000", source: "setting" });
		expect(effectiveHindsightUrl({ ULTRON_HINDSIGHT_URL: "off" }, "http://mem:9000")).toEqual({
			url: undefined,
			source: "env",
		});
		// The worker resolves the same way.
		expect(hindsightUrl(undefined, "http://mem:9000")).toBe("http://mem:9000");
		expect(hindsightUrl("", "off")).toBeUndefined();
		expect(hindsightUrl("http://env:1", "http://mem:9000")).toBe("http://env:1");
		expect(hindsightUrl(undefined)).toBe("http://localhost:8888");
	});

	it("saves the URL as a global setting only", () => {
		const settings = SettingsManager.inMemory({ hindsightUrl: " http://mem:9000 " });
		expect(settings.getHindsightUrl()).toBe("http://mem:9000");
		settings.setHindsightUrl("off");
		expect(settings.getHindsightUrl()).toBe("off");
		// A trusted project's settings.json cannot point memory at another server.
		const storage = new InMemorySettingsStorage();
		storage.withLock("project", () => JSON.stringify({ hindsightUrl: "http://elsewhere:1" }));
		const project = SettingsManager.fromStorage(storage, { projectTrusted: true });
		expect(project.getHindsightUrl()).toBeUndefined();
	});
});

describe("environment", () => {
	it("flags an old Node and a missing python3 with fixes; Docker is optional", () => {
		const checks = checkEnvironment("20.1.0", (command) => (command === "docker" ? "Docker version 27" : undefined));
		expect(checks.map((check) => [check.label, check.ok])).toEqual([
			["Node.js", false],
			["python3", false],
			["Docker", true],
		]);
		expect(checks[0]?.fix).toMatch(/22\.19\.0/);
		expect(checks[1]?.fix).toMatch(/python3/);
		const good = checkEnvironment("22.19.0", () => "Python 3.12.1");
		expect(good.every((check) => check.ok)).toBe(true);
	});
});

describe("first-run offer", () => {
	const base = {
		stdinIsTTY: true,
		stdoutIsTTY: true,
		interactive: true,
		explicitModel: false,
		env: {},
		dismissed: false,
	};

	it("offers setup on an interactive start with no usable model", async () => {
		expect(await shouldOfferSetup({ ...base, hasUsableModel: async () => false })).toBe(true);
		expect(await shouldOfferSetup({ ...base, hasUsableModel: async () => true })).toBe(false);
	});

	it("never prompts without a terminal, in print/json/rpc runs, or when told not to", async () => {
		const hasUsableModel = vi.fn(async () => false);
		for (const input of [
			{ ...base, stdinIsTTY: false },
			{ ...base, stdoutIsTTY: false },
			{ ...base, interactive: false },
			{ ...base, explicitModel: true },
			{ ...base, dismissed: true },
			{ ...base, env: { ULTRON_SKIP_SETUP: "1" } },
			{ ...base, env: { CI: "true" } },
		]) {
			expect(await shouldOfferSetup({ ...input, hasUsableModel })).toBe(false);
		}
		// The model catalog is not even loaded in those cases.
		expect(hasUsableModel).not.toHaveBeenCalled();
		expect(await shouldOfferSetup({ ...base, env: { ULTRON_SKIP_SETUP: "0" }, hasUsableModel })).toBe(true);
	});
});

describe("masked input", () => {
	it("never renders the value of a secret", () => {
		const input = new Input({ mask: "•" });
		input.focused = true;
		for (const char of "sk-very-secret") input.handleInput(char);
		const line = input.render(40).join("");
		expect(line).not.toContain("secret");
		expect(line).toContain("•".repeat(13));
		expect(input.getValue()).toBe("sk-very-secret");
	});
});
