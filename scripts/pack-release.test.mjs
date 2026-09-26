import assert from "node:assert/strict";
import test from "node:test";
import { BUNDLED, INLINED, releaseManifest } from "./pack-release.mjs";

const manifest = {
	name: "@bryandlp/ultron-coding-agent",
	version: "0.87.1",
	files: ["dist", "npm-shrinkwrap.json"],
	dependencies: {
		...Object.fromEntries(INLINED.map((name) => [name, "^0.87.1"])),
		[BUNDLED]: "^0.87.1",
		chalk: "5.6.2",
	},
};

test("the release installs from public npm only, with chord bundled", () => {
	const release = releaseManifest(manifest, { version: "0.87.1", dependencies: { esbuild: "0.28.2" } });
	for (const name of INLINED) assert.equal(release.dependencies[name], undefined, name);
	assert.equal(release.dependencies[BUNDLED], "0.87.1");
	assert.deepEqual(release.bundleDependencies, [BUNDLED]);
	// Chord's own dependencies are hoisted, because global installs skip a bundled package's dependencies.
	assert.equal(release.dependencies.esbuild, "0.28.2");
	assert.equal(release.dependencies.chalk, "5.6.2");
	assert.ok(!release.files.includes("npm-shrinkwrap.json"));
	assert.ok(release.files.includes("node_modules/@ultron/chord"));
	assert.match(release.repository.url, /alfredosdpiii\/ultron/);
});

test("an unexpected workspace dependency stops the release", () => {
	const broken = { ...manifest, dependencies: { ...manifest.dependencies, "@ultron/durable": "^0.87.1" } };
	assert.throws(() => releaseManifest(broken, { version: "0.87.1" }), /@ultron\/durable/);
});
