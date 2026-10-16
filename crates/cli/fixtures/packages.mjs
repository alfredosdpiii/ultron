// Generates crates/cli/fixtures/packages.json: the TypeScript package manager.
//   units: parseGitUrl, the npm source parser, node-semver, minimatch, `ignore` and DefaultPackageManager.resolve()
//          over directory trees, answered in-process;
//   runs:  `ultron install|remove|update|list` run as processes (node packages/coding-agent/src/cli.ts) with a fake
//          npm (a python script that logs its arguments and writes node_modules from a local registry file) and
//          the real git against local repositories only: GIT_CONFIG_GLOBAL maps https://github.com/ to
//          file://<TMP>/remotes/, so every clone and fetch stays on the machine. Each run gets a private temp HOME,
//          agent dir, server dir and cwd; its setup commands, stdout, stderr, exit code, the npm log and the files
//          left under agent/ and work/.pi (without .git internals, with each checkout's HEAD) are recorded.
// offline-guard.mjs is loaded here and into every TypeScript process; the generator fails if anything tried to leave
// loopback. Temp paths become <TMP>. crates/cli/tests/packages.rs and packages_units.rs replay it.
// Usage: node crates/cli/fixtures/packages.mjs
import { assertOffline, guardNodeOptions } from "./offline-guard.mjs";
import { execFileSync, spawnSync } from "node:child_process";
import {
	closeSync,
	existsSync,
	lstatSync,
	mkdirSync,
	mkdtempSync,
	openSync,
	readdirSync,
	readFileSync,
	realpathSync,
	rmSync,
	symlinkSync,
	writeFileSync,
} from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../../..");
const src = join(repo, "packages/coding-agent/src");
const require = createRequire(join(src, "x.js"));
const semver = require("semver");
const { minimatch } = require("minimatch");
const ignore = require("ignore");
const { parseGitUrl } = await import(join(src, "utils/git.ts"));
const { DefaultPackageManager } = await import(join(src, "core/package-manager.ts"));
const { SettingsManager } = await import(join(src, "core/settings-manager.ts"));
const guardLog = join(mkdtempSync(join(tmpdir(), "ultron-pkg-guard-")), "guard.log");
process.env.ULTRON_GUARD_LOG = guardLog;

// ------------------------------------------------------------------------------------------------------- units

const gitSources = [
	"https://github.com/user/repo",
	"https://github.com/user/repo.git",
	"https://github.com/user/repo@v1.2.0",
	"https://github.com/user/repo#main",
	"https://github.com/user/repo/tree/dev",
	"https://GitHub.com/User/Repo",
	"http://github.com/user/repo",
	"ssh://git@github.com/user/repo",
	"ssh://git@github.com/user/repo.git@abc123",
	"git://github.com/user/repo",
	"git:github.com/user/repo",
	"git:github.com/user/repo@v1",
	"git:git@github.com:user/repo",
	"git:git@github.com:user/repo.git@v2",
	"git:user/repo",
	"git:gitlab.com/group/sub/proj",
	"https://gitlab.com/group/sub/proj",
	"https://gitlab.com/group/proj/-/tree/main",
	"git:bitbucket.org/team/repo",
	"https://bitbucket.org/team/repo/get/x",
	"git:git.sr.ht/~user/repo",
	"https://gist.github.com/abc123",
	"https://gist.github.com/user/abc123",
	"git:github:user/repo",
	"git:gitlab:user/repo#v3",
	"https://example.com/org/repo",
	"https://example.com/org/repo@ref",
	"https://example.com:8443/org/repo.git",
	"ssh://git@example.com:2222/org/repo",
	"git:example.com/org/repo",
	"git:example.com/org/repo@feature/x",
	"git:git@example.com:org/repo",
	"git:git@example.com:org/repo@v1",
	"git:localhost/org/repo",
	"git:nodots/org/repo",
	"https://example.com/onlyone",
	"https://example.com/org/../repo",
	"https://example.com/org/%2e%2e/repo",
	"https://example.com/org/re%zzpo",
	"git:example.com/a\\b/c",
	"github.com/user/repo",
	"git@github.com:user/repo",
	"  https://github.com/user/repo  ",
	"git: github.com/user/repo",
	"https://user:pw@github.com/user/repo",
	"https://www.github.com/user/repo",
];

const versions = ["1.0.0", "1.2.3", "v1.2.3", "=1.2.3", " 1.2.3 ", "1.2", "01.2.3", "1.2.3-beta.1", "1.2.3-0", "1.2.3+build.5", "2.0.0-rc.1", "1.2.3-01", "latest", "", "1.2.3-beta..1", "0.0.4", "0.3.0", "1.3.0", "2.0.0", "3.0.0-alpha"];
const ranges = ["^1.2.0", "~1.2", "~1.2.3", "^0.2.3", "^0.0.3", "1.x", "1", "*", "", ">=1.2.0 <2.0.0", ">1.2", "<=1.2", "<1", "1.2.3 - 2.0.0", "1.2 - 2", ">=1.2.3-beta.0", "^1.2.3-beta.0", "1.2.3", "=1.2.3", "^1 || ^2", "~>1.2", "> 1.2.3", "^ 1", "latest", "next", "1.2.3.4", "x.1.2", ">*", "<*", "^0.x", "^0.0"];
const semverCases = {
	valid: versions.map((v) => [v, semver.valid(v)]),
	validRange: ranges.map((r) => [r, semver.validRange(r) !== null]),
	satisfies: ranges.flatMap((r) => versions.map((v) => [v, r, semver.satisfies(v, r)])),
	maxSatisfying: ranges.map((r) => [r, semver.maxSatisfying(["1.0.0", "1.2.3", "1.2.4-beta.1", "1.3.0", "2.0.0", "2.1.0", "0.3.0", "0.0.4", "bogus"], r)]),
	gt: [["1.2.0", "1.1.9"], ["1.2.0-beta", "1.2.0"], ["1.2.0-beta.2", "1.2.0-beta.10"], ["1.2.0-alpha", "1.2.0-1"], ["2.0.0", "10.0.0"], ["1.0.0", "1.0.0"]].map(([a, b]) => [a, b, semver.gt(a, b)]),
	highest: [["1.0.0", "1.10.0", "1.9.0"], ["2.0.0-beta", "2.0.0", "1.9.9"], ["1.0.0-a", "1.0.0-b"]].map((list) => [list, [...list].sort(semver.rcompare)[0]]),
};

const globPaths = ["extensions/a.ts", "extensions/sub/b.ts", "a.ts", ".hidden/x.ts", "skills/x/SKILL.md", "prompts/p.md", "extensions/.dot.ts", "/abs/extensions/a.ts", "Extensions/A.ts"];
const globPatterns = ["*.ts", "extensions/*.ts", "extensions/**", "extensions/**/*.ts", "**/*.ts", "**", "a.ts", "!a.ts", "skills/*", "skills/**/SKILL.md", "*.{ts,md}", "extensions/[ab].ts", "extensions/?.ts", "/abs/**", "#c", "extensions/*", "*/*.ts", "+(a|b).ts", "@(extensions)/*.ts"];
const minimatchCases = globPatterns.flatMap((p) => globPaths.map((t) => [t, p, minimatch(t, p)]));

const ignoreRules = [
	["node_modules"],
	["*.log"],
	["/build"],
	["build/"],
	["docs/*.md"],
	["**/tmp"],
	["a/**/b"],
	["a/**"],
	["a/**/"],
	["*.ts", "!keep.ts"],
	["!keep.ts", "*.ts"],
	["lib", "!lib/x.ts"],
	["[ab].ts"],
	["[!a].ts"],
	["foo?.ts"],
	["\\#hash.ts"],
	["\\!bang.ts"],
	["trailing   "],
	["# comment", "", "real.ts"],
	["SUB/"],
	["x/*"],
	["*"],
	["/*.md"],
	["**"],
	["[a-c]x.ts"],
	["[z-a].ts"],
	["[[:digit:]].ts"],
	["abc[.ts"],
];
const ignorePaths = ["node_modules/", "node_modules/x.ts", "a.log", "dir/a.log", "build/", "build/x.ts", "src/build/", "src/build", "docs/a.md", "docs/sub/a.md", "tmp/", "x/tmp/", "a/b", "a/x/y/b", "a/", "a/z.ts", "keep.ts", "other.ts", "lib/x.ts", "lib/y.ts", "lib/", "a.ts", "b.ts", "c.ts", "foo1.ts", "foo12.ts", "#hash.ts", "!bang.ts", "trailing", "real.ts", "sub/", "Sub/x.ts", "x/y", "x/", "x/y/z", "a.md", "d/a.md", "1.ts", "bx.ts", "abc[.ts", "z.ts"];
const ignoreCases = ignoreRules.map((rules) => {
	const ig = ignore().add(rules);
	return [rules, ignorePaths.map((p) => ig.ignores(p))];
});

const npmSources = ["npm:foo", "npm:foo@1.2.3", "npm:foo@^1.2.0", "npm:@scope/pkg", "npm:@scope/pkg@~2.0", "npm:foo@latest", "npm: spaced@1.0.0 ", "npm:@scope", "npm:a@b@c", "npm:foo@v1.0.0"];
const parseCases = npmSources.map((s) => {
	const pm = new DefaultPackageManager({ cwd: "/", agentDir: "/agent", settingsManager: SettingsManager.inMemory() });
	return [s, pm.parseSource(s)];
});

// resolve(): directory trees and settings, recorded as ResolvedPaths with <TMP> paths.
const resolveCases = [
	{
		name: "conventions and manifests",
		files: {
			"agent/extensions/one.ts": "",
			"agent/extensions/dir/index.ts": "",
			"agent/extensions/dir2/package.json": JSON.stringify({ pi: { extensions: ["./main.js", "./missing.ts"] } }),
			"agent/extensions/dir2/main.js": "",
			"agent/extensions/notes.txt": "",
			"agent/skills/alpha/SKILL.md": "",
			"agent/skills/loose.md": "",
			"agent/skills/nested/deep/SKILL.md": "",
			"agent/skills/nested/deep/more/SKILL.md": "",
			"agent/prompts/p.md": "",
			"agent/prompts/sub/q.md": "",
			"agent/themes/t.json": "",
			"home/.agents/skills/s1/SKILL.md": "",
			"home/.agents/skills/top.md": "",
			"home/.agents/skills/grp/inner.md": "",
			"agent/npm/node_modules/pkg/package.json": JSON.stringify({ name: "pkg", version: "1.0.0", pi: { extensions: ["extensions/*.ts", "!extensions/skip.ts"], skills: ["skills"] } }),
			"agent/npm/node_modules/pkg/extensions/a.ts": "",
			"agent/npm/node_modules/pkg/extensions/skip.ts": "",
			"agent/npm/node_modules/pkg/skills/k/SKILL.md": "",
			"local-pkg/extensions/l.ts": "",
			"local-pkg/prompts/lp.md": "",
			"local-pkg/.gitignore": "ignored.md\n",
			"local-pkg/prompts/ignored.md": "",
			"single.ts": "",
		},
		global: { packages: ["npm:pkg", "../local-pkg", { source: "../single.ts" }], extensions: ["-extensions/one.ts"], skills: ["!**/loose.md"] },
	},
	{
		name: "filters and project",
		trust: true,
		files: {
			"agent/npm/node_modules/flt/package.json": JSON.stringify({ name: "flt", version: "2.0.0" }),
			"agent/npm/node_modules/flt/extensions/x.ts": "",
			"agent/npm/node_modules/flt/extensions/y.ts": "",
			"agent/npm/node_modules/flt/prompts/a.md": "",
			"agent/npm/node_modules/flt/themes/dark.json": "",
			"work/.pi/extensions/pe.ts": "",
			"work/.pi/skills/ps/SKILL.md": "",
			"work/.pi/prompts/pp.md": "",
			"work/.agents/skills/as/SKILL.md": "",
			"work/.git/HEAD": "",
			"extra/e.ts": "",
			"extra/f.ts": "",
		},
		global: { packages: [{ source: "npm:flt", extensions: ["x.ts"], prompts: [], themes: ["+themes/dark.json"] }], extensions: ["../extra", "!**/f.ts"] },
		project: { packages: [{ source: "npm:flt", autoload: false, extensions: ["-extensions/x.ts", "+extensions/y.ts"] }], prompts: ["-prompts/pp.md"] },
	},
	{
		name: "project wins over user",
		trust: true,
		files: {
			"agent/npm/node_modules/dup/package.json": JSON.stringify({ name: "dup", version: "1.0.0" }),
			"agent/npm/node_modules/dup/extensions/u.ts": "",
			"work/.pi/npm/node_modules/dup/package.json": JSON.stringify({ name: "dup", version: "1.0.0" }),
			"work/.pi/npm/node_modules/dup/extensions/p.ts": "",
			"work/.pi/local/index.ts": "",
		},
		global: { packages: ["npm:dup"] },
		project: { packages: ["npm:dup", "./local"] },
	},
	{
		name: "untrusted project",
		files: {
			"work/.pi/extensions/pe.ts": "",
			"work/.agents/skills/as/SKILL.md": "",
			"agent/prompts/z.md": "",
		},
		global: {},
		project: { packages: ["./nothing"] },
	},
];

function writeTree(root, files) {
	for (const [rel, text] of Object.entries(files)) {
		const path = join(root, rel);
		mkdirSync(dirname(path), { recursive: true });
		writeFileSync(path, text);
	}
}

const sub = (s, tmp) => s.split(tmp).join("<TMP>").split(repo).join("<REPO>");

async function runResolve(c) {
	const tmp = realpathSync(mkdtempSync(join(tmpdir(), "ultron-pkg-resolve-")));
	for (const d of ["agent", "work", "home"]) mkdirSync(join(tmp, d), { recursive: true });
	writeTree(tmp, c.files);
	writeFileSync(join(tmp, "agent/settings.json"), JSON.stringify(c.global ?? {}));
	if (c.project) {
		mkdirSync(join(tmp, "work/.pi"), { recursive: true });
		writeFileSync(join(tmp, "work/.pi/settings.json"), JSON.stringify(c.project));
	}
	const previousHome = process.env.HOME;
	process.env.HOME = join(tmp, "home");
	process.env.PI_OFFLINE = "1";
	try {
		const settingsManager = SettingsManager.create(join(tmp, "work"), join(tmp, "agent"), { projectTrusted: Boolean(c.trust) });
		const pm = new DefaultPackageManager({ cwd: join(tmp, "work"), agentDir: join(tmp, "agent"), settingsManager });
		const resolved = await pm.resolve();
		const out = {};
		for (const kind of ["extensions", "skills", "prompts", "themes"]) {
			out[kind] = resolved[kind].map((r) => ({
				path: sub(r.path, tmp),
				enabled: r.enabled,
				source: r.metadata.source,
				scope: r.metadata.scope,
				origin: r.metadata.origin,
				baseDir: r.metadata.baseDir === undefined ? null : sub(r.metadata.baseDir, tmp),
			}));
		}
		return { ...c, resolved: out };
	} finally {
		process.env.HOME = previousHome;
		delete process.env.PI_OFFLINE;
		rmSync(tmp, { recursive: true, force: true });
	}
}

const resolved = [];
for (const c of resolveCases) resolved.push(await runResolve(c));

// --------------------------------------------------------------------------------------------------------- runs

/** The fake npm: logs `cwd: args` to $FAKE_NPM_LOG and installs from the registry file $FAKE_NPM_REGISTRY. */
const FAKE_NPM = `#!/usr/bin/python3
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
    print(os.environ.get("FAKE_NPM_GLOBAL_ROOT", "/nonexistent")); sys.exit(0)
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
if args[:1] == ["uninstall"]:
    root = option("--prefix") or option("--cwd")
    import shutil
    shutil.rmtree(os.path.join(root, "node_modules", args[1]), ignore_errors=True)
    print("removed 1 package"); sys.exit(0)
sys.stderr.write("fake npm: unhandled " + " ".join(args) + "\\n"); sys.exit(2)
`;

const REGISTRY = JSON.stringify({
	foo: { latest: "1.1.0", versions: ["1.0.0", "1.1.0", "2.0.0-beta.1"] },
	"@scope/bar": { latest: "3.0.0", versions: ["1.2.0", "1.4.0", "3.0.0"] },
	ranged: { latest: "2.0.0", versions: ["1.0.0", "1.3.0", "2.0.0"] },
});

// A source repository with commits v1 (tagged) and its bare copy at remotes/test/ext (https://github.com/test/ext).
const MAKE_REMOTE = [
	"mkdir -p src remotes/test",
	"cd src && git init -q -b main && printf '{\"name\":\"ext\",\"version\":\"1.0.0\"}' > package.json && echo '// v1' > index.ts && git add . && git commit -qm v1 && git tag v1",
	"git clone -q --bare src remotes/test/ext",
];
const PUSH_V2 = ["cd src && echo '// v2' > index.ts && git commit -qam v2 && git tag v2 && git push -q ../remotes/test/ext main v2"];
const INSTALLED_CLONE = ["mkdir -p agent/git/github.com/test && git clone -q https://github.com/test/ext agent/git/github.com/test/ext"];

const VERSION = JSON.parse(readFileSync(join(repo, "packages/coding-agent/package.json"), "utf8")).version;
const settings = (value) => ({ "agent/settings.json": JSON.stringify(value) });

const CASES = [
	{ name: "install npm", args: ["install", "npm:foo"] },
	{ name: "install scoped npm with version, project", args: ["install", "npm:@scope/bar@1.4.0", "-l", "--approve"] },
	{ name: "install npm with bun", args: ["install", "npm:foo"], files: settings({ npmCommand: ["bun"] }), bin: ["bun"] },
	{ name: "install npm with pnpm wrapper", args: ["install", "npm:foo"], files: settings({ npmCommand: ["env", "--", "pnpm"] }), bin: ["pnpm"] },
	{ name: "install npm failure", args: ["install", "npm:missing-pkg"] },
	{ name: "install twice keeps one entry", args: ["install", "npm:foo@1.0.0"], files: settings({ packages: ["npm:foo"] }) },
	{ name: "remove npm", args: ["remove", "npm:foo"], files: settings({ packages: ["npm:foo", "npm:other"] }), setup: ["mkdir -p agent/npm/node_modules/foo && echo '{}' > agent/npm/node_modules/foo/package.json"] },
	{ name: "remove unknown npm", args: ["uninstall", "npm:nope"], files: settings({ packages: ["npm:foo"] }) },
	{ name: "update npm newer", args: ["update", "--extensions"], files: settings({ packages: ["npm:foo"] }), setup: ["mkdir -p agent/npm/node_modules/foo && echo '{\"version\":\"1.0.0\"}' > agent/npm/node_modules/foo/package.json"] },
	{ name: "update npm current", args: ["update", "npm:foo"], files: settings({ packages: ["npm:foo"] }), setup: ["mkdir -p agent/npm/node_modules/foo && echo '{\"version\":\"1.1.0\"}' > agent/npm/node_modules/foo/package.json"] },
	{ name: "update npm range", args: ["update", "--extension", "npm:ranged"], files: settings({ packages: ["npm:ranged@^1.0.0"] }), setup: ["mkdir -p agent/npm/node_modules/ranged && echo '{\"version\":\"1.0.0\"}' > agent/npm/node_modules/ranged/package.json"] },
	{ name: "update npm pinned skipped", args: ["update", "--extensions"], files: settings({ packages: ["npm:foo@1.0.0"] }) },
	{ name: "update suggests prefix", args: ["update", "foo"], files: settings({ packages: ["npm:foo"] }) },
	{ name: "update offline does nothing", args: ["update", "--extensions"], env: { PI_OFFLINE: "1" }, files: settings({ packages: ["npm:foo"] }) },
	{ name: "install git", args: ["install", "git:github.com/test/ext"], setup: MAKE_REMOTE, checkouts: ["agent/git/github.com/test/ext"] },
	{ name: "install git pinned", args: ["install", "https://github.com/test/ext@v1"], setup: [...MAKE_REMOTE, ...PUSH_V2], checkouts: ["agent/git/github.com/test/ext"] },
	{ name: "install git missing remote", args: ["install", "git:github.com/test/none"], setup: ["mkdir -p remotes/test"] },
	{ name: "update git", args: ["update", "--extensions"], files: settings({ packages: ["git:github.com/test/ext"] }), setup: [...MAKE_REMOTE, ...INSTALLED_CLONE, ...PUSH_V2], checkouts: ["agent/git/github.com/test/ext"] },
	{ name: "update git current", args: ["update", "git:github.com/test/ext"], files: settings({ packages: ["git:github.com/test/ext"] }), setup: [...MAKE_REMOTE, ...INSTALLED_CLONE], checkouts: ["agent/git/github.com/test/ext"] },
	{ name: "update git pinned", args: ["update", "--extensions"], files: settings({ packages: ["git:github.com/test/ext@v2"] }), setup: [...MAKE_REMOTE, ...INSTALLED_CLONE, ...PUSH_V2], checkouts: ["agent/git/github.com/test/ext"] },
	{ name: "remove git", args: ["remove", "git:github.com/test/ext"], files: settings({ packages: ["git:github.com/test/ext"] }), setup: [...MAKE_REMOTE, ...INSTALLED_CLONE] },
	{ name: "list legacy npm root", args: ["list"], files: { ...settings({ packages: ["npm:legacy", "npm:foo", { source: "git:github.com/test/ext" }] }), "global/legacy/package.json": "{}" }, env: { FAKE_NPM_GLOBAL_ROOT: "<TMP>/global" } },
	{ name: "install local missing", args: ["install", "./nope"] },
	{ name: "update self offline", args: ["update", "--self"], env: { PI_OFFLINE: "1" } },
	{ name: "update default offline", args: ["update"], env: { PI_OFFLINE: "1" } },
	// The registry answers through offline-guard.mjs's fetch stubs; the Rust test serves the same answers locally.
	{ name: "update self current", args: ["update", "--self"], stubs: [{ status: 200, body: { version: VERSION } }] },
	{ name: "update self retries", args: ["update", "self"], stubs: [{ status: 503, body: "busy" }, { status: 502, body: "busy" }, { status: 200, body: { version: VERSION } }] },
	{ name: "update self error status", args: ["update", "--self"], stubs: [{ status: 500, body: "down" }] },
	{ name: "update self bad version", args: ["update", "--self"], stubs: [{ status: 200, body: { version: "not-a-version" } }] },
	{ name: "update all current", args: ["update", "--all"], files: settings({ packages: ["npm:foo@1.0.0"] }), stubs: [{ status: 200, body: { version: VERSION } }] },
];

const node = process.execPath;
const GIT_ENV = {
	GIT_AUTHOR_NAME: "Test",
	GIT_AUTHOR_EMAIL: "test@example.com",
	GIT_COMMITTER_NAME: "Test",
	GIT_COMMITTER_EMAIL: "test@example.com",
	GIT_AUTHOR_DATE: "2026-01-01T00:00:00Z",
	GIT_COMMITTER_DATE: "2026-01-01T00:00:00Z",
	GIT_CONFIG_NOSYSTEM: "1",
	GIT_TERMINAL_PROMPT: "0",
};

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
				out.push({ path: p, kind: "dir" });
				walk(p);
			} else out.push({ path: p, kind: "file", text: sub(readFileSync(join(root, p), "utf8"), tmp) });
		}
	};
	walk("");
	return out;
}

const runs = CASES.map((c) => {
	const tmp = realpathSync(mkdtempSync(join(tmpdir(), "ultron-pkg-fixture-")));
	for (const d of ["work", "agent", "server", "bin"]) mkdirSync(join(tmp, d), { recursive: true });
	writeFileSync(join(tmp, "bin/npm"), FAKE_NPM, { mode: 0o755 });
	for (const name of c.bin ?? []) symlinkSync(join(tmp, "bin/npm"), join(tmp, "bin", name));
	writeFileSync(join(tmp, "registry.json"), REGISTRY);
	writeFileSync(join(tmp, "gitconfig"), `[url "file://${tmp}/remotes/"]\n\tinsteadOf = https://github.com/\n[advice]\n\tdetachedHead = false\n[init]\n\tdefaultBranch = main\n`);
	writeTree(tmp, Object.fromEntries(Object.entries(c.files ?? {}).map(([k, v]) => [k, v.split("<TMP>").join(tmp)])));
	const env = {
		PATH: `${join(tmp, "bin")}:/usr/bin:/bin`,
		HOME: tmp,
		TZ: "UTC",
		ULTRON_CODING_AGENT_DIR: join(tmp, "agent"),
		ULTRON_SERVER_DIR: join(tmp, "server"),
		GIT_CONFIG_GLOBAL: join(tmp, "gitconfig"),
		FAKE_NPM_LOG: join(tmp, "npm.log"),
		FAKE_NPM_REGISTRY: join(tmp, "registry.json"),
		...GIT_ENV,
		...Object.fromEntries(Object.entries(c.env ?? {}).map(([k, v]) => [k, v.split("<TMP>").join(tmp)])),
	};
	for (const command of c.setup ?? []) execFileSync("/bin/sh", ["-c", command], { cwd: tmp, env, stdio: "ignore" });
	writeFileSync(join(tmp, "npm.log"), "");
	if (c.stubs) {
		writeFileSync(join(tmp, "stubs.json"), JSON.stringify({ "GET https://registry.npmjs.org/ultron-agent/latest": c.stubs }));
		env.ULTRON_FETCH_STUBS = join(tmp, "stubs.json");
		env.ULTRON_FETCH_LOG = join(tmp, "fetch.log");
	}
	const out = openSync(join(tmp, "stdout"), "w");
	const err = openSync(join(tmp, "stderr"), "w");
	const r = spawnSync(node, [join(src, "cli.ts"), ...c.args], {
		cwd: join(tmp, "work"),
		env: { ...env, NODE_OPTIONS: guardNodeOptions, ULTRON_GUARD_LOG: guardLog },
		stdio: ["ignore", out, err],
		timeout: 120_000,
	});
	closeSync(out);
	closeSync(err);
	const rec = {
		...c,
		stdout: sub(readFileSync(join(tmp, "stdout"), "utf8"), tmp),
		stderr: sub(readFileSync(join(tmp, "stderr"), "utf8"), tmp),
		status: r.status,
		npmLog: sub(readFileSync(join(tmp, "npm.log"), "utf8"), tmp),
		agent: tree(tmp, "agent"),
		project: tree(tmp, "work/.pi"),
		requests: c.stubs ? readFileSync(join(tmp, "fetch.log"), "utf8").split("\n").filter(Boolean).length : 0,
		heads: (c.checkouts ?? []).map((rel) => (existsSync(join(tmp, rel)) ? execFileSync("git", ["rev-parse", "HEAD"], { cwd: join(tmp, rel), env, encoding: "utf8" }).trim() : null)),
	};
	rmSync(tmp, { recursive: true, force: true });
	return rec;
});

assertOffline(guardLog);
const fixture = {
	units: {
		git: gitSources.map((s) => [s, parseGitUrl(s) ?? null]),
		npm: parseCases,
		semver: semverCases,
		minimatch: minimatchCases,
		ignore: { paths: ignorePaths, cases: ignoreCases },
		resolve: resolved,
	},
	fakeNpm: FAKE_NPM,
	registry: REGISTRY,
	gitEnv: GIT_ENV,
	runs,
};
writeFileSync(join(here, "packages.json"), `${JSON.stringify(fixture, null, 1)}\n`);
