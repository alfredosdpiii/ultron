#!/usr/bin/env node
/**
 * Build, pack and publish the `ultron-agent` npm package (the same tarball as the GitHub release asset):
 *
 *   npm run publish:npm                    # build, pack, npm publish <tarball> --access public
 *   npm run publish:npm -- --dry-run       # everything except the upload
 *   npm run publish:npm -- --no-build      # reuse the current build
 *
 * Requires `npm login` (or an automation token) for the real publish. Pass `--otp <code>` for two-factor accounts.
 */
import { execFileSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { NPM_NAME, packRelease } from "./pack-release.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
const dryRun = args.includes("--dry-run");
const otpIndex = args.indexOf("--otp");
const otp = otpIndex === -1 ? undefined : args[otpIndex + 1];

function npm(npmArgs, options = {}) {
	return execFileSync("npm", npmArgs, { cwd: root, stdio: "inherit", ...options });
}

/** True when this exact version is already on the registry; publishing it again would fail after the build. */
function isPublished(name, version) {
	try {
		const out = execFileSync("npm", ["view", `${name}@${version}`, "version", "--json"], {
			cwd: root,
			encoding: "utf8",
			stdio: ["ignore", "pipe", "ignore"],
		});
		return out.trim().length > 0;
	} catch {
		return false; // E404: the package or the version does not exist yet.
	}
}

if (!args.includes("--no-build")) npm(["run", "build:offline"]);
const { tarball, manifest } = packRelease();
if (manifest.name !== NPM_NAME) throw new Error(`Packed ${manifest.name}, expected ${NPM_NAME}`);
if (!dryRun && isPublished(manifest.name, manifest.version)) {
	throw new Error(`${manifest.name}@${manifest.version} is already published; bump the version first.`);
}
if (!dryRun) {
	try {
		execFileSync("npm", ["whoami"], { cwd: root, stdio: ["ignore", "pipe", "pipe"] });
	} catch {
		throw new Error("Not logged in to npm. Run `npm login` first.");
	}
}
npm([
	"publish",
	tarball,
	"--access",
	"public",
	...(dryRun ? ["--dry-run"] : []),
	...(otp === undefined ? [] : ["--otp", otp]),
]);
console.log(dryRun ? `Dry run of ${manifest.name}@${manifest.version} passed.` : `Published ${manifest.name}@${manifest.version}.`);
