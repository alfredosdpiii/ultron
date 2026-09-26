#!/usr/bin/env node
/**
 * Build a self-contained release tarball of the `ultron` CLI for installing without the monorepo:
 *
 *   npm run build:offline && node scripts/pack-release.mjs [--out dist-release]
 *   npm install -g ./dist-release/ultron-<version>.tgz        # or the GitHub release URL
 *
 * The CLI bundle inlines the workspace packages except `@ultron/chord`, which stays external (plugins load it at
 * runtime). None of the `@ultron/*` packages are on the npm registry, so the release manifest drops the inlined ones,
 * ships `@ultron/chord` inside the tarball as a bundled dependency, and depends only on public npm packages.
 */
import { execFileSync } from "node:child_process";
import { copyFileSync, cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const outIndex = process.argv.indexOf("--out");
const outDir = resolve(root, outIndex === -1 ? "dist-release" : process.argv[outIndex + 1]);
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
	for (const [name, version] of Object.entries(chordManifest.dependencies ?? {})) dependencies[name] ??= version;
	const unknown = Object.keys(dependencies).filter((name) => name.startsWith("@ultron/") && name !== BUNDLED);
	if (unknown.length) throw new Error(`Unexpected workspace dependencies in the release: ${unknown.join(", ")}`);
	return {
		...manifest,
		dependencies,
		bundleDependencies: [BUNDLED],
		files: [...(manifest.files ?? []).filter((file) => file !== "npm-shrinkwrap.json"), "node_modules/@ultron/chord"],
		repository: { type: "git", url: "git+https://github.com/alfredosdpiii/ultron.git", directory: "packages/coding-agent" },
		homepage: "https://github.com/alfredosdpiii/ultron#readme",
		bugs: { url: "https://github.com/alfredosdpiii/ultron/issues" },
	};
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
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
		mkdirSync(outDir, { recursive: true });
		const [packed] = JSON.parse(run("npm", ["pack", "--ignore-scripts", "--json", "--pack-destination", work], stage));
		const target = join(outDir, `ultron-${manifest.version}.tgz`);
		// Copy, not rename: the temp dir may be on another filesystem.
		copyFileSync(join(work, packed.filename), target);
		console.log(`Wrote ${target} (${(packed.size / 1024 / 1024).toFixed(1)} MiB, ${packed.entryCount} files)`);
	} finally {
		rmSync(work, { recursive: true, force: true });
	}
}
