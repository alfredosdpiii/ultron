/**
 * Prepared test environments for `ultron autoreview`: dependencies installed *before* a review, with the network,
 * into a cache the sandbox later binds read-only. Nothing is ever installed during a review.
 *
 * - Python projects: a `.venv` built with uv (`uv venv`, then `uv sync --frozen` for a uv.lock, `uv pip install -e
 *   .[dev,test]` for a pyproject, `uv pip install -r` for requirements files); uv reads PEP 621 and poetry metadata.
 * - Node projects: `npm ci --ignore-scripts` (pnpm or yarn by lockfile) into the cache from a copy of the manifest
 *   and lockfile.
 * - Toolchains via mise: the versions the repository asks for (`mise.toml`, `.tool-versions`, `.python-version`,
 *   `.nvmrc`, `.node-version`, `engines.node`, `requires-python`) are resolved with `mise ls --json`, installed with
 *   `mise install <tool>@<version>` when missing (prepare only), and their exact install directories are what the
 *   sandbox binds read-only, with their `bin` first on its PATH; never the whole mise tree, never shims.
 *
 * The result is cached per repository and lockfile hash under `<cache>/envs/<hash>/` with a `toolchain.json` that
 * records what was prepared and where each tool came from (mise, system, uv). Failures are a limit the review
 * states, never a finding.
 */
import { createHash } from "node:crypto";
import {
	copyFileSync,
	type Dirent,
	existsSync,
	mkdirSync,
	readdirSync,
	readFileSync,
	realpathSync,
	rmSync,
	writeFileSync,
} from "node:fs";
import { homedir } from "node:os";
import { basename, dirname, join } from "node:path";
import type { Runner } from "./runner.ts";

export interface ToolchainEntry {
	readonly tool: "python" | "node";
	readonly version: string;
	/** The install prefix (`<prefix>/bin/<tool>`), bound read-only into the sandbox. */
	readonly path: string;
	readonly source: "mise" | "system" | "uv";
	/** True when `mise install` ran for it during this prepare. */
	readonly installed?: boolean;
}

export interface PreparedEnvironment {
	/** The environment directory: holds `.venv` and/or `node_modules`, bound like a checkout's. */
	readonly dir: string;
	readonly hash: string;
	/** What was built in this run, or found cached. */
	readonly prepared: readonly string[];
	readonly cached: boolean;
	readonly toolchain: readonly ToolchainEntry[];
	readonly failures: readonly string[];
	readonly ms: number;
}

export interface PrepareOptions {
	readonly runner: Runner;
	/** `<cache>/envs` lives under this directory (`autoreviewPaths().cache`). */
	readonly cacheDir: string;
	/** `--python 3.x`: the Python version to build the virtualenv with (overrides the repository's). */
	readonly python?: string;
	/** Use mise to resolve and install toolchains (default: when a mise binary is found). */
	readonly mise?: boolean | string;
	readonly env?: NodeJS.ProcessEnv;
	readonly home?: string;
	readonly log?: (line: string) => void;
	readonly timeoutMs?: number;
	/** Rebuild even when the cache has this hash. */
	readonly force?: boolean;
}

const MANIFESTS = {
	python: [
		"pyproject.toml",
		"setup.py",
		"setup.cfg",
		"requirements.txt",
		"requirements-dev.txt",
		"dev-requirements.txt",
	],
	pythonLocks: ["uv.lock", "poetry.lock", "requirements.txt", "requirements-dev.txt", "dev-requirements.txt"],
	node: ["package.json"],
	nodeLocks: ["package-lock.json", "pnpm-lock.yaml", "yarn.lock", "npm-shrinkwrap.json"],
	tools: ["mise.toml", ".mise.toml", ".tool-versions", ".python-version", ".nvmrc", ".node-version"],
} as const;

/** Directories the walk for nested requirements files and pyprojects does not enter (hidden ones neither). */
const NOT_A_SERVICE_DIR = new Set([
	"node_modules",
	"venv",
	"dist",
	"build",
	"vendor",
	"docs",
	"mockups",
	"requirements",
	"site-packages",
	"__pycache__",
	"target",
	"coverage",
]);
/** How many directory levels below the root the walk goes. */
const NESTED_DEPTH = 4;

function read(path: string): string | undefined {
	try {
		return readFileSync(path, "utf8");
	} catch {
		return undefined;
	}
}

/** Which ecosystems a repository has, and the files that define them. */
export function detectEcosystems(repoDir: string): { python: string[]; node: string[]; tools: string[] } {
	const present = (names: readonly string[]) => names.filter((name) => existsSync(join(repoDir, name)));
	const requirementsDir = join(repoDir, "requirements");
	const extra = existsSync(requirementsDir)
		? readdirSync(requirementsDir)
				.filter((name) => name.endsWith(".txt"))
				.map((name) => join("requirements", name))
		: [];
	// Services kept below the root (backend/, services/api/, ...) with their own requirements files or pyprojects: a
	// root pyproject that only names the tooling is not where the dependencies are. Bounded walk, build and vendor
	// directories left out; everything found is installed into the one environment and hashed into its key.
	const nested: string[] = [];
	const walk = (dir: string, depth: number): void => {
		let entries: Dirent[];
		try {
			entries = readdirSync(join(repoDir, dir), { withFileTypes: true });
		} catch {
			return;
		}
		for (const entry of entries) {
			const relative = dir === "" ? entry.name : join(dir, entry.name);
			if (entry.isDirectory()) {
				if (depth < NESTED_DEPTH && !entry.name.startsWith(".") && !NOT_A_SERVICE_DIR.has(entry.name))
					walk(relative, depth + 1);
			} else if (
				dir !== "" &&
				(/^requirements.*\.txt$/.test(entry.name) || entry.name === "pyproject.toml" || entry.name === "setup.py")
			)
				nested.push(relative);
		}
	};
	walk("", 0);
	nested.sort();
	return {
		python: [...present([...MANIFESTS.python, ...MANIFESTS.pythonLocks]), ...extra, ...nested].filter(
			(item, index, all) => all.indexOf(item) === index,
		),
		node: present([...MANIFESTS.node, ...MANIFESTS.nodeLocks]),
		tools: present(MANIFESTS.tools),
	};
}

/** What the repository asks for: a Python version constraint and a Node version constraint, if any. */
export function toolchainRequests(repoDir: string): { python?: string; node?: string } {
	const out: { python?: string; node?: string } = {};
	const toml = read(join(repoDir, "mise.toml")) ?? read(join(repoDir, ".mise.toml"));
	if (toml) {
		const tools = /\[tools\]([\s\S]*?)(?:\n\[|$)/.exec(toml)?.[1] ?? "";
		const python = /^\s*python\s*=\s*["']([^"']+)["']/m.exec(tools)?.[1];
		const node = /^\s*node(?:js)?\s*=\s*["']([^"']+)["']/m.exec(tools)?.[1];
		if (python) out.python = python;
		if (node) out.node = node;
	}
	const versions = read(join(repoDir, ".tool-versions"));
	if (versions)
		for (const line of versions.split("\n")) {
			const [tool, version] = line.trim().split(/\s+/);
			if (tool === "python" && version && !out.python) out.python = version;
			if ((tool === "node" || tool === "nodejs") && version && !out.node) out.node = version;
		}
	const pythonVersion = read(join(repoDir, ".python-version"))?.trim().split("\n")[0]?.trim();
	if (pythonVersion && !out.python) out.python = pythonVersion;
	const nodeVersion = (read(join(repoDir, ".nvmrc")) ?? read(join(repoDir, ".node-version")))
		?.trim()
		.replace(/^v/, "");
	if (nodeVersion && !out.node) out.node = nodeVersion;
	const pyproject = read(join(repoDir, "pyproject.toml"));
	const requires = pyproject && /^\s*requires-python\s*=\s*["']([^"']+)["']/m.exec(pyproject)?.[1];
	if (requires && !out.python) {
		// ">=3.11" or ">=3.11,<4": the lowest named minor is what the environment is built with.
		const minimum = /(\d+)\.(\d+)/.exec(requires);
		if (minimum) out.python = `${minimum[1]}.${minimum[2]}`;
	}
	const packageJson = read(join(repoDir, "package.json"));
	if (packageJson && !out.node)
		try {
			const engines = (JSON.parse(packageJson) as { engines?: { node?: string } }).engines?.node;
			const major = engines && /(\d+)/.exec(engines)?.[1];
			if (major) out.node = major;
		} catch {
			// Not JSON: no engines constraint.
		}
	return out;
}

/** The content hash that names the cache entry: manifests, lockfiles, toolchain files and the explicit Python. */
export function environmentHash(repoDir: string, python?: string): string {
	const found = detectEcosystems(repoDir);
	const hash = createHash("sha256");
	for (const name of [...found.python, ...found.node, ...found.tools].sort()) {
		hash.update(`${name}\n`);
		hash.update(read(join(repoDir, name)) ?? "");
		hash.update("\n");
	}
	hash.update(`python=${python ?? ""}\n`);
	return hash.digest("hex").slice(0, 16);
}

/** `mise ls --json`: installed versions per tool. */
async function miseInstalled(
	runner: Runner,
	mise: string,
	env: NodeJS.ProcessEnv,
): Promise<Record<string, Array<{ version: string; install_path: string; installed?: boolean }>>> {
	const result = await runner([mise, "ls", "--json"], { env: env as Record<string, string>, timeoutMs: 60_000 });
	if (result.code !== 0) return {};
	try {
		const parsed = JSON.parse(result.stdout) as Record<
			string,
			Array<{ version: string; install_path: string; installed?: boolean }>
		>;
		return typeof parsed === "object" && parsed !== null ? parsed : {};
	} catch {
		return {};
	}
}

/** The installed version satisfying `request` (a version or prefix such as "3.12" or "20"), the newest. */
function pick(
	entries: Array<{ version: string; install_path: string; installed?: boolean }> | undefined,
	request: string,
): { version: string; install_path: string } | undefined {
	const wanted = request.replace(/^[~^>=<v\s]+/, "").split(/[,\s<]/)[0] ?? request;
	const candidates = (entries ?? []).filter(
		(entry) =>
			entry.installed !== false &&
			(entry.version === wanted || entry.version.startsWith(`${wanted}.`)) &&
			existsSync(entry.install_path),
	);
	candidates.sort((a, b) => b.version.localeCompare(a.version, undefined, { numeric: true, sensitivity: "base" }));
	return candidates[0];
}

/** The mise binary to use: `options.mise` as a path, else the first `mise` on PATH or under ~/.local/bin. */
export function findMise(
	env: NodeJS.ProcessEnv,
	home: string,
	setting: boolean | string | undefined,
): string | undefined {
	if (setting === false) return undefined;
	if (typeof setting === "string" && setting.trim() && setting !== "true") return setting;
	const candidates = [
		...(env.PATH ?? "").split(":").map((dir) => join(dir, "mise")),
		join(home, ".local", "bin", "mise"),
		"/usr/bin/mise",
		"/usr/local/bin/mise",
	];
	return candidates.find((path) => path !== "mise" && existsSync(path));
}

/** The install prefix of a binary on PATH that is not a system one (a mise shim resolves to its install dir). */
function systemOrManaged(
	binary: string,
	env: NodeJS.ProcessEnv,
): { path: string; source: "system" | "mise" } | undefined {
	for (const dir of (env.PATH ?? "").split(":")) {
		const candidate = join(dir, binary);
		if (!existsSync(candidate)) continue;
		let real: string;
		try {
			real = realpathSync(candidate);
		} catch {
			continue;
		}
		const prefix = dirname(dirname(real));
		if (/^\/(usr|bin|sbin|lib)(\/|$)/.test(real)) return { path: prefix, source: "system" };
		if (basename(dirname(real)) === "bin" && existsSync(join(prefix, "lib"))) return { path: prefix, source: "mise" };
	}
	return undefined;
}

/**
 * Resolve (and, when missing, install with mise) the toolchain the repository asks for. Returns the entries and the
 * failures; an entry's `path` is the exact install directory the sandbox binds.
 */
export async function resolveToolchain(
	options: PrepareOptions,
	requests: { python?: string; node?: string },
	needs: { python: boolean; node: boolean },
): Promise<{ entries: ToolchainEntry[]; failures: string[] }> {
	const env = options.env ?? process.env;
	const home = options.home ?? homedir();
	const entries: ToolchainEntry[] = [];
	const failures: string[] = [];
	const mise = findMise(env, home, options.mise);
	let installed = mise ? await miseInstalled(options.runner, mise, env) : {};
	for (const tool of ["python", "node"] as const) {
		if (!needs[tool]) continue;
		const request = requests[tool];
		if (mise && request) {
			let chosen = pick(installed[tool], request);
			let wasInstalled = false;
			if (!chosen) {
				options.log?.(`prepare: mise install ${tool}@${request}`);
				const result = await options.runner([mise, "install", `${tool}@${request}`], {
					env: env as Record<string, string>,
					timeoutMs: options.timeoutMs ?? 600_000,
				});
				if (result.code !== 0) {
					failures.push(
						`mise install ${tool}@${request} failed: ${(result.stderr || result.stdout).trim().slice(0, 200)}`,
					);
				} else {
					wasInstalled = true;
					installed = await miseInstalled(options.runner, mise, env);
					chosen = pick(installed[tool], request);
				}
			}
			if (chosen) {
				let path = chosen.install_path;
				try {
					path = realpathSync(path);
				} catch {
					// Keep the listed path.
				}
				entries.push({
					tool,
					version: chosen.version,
					path,
					source: "mise",
					...(wasInstalled ? { installed: true } : {}),
				});
				continue;
			}
		}
		const found = systemOrManaged(tool === "python" ? "python3" : "node", env);
		if (found) {
			const version = request ?? "";
			entries.push({ tool, version, path: found.path, source: found.source });
		} else if (needs[tool])
			failures.push(`no ${tool} found on PATH${request ? ` for the requested ${request}` : ""}`);
	}
	return { entries, failures };
}

function binaryOf(entry: ToolchainEntry | undefined, name: string): string | undefined {
	if (!entry) return undefined;
	const path = join(entry.path, "bin", name);
	return existsSync(path) ? path : undefined;
}

/**
 * Prepare the environment of `repoDir`: toolchains, then the Python virtualenv and/or the Node modules, into
 * `<cacheDir>/envs/<hash>/`. Cached when `toolchain.json` of that hash says it succeeded.
 */
export async function prepareEnvironment(repoDir: string, options: PrepareOptions): Promise<PreparedEnvironment> {
	const started = Date.now();
	const env = options.env ?? process.env;
	const found = detectEcosystems(repoDir);
	const needs = { python: found.python.length > 0, node: found.node.length > 0 };
	const hash = environmentHash(repoDir, options.python);
	const dir = join(options.cacheDir, "envs", hash);
	const recordPath = join(dir, "toolchain.json");
	if (!options.force && existsSync(recordPath)) {
		try {
			const record = JSON.parse(readFileSync(recordPath, "utf8")) as PreparedEnvironment & { ok?: boolean };
			if (record.ok) return { ...record, dir, hash, cached: true, ms: Date.now() - started };
		} catch {
			// Rebuild.
		}
	}
	rmSync(dir, { recursive: true, force: true });
	mkdirSync(dir, { recursive: true, mode: 0o755 });
	const prepared: string[] = [];
	const failures: string[] = [];
	const requests = { ...toolchainRequests(repoDir), ...(options.python ? { python: options.python } : {}) };
	const toolchain = await resolveToolchain(options, requests, needs);
	failures.push(...toolchain.failures);
	const python = toolchain.entries.find((entry) => entry.tool === "python");
	const timeoutMs = options.timeoutMs ?? 900_000;
	// The toolchain's bin directories first on PATH for the build tools below.
	const path = [
		...toolchain.entries.map((entry) => join(entry.path, "bin")),
		join(options.home ?? homedir(), ".local", "bin"),
		env.PATH ?? "",
	].join(":");
	const runEnv: Record<string, string> = { PATH: path, UV_NO_PROGRESS: "1", npm_config_update_notifier: "false" };
	const run = (argv: string[], cwd: string, extra: Record<string, string> = {}) =>
		options.runner(argv, { cwd, env: { ...runEnv, ...extra }, timeoutMs });
	const uv = (env.PATH ?? "")
		.split(":")
		.concat(join(options.home ?? homedir(), ".local", "bin"))
		.map((item) => join(item, "uv"))
		.find((candidate) => existsSync(candidate));

	if (needs.python) {
		if (!uv)
			failures.push("uv is not installed (https://docs.astral.sh/uv/): the Python environment was not prepared");
		else {
			const venv = join(dir, ".venv");
			const pythonArg = binaryOf(python, "python3") ?? binaryOf(python, "python") ?? requests.python;
			const created = await run([uv, "venv", venv, ...(pythonArg ? ["--python", pythonArg] : [])], dir);
			if (created.code !== 0)
				failures.push(`uv venv failed: ${(created.stderr || created.stdout).trim().slice(0, 300)}`);
			else {
				const interpreter = join(venv, "bin", "python");
				const pip = (args: string[]) => run([uv, "pip", "install", "--python", interpreter, ...args], repoDir);
				let ok = false;
				if (found.python.includes("uv.lock") && found.python.includes("pyproject.toml")) {
					const synced = await run([uv, "sync", "--frozen", "--all-extras", "--project", repoDir], repoDir, {
						UV_PROJECT_ENVIRONMENT: venv,
					});
					if (synced.code === 0) {
						prepared.push("python: uv sync --frozen --all-extras (uv.lock)");
						ok = true;
					} else failures.push(`uv sync failed: ${(synced.stderr || synced.stdout).trim().slice(0, 300)}`);
				} else if (found.python.includes("pyproject.toml") || found.python.includes("setup.py")) {
					for (const spec of [`${repoDir}[dev,test]`, `${repoDir}[dev]`, `${repoDir}[test]`, repoDir]) {
						const result = await pip(["-e", spec]);
						if (result.code === 0) {
							prepared.push(
								`python: uv pip install -e ${spec === repoDir ? "." : `.${spec.slice(repoDir.length)}`}`,
							);
							ok = true;
							break;
						}
						if (spec === repoDir)
							failures.push(
								`uv pip install -e . failed: ${(result.stderr || result.stdout).trim().slice(0, 300)}`,
							);
					}
				}
				// Sub-projects (a pyproject or setup.py below the root): installed into the same environment, editable.
				for (const manifest of found.python.filter(
					(name) => name.includes("/") && /(^|\/)(pyproject\.toml|setup\.py)$/.test(name),
				)) {
					const project = join(repoDir, dirname(manifest));
					for (const spec of [`${project}[dev,test]`, `${project}[dev]`, `${project}[test]`, project]) {
						const result = await pip(["-e", spec]);
						if (result.code === 0) {
							prepared.push(`python: uv pip install -e ${dirname(manifest)}${spec.slice(project.length)}`);
							ok = true;
							break;
						}
						if (spec === project)
							failures.push(
								`uv pip install -e ${dirname(manifest)} failed: ${(result.stderr || result.stdout).trim().slice(0, 300)}`,
							);
					}
				}
				const requirements = found.python.filter((name) => /requirements.*\.txt$|requirements\//.test(name));
				for (const file of requirements) {
					const result = await pip(["-r", join(repoDir, file)]);
					if (result.code === 0) {
						prepared.push(`python: uv pip install -r ${file}`);
						ok = true;
					} else
						failures.push(
							`uv pip install -r ${file} failed: ${(result.stderr || result.stdout).trim().slice(0, 300)}`,
						);
				}
				if (!ok && prepared.length === 0 && requirements.length === 0)
					prepared.push("python: .venv (no dependency list found)");
			}
		}
	}
	if (needs.node) {
		const lock = MANIFESTS.nodeLocks.find((name) => found.node.includes(name));
		if (!lock)
			failures.push(
				"no Node lockfile (package-lock.json, pnpm-lock.yaml, yarn.lock): node_modules was not prepared",
			);
		else {
			for (const name of ["package.json", lock, ".npmrc"])
				if (existsSync(join(repoDir, name))) copyFileSync(join(repoDir, name), join(dir, name));
			const manager = lock === "pnpm-lock.yaml" ? "pnpm" : lock === "yarn.lock" ? "yarn" : "npm";
			const argv =
				manager === "pnpm"
					? ["pnpm", "install", "--frozen-lockfile", "--ignore-scripts"]
					: manager === "yarn"
						? ["yarn", "install", "--frozen-lockfile", "--ignore-scripts", "--non-interactive"]
						: ["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"];
			const result = await run(argv, dir);
			if (result.code === 0) prepared.push(`node: ${argv.join(" ")} (${lock})`);
			else failures.push(`${argv.join(" ")} failed: ${(result.stderr || result.stdout).trim().slice(0, 300)}`);
		}
	}
	const ok = prepared.length > 0 && failures.length === 0;
	const record: PreparedEnvironment & { ok: boolean; at: string; repo: string } = {
		dir,
		hash,
		prepared,
		cached: false,
		toolchain: toolchain.entries,
		failures,
		ms: Date.now() - started,
		ok,
		at: new Date().toISOString(),
		repo: repoDir,
	};
	writeFileSync(recordPath, JSON.stringify(record, null, 1), { mode: 0o644 });
	return record;
}

/** The prepared environment of `repoDir` already in the cache, if any (no build). */
export function cachedEnvironment(repoDir: string, cacheDir: string, python?: string): PreparedEnvironment | undefined {
	const hash = environmentHash(repoDir, python);
	const recordPath = join(cacheDir, "envs", hash, "toolchain.json");
	if (!existsSync(recordPath)) return undefined;
	try {
		const record = JSON.parse(readFileSync(recordPath, "utf8")) as PreparedEnvironment & { ok?: boolean };
		return record.ok ? { ...record, cached: true } : undefined;
	} catch {
		return undefined;
	}
}

/** One line per toolchain entry: the version and where it came from. */
export function describeToolchain(entries: readonly ToolchainEntry[]): string[] {
	return entries.map(
		(entry) =>
			`${entry.tool} ${entry.version || "(version not pinned)"} from ${entry.source}${entry.installed ? " (installed by prepare)" : ""}: ${entry.path}`,
	);
}
