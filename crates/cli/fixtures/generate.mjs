// Generates crates/cli/fixtures/cli.json from the TypeScript CLI in this checkout.
// Two tables: `parse` records parseArgs (packages/coding-agent/src/cli/args.ts) for argument vectors in-process;
// `process` runs `node packages/coding-agent/src/cli.ts` per argument vector in a private temp HOME, agent dir and
// cwd, with stdout/stderr redirected to files, and records stdout, stderr and the exit code. Temp paths are replaced
// by <TMP> and the repository root by <REPO>, so the fixture is deterministic.
// Usage: node crates/cli/fixtures/generate.mjs
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, openSync, closeSync, readFileSync, rmSync, writeFileSync, realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../../..");
const { parseArgs } = await import(join(repo, "packages/coding-agent/src/cli/args.ts"));

const PARSE = [
	[], ["--version"], ["-v"], ["--version", "--help", "some message"], ["--help"], ["-h"],
	["--print"], ["-p"], ["-p", "---\ntitle: hello\n---\nSay hi."], ["-p", "--provider", "openai", "Say hi."],
	["-p", "@file.md"], ["-p", "-- x"], ["-p", "hello", "world"],
	["--continue"], ["-c"], ["--resume"], ["-r"], ["--mode"], ["--mode", "-p"], ["--mode", "json"], ["--mode", "rpc"],
	["--mode", "text"], ["--mode", "xml"], ["--provider", "openai"], ["--provider"], ["--model", "gpt-4o"], ["--model"],
	["--api-key", "k"], ["--system-prompt", "You are"], ["--append-system-prompt", "a", "--append-system-prompt", "b"],
	["--append-system-prompt"], ["--name", "My session"], ["-n", " x "], ["--name"], ["-n"], ["--no-session"],
	["--session", "abc"], ["--session"], ["--session-id", "id1"], ["--fork", "f"], ["--session-dir", "/tmp/s"],
	["--models", "a, b ,,c"], ["--models"], ["--no-tools"], ["-nt"], ["--no-builtin-tools"], ["-nbt"],
	["--tools", "read, bash,,"], ["-t", ""], ["--exclude-tools", "a,b"], ["-xt", " c "], ["--tools"],
	["--thinking", "high"], ["--thinking", "max"], ["--thinking", "bogus"], ["--thinking"],
	["--export", "s.jsonl", "out.html"], ["--extension", "a", "-e", "b"], ["--no-extensions"], ["-ne"],
	["--skill", "s1", "--skill", "s2"], ["--prompt-template", "p"], ["--theme", "t"], ["--use-theme", "dark"],
	["--use-theme"], ["--use-theme", "-x"], ["--no-skills"], ["-ns"], ["--no-prompt-templates"], ["-np"], ["--no-themes"],
	["--no-context-files"], ["-nc"], ["--list-models"], ["--list-models", "sonnet"], ["--list-models", "-p"],
	["--list-models", "@f"], ["--tui-mode", "regular"], ["--tui-mode", "fullscreen"], ["--tui-mode", "big"],
	["--tui-mode"], ["--tui-mode", "--verbose"], ["--verbose"], ["--approve"], ["-a"], ["--no-approve"], ["-na"],
	["--offline"], ["--claude"], ["@a.md", "@b.png", "hi"], ["--", "--help", "@x", "-v"], ["--plan"], ["--plan", "x"],
	["--plan", "-x"], ["--plan", "@x"], ["--key=value"], ["--key=a=b"], ["--=v"], ["-x"], ["-"], ["---"], ["--"],
	["hello", "--model", "m", "world"], ["--b", "1", "--a", "--b=2", "--c=3"], ["--name", "--model"], ["-p", "-"],
];

const clean = (r) => ({
	...r,
	unknownFlags: [...r.unknownFlags.entries()],
});
const parse = PARSE.map((args) => ({ args, result: clean(parseArgs(args)) }));

// Every built-in (provider, id) pair in `--list-models` order (localeCompare on provider, then id).
const { MODELS } = await import(join(repo, "packages/ai/src/models.generated.ts"));
const sorted = Object.entries(MODELS)
	.flatMap(([provider, models]) => Object.keys(models).map((id) => [provider, id]))
	.sort((a, b) => a[0].localeCompare(b[0]) || a[1].localeCompare(b[1]));
const PROCESS = [
	{ args: ["--version"] },
	{ args: ["-v", "--offline"] },
	{ args: ["--help"] },
	{ args: ["-h", "hello"] },
	{ args: ["--mode"] },
	{ args: ["--mode", "xml"] },
	{ args: ["-x"] },
	{ args: ["-x", "--use-theme", "--tui-mode", "big"] },
	{ args: ["--thinking", "bogus", "--version"] },
	{ args: ["--name"] },
	{ args: ["--list-models"] },
	{ args: ["--list-models"], env: { ANTHROPIC_API_KEY: "fake-key" } },
	{ args: ["--list-models", "sonnet"], env: { ANTHROPIC_API_KEY: "fake-key" } },
	{ args: ["--list-models", "gpt-5"], env: { OPENAI_API_KEY: "fake-key" } },
	{ args: ["--list-models", "zzzzqqq"], env: { OPENAI_API_KEY: "fake-key" } },
	{ args: ["--list-models"], auth: { groq: { type: "api_key", key: "fake-key" } } },
	{ args: ["--fork", "a", "--session", "b"] },
	{ args: ["--fork", "a", "--continue"] },
	{ args: ["--session-dir", "relative/dir"] },
	{ args: ["--name", "   "] },
	{ args: ["--session", "nope"] },
	{ args: ["--session", "bad.jsonl"], files: { "work/bad.jsonl": '{"kind":"other"}\n' } },
	{ args: ["--fork", "zzz", "--no-session"] },
	{ args: ["--session", "missing.jsonl"] },
	{ args: ["--session", "sub/x"], files: { "work/sub": null, "work/sub/x": '{"kind":"header"}\n' } },
];

const node = process.execPath;
const runs = PROCESS.map((c) => {
	const tmp = realpathSync(mkdtempSync(join(tmpdir(), "ultron-cli-fixture-")));
	mkdirSync(join(tmp, "work"));
	mkdirSync(join(tmp, "agent"));
	if (c.auth) writeFileSync(join(tmp, "agent", "auth.json"), JSON.stringify(c.auth));
	for (const [p, text] of Object.entries(c.files ?? {})) {
		if (text === null) mkdirSync(join(tmp, p));
		else writeFileSync(join(tmp, p), text);
	}
	const out = openSync(join(tmp, "stdout"), "w");
	const err = openSync(join(tmp, "stderr"), "w");
	const env = {
		PATH: dirname(node) + ":/usr/bin:/bin",
		HOME: tmp,
		ULTRON_CODING_AGENT_DIR: join(tmp, "agent"),
		ULTRON_SERVER_DIR: join(tmp, "server"),
		...(c.env ?? {}),
	};
	const r = spawnSync(node, [join(repo, "packages/coding-agent/src/cli.ts"), ...c.args], {
		cwd: join(tmp, "work"),
		env,
		stdio: ["ignore", out, err],
		timeout: 120_000,
	});
	closeSync(out);
	closeSync(err);
	const sub = (s) => s.split(tmp).join("<TMP>").split(repo).join("<REPO>");
	const rec = {
		...c,
		stdout: sub(readFileSync(join(tmp, "stdout"), "utf8")),
		stderr: sub(readFileSync(join(tmp, "stderr"), "utf8")),
		status: r.status,
	};
	rmSync(tmp, { recursive: true, force: true });
	return rec;
});

const version = JSON.parse(readFileSync(join(repo, "packages/coding-agent/package.json"), "utf8")).version;
writeFileSync(join(here, "cli.json"), `${JSON.stringify({ version, parse, sorted, process: runs }, null, 1)}\n`);
