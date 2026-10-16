// Generates crates/cli/fixtures/login-options.json: what `/login` lists in the native TUI
// (experimental/client-tui-auth.ts `loginProviderOptions` over client-tui.ts `createLoginRuntime`: a ModelRuntime
// over the profile's auth.json and models.json only, so providers an extension registers in the session are never
// offered), for a profile with a models.json custom provider and a Radius gateway. Run with PI_OFFLINE=1 and no
// provider keys; nothing leaves the machine. crates/cli/tests/login_options.rs replays it.
// Usage: node crates/cli/fixtures/generate-login-options.mjs
import { assertOffline } from "./offline-guard.mjs";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const tmp = realpathSync(mkdtempSync(join(tmpdir(), "ultron-login-options-")));
const agent = join(tmp, "agent");
mkdirSync(agent, { recursive: true });
for (const key of Object.keys(process.env)) delete process.env[key];
Object.assign(process.env, { HOME: join(tmp, "home"), PATH: "/usr/bin:/bin", PI_OFFLINE: "1", ULTRON_CODING_AGENT_DIR: agent, ULTRON_SERVER_DIR: join(tmp, "server"), LOGIN_SET_KEY: "v" });
export const MODELS = {
	providers: {
		gw: { oauth: "radius", baseUrl: "http://127.0.0.1:9/v1", name: "My Radius" },
		local: { baseUrl: "http://127.0.0.1:9/v1", api: "openai-completions", apiKey: "x", models: [{ id: "m" }] },
		nokey: { baseUrl: "http://127.0.0.1:9/v1", api: "openai-completions", models: [{ id: "m" }] },
		cmdkey: { baseUrl: "http://127.0.0.1:9/v1", api: "openai-completions", apiKey: "!echo k", models: [{ id: "m" }] },
		envkey: { baseUrl: "http://127.0.0.1:9/v1", api: "openai-completions", apiKey: "LOGIN_FIXTURE_KEY", models: [{ id: "m" }] },
		envunset: { baseUrl: "http://127.0.0.1:9/v1", api: "openai-completions", apiKey: "$LOGIN_FIXTURE_KEY", models: [{ id: "m" }] },
		envset: { baseUrl: "http://127.0.0.1:9/v1", api: "openai-completions", apiKey: "${LOGIN_SET_KEY}", models: [{ id: "m" }] },
		named: { name: "Named One", baseUrl: "http://127.0.0.1:9/v1", api: "openai-completions", apiKey: "y", models: [{ id: "m" }] },
		anthropic: { baseUrl: "http://127.0.0.1:9" },
	},
};
writeFileSync(join(agent, "models.json"), JSON.stringify(MODELS));
const src = (p) => import(pathToFileURL(join(here, "../../../packages/coding-agent/src", p)).href);
const { ModelRuntime } = await src("core/model-runtime.ts");
const { loginProviderOptions } = await src("experimental/client-tui-auth.ts");
// client-tui.ts createLoginRuntime(agentDir).
const runtime = await ModelRuntime.create({
	refreshOnCreate: false,
	allowModelNetwork: false,
	authPath: join(agent, "auth.json"),
	modelsPath: join(agent, "models.json"),
});
await runtime.refresh({ allowNetwork: false });
const options = loginProviderOptions(runtime).map((o) => ({
	id: o.id,
	name: o.name,
	authType: o.authType,
	method: o.method.name ?? null,
	canLogin: typeof o.method.login === "function",
	status: o.status ? { type: o.status.type, source: o.status.source ?? null } : null,
}));
assertOffline();
rmSync(tmp, { recursive: true, force: true });
writeFileSync(join(here, "login-options.json"), `${JSON.stringify({ models: MODELS, env: { LOGIN_SET_KEY: "v" }, options }, null, 1)}\n`);
console.log(`wrote ${options.length} options`);
