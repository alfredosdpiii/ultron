import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { checkEngine, ENGINE_FILES, fromCheckout, fromRelease, sha256, writeEngine } from "./loki-update.mjs";

function fakeLoki(directory, version = "9.9.9") {
	mkdirSync(join(directory, "templates"), { recursive: true });
	writeFileSync(join(directory, "loki.py"), `#!/usr/bin/env python3\n__version__ = "${version}"\n`);
	writeFileSync(join(directory, "templates", "loki.json"), '{"rule_packs": ["core"]}\n');
	writeFileSync(join(directory, "LICENSE"), "MIT License\n");
}

function git(directory, ...args) {
	return execFileSync("git", ["-C", directory, ...args], {
		encoding: "utf8",
		env: {
			...process.env,
			GIT_AUTHOR_NAME: "t",
			GIT_AUTHOR_EMAIL: "t@example.com",
			GIT_COMMITTER_NAME: "t",
			GIT_COMMITTER_EMAIL: "t@example.com",
		},
	});
}

test("the bundled Loki engine matches its pinned sha256 digests", () => {
	assert.deepEqual(checkEngine(), []);
});

test("an update from a clean checkout pins every file and the commit, never a local path", () => {
	const work = mkdtempSync(join(tmpdir(), "loki-update-"));
	try {
		const checkout = join(work, "loki");
		mkdirSync(checkout);
		fakeLoki(checkout);
		git(checkout, "init", "-q");
		git(checkout, "remote", "add", "origin", "git@github.com:someone/loki.git");
		git(checkout, "add", ".");
		git(checkout, "-c", "commit.gpgsign=false", "commit", "-qm", "init");
		const target = join(work, "engine");
		const pin = writeEngine(target, fromCheckout(checkout));
		assert.equal(pin.version, "9.9.9");
		assert.equal(pin.source.commit, git(checkout, "rev-parse", "HEAD").trim());
		assert.equal(pin.source.repository, "https://github.com/someone/loki");
		assert.deepEqual(Object.keys(pin.files), ENGINE_FILES);
		assert.ok(!readFileSync(join(target, "VERSION.json"), "utf8").includes(work));
		assert.deepEqual(checkEngine(target), []);
		writeFileSync(join(target, "loki.py"), '__version__ = "9.9.9"\n# tampered\n');
		assert.deepEqual(checkEngine(target), ["loki.py does not match its pinned sha256"]);
		// Uncommitted engine changes cannot be pinned by commit.
		writeFileSync(join(checkout, "loki.py"), '__version__ = "9.9.10"\n');
		assert.throws(() => fromCheckout(checkout), /Commit the Loki engine files first/);
	} finally {
		rmSync(work, { recursive: true, force: true });
	}
});

test("an update from a release verifies the sdist digest", async () => {
	const work = mkdtempSync(join(tmpdir(), "loki-release-"));
	try {
		const top = join(work, "loki_guardrails-9.9.9");
		fakeLoki(top);
		const archivePath = join(work, "sdist.tar.gz");
		execFileSync("tar", ["-czf", archivePath, "-C", work, "loki_guardrails-9.9.9"]);
		const archive = readFileSync(archivePath);
		const url = "https://files.example/loki_guardrails-9.9.9.tar.gz";
		const fetchWith = (digest) => async (target) =>
			target === url
				? new Response(archive)
				: Response.json({ urls: [{ packagetype: "sdist", url, digests: { sha256: digest } }] });
		const update = await fromRelease("9.9.9", fetchWith(sha256(archive)));
		assert.equal(update.source.kind, "pypi");
		assert.equal(update.files["loki.py"].toString("utf8"), '#!/usr/bin/env python3\n__version__ = "9.9.9"\n');
		await assert.rejects(fromRelease("9.9.9", fetchWith("0".repeat(64))), /does not match PyPI's sha256/);
	} finally {
		rmSync(work, { recursive: true, force: true });
	}
});
