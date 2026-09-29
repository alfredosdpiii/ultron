import { defineConfig, mergeConfig } from "vitest/config";
import baseConfig, { workspaceSourcePaths } from "../../vitest.base.ts";

export default mergeConfig(
	baseConfig,
	defineConfig({
		test: {
			globals: true,
			environment: "node",
			testTimeout: 30000,
			// Tests run offline by default; opt in with allowNetwork() from test/test-network-env.ts.
			// Offline by default, never against the user's real Hindsight memory server, and never writing RLM
			// snapshot keys of temporary profiles into the user's login keyring. Loki guardrails are off unless a test
			// turns them on (ultron-loki*.test.ts), so sessions in temporary repositories get no `.loki/` commit.
			env: {
				PI_OFFLINE: "1",
				ULTRON_HINDSIGHT_URL: "off",
				ULTRON_RLM_SNAPSHOT_KEY_STORE: "file",
				ULTRON_LOKI: "off",
			},
			unstubEnvs: true,
			reporters: process.env.GITHUB_ACTIONS ? ["dot", "github-actions"] : ["dot"],
			silent: "passed-only",
			server: {
				deps: {
					external: [/@silvia-odwyer\/photon-node/],
				},
			},
		},
		resolve: {
			alias: [
				{ find: /^@ultron\/ai$/, replacement: workspaceSourcePaths.aiIndex },
				{ find: /^@ultron\/agent-core$/, replacement: workspaceSourcePaths.agentIndex },
				{ find: /^@mariozechner\/pi-ai$/, replacement: workspaceSourcePaths.aiIndex },
				{ find: /^@mariozechner\/pi-ai\/oauth$/, replacement: workspaceSourcePaths.aiOAuth },
				{ find: /^@mariozechner\/pi-agent-core$/, replacement: workspaceSourcePaths.agentIndex },
				{ find: /^@mariozechner\/pi-tui$/, replacement: workspaceSourcePaths.tuiIndex },
			],
		},
	}),
);
