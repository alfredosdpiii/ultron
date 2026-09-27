/** Committed task fixtures (tasks-delegation.mjs, tasks-delegation-deep.mjs): read a tree and pin it by sha256. */
import { createHash } from "node:crypto";
import { readdirSync, readFileSync } from "node:fs";
import { join, relative } from "node:path";

/** Every file under `dir` as {relative path: content}, skipping bytecode caches. */
export function readTree(dir) {
	const files = {};
	const walk = (current) => {
		for (const entry of readdirSync(current, { withFileTypes: true })) {
			if (entry.name === "__pycache__") continue;
			const path = join(current, entry.name);
			if (entry.isDirectory()) walk(path);
			else files[relative(dir, path)] = readFileSync(path, "utf8");
		}
	};
	walk(dir);
	return files;
}

/** sha256 over the project and hidden trees (path and content, sorted by path). */
export function fixtureDigest(project, hidden) {
	const hash = createHash("sha256");
	for (const [prefix, tree] of [
		["project", project],
		["hidden", hidden],
	])
		for (const path of Object.keys(tree).sort()) hash.update(`${prefix}/${path}\0${tree[path]}\0`);
	return hash.digest("hex");
}
