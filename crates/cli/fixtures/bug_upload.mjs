// Records `uploadBugReport` (packages/coding-agent/src/core/bug-report-upload.ts) against a 127.0.0.1 gateway with
// scripted answers: the request it sends (method, path, Authorization, and the multipart parts: field name, file
// name, content type, data) and what it makes of the answer (the report id, or the error it throws).
// crates/cli/tests/bug_report.rs replays every case. Run behind the offline guard, twice, same sha256:
// node crates/cli/fixtures/bug_upload.mjs
import "./offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../../..");
const { uploadBugReport } = await import(pathToFileURL(join(repo, "packages/coding-agent/src/core/bug-report-upload.ts")).href);

const bundle = (extra = {}) => ({
	metadata: { schemaVersion: 1, id: "019a0000-0000-7000-8000-000000000001", hint: "it broke" },
	diagnostics: { schemaVersion: 1, sessionId: "s1", entryCount: 0, assistantMessageCount: 0, assistant: [], crashes: [] },
	...extra,
});
const json = (status, body) => ({ status, contentType: "application/json", body: JSON.stringify(body) });

export const cases = [
	{ name: "uploaded", token: "radius-token", bundle: bundle({ sessionJsonl: '{"type":"session"}\n', summary: "## What went wrong\nAll of it" }), reply: json(200, { ok: true, bug_report: { id: "br_123" } }) },
	{ name: "anonymous", bundle: bundle(), reply: json(201, { ok: true, bug_report: { id: "br_anon" } }) },
	{ name: "described-failure", token: "t", bundle: bundle(), reply: json(413, { ok: false, error: "too_large", description: "Report too large" }) },
	{ name: "error-only", bundle: bundle(), reply: json(403, { ok: false, error: "forbidden" }) },
	{ name: "not-json", bundle: bundle(), reply: { status: 500, contentType: "text/plain", body: "boom" } },
	{ name: "ok-status-not-ok", bundle: bundle(), reply: json(200, { ok: false }) },
	{ name: "ok-without-id", bundle: bundle(), reply: json(200, { ok: true, bug_report: { id: 7 } }) },
	{ name: "error-status-ok-body", bundle: bundle(), reply: json(502, { ok: true }) },
];

async function run(c) {
	const requests = [];
	const server = createServer((req, res) => {
		const chunks = [];
		req.on("data", (chunk) => chunks.push(chunk));
		req.on("end", async () => {
			const body = Buffer.concat(chunks);
			const form = await new Response(body, { headers: { "content-type": req.headers["content-type"] } }).formData();
			const parts = [];
			for (const [name, value] of form.entries()) {
				parts.push({ name, filename: value.name, contentType: value.type, data: await value.text() });
			}
			requests.push({ method: req.method, url: req.url, authorization: req.headers.authorization ?? null, parts });
			res.writeHead(c.reply.status, { "content-type": c.reply.contentType });
			res.end(c.reply.body);
		});
	});
	await new Promise((r) => server.listen(0, "127.0.0.1", r));
	let outcome;
	try {
		const result = await uploadBugReport(c.bundle, { token: c.token, gatewayUrl: `http://127.0.0.1:${server.address().port}/ignored/path` });
		outcome = { id: result.id };
	} catch (error) {
		outcome = { error: error.message };
	}
	server.close();
	return { ...c, requests, outcome };
}

if (import.meta.url === `file://${process.argv[1]}`) {
	const out = [];
	for (const c of cases) out.push(await run(c));
	writeFileSync(join(here, "bug_upload.json"), `${JSON.stringify(out, null, 1)}\n`);
}
