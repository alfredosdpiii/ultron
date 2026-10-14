// Records `serviceFile` of packages/coding-agent/src/ultron/autoreview/service.ts (the systemd unit and launchd agent
// `ultron autoreview install` writes) over recorded self commands, platforms, homes and environments, to service.json.
// Usage: node crates/review/fixtures/generate_service.mjs
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const service = await import(pathToFileURL(join(root, "packages/coding-agent/src/ultron/autoreview/service.ts")).href);
const selves = [
	{ command: "/usr/bin/ultron", args: [] },
	{ command: "/opt/node", args: ["--import", "/opt/x y/resolver.ts", "/opt/ultron/cli.js"] },
	{ command: "/home/me/my \"bin\"/ult\\ron", args: ["a&b<c>d", "it's"] },
];
const targets = [
	{ platform: "linux", home: "/home/me", env: { PATH: "/usr/bin:/bin" } },
	{ platform: "linux", home: "/home/me", env: { PATH: "/usr/bin:/my bin", ULTRON_CODING_AGENT_DIR: "/home/me/agent dir", XDG_CONFIG_HOME: "  /cfg  " } },
	{ platform: "linux", home: "/home/me", env: { XDG_CONFIG_HOME: "   ", ULTRON_CODING_AGENT_DIR: "" } },
	{ platform: "darwin", home: "/Users/x", env: { PATH: "/bin" } },
	{ platform: "darwin", home: "/Users/x y", env: { PATH: "/a&b", ULTRON_CODING_AGENT_DIR: "/Users/x/<agent>" } },
	{ platform: "darwin", home: "/Users/it's", env: {} },
	{ platform: "win32", home: "C:/Users/x", env: {} },
	{ platform: "freebsd", home: "/home/x", env: {} },
];
const cases = [];
for (const self of selves)
	for (const target of targets) cases.push({ self, target, file: service.serviceFile(self, target) ?? null });
const quotes = ["simple", "with space", "it's", "", "a/b:c,d@e%f+g=h.i_j-k", "$HOME", "a\"b"].map((value) => ({
	value,
	quoted: service.serviceFile({ command: value, args: [] }, { platform: "darwin", home: value, env: {} })?.enable[0] ?? null,
}));
writeFileSync(join(here, "service.json"), `${JSON.stringify({ cases, quotes }, null, 1)}\n`);
