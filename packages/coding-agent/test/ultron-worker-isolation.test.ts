import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, test, vi } from "vitest";
import { kernelEnvironment, RlmKernel } from "../src/ultron/rlm/kernel.ts";

const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));

afterEach(() => {
	vi.unstubAllEnvs();
});

describe("A10 worker isolation", () => {
	test("model-written Python sees no ambient credentials or worker control channel", async () => {
		vi.stubEnv("ANTHROPIC_API_KEY", "sk-ant-planted");
		vi.stubEnv("GITHUB_TOKEN", "ghp_planted");
		vi.stubEnv("AWS_SECRET_ACCESS_KEY", "aws-planted");
		vi.stubEnv("PI_SESSION_WORKER_CONTROL_TOKEN", "control-planted");
		vi.stubEnv("ULTRON_HARMLESS_SETTING", "kept");
		const kernel = new RlmKernel({ cwd: process.cwd(), runtimePath }, async () => {
			throw new Error("no host capability is granted in this test");
		});
		try {
			const result = await kernel.execute(
				"import os\nsorted(k for k, v in os.environ.items() if 'planted' in v), os.environ.get('ULTRON_HARMLESS_SETTING')",
			);
			expect(result).toMatchObject({ status: "ok", result: "([], 'kept')" });
			// Host capabilities exist only as host requests, and an ungranted one fails in Python.
			expect(await kernel.execute("await rlm.host_request('secrets.read', {})")).toMatchObject({
				status: "error",
				error: { evalue: expect.stringContaining("no host capability") },
			});
		} finally {
			await kernel.shutdown();
		}
	});

	test("explicit allow-list passes a named variable through", () => {
		const environment = kernelEnvironment({
			GITHUB_TOKEN: "t",
			OPENAI_API_KEY: "k",
			ULTRON_RLM_ENV_ALLOW: "GITHUB_TOKEN",
			PATH: "/bin",
		});
		expect(environment).toEqual({ GITHUB_TOKEN: "t", ULTRON_RLM_ENV_ALLOW: "GITHUB_TOKEN", PATH: "/bin" });
	});
});
