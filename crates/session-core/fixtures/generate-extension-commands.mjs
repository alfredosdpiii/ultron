// Records how the TypeScript `ultron --mode rpc` lists extension commands in `get_commands`: commands registered by
// extensions found in the profile's extensions directory (two of them registering the same name), one listed by path
// in the profile settings (and one auto-discovered one listed there again), one in the project's extensions directory
// and one listed in the project settings, in their load order with their Pi `SourceInfo`. Each extension is a directory with an `index.ts`; the Rust build runs the
// same extensions as processes, one directory each with an `extension.json` (crates/extensions/PROTOCOL.md).
// Temp paths become `<TMP>`.
// Usage: node crates/session-core/fixtures/generate-extension-commands.mjs
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const root = new URL("../../../", import.meta.url).pathname;
const SYSTEM_PATH = "/usr/local/bin:/usr/bin:/bin";

/** The extensions: directory (relative to the temp root) and the commands each registers, in order. */
export const EXTENSIONS = [
	{ dir: "agent/extensions/greet", commands: [{ name: "greet", description: "Say hello" }, { name: "bare" }] },
	{ dir: "agent/extensions/zeta", commands: [{ name: "greet", description: "Another greet" }] },
	{ dir: "listed", commands: [{ name: "listed-cmd", description: "Listed in settings" }] },
	{ dir: "cwd/.pi/extensions/proj", commands: [{ name: "proj-cmd", description: "Project command" }] },
	{ dir: "cwd/.pi/local/proj-entry", commands: [{ name: "proj-entry", description: "Listed in project settings" }] },
];

/** The settings files' `extensions` lists: the profile's (absolute, and an auto-discovered one listed again), the
 * project's (relative to its `.pi`). */
export const SETTINGS = {
	user: ["<TMP>/listed", "extensions/zeta"],
	project: ["local/proj-entry"],
};

function source(commands) {
	const lines = commands.map(
		(c) =>
			`\tpi.registerCommand(${JSON.stringify(c.name)}, { ${c.description === undefined ? "" : `description: ${JSON.stringify(c.description)}, `}handler: async () => {} });`,
	);
	return `export default function (pi) {\n${lines.join("\n")}\n}\n`;
}

async function record() {
	const tmp = realpathSync(mkdtempSync(join(tmpdir(), "ultron-ext-commands-")));
	const agentDir = join(tmp, "agent");
	const cwd = join(tmp, "cwd");
	mkdirSync(agentDir, { recursive: true });
	mkdirSync(cwd, { recursive: true });
	for (const extension of EXTENSIONS) {
		mkdirSync(join(tmp, extension.dir), { recursive: true });
		writeFileSync(join(tmp, extension.dir, "index.ts"), source(extension.commands));
	}
	const paths = (entries) => entries.map((entry) => entry.split("<TMP>").join(tmp));
	writeFileSync(join(agentDir, "settings.json"), JSON.stringify({ extensions: paths(SETTINGS.user) }));
	writeFileSync(join(cwd, ".pi", "settings.json"), JSON.stringify({ extensions: paths(SETTINGS.project) }));
	writeFileSync(join(agentDir, "models.json"), JSON.stringify({ providers: { anthropic: { baseUrl: "http://127.0.0.1:9" } } }));
	const child = spawn(process.execPath, [join(root, "packages/coding-agent/src/cli.ts"), "--mode", "rpc", "--no-session"], {
		cwd,
		env: {
			PATH: SYSTEM_PATH,
			HOME: tmp,
			ULTRON_CODING_AGENT_DIR: agentDir,
			ULTRON_SERVER_DIR: join(tmp, "server"),
			ANTHROPIC_API_KEY: "test-key",
			ULTRON_HINDSIGHT_URL: "off",
			PI_OFFLINE: "1",
			NO_COLOR: "1",
			TZ: "UTC",
		},
		stdio: ["pipe", "pipe", "pipe"],
	});
	let buffer = "";
	let stderr = "";
	child.stderr.on("data", (chunk) => (stderr += chunk));
	const response = new Promise((resolve, reject) => {
		const timer = setTimeout(() => reject(new Error(`timed out\n${stderr}`)), 120_000);
		child.stdout.on("data", (chunk) => {
			buffer += chunk;
			let i;
			while ((i = buffer.indexOf("\n")) !== -1) {
				const line = JSON.parse(buffer.slice(0, i));
				buffer = buffer.slice(i + 1);
				if (line.type === "response" && line.id === "c") {
					clearTimeout(timer);
					resolve(line);
				}
			}
		});
	});
	child.stdin.write(`${JSON.stringify({ id: "c", type: "get_commands" })}\n`);
	const line = await response;
	child.stdin.end();
	await new Promise((r) => child.on("close", r));
	rmSync(tmp, { recursive: true, force: true });
	const scrub = (v) => JSON.parse(JSON.stringify(v).split(tmp).join("<TMP>"));
	return scrub(line.data.commands.filter((c) => c.source === "extension"));
}

const commands = await record();
writeFileSync(
	new URL("./extension-commands.json", import.meta.url),
	`${JSON.stringify({ extensions: EXTENSIONS, settings: SETTINGS, commands }, null, 1)}\n`,
);
