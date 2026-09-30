#!/usr/bin/env node
/**
 * Build a self-contained release tarball of the `ultron` CLI for installing without the monorepo:
 *
 *   npm run build:offline && node scripts/pack-release.mjs [--out dist-release]
 *   npm install -g ./dist-release/ultron-<version>.tgz        # or the GitHub release URL
 *
 * The same tarball is the npm package `ultron-agent` (`npm run publish:npm` builds, packs and publishes it).
 *
 * The CLI bundle inlines the workspace packages except `@ultron/chord`, which stays external (plugins load it at
 * runtime). None of the `@ultron/*` packages are on the npm registry, so the release manifest drops the inlined ones,
 * ships `@ultron/chord` inside the tarball as a bundled dependency, and depends only on public npm packages. It leaves
 * out esbuild (see OMITTED), so the install runs no dependency install scripts.
 *
 * Before packing, the staged package is scanned for credentials and home-directory paths (scripts/secret-scan.mjs);
 * a finding stops the release. `--skip-secret-scan` packs anyway, for investigating a finding only.
 */
import { execFileSync } from "node:child_process";
import { copyFileSync, cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { DEFAULT_ALLOWLIST, formatFindings, loadAllowlist, scanDirectory } from "./secret-scan.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const agentDir = join(root, "packages", "coding-agent");
const chordDir = join(root, "packages", "chord");

/** Packages inlined into the CLI bundle; they must not be installed from a registry. */
export const INLINED = [
	"@ultron/agent-core",
	"@ultron/ai",
	"@ultron/client",
	"@ultron/protocol",
	"@ultron/server",
	"@ultron/tui",
];
/** The one workspace package the bundle imports at runtime. */
export const BUNDLED = "@ultron/chord";
/**
 * Chord dependencies the release leaves out. esbuild only builds experimental plugin packages (loaded lazily by
 * `@ultron/chord/bundler`), and its postinstall script makes npm 11 print an allow-scripts warning on every global
 * install, which a package cannot silence for its users. Users of plugin packages install it next to ultron.
 */
export const OMITTED = ["esbuild"];
/** The package name on the npm registry; the command stays `ultron`. */
export const NPM_NAME = "ultron-agent";
const REPOSITORY = "https://github.com/alfredosdpiii/ultron";
/** Relative links in the README resolve against this on npm, where the repository layout is not browsable. */
const REPOSITORY_BLOB = `${REPOSITORY}/blob/main/`;
const REPOSITORY_TREE = `${REPOSITORY}/tree/main/`;

function run(command, args, cwd) {
	return execFileSync(command, args, { cwd, encoding: "utf8", stdio: ["ignore", "pipe", "inherit"] });
}

/** `npm pack` a directory into `destination` and unpack it; returns the unpacked `package/` directory. */
function packAndExtract(directory, destination) {
	mkdirSync(destination, { recursive: true });
	const [packed] = JSON.parse(run("npm", ["pack", "--ignore-scripts", "--json", "--pack-destination", destination], directory));
	run("tar", ["-xzf", join(destination, packed.filename), "-C", destination], destination);
	rmSync(join(destination, packed.filename));
	return join(destination, "package");
}

/** The release manifest: no inlined workspace packages, chord bundled, its public dependencies hoisted. */
export function releaseManifest(manifest, chordManifest) {
	const dependencies = { ...manifest.dependencies };
	for (const name of INLINED) delete dependencies[name];
	dependencies[BUNDLED] = chordManifest.version;
	for (const [name, version] of Object.entries(chordManifest.dependencies ?? {}))
		if (!OMITTED.includes(name)) dependencies[name] ??= version;
	const unknown = Object.keys(dependencies).filter((name) => name.startsWith("@ultron/") && name !== BUNDLED);
	if (unknown.length) throw new Error(`Unexpected workspace dependencies in the release: ${unknown.join(", ")}`);
	// Workspace scripts (build, prepublishOnly) and dev dependencies mean nothing outside the monorepo.
	const { scripts: _scripts, devDependencies: _devDependencies, ...published } = manifest;
	return {
		...published,
		name: NPM_NAME,
		description:
			"Terminal coding agent that works through a persistent Python REPL: sub-agents, bounded sub-model calls, background jobs and gated memory. A fork of Pi.",
		keywords: ["ultron", "coding-agent", "ai", "llm", "agent", "cli", "tui", "repl", "rlm", "pi"],
		author: "alfredosdpiii",
		license: "MIT",
		engines: { node: ">=22.19.0" },
		dependencies,
		bundleDependencies: [BUNDLED],
		files: [...(manifest.files ?? []).filter((file) => file !== "npm-shrinkwrap.json"), "node_modules/@ultron/chord"],
		repository: { type: "git", url: `git+${REPOSITORY}.git` },
		homepage: `${REPOSITORY}#readme`,
		bugs: { url: `${REPOSITORY}/issues` },
		publishConfig: { access: "public" },
	};
}

/** Images in the README load from here on npm: a `blob` page is HTML, so an image must point at the raw file. */
const REPOSITORY_RAW = "https://raw.githubusercontent.com/alfredosdpiii/ultron/main/";

/** A relative README target made absolute; absolute URLs, anchors and `mailto:` are returned unchanged. */
function absoluteTarget(target, image) {
	if (/^([a-z][a-z0-9+.-]*:|#|\/\/)/i.test(target)) return target;
	const path = target.replace(/^\.\//, "");
	if (image) return `${REPOSITORY_RAW}${path}`;
	const file = path.split("#")[0].split("/").pop() ?? "";
	const base = file.includes(".") || file === "LICENSE" ? REPOSITORY_BLOB : REPOSITORY_TREE;
	return `${base}${path}`;
}

/**
 * The repository README with its relative links made absolute, for the npm package page. Anchors, absolute URLs and
 * `mailto:` links are left alone; a path without an extension is treated as a directory. Images (`![alt](path)` and
 * `<img src="path">`) point at raw.githubusercontent.com, since npm renders only an image URL that serves the file.
 */
export function npmReadme(readme) {
	return readme
		.replace(/(!\[[^\]]*)?\]\(([^)\s]+)\)/g, (_match, image, target) => `${image ?? ""}](${absoluteTarget(target, image)})`)
		.replace(/(<img\b[^>]*\bsrc=")([^"]+)(")/gi, (_match, before, target, after) => `${before}${absoluteTarget(target, true)}${after}`);
}

/** Scan a staged package (node_modules included); throws with the redacted findings when there are any. */
export function scanStagedPackage(stage, allowlist = loadAllowlist(DEFAULT_ALLOWLIST)) {
	const { findings } = scanDirectory(stage, { allowlist, includeNodeModules: true, prefix: "package/" });
	if (findings.length > 0)
		throw new Error(
			`The release package contains ${findings.length} possible secret(s) or home-directory path(s):\n${formatFindings(findings)}\nRemove them, or list a deliberate fake in .secret-scan-allow.`,
		);
}

/** Pack the built CLI into `<outDir>/ultron-<version>.tgz`; returns the tarball path and the release manifest. */
export function packRelease(outDir = join(root, "dist-release"), { secretScan = true } = {}) {
	if (!existsSync(join(agentDir, "dist", "bundle", "cli.js"))) throw new Error("Build first: npm run build:offline");
	const work = mkdtempSync(join(tmpdir(), "ultron-release-"));
	try {
		const stage = packAndExtract(agentDir, join(work, "agent"));
		const chord = packAndExtract(chordDir, join(work, "chord"));
		const manifest = JSON.parse(readFileSync(join(stage, "package.json"), "utf8"));
		const chordManifest = JSON.parse(readFileSync(join(chord, "package.json"), "utf8"));
		// The shrinkwrap pins the workspace graph, including the inlined packages; the release resolves public
		// packages from the exact versions in the manifest instead.
		rmSync(join(stage, "npm-shrinkwrap.json"), { force: true });
		cpSync(chord, join(stage, "node_modules", "@ultron", "chord"), { recursive: true });
		// Global installs treat a bundled package's own dependencies as part of the bundle and leave them as empty
		// directories. The release hoists them to the top level instead, so the bundled copy declares none.
		const bundledManifestPath = join(stage, "node_modules", "@ultron", "chord", "package.json");
		const bundledManifest = JSON.parse(readFileSync(bundledManifestPath, "utf8"));
		delete bundledManifest.dependencies;
		writeFileSync(bundledManifestPath, `${JSON.stringify(bundledManifest, null, "\t")}\n`);
		writeFileSync(join(stage, "package.json"), `${JSON.stringify(releaseManifest(manifest, chordManifest), null, "\t")}\n`);
		// npm shows the package's README: the project README, not the coding-agent package's Pi README.
		writeFileSync(join(stage, "README.md"), npmReadme(readFileSync(join(root, "README.md"), "utf8")));
		copyFileSync(join(root, "LICENSE"), join(stage, "LICENSE"));
		if (secretScan) scanStagedPackage(stage);
		mkdirSync(outDir, { recursive: true });
		const [packed] = JSON.parse(run("npm", ["pack", "--ignore-scripts", "--json", "--pack-destination", work], stage));
		const target = join(outDir, `ultron-${manifest.version}.tgz`);
		// Copy, not rename: the temp dir may be on another filesystem.
		copyFileSync(join(work, packed.filename), target);
		console.log(`Wrote ${target} (${(packed.size / 1024 / 1024).toFixed(1)} MiB, ${packed.entryCount} files)`);
		return { tarball: target, manifest: JSON.parse(readFileSync(join(stage, "package.json"), "utf8")) };
	} finally {
		rmSync(work, { recursive: true, force: true });
	}
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
	const outIndex = process.argv.indexOf("--out");
	packRelease(resolve(root, outIndex === -1 ? "dist-release" : process.argv[outIndex + 1]), {
		secretScan: !process.argv.includes("--skip-secret-scan"),
	});
}
