// Generates crates/mcp/fixtures/config.json from the TypeScript MCP server configuration
// (packages/coding-agent/src/core/mcp-servers.ts and extensions/mcp/config.ts): validation results for many
// server entries, tool exposure lookups, and loadMcpConfig over global and project mcp.json files.
// Usage: node crates/mcp/fixtures/generate-config.mjs   (from the repository root)
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const src = (path) => import(pathToFileURL(join(root, "packages/coding-agent/src", path)).href);
const { validateMcpServerConfig, normalizeAdapterServerConfig, getMcpToolExposure, mcpNamespace } = await src(
	"core/mcp-servers.ts",
);
const { loadMcpConfig } = await src("extensions/mcp/config.ts");

const stdio = { command: "npx", args: ["-y", "server"] };
const http = { url: "https://example.com/mcp" };
const VALIDATE = [
	["fs", stdio],
	["fs", { ...stdio, type: "stdio", env: { A: "${HOME}" }, cwd: "~/x", inheritEnv: false, enabled: false }],
	["bad name", stdio],
	["a.b", stdio],
	["fs", "nope"],
	["fs", null],
	["fs", [1]],
	["fs", { ...stdio, lifecycle: "eager" }],
	["fs", { ...stdio, lifecycle: "sometimes" }],
	["fs", { ...stdio, exposure: "codemode-deferred", toolExposure: { "a*": "codemode-deferred", b: "direct" } }],
	["fs", { ...stdio, exposure: "loud" }],
	["fs", { ...stdio, toolExposure: [] }],
	["fs", { ...stdio, toolExposure: { x: "nope" } }],
	["fs", { ...stdio, enabled: "yes" }],
	["fs", { ...stdio, description: 3 }],
	["fs", { ...stdio, description: "Files" }],
	["fs", { ...stdio, timeout: 0 }],
	["fs", { ...stdio, timeout: -1 }],
	["fs", { ...stdio, timeout: "5" }],
	["fs", { ...stdio, timeout: 2.5 }],
	["fs", { ...stdio, type: "sse" }],
	["fs", { ...stdio, args: "x" }],
	["fs", { ...stdio, args: [1] }],
	["fs", { ...stdio, env: { A: 1 } }],
	["fs", { ...stdio, cwd: 1 }],
	["fs", { ...stdio, inheritEnv: "no" }],
	["fs", { ...stdio, type: "http" }],
	["fs", {}],
	["web", http],
	["web", { ...http, type: "streamable-http", headers: { Authorization: "Bearer x" } }],
	["web", { url: "ftp://example.com" }],
	["web", { url: "not a url" }],
	["web", { url: "http://[::1]:3000/mcp" }],
	["web", { ...http, headers: { A: 1 } }],
	["web", { ...http, headers: [] }],
	["web", { ...http, oauth: "x" }],
	["web", { ...http, oauth: { clientId: 1 } }],
	["web", { ...http, oauth: { clientSecret: 1 } }],
	["web", { ...http, oauth: { callbackPort: 0 } }],
	["web", { ...http, oauth: { callbackPort: 1.5 } }],
	["web", { ...http, oauth: { callbackPort: 65536 } }],
	["web", { ...http, oauth: { callbackPort: 8080, clientId: "c" } }],
	["web", { ...http, oauth: { callbackUrl: "http://localhost:8080/oauth/callback" } }],
	["web", { ...http, oauth: { callbackUrl: "http://localhost:8080/cb", callbackPort: 9090 } }],
	["web", { ...http, oauth: { callbackUrl: "http://localhost/cb", callbackPort: 9090 } }],
	["web", { ...http, oauth: { callbackUrl: "https://localhost/cb" } }],
	["web", { ...http, oauth: { callbackUrl: "http://example.com/cb" } }],
	["web", { ...http, oauth: { callbackUrl: "http://127.0.0.1/cb?x=1" } }],
	["web", { ...http, oauth: { callbackUrl: "http://[::1]/cb#f" } }],
	["web", { ...http, oauth: { callbackUrl: "http://[::1]/cb" } }],
	["web", { ...http, oauth: { scope: 1 } }],
	["web", { ...http, oauth: { clientName: " " } }],
	["web", { ...http, oauth: { clientName: "ultron" } }],
	["web", { ...http, oauth: { authServerMetadataUrl: "http://example.com/x" } }],
	["web", { ...http, oauth: { authServerMetadataUrl: "http://127.0.0.1:9/x" } }],
	["web", { ...http, oauth: { authServerMetadataUrl: "https://auth.example.com/.well-known/x" } }],
	["web", { ...http, oauth: { authServerMetadataUrl: "nope" } }],
	["web", { ...http, auth: { provider: "github" } }],
	["web", { ...http, auth: { provider: "" } }],
	["web", { ...http, auth: "x" }],
	["web", { url: "http://example.com/mcp", auth: { provider: "github" } }],
	["web", { url: "http://localhost:3/mcp", auth: { provider: "github" } }],
	["web", { url: "https://example.com/mcp", command: "x" }],
	["both", { url: 5, command: "x" }],
];
const NORMALIZE = [
	[{ ...stdio, disabled: true }, {}],
	[{ ...stdio, disabled: true, enabled: true }, {}],
	[{ ...stdio, requestTimeoutMs: 1500 }, {}],
	[{ ...stdio }, { requestTimeoutMs: 2500 }],
	[{ ...stdio, timeout: 9, requestTimeoutMs: 1500 }, {}],
	[{ ...stdio, requestTimeoutMs: 0 }, {}],
	[{ ...http, auth: "bearer", bearerToken: "tok" }, {}],
	[{ ...http, bearerTokenEnv: "TOKEN" }, {}],
	[{ ...http, bearerToken: "tok", headers: { authorization: "Basic x" } }, {}],
	[{ ...http, bearerToken: "!pass show x" }, {}],
	[{ ...stdio, bearerToken: "tok" }, {}],
	[{ ...http, bearerToken: "", bearerTokenEnv: "T", headers: { X: "y" } }, {}],
	["raw", {}],
];
const EXPOSURE = [
	[{ ...stdio }, "x"],
	[{ ...stdio, exposure: "direct" }, "x"],
	[{ ...stdio, exposure: "hidden", toolExposure: { "read_*": "direct", read_secret: "hidden", "*": "deferred" } }, "read_file"],
	[{ ...stdio, exposure: "hidden", toolExposure: { "read_*": "direct", read_secret: "hidden" } }, "read_secret"],
	[{ ...stdio, exposure: "hidden", toolExposure: { "*": "deferred", "read_*": "direct" } }, "read_file"],
	[{ ...stdio, toolExposure: { "a.b*": "direct" } }, "axb"],
	[{ ...stdio, toolExposure: { "a.b*": "direct" } }, "a.bc"],
	[{ ...stdio, toolExposure: { "(x)*": "direct" } }, "(x)y"],
	[{ ...stdio, toolExposure: { "read_*_x": "direct" } }, "read_a_xy"],
	[{ ...stdio, toolExposure: { "read_*_x": "direct" } }, "read_a_x"],
];

const dir = mkdtempSync(join(tmpdir(), "mcp-config-"));
const agentDir = join(dir, "agent");
const cwd = join(dir, "project");
mkdirSync(agentDir, { recursive: true });
mkdirSync(join(cwd, ".pi"), { recursive: true });
const files = {
	global: {
		autoEnableCodemode: false,
		settings: { requestTimeoutMs: 4000 },
		mcpServers: {
			fs: { command: "npx", args: ["fs"] },
			"my-server": { url: "https://a.example/mcp", auth: { provider: "github" } },
			my_server: { command: "x" },
			broken: { url: 3 },
			web: { url: "https://b.example/mcp", disabled: true },
		},
	},
	project: {
		autoEnableCodemode: "yes",
		mcpServers: {
			fs: { command: "uvx", args: ["fs2"], description: "Project files" },
			secret: { url: "https://c.example/mcp", auth: { provider: "github" } },
			"bad name": { command: "x" },
		},
	},
};
const LOADS = [
	{ global: files.global, project: files.project, trusted: true },
	{ global: files.global, project: files.project, trusted: false },
	{ global: { mcpServers: [] }, project: { mcpServers: { a: { command: "a" } }, autoEnableCodemode: true }, trusted: true },
	{ global: [1], project: "text", trusted: true },
	{ global: { other: 1 }, trusted: true },
	{ trusted: true },
];
const loads = [];
for (const load of LOADS) {
	rmSync(join(agentDir, "mcp.json"), { force: true });
	rmSync(join(cwd, ".pi", "mcp.json"), { force: true });
	if (load.global !== undefined) writeFileSync(join(agentDir, "mcp.json"), JSON.stringify(load.global, null, 2));
	if (load.project !== undefined) writeFileSync(join(cwd, ".pi", "mcp.json"), JSON.stringify(load.project, null, 2));
	const result = loadMcpConfig({ agentDir, cwd, projectTrusted: load.trusted });
	const text = JSON.stringify(result).replaceAll(dir, "$ROOT");
	loads.push({ ...load, result: JSON.parse(text) });
}
rmSync(dir, { recursive: true, force: true });

const fixture = {
	validate: VALIDATE.map(([name, raw]) => ({ name, raw, result: validateMcpServerConfig(name, raw) })),
	normalize: NORMALIZE.map(([raw, defaults]) => ({ raw, defaults, result: normalizeAdapterServerConfig(raw, defaults) })),
	exposure: EXPOSURE.map(([config, tool]) => ({ config, tool, result: getMcpToolExposure(config, tool) })),
	namespace: ["fs", "my-server", "a-b_c-"].map((name) => ({ name, result: mcpNamespace(name) })),
	loads,
};
writeFileSync(join(here, "config.json"), `${JSON.stringify(fixture, null, 1)}\n`);
