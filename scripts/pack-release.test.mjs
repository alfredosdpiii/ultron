import assert from "node:assert/strict";
import test from "node:test";
import { BUNDLED, INLINED, NPM_NAME, npmReadme, releaseManifest } from "./pack-release.mjs";

const manifest = {
	name: "@bryandlp/ultron-coding-agent",
	version: "0.87.1",
	bin: { ultron: "dist/bundle/cli.js" },
	piConfig: { name: "ultron", configDir: ".ultron" },
	scripts: { prepublishOnly: "npm run build" },
	devDependencies: { vitest: "4.1.9" },
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

test("the release is the unscoped npm package ultron-agent, still installing the ultron command", () => {
	const release = releaseManifest(manifest, { version: "0.87.1" });
	assert.equal(NPM_NAME, "ultron-agent");
	assert.equal(release.name, "ultron-agent");
	assert.deepEqual(release.bin, { ultron: "dist/bundle/cli.js" });
	// The runtime reads its app name and config directory from piConfig.
	assert.deepEqual(release.piConfig, { name: "ultron", configDir: ".ultron" });
	assert.equal(release.license, "MIT");
	assert.deepEqual(release.engines, { node: ">=22.19.0" });
	assert.ok(release.description.length > 20);
	assert.ok(release.keywords.includes("coding-agent"));
	assert.deepEqual(release.publishConfig, { access: "public" });
	assert.equal(release.homepage, "https://github.com/alfredosdpiii/ultron#readme");
	assert.equal(release.bugs.url, "https://github.com/alfredosdpiii/ultron/issues");
	// Monorepo scripts and dev dependencies do not ship.
	assert.equal(release.scripts, undefined);
	assert.equal(release.devDependencies, undefined);
});

test("the npm README links into the repository", () => {
	const readme = [
		"See [`scripts/eval-quality.mjs`](scripts/eval-quality.mjs) and [`acceptance/quality/`](acceptance/quality).",
		"Guide: [containerization](packages/coding-agent/docs/containerization.md), [LICENSE](LICENSE).",
		"Kept: [Pi](https://github.com/badlogic/pi-mono), [install](#install), [mail](mailto:a@b.c).",
	].join("\n");
	const out = npmReadme(readme);
	assert.match(out, /\]\(https:\/\/github\.com\/alfredosdpiii\/ultron\/blob\/main\/scripts\/eval-quality\.mjs\)/);
	assert.match(out, /\]\(https:\/\/github\.com\/alfredosdpiii\/ultron\/tree\/main\/acceptance\/quality\)/);
	assert.match(out, /blob\/main\/packages\/coding-agent\/docs\/containerization\.md\)/);
	assert.match(out, /blob\/main\/LICENSE\)/);
	assert.match(out, /\]\(https:\/\/github\.com\/badlogic\/pi-mono\)/);
	assert.match(out, /\]\(#install\)/);
	assert.match(out, /\]\(mailto:a@b\.c\)/);
	assert.doesNotMatch(out, /\]\((?!https:|#|mailto:)/);
});
