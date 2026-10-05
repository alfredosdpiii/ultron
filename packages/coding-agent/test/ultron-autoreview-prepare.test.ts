/**
 * Prepared test environments for `ultron autoreview` (`autoreview/prepare.ts`): ecosystem detection, the toolchain
 * the repository asks for, mise resolution and installation through a fake mise on PATH, a real `uv venv` on a
 * dependency-free project, `npm ci` on a dependency-free lockfile, the cache keyed by the lockfile hash, and the
 * sandbox side: a mise install directory bound read-only with nothing else of the mise tree visible.
 */
import { execFileSync } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, test } from "vitest";
import {
	cachedEnvironment,
	describeToolchain,
	detectEcosystems,
	environmentHash,
	findMise,
	prepareEnvironment,
	resolveToolchain,
	toolchainRequests,
} from "../src/ultron/autoreview/prepare.ts";
import type { Runner } from "../src/ultron/autoreview/runner.ts";
import { runProcess } from "../src/ultron/autoreview/runner.ts";

const PYTHON =
	process.env.ULTRON_PYTHON ??
	(process.platform === "linux" && existsSync("/usr/bin/python3") ? "/usr/bin/python3" : "python3");
const here = dirname(fileURLToPath(import.meta.url));
const RLM_DIR = resolve(here, "../src/ultron/rlm");
const UV = [join(process.env.HOME ?? "", ".local/bin/uv"), "/usr/bin/uv", "/usr/local/bin/uv"].find((path) =>
	existsSync(path),
);

const dirs: string[] = [];
afterEach(() => {
	for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

function tempDir(prefix: string): string {
	const dir = mkdtempSync(join(tmpdir(), prefix));
	dirs.push(dir);
	return dir;
}

/**
 * A fake mise: a shell script on PATH that answers `ls --json` from a JSON file, `which <tool>` from it, and
 * `install <tool>@<version>` by adding the version to that file and creating its install directory. Every call is
 * appended to a log.
 */
function fakeMise(
	root: string,
	installed: Record<string, Array<{ version: string }>>,
): { bin: string; log: string; installs: string } {
	const bin = join(root, "bin");
	const installs = join(root, "installs");
	const state = join(root, "state.json");
	const log = join(root, "calls.log");
	mkdirSync(bin, { recursive: true });
	const listing: Record<string, Array<{ version: string; install_path: string; installed: boolean }>> = {};
	for (const [tool, versions] of Object.entries(installed)) {
		listing[tool] = versions.map(({ version }) => {
			const path = join(installs, tool, version);
			mkdirSync(join(path, "bin"), { recursive: true });
			mkdirSync(join(path, "lib"), { recursive: true });
			writeFileSync(join(path, "bin", tool === "python" ? "python3" : "node"), "#!/bin/sh\necho fake\n", {
				mode: 0o755,
			});
			return { version, install_path: path, installed: true };
		});
	}
	writeFileSync(state, JSON.stringify(listing));
	writeFileSync(
		join(bin, "mise"),
		`#!/bin/sh
echo "$@" >> ${JSON.stringify(log)}
case "$1" in
  ls) cat ${JSON.stringify(state)} ;;
  which) echo "${installs}/$2/bin/$2" ;;
  install)
    tool=$(echo "$2" | cut -d@ -f1); version=$(echo "$2" | cut -d@ -f2)
    mkdir -p "${installs}/$tool/$version.0/bin" "${installs}/$tool/$version.0/lib"
    printf '#!/bin/sh\\necho fake\\n' > "${installs}/$tool/$version.0/bin/$([ "$tool" = python ] && echo python3 || echo node)"
    chmod 755 "${installs}/$tool/$version.0/bin/"*
    ${PYTHON} - "$tool" "$version.0" "${installs}/$tool/$version.0" ${JSON.stringify(state)} <<'EOF'
import json, sys
tool, version, path, state = sys.argv[1:5]
data = json.load(open(state))
data.setdefault(tool, []).append({"version": version, "install_path": path, "installed": True})
json.dump(data, open(state, "w"))
EOF
    ;;
  *) echo "unknown" >&2; exit 2 ;;
esac
`,
	);
	chmodSync(join(bin, "mise"), 0o755);
	return { bin, log, installs };
}

describe("prepared environments: detection and toolchain requests", () => {
	test("ecosystems, lockfiles and the toolchain the repository asks for are read from its files", () => {
		const repo = tempDir("ultron-prepare-detect-");
		writeFileSync(join(repo, "pyproject.toml"), '[project]\nname = "x"\nrequires-python = ">=3.11,<4"\n');
		writeFileSync(join(repo, "uv.lock"), "version = 1\n");
		mkdirSync(join(repo, "requirements"));
		writeFileSync(join(repo, "requirements/dev.txt"), "pytest\n");
		writeFileSync(join(repo, "package.json"), JSON.stringify({ name: "web", engines: { node: ">=20" } }));
		writeFileSync(join(repo, "pnpm-lock.yaml"), "lockfileVersion: 9\n");
		writeFileSync(join(repo, ".nvmrc"), "v22.13\n");
		expect(detectEcosystems(repo)).toEqual({
			python: ["pyproject.toml", "uv.lock", "requirements/dev.txt"],
			node: ["package.json", "pnpm-lock.yaml"],
			tools: [".nvmrc"],
		});
		// .nvmrc wins over engines.node; requires-python gives the lowest minor.
		expect(toolchainRequests(repo)).toEqual({ python: "3.11", node: "22.13" });
		writeFileSync(join(repo, "mise.toml"), '[tools]\npython = "3.12"\nnode = "20"\n\n[env]\nX = "1"\n');
		expect(toolchainRequests(repo)).toEqual({ python: "3.12", node: "20" });
		rmSync(join(repo, "mise.toml"));
		writeFileSync(join(repo, ".tool-versions"), "python 3.10.2\nnodejs 18.0.0\n");
		expect(toolchainRequests(repo)).toEqual({ python: "3.10.2", node: "18.0.0" });
		// The hash follows the manifests, lockfiles and toolchain files, and the explicit Python.
		const first = environmentHash(repo);
		writeFileSync(join(repo, "uv.lock"), "version = 2\n");
		expect(environmentHash(repo)).not.toBe(first);
		expect(environmentHash(repo, "3.13")).not.toBe(environmentHash(repo));
		expect(environmentHash(repo)).toBe(environmentHash(repo));
	});

	test("mise: the installed version matching the request is chosen (newest), a missing one is installed during prepare only, and the exact install directory is what gets bound", async () => {
		const root = tempDir("ultron-prepare-mise-");
		const mise = fakeMise(root, {
			python: [{ version: "3.12.4" }, { version: "3.12.9" }, { version: "3.11.1" }],
			node: [],
		});
		const env = { ...process.env, PATH: `${mise.bin}:/usr/bin:/bin` };
		expect(findMise(env, root, undefined)).toBe(join(mise.bin, "mise"));
		expect(findMise(env, root, false)).toBeUndefined();
		const calls: string[][] = [];
		const runner: Runner = (argv, options) => {
			calls.push([...argv]);
			return runProcess(argv, options);
		};
		const out = await resolveToolchain(
			{ runner, cacheDir: root, env, home: root },
			{ python: "3.12", node: "20" },
			{ python: true, node: true },
		);
		expect(out.failures).toEqual([]);
		const python = out.entries.find((entry) => entry.tool === "python")!;
		expect(python).toMatchObject({
			version: "3.12.9",
			source: "mise",
			path: join(mise.installs, "python", "3.12.9"),
		});
		// node 20 was not installed: mise install ran, and the new directory is the one bound.
		const node = out.entries.find((entry) => entry.tool === "node")!;
		expect(node).toMatchObject({
			version: "20.0",
			source: "mise",
			installed: true,
			path: join(mise.installs, "node", "20.0"),
		});
		expect(readFileSync(mise.log, "utf8").trim().split("\n")).toEqual(["ls --json", "install node@20", "ls --json"]);
		expect(calls.filter((argv) => argv[1] === "install")).toHaveLength(1);
		expect(describeToolchain(out.entries)).toEqual([
			`python 3.12.9 from mise: ${join(mise.installs, "python", "3.12.9")}`,
			`node 20.0 from mise (installed by prepare): ${join(mise.installs, "node", "20.0")}`,
		]);
	});
});

describe("prepared environments: building and caching", () => {
	test("a dependency-free Python project gets a real uv virtualenv; the second prepare is a cache hit; a lockfile change rebuilds", async (context) => {
		if (UV === undefined) {
			context.skip();
			return;
		}
		const repo = tempDir("ultron-prepare-py-");
		const cache = tempDir("ultron-prepare-cache-");
		writeFileSync(join(repo, "requirements.txt"), "# nothing to install\n");
		const env = { ...process.env, PATH: `${dirname(UV)}:${process.env.PATH ?? ""}` };
		const first = await prepareEnvironment(repo, {
			runner: runProcess,
			cacheDir: cache,
			env,
			mise: false,
			timeoutMs: 300_000,
		});
		expect(first.failures).toEqual([]);
		expect(first.cached).toBe(false);
		expect(first.prepared).toEqual(["python: uv pip install -r requirements.txt"]);
		expect(existsSync(join(first.dir, ".venv", "bin", "python"))).toBe(true);
		expect(first.dir).toBe(join(cache, "envs", first.hash));
		const record = JSON.parse(readFileSync(join(first.dir, "toolchain.json"), "utf8")) as { ok: boolean };
		expect(record.ok).toBe(true);
		// Cached: nothing runs again.
		const calls: string[][] = [];
		const counting: Runner = (argv, options) => {
			calls.push([...argv]);
			return runProcess(argv, options);
		};
		const second = await prepareEnvironment(repo, { runner: counting, cacheDir: cache, env, mise: false });
		expect(second.cached).toBe(true);
		expect(second.hash).toBe(first.hash);
		expect(calls).toEqual([]);
		expect(cachedEnvironment(repo, cache)?.dir).toBe(first.dir);
		// The lockfile changed: a new hash, a new build.
		writeFileSync(join(repo, "requirements.txt"), "# changed\n");
		expect(cachedEnvironment(repo, cache)).toBeUndefined();
		const third = await prepareEnvironment(repo, {
			runner: counting,
			cacheDir: cache,
			env,
			mise: false,
			timeoutMs: 300_000,
		});
		expect(third.hash).not.toBe(first.hash);
		expect(third.cached).toBe(false);
		expect(calls.some((argv) => argv[1] === "venv")).toBe(true);
	}, 300_000);

	test("a dependency-free Node project gets node_modules from npm ci --ignore-scripts; a project without a lockfile is a stated failure", async () => {
		const repo = tempDir("ultron-prepare-node-");
		const cache = tempDir("ultron-prepare-cache-");
		writeFileSync(join(repo, "package.json"), JSON.stringify({ name: "web", version: "1.0.0", private: true }));
		writeFileSync(
			join(repo, "package-lock.json"),
			JSON.stringify({
				name: "web",
				version: "1.0.0",
				lockfileVersion: 3,
				requires: true,
				packages: { "": { name: "web", version: "1.0.0" } },
			}),
		);
		const out = await prepareEnvironment(repo, {
			runner: runProcess,
			cacheDir: cache,
			mise: false,
			timeoutMs: 300_000,
		});
		expect(out.failures).toEqual([]);
		expect(out.prepared).toEqual(["node: npm ci --ignore-scripts --no-audit --no-fund (package-lock.json)"]);
		// npm ci on a lockfile with no dependencies installs nothing, so node_modules may not exist; the record does.
		expect(existsSync(join(out.dir, "package-lock.json"))).toBe(true);
		expect((JSON.parse(readFileSync(join(out.dir, "toolchain.json"), "utf8")) as { ok: boolean }).ok).toBe(true);
		// The node on PATH is recorded with where it came from.
		const node = out.toolchain.find((entry) => entry.tool === "node");
		expect(node === undefined || ["system", "mise"].includes(node.source)).toBe(true);
		rmSync(join(repo, "package-lock.json"));
		const missing = await prepareEnvironment(repo, { runner: runProcess, cacheDir: cache, mise: false });
		expect(missing.prepared).toEqual([]);
		expect(missing.failures).toEqual([
			"no Node lockfile (package-lock.json, pnpm-lock.yaml, yarn.lock): node_modules was not prepared",
		]);
		expect(cachedEnvironment(repo, cache)).toBeUndefined();
	}, 300_000);
});

describe("prepared environments: the sandbox side", () => {
	test("a toolchain directory is bound read-only at its own path with its bin first on PATH, and nothing else of the mise tree is visible", (context) => {
		const root = tempDir("ultron-prepare-sandbox-");
		const chosen = join(root, "installs", "python", "3.99.0");
		const sibling = join(root, "installs", "python", "3.98.0");
		mkdirSync(join(chosen, "bin"), { recursive: true });
		mkdirSync(join(sibling, "bin"), { recursive: true });
		writeFileSync(join(chosen, "bin", "marker"), "#!/bin/sh\necho chosen-marker\n", { mode: 0o755 });
		writeFileSync(join(sibling, "bin", "marker"), "#!/bin/sh\necho sibling-marker\n", { mode: 0o755 });
		const out = JSON.parse(
			execFileSync(
				PYTHON,
				[
					"-c",
					`
import sys, json, os, tempfile
sys.path.insert(0, ${JSON.stringify(RLM_DIR)})
import autoreview_tests as t
sandbox = t.detect_sandbox()
if sandbox is None or sandbox.mechanism == "unshare":
    print(json.dumps({"skip": True})); raise SystemExit(0)
work = tempfile.mkdtemp()
env = t.sandbox_env(${JSON.stringify(join(chosen, "bin"))})
script = "marker; ls ${root}/installs/python 2>&1; ls ${root}/installs 2>&1; touch ${chosen}/bin/x 2>&1 && echo WRITABLE || echo read-only; echo PATH=$PATH"
code, output = t.run_process(sandbox.wrap(["sh", "-c", script], work, env, [${JSON.stringify(chosen)}]), work, t._launch_env(), 60)
print(json.dumps({"skip": False, "code": code, "output": output}))
`,
				],
				{ encoding: "utf8", env: { ...process.env, PYTHONDONTWRITEBYTECODE: "1" } },
			)
				.trim()
				.split("\n")
				.at(-1)!,
		) as { skip: boolean; code: number; output: string };
		if (out.skip) {
			context.skip();
			return;
		}
		// The chosen tool runs from its bin (first on PATH); its sibling version and the rest of the tree are not there.
		expect(out.output).toContain("chosen-marker");
		expect(out.output).not.toContain("sibling-marker");
		expect(out.output).not.toContain("3.98.0");
		expect(out.output).toContain("read-only");
		expect(out.output).not.toContain("WRITABLE");
		expect(out.output).toContain(`PATH=${join(chosen, "bin")}:/usr/local/bin:/usr/bin:/bin`);
		// Only the chosen directory appears under its parent inside the sandbox.
		const listed = out.output
			.split("\n")
			.filter((line) => line.trim() === "3.99.0" || line.trim() === "3.98.0" || line.trim() === "python");
		expect(listed).toContain("3.99.0");
		expect(listed).not.toContain("3.98.0");
	});
});
