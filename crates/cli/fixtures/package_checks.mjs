// Generates crates/cli/fixtures/package_checks.json: the TypeScript package manager's startup update check
// (`DefaultPackageManager.checkForAvailableUpdates`) and its temporary `-e` sources
// (`resolveExtensionSources(sources, { temporary: true })`), run in-process against a private temp root per case.
// npm is packages.mjs's fake (a python script that logs its arguments and installs from a local registry file); git
// is the real git against local repositories only: GIT_CONFIG_GLOBAL maps https://github.com/ to
// file://<TMP>/remotes/ and GIT_ALLOW_PROTOCOL=file refuses every other transport, so nothing leaves the machine.
// Each case records the result (updates, or the enabled resources and package roots), the error, the npm log, the
// files left under agent/tmp (without .git internals) and each temporary checkout's commit subject.
// offline-guard.mjs is loaded here; the generator fails if anything tried to leave loopback. Temp paths become <TMP>.
// crates/cli/tests/package_checks.rs replays it.
// Usage: node crates/cli/fixtures/package_checks.mjs
import { assertOffline } from "./offline-guard.mjs";
import { execFileSync } from "node:child_process";
import { existsSync, lstatSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../../..");
const src = join(repo, "packages/coding-agent/src");
const { DefaultPackageManager } = await import(join(src, "core/package-manager.ts"));
const { SettingsManager } = await import(join(src, "core/settings-manager.ts"));

/** packages.mjs's fake npm: logs `name [cwd] args` to $FAKE_NPM_LOG, installs from $FAKE_NPM_REGISTRY. */
export const FAKE_NPM = `#!/usr/bin/python3
import json, os, sys
args = sys.argv[1:]
with open(os.environ["FAKE_NPM_LOG"], "a") as log:
    log.write(os.path.basename(sys.argv[0]) + " [" + os.getcwd() + "] " + " ".join(args) + "\\n")
registry = json.load(open(os.environ["FAKE_NPM_REGISTRY"]))
def split(spec):
    at = spec.find("@", 1)
    return (spec, None) if at < 0 else (spec[:at], spec[at + 1:])
def option(name):
    return args[args.index(name) + 1] if name in args else None
if args[:1] == ["view"]:
    name, version = split(args[1])
    entry = registry.get(name)
    if entry is None:
        sys.stderr.write("npm ERR! 404 Not Found - " + name + "\\n"); sys.exit(1)
    print(json.dumps(entry["versions"] if version else entry["latest"]))
    sys.exit(0)
if args[:2] == ["root", "-g"]:
    print("/nonexistent"); sys.exit(0)
if args[:1] == ["install"]:
    root = option("--prefix") or option("--cwd")
    specs = [a for a in args[1:] if not a.startswith("-") and a != root]
    if root is None:
        open(os.path.join(os.getcwd(), "deps-installed"), "w").write(" ".join(args))
        print("installed dependencies"); sys.exit(0)
    for spec in specs:
        name, version = split(spec)
        entry = registry.get(name)
        if entry is None:
            sys.stderr.write("npm ERR! 404 Not Found - " + name + "\\n"); sys.exit(1)
        version = entry["latest"] if version in (None, "latest") or version not in entry["versions"] else version
        target = os.path.join(root, "node_modules", name)
        os.makedirs(os.path.join(target, "extensions"), exist_ok=True)
        json.dump({"name": name, "version": version}, open(os.path.join(target, "package.json"), "w"))
        open(os.path.join(target, "extensions", "index.ts"), "w").write("// " + version)
    print("added " + str(len(specs)) + " packages"); sys.exit(0)
sys.stderr.write("fake npm: unhandled " + " ".join(args) + "\\n"); sys.exit(2)
`;

export const REGISTRY = JSON.stringify({
	foo: { latest: "1.1.0", versions: ["1.0.0", "1.1.0", "2.0.0-beta.1"] },
	ranged: { latest: "2.0.0", versions: ["1.0.0", "1.3.0", "2.0.0"] },
	weird: { latest: "not-a-version", versions: ["not-a-version"] },
});

export const GIT_ENV = {
	GIT_AUTHOR_NAME: "Test",
	GIT_AUTHOR_EMAIL: "test@example.com",
	GIT_COMMITTER_NAME: "Test",
	GIT_COMMITTER_EMAIL: "test@example.com",
	GIT_AUTHOR_DATE: "2026-01-01T00:00:00Z",
	GIT_COMMITTER_DATE: "2026-01-01T00:00:00Z",
	GIT_CONFIG_NOSYSTEM: "1",
	GIT_TERMINAL_PROMPT: "0",
	GIT_ALLOW_PROTOCOL: "file",
};

// A source repository (commit v1, tagged) with an extension and a skill, and its bare copy at remotes/test/ext
// (https://github.com/test/ext).
const MAKE_REMOTE = [
	"mkdir -p src/extensions src/skills/s remotes/test",
	"cd src && git init -q -b main && echo '// v1' > extensions/main.ts && echo '# s' > skills/s/SKILL.md && git add . && git commit -qm v1 && git tag v1",
	"git clone -q --bare src remotes/test/ext",
];
const PUSH_V2 = ["cd src && echo '// v2' > extensions/main.ts && git commit -qam v2 && git tag v2 && git push -q ../remotes/test/ext main v2"];
const INSTALLED_CLONE = ["mkdir -p agent/git/github.com/test && git clone -q https://github.com/test/ext agent/git/github.com/test/ext"];
const npmInstalled = (dir, name, version) => [`mkdir -p ${dir}/node_modules/${name} && echo '{"name":"${name}","version":"${version}"}' > ${dir}/node_modules/${name}/package.json`];
// The temporary npm root and git checkout of the cases below (getTemporaryDir; the hashes are sha256 prefixes).
const TEMP_NPM = "agent/tmp/extensions/npm/f35b2129";
const TEMP_GIT = "agent/tmp/extensions/git-github.com/5896401d/test/ext";
const TEMP_CLONE = [`mkdir -p ${dirname(TEMP_GIT)} && git clone -q https://github.com/test/ext ${TEMP_GIT}`];

export const CASES = [
	// checkForAvailableUpdates
	{ name: "check npm newer", op: "check", global: { packages: ["npm:foo"] }, setup: npmInstalled("agent/npm", "foo", "1.0.0") },
	{ name: "check npm current", op: "check", global: { packages: ["npm:foo"] }, setup: npmInstalled("agent/npm", "foo", "1.1.0") },
	{ name: "check npm pinned", op: "check", global: { packages: ["npm:foo@1.0.0"] }, setup: npmInstalled("agent/npm", "foo", "1.0.0") },
	{ name: "check npm range", op: "check", global: { packages: ["npm:ranged@^1.0.0"] }, setup: npmInstalled("agent/npm", "ranged", "1.0.0") },
	{ name: "check npm not installed", op: "check", global: { packages: ["npm:foo"] } },
	{ name: "check npm registry failure", op: "check", global: { packages: ["npm:gone"] }, setup: npmInstalled("agent/npm", "gone", "1.0.0") },
	{ name: "check npm invalid version", op: "check", global: { packages: ["npm:weird"] }, setup: npmInstalled("agent/npm", "weird", "1.0.0") },
	{ name: "check offline", op: "check", env: { PI_OFFLINE: "1" }, global: { packages: ["npm:foo"] }, setup: npmInstalled("agent/npm", "foo", "1.0.0") },
	{ name: "check git behind", op: "check", global: { packages: ["git:github.com/test/ext"] }, setup: [...MAKE_REMOTE, ...INSTALLED_CLONE, ...PUSH_V2] },
	{ name: "check git current", op: "check", global: { packages: ["https://github.com/test/ext"] }, setup: [...MAKE_REMOTE, ...INSTALLED_CLONE] },
	{ name: "check git pinned", op: "check", global: { packages: ["git:github.com/test/ext@v1"] }, setup: [...MAKE_REMOTE, ...INSTALLED_CLONE, ...PUSH_V2] },
	{ name: "check git detached without upstream", op: "check", global: { packages: ["git:github.com/test/ext"] }, setup: [...MAKE_REMOTE, ...INSTALLED_CLONE, "cd agent/git/github.com/test/ext && git checkout -q --detach", ...PUSH_V2] },
	{
		name: "check project and user, local skipped",
		op: "check",
		trust: true,
		global: { packages: ["npm:foo", "npm:ranged", "./local"] },
		project: { packages: ["npm:foo"] },
		setup: [...npmInstalled("agent/npm", "foo", "1.0.0"), ...npmInstalled("work/.pi/npm", "foo", "1.1.0"), ...npmInstalled("agent/npm", "ranged", "1.0.0")],
	},
	{ name: "check untrusted project", op: "check", global: { packages: ["npm:foo"] }, project: { packages: ["npm:ranged"] }, setup: npmInstalled("agent/npm", "foo", "1.0.0") },
	// resolveExtensionSources(..., { temporary: true })
	{ name: "temporary npm install", op: "temporary", sources: ["npm:foo"] },
	{ name: "temporary npm installed", op: "temporary", sources: ["npm:foo@1.1.0"], setup: npmInstalled(TEMP_NPM, "foo", "1.1.0") },
	{ name: "temporary npm wrong version reinstalls", op: "temporary", sources: ["npm:foo@1.0.0"], setup: npmInstalled(TEMP_NPM, "foo", "1.1.0") },
	{ name: "temporary npm failure", op: "temporary", sources: ["npm:missing-pkg"] },
	{ name: "temporary npm offline", op: "temporary", env: { PI_OFFLINE: "1" }, sources: ["npm:foo"] },
	{ name: "temporary git clone", op: "temporary", sources: ["git:github.com/test/ext"], setup: MAKE_REMOTE },
	{ name: "temporary git refresh", op: "temporary", sources: ["git:github.com/test/ext"], setup: [...MAKE_REMOTE, ...TEMP_CLONE, ...PUSH_V2] },
	{ name: "temporary git pinned not refreshed", op: "temporary", sources: ["git:github.com/test/ext@v1"], setup: [...MAKE_REMOTE, ...TEMP_CLONE, ...PUSH_V2] },
	{ name: "temporary git refresh failure keeps checkout", op: "temporary", sources: ["git:github.com/test/ext"], setup: [...MAKE_REMOTE, ...TEMP_CLONE, "rm -rf remotes/test/ext"] },
	{ name: "temporary git clone failure", op: "temporary", sources: ["git:github.com/test/none"], setup: ["mkdir -p remotes/test"] },
	{ name: "temporary local paths", op: "temporary", sources: ["./ext-dir", "single.ts", "./nothing"], files: { "work/ext-dir/extensions/a.ts": "", "work/single.ts": "" } },
	{ name: "temporary mixed order", op: "temporary", sources: ["git:github.com/test/ext", "npm:foo"], setup: MAKE_REMOTE },
];

const sub = (s, tmp) => s.split(tmp).join("<TMP>");

function tree(tmp, rel) {
	const root = join(tmp, rel);
	if (!existsSync(root)) return [];
	const out = [];
	const walk = (dir) => {
		for (const name of readdirSync(join(root, dir)).sort()) {
			if (name === ".git") continue;
			const p = dir ? `${dir}/${name}` : name;
			const info = lstatSync(join(root, p));
			if (info.isDirectory()) {
				out.push({ path: p, kind: "dir", mode: (info.mode & 0o777).toString(8) });
				walk(p);
			} else out.push({ path: p, kind: "file", text: sub(readFileSync(join(root, p), "utf8"), tmp) });
		}
	};
	walk("");
	return out;
}

const managed = ["PATH", "HOME", "PI_OFFLINE", "FAKE_NPM_LOG", "FAKE_NPM_REGISTRY", "GIT_CONFIG_GLOBAL", ...Object.keys(GIT_ENV)];

async function run(c) {
	const tmp = realpathSync(mkdtempSync(join(tmpdir(), "ultron-pkg-checks-")));
	for (const d of ["work", "agent", "home", "bin"]) mkdirSync(join(tmp, d), { recursive: true });
	writeFileSync(join(tmp, "bin/npm"), FAKE_NPM, { mode: 0o755 });
	writeFileSync(join(tmp, "registry.json"), REGISTRY);
	writeFileSync(join(tmp, "gitconfig"), `[url "file://${tmp}/remotes/"]\n\tinsteadOf = https://github.com/\n[advice]\n\tdetachedHead = false\n[init]\n\tdefaultBranch = main\n`);
	writeFileSync(join(tmp, "npm.log"), "");
	for (const [rel, text] of Object.entries(c.files ?? {})) {
		mkdirSync(dirname(join(tmp, rel)), { recursive: true });
		writeFileSync(join(tmp, rel), text);
	}
	writeFileSync(join(tmp, "agent/settings.json"), JSON.stringify(c.global ?? {}));
	if (c.project) {
		mkdirSync(join(tmp, "work/.pi"), { recursive: true });
		writeFileSync(join(tmp, "work/.pi/settings.json"), JSON.stringify(c.project));
	}
	const env = {
		PATH: `${join(tmp, "bin")}:/usr/bin:/bin`,
		HOME: join(tmp, "home"),
		FAKE_NPM_LOG: join(tmp, "npm.log"),
		FAKE_NPM_REGISTRY: join(tmp, "registry.json"),
		GIT_CONFIG_GLOBAL: join(tmp, "gitconfig"),
		...GIT_ENV,
	};
	for (const command of c.setup ?? []) execFileSync("/bin/sh", ["-c", command], { cwd: tmp, env, stdio: "ignore" });
	const saved = Object.fromEntries(managed.map((k) => [k, process.env[k]]));
	Object.assign(process.env, env, c.env ?? {});
	let result = null;
	let error = null;
	try {
		const settingsManager = SettingsManager.create(join(tmp, "work"), join(tmp, "agent"), { projectTrusted: Boolean(c.trust) });
		const pm = new DefaultPackageManager({ cwd: join(tmp, "work"), agentDir: join(tmp, "agent"), settingsManager });
		if (c.op === "check") {
			result = (await pm.checkForAvailableUpdates()).map((u) => ({ source: u.source, displayName: u.displayName, type: u.type, scope: u.scope }));
		} else {
			const resolved = await pm.resolveExtensionSources(c.sources, { temporary: true });
			result = {};
			for (const kind of ["extensions", "skills", "prompts", "themes"]) {
				result[kind] = resolved[kind].filter((r) => r.enabled).map((r) => sub(r.path, tmp));
			}
		}
	} catch (e) {
		error = sub(e instanceof Error ? e.message : String(e), tmp);
	} finally {
		for (const [k, v] of Object.entries(saved)) {
			if (v === undefined) delete process.env[k];
			else process.env[k] = v;
		}
	}
	// npm runs without a cwd of its own in the process's directory: <CWD>.
	const npmLog = readFileSync(join(tmp, "npm.log"), "utf8")
		.split("\n")
		.filter(Boolean)
		.map((l) => sub(l.split(`[${process.cwd()}]`).join("[<CWD>]"), tmp))
		.sort();
	const subjects = {};
	for (const checkout of [TEMP_GIT]) {
		if (existsSync(join(tmp, checkout, ".git"))) {
			subjects[checkout] = execFileSync("git", ["log", "-1", "--format=%s"], { cwd: join(tmp, checkout), env, encoding: "utf8" }).trim();
		}
	}
	const record = { ...c, result, error, npmLog, tmpTree: tree(tmp, "agent/tmp"), subjects };
	rmSync(tmp, { recursive: true, force: true });
	return record;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
	const cases = [];
	for (const c of CASES) cases.push(await run(c));
	assertOffline();
	const out = join(here, "package_checks.json");
	writeFileSync(out, `${JSON.stringify({ generator: "crates/cli/fixtures/package_checks.mjs", fakeNpm: FAKE_NPM, registry: REGISTRY, gitEnv: GIT_ENV, cases }, null, 1)}\n`);
	console.log(`wrote ${cases.length} cases to ${out}`);
}
