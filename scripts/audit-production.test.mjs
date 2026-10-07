import assert from "node:assert/strict";
import test from "node:test";
import { unaccepted } from "./audit-production.mjs";

const forge = (effects) => ({
	severity: "high",
	via: [{ url: "https://github.com/advisories/GHSA-86w9-cpqp-85rv", title: "node-forge RSA PKCS#1" }],
	effects,
});
const gondolin = { severity: "high", via: ["node-forge"], effects: [] };

test("the accepted node-forge advisory passes while it only reaches the tree through gondolin", () => {
	const report = { vulnerabilities: { "node-forge": forge(["@earendil-works/gondolin"]), "@earendil-works/gondolin": gondolin } };
	assert.deepEqual(unaccepted(report), []);
});

test("the same advisory through another dependency fails", () => {
	const report = {
		vulnerabilities: {
			"node-forge": forge(["@earendil-works/gondolin", "some-tls-lib"]),
			"@earendil-works/gondolin": gondolin,
			"some-tls-lib": { severity: "high", via: ["node-forge"], effects: [] },
		},
	};
	assert.deepEqual(unaccepted(report).map((line) => line.split(" ")[0]).sort(), ["@earendil-works/gondolin", "node-forge", "some-tls-lib"]);
});

test("any other advisory fails, and findings below the bar are ignored", () => {
	const report = {
		vulnerabilities: {
			"shell-quote": { severity: "critical", via: [{ url: "https://github.com/advisories/GHSA-pqg4-j6r4-53mv" }], effects: [] },
			"tiny-low": { severity: "low", via: [{ url: "https://github.com/advisories/GHSA-xxxx" }], effects: [] },
		},
	};
	assert.deepEqual(unaccepted(report), ["shell-quote (critical): https://github.com/advisories/GHSA-pqg4-j6r4-53mv"]);
});
