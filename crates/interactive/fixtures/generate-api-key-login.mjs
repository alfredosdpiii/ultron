// Generates crates/interactive/fixtures/api-key-login.json from packages/ai's multi-field API-key logins (the providers
// whose `/login` key prompt is not Pi's single generic one: amazon-bedrock, google-vertex, cloudflare-workers-ai,
// cloudflare-ai-gateway). Each case scripts the dialog's answers (null: the user cancels that prompt) and records
// every prompt and notify event in order, then the credential `ModelRuntime.login` saves (or the error).
// crates/interactive/tests/api_key_login.rs replays every case through the Rust flow.
// Usage: node crates/interactive/fixtures/generate-api-key-login.mjs   (deterministic, offline)
import "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const src = (p) => import(pathToFileURL(join(here, "../../../packages/ai/src/providers", p)).href);
const { amazonBedrockProvider } = await src("amazon-bedrock.ts");
const { googleVertexProvider } = await src("google-vertex.ts");
const { cloudflareWorkersAIProvider } = await src("cloudflare-workers-ai.ts");
const { cloudflareAIGatewayProvider } = await src("cloudflare-ai-gateway.ts");

const providers = {
	"amazon-bedrock": amazonBedrockProvider(),
	"google-vertex": googleVertexProvider(),
	"cloudflare-workers-ai": cloudflareWorkersAIProvider(),
	"cloudflare-ai-gateway": cloudflareAIGatewayProvider(),
};

async function run(provider, answers) {
	const steps = [];
	const queue = [...answers];
	const interaction = {
		signal: new AbortController().signal,
		prompt: async (request) => {
			const { signal: _signal, ...shown } = request;
			steps.push({ prompt: shown });
			const answer = queue.shift();
			if (answer === null || answer === undefined) throw new Error("Login cancelled");
			return answer;
		},
		notify: (event) => steps.push({ event }),
	};
	try {
		const credential = await providers[provider].auth.apiKey.login(interaction);
		return { provider, answers, steps, credential };
	} catch (error) {
		return { provider, answers, steps, error: error.message };
	}
}

const cases = [
	["amazon-bedrock", ["bearer-token", "tok-123"]],
	["amazon-bedrock", ["aws-profile", "work"]],
	["amazon-bedrock", ["credential-chain", ""]],
	["amazon-bedrock", ["bearer-token", null]],
	["amazon-bedrock", [null]],
	["google-vertex", ["api-key", "AIza-1"]],
	["google-vertex", ["adc", "proj-1", "us-central1"]],
	["google-vertex", ["service-account", "proj-2", "europe-west4", "/keys/sa.json"]],
	["google-vertex", ["service-account", "proj-3", "asia-east1", ""]],
	["google-vertex", ["adc", "proj-1", null]],
	["cloudflare-workers-ai", ["cf-key", "acct-1"]],
	["cloudflare-workers-ai", ["cf-key", null]],
	["cloudflare-ai-gateway", ["cf-key", "acct-2", "gw-9"]],
	["cloudflare-ai-gateway", [null]],
];
const out = [];
for (const [provider, answers] of cases) out.push(await run(provider, answers));
writeFileSync(join(here, "api-key-login.json"), `${JSON.stringify({ generator: "generate-api-key-login.mjs", cases: out }, null, "\t")}\n`);
