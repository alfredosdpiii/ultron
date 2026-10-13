// The built-in providers as `/login` and `/logout` list them (`runtime.getProvider(id)`), from the TypeScript registry
// packages/ai/src/providers/all.ts: display name, the API-key method (its name, whether Ultron can save a key for it)
// and the subscription method (its login label). Writes provider_names.json (id -> provider), sorted by id.
// Usage: node crates/interactive/fixtures/provider_names.mjs
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { builtinProviders } from "../../../packages/ai/src/providers/all.ts";

const here = dirname(fileURLToPath(import.meta.url));
const providers = Object.fromEntries(
	builtinProviders()
		.map((provider) => [
			provider.id,
			{
				name: provider.name,
				...(provider.auth.apiKey
					? { apiKey: { name: provider.auth.apiKey.name, login: typeof provider.auth.apiKey.login === "function" } }
					: {}),
				...(provider.auth.oauth
					? { oauth: { name: provider.auth.oauth.name, loginLabel: provider.auth.oauth.loginLabel ?? null } }
					: {}),
			},
		])
		.sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)),
);
writeFileSync(join(here, "provider_names.json"), `${JSON.stringify(providers, null, 1)}\n`);
