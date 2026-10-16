// Generates crates/ai/fixtures/google-vertex-adc.json from this worktree's TypeScript google-vertex provider in
// application-default-credentials mode (no API key; project and location; google-auth-library credentials).
// Every request is answered by a stub: `globalThis.fetch` for the @google/genai SDK and gaxios' fetch implementation for
// google-auth-library's token and metadata requests. Credential files go to a fresh temp directory per scenario, recorded
// as `<dir>`; HOME points there, so the machine's own gcloud credentials are never read. Each scenario records the
// requests (form bodies as ordered pairs; a JWT assertion as its decoded header and payload plus the sha256 of its
// signature, so no signed token lands in the repository), every pushed event and the final message.
// tests/vertex_adc.rs replays them through the Rust port.
// Usage: node crates/ai/fixtures/generate-vertex-adc.mjs
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { createHash } from "node:crypto";
import { chmodSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const src = (path) => import(pathToFileURL(join(root, "packages/ai/src", path)).href);

for (const name of Object.keys(process.env)) {
	if (/^(PI_|GOOGLE_|GEMINI_|GCLOUD_|GCE_|METADATA_|AWS_|CLOUDSDK_|CLOUD_RUN_JOB$|FUNCTION_NAME$|K_SERVICE$|google_|HTTPS?_PROXY$)/i.test(name)) {
		delete process.env[name];
	}
}
// google-auth-library looks the ADC project ID up after resolving the credential (an unused side lookup that would
// run the gcloud CLI or ask the metadata server); a process-env project short-circuits it.
process.env.GCLOUD_PROJECT = "side-lookup-project";

const NOW = 1767225600000;
const RealDate = Date;
class PinnedDate extends RealDate {
	constructor(...args) {
		if (args.length === 0) super(NOW);
		else super(...args);
	}
	static now() {
		return NOW;
	}
}
globalThis.Date = PinnedDate;

const vertex = await src("api/google-vertex.ts");
const { AssistantMessageEventStream } = await src("utils/event-stream.ts");
const { getPiUserAgent } = await src("utils/pi-user-agent.ts");
const requireFromGenai = createRequire(join(root, "node_modules/@google/genai/dist/node/index.cjs"));
const requireFromAuth = createRequire(requireFromGenai.resolve("google-auth-library"));
const { Gaxios } = requireFromAuth("gaxios");
const gcpMetadata = requireFromAuth("gcp-metadata");

// A throwaway 2048-bit RSA key (PKCS#8 DER, base64) made for this fixture only; the PEM armor is added at run time.
const PRIVATE_KEY_DER =
	"MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCVJbRwve/PEBswVCctZhkH06BiH9TKz4v7RAYSMOgrKQFXOl4hzvDr5mRxsxhxvCK+LoAJ5yIGWg9jUkfsTlCxEBcD6SWxV3nC3DeGwa38T/z/fqkDgc+RZighn7N1oCFneu68C1MX4t+BIGmn7oz9hfZPlFIA4LLJ9sk0AB8L4Bp7iqtBmAGLSlwLQgqugSNt4D+FoNDL4J7VSxiFtqG4+i+SO7s5xZ0LhIC48fcnVmFgdv/ksi0cjVfq6P0miUu702ARPdHonk+EwqvNGHz5EvkIvFyv4v2Gmlsr6loQP3W66wemctWpjSsrBZt/vw11CioYrVfYmDeEIzYStobJAgMBAAECggEABkl9qI5S6iwYIxcC0ja1g4SrZ7W4kqvTnX+9TDh0VWz7lIqR1LI7jcVCjXSnME2CV9yMRp6XI8DZkfyHji4jnvldtwCLDNPHjOwbbRpnUG5WF32C99XvERS41q/J/x3abGeORYONzk9ZlyRfeLJ6jjvFzRykbIQMTjuDHMLhqJkm5bphzIMMFdK0BIOs0C68c7Q3bL4hylP2e+hL2WvulNdrC35NsshsPIEaKLxDvu81bGhcmKh0yOOfI5H0IZ8DfGDB7LoHEm4LPA4MVb2dwKSOyCyCSN6RWwu+LmbuM5NynG4PV/HrzLl2ZGL4KdTt6eS3oiX2/iEaaNiwOkgoBwKBgQDR+KZonZFChIEnxWnNffQhf86lnxMMmpBq4FQQTNciM+ifwRmpUF+jjruj48psFMI6LNeNtRtyA3+Fkt7DV4TL5J9jTp9QVBgUxQk3ITRMrcjzprbqHI497tVkBzsg60UREmGF4u0hEj/JZorY8ETmr6QeJl3SuhaaAIp4IdZkXwKBgQC1165Lm7dT3s9VvGiKgcCkEe4o8yFn6p8qp+7/dJCmP9oxsSQu+u/gCrzzQJOqYrFR7jMtAPfB5bxsHEWpo6WS422JJo8Ey0DhKFQRnZwhlnLSVVhtuhrWSmZOgjsOjbUrerait2JfM4iLID3w1asNGoVQVL0PBeucepK/cmml1wKBgAXT6/J9ZRm+icMr1IrykwMXBApOxv2TkKyWu16Uc2gsnqlId8jeRTlJ7eVXnjevv9BTjEwPmcppVHHFqM8tL69lEhN0jDFzlBOHgID6N/Rk4m3405zd3AjWBzA2jyqTvSADLZfEu6YuVnzljrcEvDTW7Sw1DmeZ6KSvnShGcCfrAoGAcv+RZJN3nvzbllburhlcoOYN/au0xqkkO6+0f61SBTGAZcinH2bm6IMh95wxRqHfjfbcOmvQQ7lsLdq7W4r4nSpjDUdrTxbTUxFUCUfpg3xH5SjAIbR+YGLwzFmjy5JmrraOZd0fwyEuCv467eHSqRjKkoDkh2mxFWaXA6fxlEUCgYEAxBrBgKZjtM2U+xEhx3C7nmM5D5SViYNKc6YJ8V78I9Be8F4z4BzoRE8RQONzIIS1geSWRyKkajx4UM/BvfePAk3acxXQFG6swcLHmPFZWcnRM8zILns/nsr9cZSWqDtiUUX8LsTWQHj9tOMKb3a6D5b7/pvX6p6vemmViF1QySY=";
const pem = (der) =>
	`-----BEGIN ${"PRIVATE"} KEY-----\n${der.match(/.{1,64}/g).join("\n")}\n-----END ${"PRIVATE"} KEY-----\n`;
// Throwaway self-signed X.509 certificates (DER, base64) and the leaf's P-256 key (PKCS#8 DER, base64), made for this
// fixture only: an identity-pool `certificate` source (mTLS workload identity) sends the chain as its subject token.
const CERTIFICATES = {
	leaf: "MIIBiDCCAS2gAwIBAgIUc/SgsSpcBQ51FdzjXKDUGA0hCh4wCgYIKoZIzj0EAwIwGDEWMBQGA1UEAwwNd2lmLWxlYWYudGVzdDAgFw0yNjEwMTYxNzUxNTlaGA8yMTI2MDkyMjE3NTE1OVowGDEWMBQGA1UEAwwNd2lmLWxlYWYudGVzdDBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABMm7S97uGf8lzIGfIzCp06Qehvfq4chYxrg9i9IFVyYccx8JFS99FLCqOa/bXX2pm9rDnm9HGoCvwjwXsJ5vCzSjUzBRMB0GA1UdDgQWBBT0VOSGLeiZgfuv4h9goi5Ex1kq6jAfBgNVHSMEGDAWgBT0VOSGLeiZgfuv4h9goi5Ex1kq6jAPBgNVHRMBAf8EBTADAQH/MAoGCCqGSM49BAMCA0kAMEYCIQCXcF4vvui6mPda5BwRbAt36BzwZk6ehK05nBAITxVgKgIhAKXLd837ViuBGzNP+riDLx83tNdylEjiwM5/ZvILWri4",
	other: "MIIBlzCCAT2gAwIBAgIUNsepR7dB/fXD/VUXMDZG30VfCagwCgYIKoZIzj0EAwIwIDEeMBwGA1UEAwwVd2lmLWludGVybWVkaWF0ZS50ZXN0MCAXDTI2MTAxNjE3NTE1OVoYDzIxMjYwOTIyMTc1MTU5WjAgMR4wHAYDVQQDDBV3aWYtaW50ZXJtZWRpYXRlLnRlc3QwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAARJP1uunQECauNMsY1CHCU1Sde9iWcNHwpCtM3M/+b2VGnw226Ii/eYa370lZwvh1e0la7YVOqJjHNC5T6UM22Ko1MwUTAdBgNVHQ4EFgQU0MZ8kDkat0KwGuw+lRZi29VXaVIwHwYDVR0jBBgwFoAU0MZ8kDkat0KwGuw+lRZi29VXaVIwDwYDVR0TAQH/BAUwAwEB/zAKBggqhkjOPQQDAgNIADBFAiAV7nHTmz8wmx14QeSeunjz6deSdOJMBXVE73nDgstwJgIhALNx751abcz410zMaxG79tEcPzaZ777krbeZBdL5zu4j",
	leafKey: "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg5EdEKZe4eIZ+OUq8zIoqeV2jC7i042Atz9TipunjZ0OhRANCAATJu0ve7hn/JcyBnyMwqdOkHob36uHIWMa4PYvSBVcmHHMfCRUvfRSwqjmv2119qZvaw55vRxqAr8I8F7Cebws0",
};
const certPem = (der) => `-----BEGIN CERTIFICATE-----\n${der.match(/.{1,64}/g).join("\n")}\n-----END CERTIFICATE-----\n`;

const clone = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));

let recording = null;
const originalPush = AssistantMessageEventStream.prototype.push;
AssistantMessageEventStream.prototype.push = function (event) {
	if (recording) recording.events.push(clone(event));
	return originalPush.call(this, event);
};

const UA = getPiUserAgent();
const b64urlJson = (part) => JSON.parse(Buffer.from(part, "base64url").toString("utf8"));
function requestHeaders(headers) {
	const out = {};
	new Headers(headers).forEach((value, key) => {
		// Telemetry naming the Node version; the Rust port does not send it.
		if (key === "x-goog-api-client") return;
		out[key] = value.replaceAll(UA, "<pi-user-agent>");
		// A self-signed JWT (a service account outside the default universe): decoded, its signature hashed.
		const jwt = /^Bearer ([\w-]+)\.([\w-]+)\.([\w-]+)$/.exec(value);
		if (key === "authorization" && jwt) {
			out[key] = { bearerJwt: { header: b64urlJson(jwt[1]), payload: b64urlJson(jwt[2]), signatureSha256: createHash("sha256").update(jwt[3]).digest("hex") } };
		}
	});
	return Object.fromEntries(Object.entries(out).sort(([a], [b]) => (a < b ? -1 : 1)));
}

function recordBody(body, headers) {
	if (body === undefined || body === null) return null;
	const text = String(body);
	const type = new Headers(headers).get("content-type") ?? "";
	if (body instanceof URLSearchParams || type.includes("x-www-form-urlencoded")) {
		return {
			form: [...new URLSearchParams(text)].map(([k, v]) => {
				// An AWS subject token is the URL-encoded JSON of a signed GetCallerIdentity request.
				if (k === "subject_token" && v.startsWith("%7B")) return [k, { awsRequest: JSON.parse(decodeURIComponent(v)) }];
				if (k !== "assertion") return [k, v];
				const [header, payload, signature] = v.split(".");
				return [k, { header: b64urlJson(header), payload: b64urlJson(payload), signatureSha256: createHash("sha256").update(signature).digest("hex") }];
			}),
		};
	}
	try {
		return JSON.parse(text);
	} catch {
		return { text };
	}
}

const abortError = () => new DOMException("This operation was aborted", "AbortError");

function respond(canned, signal) {
	const encoder = new TextEncoder();
	const chunks = canned.chunks.map((c) => encoder.encode(c));
	const body = new ReadableStream({
		start(controller) {
			signal?.addEventListener("abort", () => {
				try {
					controller.error(abortError());
				} catch {}
			});
		},
		pull(controller) {
			const next = chunks.shift();
			if (next === undefined) controller.close();
			else controller.enqueue(next);
		},
	});
	return new Response(body, { status: canned.status ?? 200, headers: canned.headers ?? { "content-type": "text/event-stream" } });
}

let queue = [];
let requests = [];
let dir = "";
const scrub = (value) => (typeof value === "string" ? value.replaceAll(dir, "<dir>") : value);
async function stubFetch(url, init = {}) {
	const request = {
		url: String(url),
		method: init.method ?? "GET",
		headers: requestHeaders(init.headers),
		body: recordBody(init.body, init.headers),
	};
	// gaxios hands its `agent` to fetch: an mTLS agent (certificate source) carries the client certificate and key.
	const tls = init.agent?.options;
	if (tls?.cert || tls?.key) {
		request.clientIdentity = { certSha256: createHash("sha256").update(tls.cert).digest("hex"), keySha256: createHash("sha256").update(tls.key).digest("hex") };
	}
	requests.push(request);
	const canned = queue.shift();
	if (!canned) throw new Error("unexpected request");
	// A connection failure: no response at all.
	if (canned.fail) throw new TypeError("fetch failed");
	return respond(canned, init.signal);
}
globalThis.fetch = stubFetch;
const defaultAdapter = Gaxios.prototype._defaultAdapter;
Gaxios.prototype._defaultAdapter = function (config) {
	return defaultAdapter.call(this, { ...config, fetchImplementation: stubFetch });
};

const placeholders = (value) =>
	typeof value === "string"
		? value
				.replaceAll("<dir>", dir)
				.replaceAll("<private-key>", pem(PRIVATE_KEY_DER))
				.replaceAll("<leaf-cert>", certPem(CERTIFICATES.leaf))
				.replaceAll("<other-cert>", certPem(CERTIFICATES.other))
				.replaceAll("<leaf-key>", pem(CERTIFICATES.leafKey))
		: Array.isArray(value)
			? value.map(placeholders)
			: value && typeof value === "object"
				? Object.fromEntries(Object.entries(value).map(([k, v]) => [k, placeholders(v)]))
				: value;

async function run(scenario) {
	dir = mkdtempSync(join(tmpdir(), "vertex-adc-"));
	// gcp-metadata remembers its metadata-server ping for the life of the process; each scenario is a fresh process.
	gcpMetadata.resetIsAvailableCache();
	queue = scenario.responses.map((r) => (r.fail ? { ...r } : { ...r, chunks: [...r.chunks] }));
	requests = [];
	for (const [path, content] of Object.entries(scenario.files ?? {})) {
		const file = join(dir, path);
		mkdirSync(dirname(file), { recursive: true });
		writeFileSync(file, typeof content === "string" ? placeholders(content) : JSON.stringify(placeholders(content), null, 2));
	}
	for (const path of scenario.executables ?? []) chmodSync(join(dir, path), 0o755);
	const processEnv = { HOME: "<dir>", METADATA_SERVER_DETECTION: "none", ...scenario.processEnv };
	for (const [name, value] of Object.entries(processEnv)) {
		if (value === null) delete process.env[name];
		else process.env[name] = placeholders(value);
	}
	const record = {
		name: scenario.name,
		fn: scenario.fn ?? "stream",
		model: scenario.model,
		context: scenario.context,
		options: clone(scenario.options),
		processEnv,
		files: scenario.files ?? {},
		executables: scenario.executables ?? [],
		responses: scenario.responses,
	};
	recording = { events: [] };
	try {
		const s = vertex[record.fn](scenario.model, scenario.context, placeholders(scenario.options));
		record.result = clone(await s.result());
		record.thrown = null;
	} catch (e) {
		record.result = null;
		record.thrown = e.message;
	}
	for (const name of Object.keys(processEnv)) delete process.env[name];
	record.events = JSON.parse(scrub(JSON.stringify(recording.events)));
	record.result = JSON.parse(scrub(JSON.stringify(record.result)));
	record.thrown = scrub(record.thrown);
	record.requests = JSON.parse(scrub(JSON.stringify(requests)));
	record.unusedResponses = queue.length;
	recording = null;
	rmSync(dir, { recursive: true, force: true });
	return record;
}

const data = (items) => items.map((d) => `data: ${JSON.stringify(d)}\r\n\r\n`);
const ok = (chunks) => ({ chunks });
const json = (status, body, headers = {}) => ({
	status,
	headers: { "content-type": "application/json", ...headers },
	chunks: [typeof body === "string" ? body : JSON.stringify(body)],
});
const metadata = (status, body, extra = {}) => ({
	status,
	headers: { "content-type": "application/json", "metadata-flavor": "Google", ...extra },
	chunks: [typeof body === "string" ? body : JSON.stringify(body)],
});
const fail = { fail: true };
const token = (access) => json(200, { access_token: access, expires_in: 3599, scope: "https://www.googleapis.com/auth/cloud-platform", token_type: "Bearer" });

const system = (content, extra = {}) => ({ role: "system", content, timestamp: 1, ...extra });
const user = (content, timestamp = 2) => ({ role: "user", content, timestamp });
const simple = (text = "hi") => ({ messages: [system("sys"), user(text)] });
const vModel = (id, extra = {}) => ({
	id,
	name: "Test Model",
	api: "google-vertex",
	provider: "google-vertex",
	baseUrl: "https://{location}-aiplatform.googleapis.com",
	reasoning: true,
	input: ["text", "image"],
	cost: { input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75 },
	contextWindow: 200000,
	maxTokens: 32000,
	...extra,
});
const v25 = vModel("gemini-2.5-pro");
const proxied = vModel("gemini-2.5-flash", { baseUrl: "https://proxy.example.com/vertex/v1" });
const cand = (parts, finishReason) => ({ candidates: [{ content: { role: "model", parts }, ...(finishReason ? { finishReason } : {}), index: 0 }] });
const done = ok(data([{ ...cand([{ text: "ok" }], "STOP"), usageMetadata: { promptTokenCount: 5, candidatesTokenCount: 1, totalTokenCount: 6 } }]));

const userCreds = (extra = {}) => ({ type: "authorized_user", client_id: "cid.apps.example.com", client_secret: "csecret", refresh_token: "rt-1", ...extra });
const serviceAccount = (extra = {}) => ({
	type: "service_account",
	project_id: "sa-project",
	private_key_id: "kid-1",
	private_key: "<private-key>",
	client_email: "bot@sa-project.iam.gserviceaccount.com",
	client_id: "1234",
	token_uri: "https://oauth2.googleapis.com/token",
	...extra,
});
const ADC = ".config/gcloud/application_default_credentials.json";
const WORKLOAD = "//iam.googleapis.com/projects/123456/locations/global/workloadIdentityPools/pool/providers/prov";
const WORKFORCE = "//iam.googleapis.com/locations/global/workforcePools/wf-pool/providers/wf-prov";
const IMPERSONATE = "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/wif-sa@p.iam.gserviceaccount.com:generateAccessToken";
const wif = (extra = {}) => ({ type: "external_account", audience: WORKLOAD, subject_token_type: "urn:ietf:params:oauth:token-type:jwt", ...extra });
const AWS_SOURCE = (extra = {}) => ({
	environment_id: "aws1",
	region_url: "http://imds.test/latest/meta-data/placement/availability-zone",
	url: "http://imds.test/latest/meta-data/iam/security-credentials",
	regional_cred_verification_url: "https://sts.{region}.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15",
	...extra,
});
const externalUser = (extra = {}) => ({
	type: "external_account_authorized_user",
	audience: "//iam.googleapis.com/locations/global/workforcePools/wf-pool/providers/wf-prov",
	client_id: "eau-client",
	client_secret: "eau-secret",
	refresh_token: "eau-refresh",
	token_info_url: "https://sts.googleapis.com/v1/introspect",
	...extra,
});
const sts = (access) => json(200, { access_token: access, issued_token_type: "urn:ietf:params:oauth:token-type:access_token", token_type: "Bearer", expires_in: 3600 });
const text = (body) => ({ status: 200, headers: { "content-type": "text/plain" }, chunks: [body] });
const script = (body) => `#!/bin/sh\n${body}\n`;

// Identity-pool `certificate` sources: the subject token is the JSON array of base64 DER certificates (the leaf, then
// a trust chain), and the STS exchange (and impersonation) run on an mTLS agent with the workload's cert and key.
function certificateScenarios() {
	const CERTS = {
		"certs/leaf.pem": "<leaf-cert>",
		"certs/leaf.key": "<leaf-key>",
		"certs/other.pem": "<other-cert>",
		"certs/chain-leaf-first.pem": "<leaf-cert><other-cert>",
		"certs/chain-leaf-second.pem": "<other-cert><leaf-cert>",
		"certs/config.json": { cert_configs: { workload: { cert_path: "<dir>/certs/leaf.pem", key_path: "<dir>/certs/leaf.key" } } },
	};
	const at = (extra = {}, certificate = { certificate_config_location: "<dir>/certs/config.json" }) =>
		wif({ subject_token_type: "urn:ietf:params:oauth:token-type:mtls", credential_source: { certificate }, ...extra });
	const scenario = (name, files, responses = [], processEnv = undefined) => ({
		name: `wif/certificate-${name}`,
		model: v25,
		context: simple(),
		files: { ...CERTS, ...files },
		...(processEnv ? { processEnv } : {}),
		options: { project: "w-cert", location: "us-central1" },
		responses,
	});
	const config = (workload) => ({ cert_configs: { workload } });
	return [
		scenario("config-location", { [ADC]: at() }, [sts("at-cert"), done]),
		scenario(
			"trust-chain-leaf-first-impersonation",
			{
				[ADC]: at(
					{ service_account_impersonation_url: IMPERSONATE },
					{ certificate_config_location: "<dir>/certs/config.json", trust_chain_path: "<dir>/certs/chain-leaf-first.pem" },
				),
			},
			[sts("at-cert-sts"), json(200, { accessToken: "at-cert-impersonated", expireTime: "2026-01-01T01:00:00Z" }), done],
		),
		scenario(
			"trust-chain-without-leaf",
			{ [ADC]: at({}, { certificate_config_location: "<dir>/certs/config.json", trust_chain_path: "<dir>/certs/other.pem" }) },
			[sts("at-cert-chain"), done],
		),
		scenario(
			"trust-chain-leaf-not-first",
			{ [ADC]: at({}, { certificate_config_location: "<dir>/certs/config.json", trust_chain_path: "<dir>/certs/chain-leaf-second.pem" }) },
		),
		scenario(
			"trust-chain-missing",
			{ [ADC]: at({}, { certificate_config_location: "<dir>/certs/config.json", trust_chain_path: "<dir>/certs/absent.pem" }) },
		),
		scenario("default-config-env", { [ADC]: at({}, { use_default_certificate_config: true }) }, [sts("at-cert-env"), done], {
			GOOGLE_API_CERTIFICATE_CONFIG: "<dir>/certs/config.json",
		}),
		scenario("default-config-env-invalid", { [ADC]: at({}, { use_default_certificate_config: true }) }, [], {
			GOOGLE_API_CERTIFICATE_CONFIG: "<dir>/certs/absent.json",
		}),
		scenario(
			"default-config-well-known",
			{ [ADC]: at({}, { use_default_certificate_config: true }), ".config/gcloud/certificate_config.json": CERTS["certs/config.json"] },
			[sts("at-cert-gcloud"), done],
		),
		scenario(
			"default-config-cloudsdk",
			{ [ADC]: at({}, { use_default_certificate_config: true }), "sdk/certificate_config.json": CERTS["certs/config.json"] },
			[sts("at-cert-sdk"), done],
			{ CLOUDSDK_CONFIG: "<dir>/sdk" },
		),
		scenario("default-config-missing", { [ADC]: at({}, { use_default_certificate_config: true }) }),
		scenario("config-location-missing", { [ADC]: at({}, { certificate_config_location: "<dir>/certs/absent.json" }) }),
		scenario("config-location-directory", { [ADC]: at({}, { certificate_config_location: "<dir>/certs" }) }),
		scenario("config-invalid-json", { [ADC]: at(), "certs/config.json": "{ not json" }),
		scenario("config-without-key-path", { [ADC]: at(), "certs/config.json": config({ cert_path: "<dir>/certs/leaf.pem" }) }),
		scenario("config-without-workload", { [ADC]: at(), "certs/config.json": { cert_configs: {} } }),
		scenario("cert-file-missing", {
			[ADC]: at(),
			"certs/config.json": config({ cert_path: "<dir>/certs/absent.pem", key_path: "<dir>/certs/leaf.key" }),
		}),
		scenario("key-file-missing", {
			[ADC]: at(),
			"certs/config.json": config({ cert_path: "<dir>/certs/leaf.pem", key_path: "<dir>/certs/absent.key" }),
		}),
		scenario("cert-file-invalid", {
			[ADC]: at(),
			"certs/bad.pem": "not a certificate",
			"certs/config.json": config({ cert_path: "<dir>/certs/bad.pem", key_path: "<dir>/certs/leaf.key" }),
		}),
		scenario("key-file-invalid", {
			[ADC]: at(),
			"certs/config.json": config({ cert_path: "<dir>/certs/leaf.pem", key_path: "<dir>/certs/leaf.pem" }),
		}),
		scenario("sts-error", { [ADC]: at() }, [json(400, { error: "invalid_grant", error_description: "The certificate is not trusted." })]),
	];
}

const scenarios = [
	{
		name: "adc/authorized-user-key-file-regional",
		model: v25,
		context: simple(),
		files: { "creds.json": userCreds({ quota_project_id: "quota-1" }) },
		options: { project: "p-1", location: "us-central1", env: { GOOGLE_APPLICATION_CREDENTIALS: "<dir>/creds.json" }, headers: { "X-Extra": "1" } },
		responses: [token("at-user"), done],
	},
	{
		name: "adc/service-account-key-file-global",
		model: v25,
		context: simple(),
		files: { "sa.json": serviceAccount() },
		options: { project: "p-2", location: "global", env: { GOOGLE_APPLICATION_CREDENTIALS: "<dir>/sa.json" } },
		responses: [token("at-sa"), done],
	},
	{
		name: "adc/well-known-file-multi-region-quota-env",
		model: v25,
		context: simple(),
		files: { [ADC]: userCreds({ quota_project_id: "file-quota" }) },
		processEnv: { GOOGLE_CLOUD_QUOTA_PROJECT: "env-quota" },
		options: { project: "p-3", location: "eu" },
		responses: [token("at-wk"), done],
	},
	{
		name: "adc/env-project-location-simple",
		fn: "streamSimple",
		model: v25,
		context: simple(),
		files: { [ADC]: userCreds() },
		options: { reasoning: "low", env: { GOOGLE_CLOUD_PROJECT: "env-p", GOOGLE_CLOUD_LOCATION: "asia-northeast1" } },
		responses: [token("at-simple"), done],
	},
	{
		name: "adc/custom-base-url-collection-scope",
		model: proxied,
		context: simple(),
		files: { "creds.json": userCreds() },
		options: { project: "p-4", location: "us-east5", env: { GOOGLE_APPLICATION_CREDENTIALS: "<dir>/creds.json" } },
		responses: [token("at-proxy"), done],
	},
	{
		name: "adc/gce-metadata-server",
		model: v25,
		context: simple(),
		processEnv: { METADATA_SERVER_DETECTION: null, GCE_METADATA_HOST: "metadata.test" },
		options: { project: "p-5", location: "us-central1" },
		responses: [metadata(200, "attributes/\nhostname\n", { "content-type": "application/text" }), metadata(200, { access_token: "at-gce", expires_in: 3000, token_type: "Bearer" }), done],
	},
	{
		name: "adc/gce-metadata-assume-present",
		model: v25,
		context: simple(),
		processEnv: { METADATA_SERVER_DETECTION: "assume-present", GCE_METADATA_HOST: "http://metadata.test:8080" },
		options: { project: "p-6", location: "us-central1" },
		responses: [metadata(200, { access_token: "at-gce2", expires_in: 3000, token_type: "Bearer" }), done],
	},
	{
		name: "adc/gce-metadata-wrong-flavor",
		model: v25,
		context: simple(),
		processEnv: { METADATA_SERVER_DETECTION: "assume-present", GCE_METADATA_HOST: "metadata.test" },
		options: { project: "p-6", location: "us-central1" },
		responses: [json(200, { access_token: "x", expires_in: 3000, token_type: "Bearer" })],
	},
	{
		name: "adc/gce-metadata-ping-404",
		model: v25,
		context: simple(),
		processEnv: { METADATA_SERVER_DETECTION: null, GCE_METADATA_HOST: "metadata.test" },
		options: { project: "p-5", location: "us-central1" },
		responses: [metadata(404, "not found", { "content-type": "text/plain" })],
	},
	{
		name: "adc/no-credentials-found",
		model: v25,
		context: simple(),
		options: { project: "p-7", location: "us-central1" },
		responses: [],
	},
	{
		name: "adc/key-file-missing",
		model: v25,
		context: simple(),
		options: { project: "p-8", location: "us-central1", env: { GOOGLE_APPLICATION_CREDENTIALS: "<dir>/missing.json" } },
		responses: [],
	},
	{
		name: "adc/well-known-file-not-json",
		model: v25,
		context: simple(),
		files: { [ADC]: "not json" },
		options: { project: "p-9", location: "us-central1" },
		responses: [],
	},
	{
		name: "adc/authorized-user-missing-refresh-token",
		model: v25,
		context: simple(),
		files: { "creds.json": userCreds({ refresh_token: undefined }) },
		options: { project: "p-10", location: "us-central1", env: { GOOGLE_APPLICATION_CREDENTIALS: "<dir>/creds.json" } },
		responses: [],
	},
	{
		name: "adc/service-account-missing-email",
		model: v25,
		context: simple(),
		files: { "sa.json": serviceAccount({ client_email: undefined }) },
		options: { project: "p-11", location: "us-central1", env: { GOOGLE_APPLICATION_CREDENTIALS: "<dir>/sa.json" } },
		responses: [],
	},
	{
		name: "adc/refresh-invalid-grant",
		model: v25,
		context: simple(),
		files: { "creds.json": userCreds() },
		options: { project: "p-12", location: "us-central1", env: { GOOGLE_APPLICATION_CREDENTIALS: "<dir>/creds.json" } },
		responses: [json(400, { error: "invalid_grant", error_description: "Bad Request" })],
	},
	{
		name: "adc/refresh-forbidden",
		model: v25,
		context: simple(),
		files: { "creds.json": userCreds() },
		options: { project: "p-13", location: "us-central1", env: { GOOGLE_APPLICATION_CREDENTIALS: "<dir>/creds.json" } },
		responses: [json(403, { error: "access_denied", error_description: "Nope" })],
	},
	{
		name: "adc/service-account-token-error",
		model: v25,
		context: simple(),
		files: { "sa.json": serviceAccount() },
		options: { project: "p-14", location: "us-central1", env: { GOOGLE_APPLICATION_CREDENTIALS: "<dir>/sa.json" } },
		responses: [json(400, { error: "invalid_grant", error_description: "Invalid JWT Signature." })],
	},
	{
		name: "adc/vertex-http-403-after-token",
		model: v25,
		context: simple(),
		files: { "creds.json": userCreds() },
		options: { project: "p-15", location: "us-central1", env: { GOOGLE_APPLICATION_CREDENTIALS: "<dir>/creds.json" } },
		responses: [token("at-403"), json(403, { error: { code: 403, message: "Permission denied on resource project p-15.", status: "PERMISSION_DENIED" } })],
	},
	{
		name: "adc/env-var-lowercase-missing-file",
		model: v25,
		context: simple(),
		processEnv: { google_application_credentials: "<dir>/nope.json" },
		options: { project: "p-16", location: "us-central1" },
		responses: [],
	},
	{
		name: "adc/env-var-lowercase-directory",
		model: v25,
		context: simple(),
		files: { "sub/keep.txt": "x" },
		processEnv: { google_application_credentials: "<dir>/sub" },
		options: { project: "p-17", location: "us-central1" },
		responses: [],
	},
	{
		name: "adc/env-var-lowercase-service-account",
		model: v25,
		context: simple(),
		files: { "sa.json": serviceAccount({ quota_project_id: "sa-quota" }) },
		processEnv: { google_application_credentials: "<dir>/sa.json" },
		options: { project: "p-18", location: "us-central1" },
		responses: [token("at-sa-env"), done],
	},
	{
		name: "adc/well-known-authorized-user-missing-field",
		model: v25,
		context: simple(),
		files: { [ADC]: userCreds({ client_secret: undefined }) },
		options: { project: "p-19", location: "us-central1" },
		responses: [],
	},
	{
		name: "adc/well-known-service-account-missing-key",
		model: v25,
		context: simple(),
		files: { [ADC]: serviceAccount({ private_key: undefined }) },
		options: { project: "p-20", location: "us-central1" },
		responses: [],
	},
	{
		name: "adc/key-file-not-json",
		model: v25,
		context: simple(),
		files: { "key.pem": "not json" },
		options: { project: "p-21", location: "us-central1", env: { GOOGLE_APPLICATION_CREDENTIALS: "<dir>/key.pem" } },
		responses: [],
	},
	{
		name: "adc/key-file-service-account-missing-key-unknown-extension",
		model: v25,
		context: simple(),
		files: { "sa.key": serviceAccount({ private_key: undefined }) },
		options: { project: "p-22", location: "us-central1", env: { GOOGLE_APPLICATION_CREDENTIALS: "<dir>/sa.key" } },
		responses: [],
	},
	{
		name: "adc/token-503-then-ok",
		model: v25,
		context: simple(),
		files: { "creds.json": userCreds() },
		options: { project: "p-23", location: "us-central1", env: { GOOGLE_APPLICATION_CREDENTIALS: "<dir>/creds.json" } },
		responses: [json(503, "unavailable", { "content-type": "text/plain" }), token("at-retry"), done],
	},
	{
		name: "adc/token-connection-failures",
		model: v25,
		context: simple(),
		files: { "creds.json": userCreds() },
		options: { project: "p-24", location: "us-central1", env: { GOOGLE_APPLICATION_CREDENTIALS: "<dir>/creds.json" } },
		responses: [fail, fail, fail, fail],
	},
	{
		name: "adc/token-error-without-error-field",
		model: v25,
		context: simple(),
		files: { "creds.json": userCreds() },
		options: { project: "p-25", location: "us-central1", env: { GOOGLE_APPLICATION_CREDENTIALS: "<dir>/creds.json" } },
		responses: [json(401, { message: "who?" })],
	},
	{
		name: "adc/token-error-object",
		model: v25,
		context: simple(),
		files: { "creds.json": userCreds() },
		options: { project: "p-26", location: "us-central1", env: { GOOGLE_APPLICATION_CREDENTIALS: "<dir>/creds.json" } },
		responses: [json(400, { error: { code: 400, message: "bad things", status: "INVALID_ARGUMENT" } })],
	},
	{
		name: "adc/refresh-reauth-required",
		model: v25,
		context: simple(),
		files: { "creds.json": userCreds() },
		options: { project: "p-27", location: "us-central1", env: { GOOGLE_APPLICATION_CREDENTIALS: "<dir>/creds.json" } },
		responses: [json(400, { error: "invalid_grant", error_description: "reauth related error (invalid_rapt)", error_uri: "https://support.google.com/a/answer/9368756", error_subtype: "invalid_rapt" })],
	},
	{
		name: "adc/impersonated-service-account",
		model: v25,
		context: simple(),
		files: {
			[ADC]: {
				type: "impersonated_service_account",
				service_account_impersonation_url: "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/target@p.iam.gserviceaccount.com:generateAccessToken",
				delegates: ["d1@p.iam.gserviceaccount.com"],
				quota_project_id: "imp-quota",
				source_credentials: userCreds(),
			},
		},
		options: { project: "p-28", location: "us-central1" },
		responses: [token("at-source"), json(200, { accessToken: "at-imp", expireTime: "2026-01-01T01:00:00Z" }), done],
	},
	{
		name: "adc/impersonated-denied",
		model: v25,
		context: simple(),
		files: {
			[ADC]: {
				type: "impersonated_service_account",
				service_account_impersonation_url: "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/target@p.iam.gserviceaccount.com:generateAccessToken",
				source_credentials: userCreds(),
			},
		},
		options: { project: "p-29", location: "us-central1" },
		responses: [token("at-source"), json(403, { error: { code: 403, message: "Permission 'iam.serviceAccounts.getAccessToken' denied", status: "PERMISSION_DENIED" } })],
	},
	{
		name: "adc/impersonated-source-refresh-fails",
		model: v25,
		context: simple(),
		files: {
			[ADC]: {
				type: "impersonated_service_account",
				service_account_impersonation_url: "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/target@p.iam.gserviceaccount.com:generateIdToken",
				source_credentials: userCreds(),
			},
		},
		options: { project: "p-29", location: "us-central1" },
		responses: [json(400, { error: "invalid_grant", error_description: "Bad Request" })],
	},
	{
		name: "adc/impersonated-bad-url",
		model: v25,
		context: simple(),
		files: { [ADC]: { type: "impersonated_service_account", service_account_impersonation_url: "https://example.com/nothing", source_credentials: userCreds() } },
		options: { project: "p-30", location: "us-central1" },
		responses: [],
	},
	{
		name: "adc/gce-metadata-forbidden",
		model: v25,
		context: simple(),
		processEnv: { METADATA_SERVER_DETECTION: "assume-present", GCE_METADATA_HOST: "metadata.test" },
		options: { project: "p-31", location: "us-central1" },
		responses: [metadata(403, "scopes", { "content-type": "text/plain" })],
	},
	{
		name: "adc/gce-metadata-token-not-found",
		model: v25,
		context: simple(),
		processEnv: { METADATA_SERVER_DETECTION: "assume-present", GCE_METADATA_HOST: "metadata.test" },
		options: { project: "p-33", location: "us-central1" },
		responses: [metadata(404, "no scopes", { "content-type": "text/plain" })],
	},
	{
		name: "adc/caller-headers-win",
		model: v25,
		context: simple(),
		files: { "creds.json": userCreds({ quota_project_id: "quota-file" }) },
		options: {
			project: "p-34",
			location: "us-central1",
			env: { GOOGLE_APPLICATION_CREDENTIALS: "<dir>/creds.json" },
			headers: { Authorization: "Bearer caller", "X-Goog-User-Project": "caller-quota" },
		},
		responses: [token("at-ignored"), done],
	},
	{
		name: "adc/gce-ping-without-flavor",
		model: v25,
		context: simple(),
		processEnv: { METADATA_SERVER_DETECTION: null, GCE_METADATA_HOST: "metadata.test" },
		options: { project: "p-32", location: "us-central1" },
		responses: [json(200, "{}")],
	},
	// ---- workload identity federation: external_account (identity pool, AWS, executable) and
	// external_account_authorized_user; non-default universe domains; GOOGLE_VERTEX_BASE_URL.
	{
		name: "wif/file-text-key-file",
		model: v25,
		context: simple(),
		files: { "token.txt": "subject-from-file", "wif.json": wif({ credential_source: { file: "<dir>/token.txt" }, quota_project_id: "wif-quota" }) },
		options: { project: "w-1", location: "us-central1", env: { GOOGLE_APPLICATION_CREDENTIALS: "<dir>/wif.json" } },
		responses: [sts("at-wif"), done],
	},
	{
		name: "wif/file-json-workforce-well-known",
		model: v25,
		context: simple(),
		files: {
			"token.json": { id_token: "subject-from-json", other: 1 },
			[ADC]: wif({
				audience: WORKFORCE,
				subject_token_type: "urn:ietf:params:oauth:token-type:id_token",
				workforce_pool_user_project: "wf-user-project",
				credential_source: { file: "<dir>/token.json", format: { type: "json", subject_token_field_name: "id_token" } },
			}),
		},
		processEnv: { GOOGLE_CLOUD_QUOTA_PROJECT: "env-quota" },
		options: { project: "w-2", location: "us-central1" },
		responses: [sts("at-wf"), done],
	},
	{
		name: "wif/url-json-client-auth-impersonation",
		model: v25,
		context: simple(),
		files: {
			"wif.json": wif({
				client_id: "wif-client",
				client_secret: "wif-secret",
				service_account_impersonation_url: IMPERSONATE,
				service_account_impersonation: { token_lifetime_seconds: 600 },
				credential_source: { url: "http://token.test/token", headers: { Metadata: "True" }, format: { type: "json", subject_token_field_name: "access_token" } },
			}),
		},
		options: { project: "w-3", location: "us-central1", env: { GOOGLE_APPLICATION_CREDENTIALS: "<dir>/wif.json" } },
		responses: [json(200, { access_token: "subject-from-url" }), sts("at-sts"), json(200, { accessToken: "at-impersonated", expireTime: "2026-01-01T01:00:00Z" }), done],
	},
	{
		name: "wif/url-text-universe-domain",
		model: v25,
		context: simple(),
		files: { [ADC]: wif({ universe_domain: "example-universe.com", credential_source: { url: "http://token.test/plain" } }) },
		options: { project: "w-4", location: "us-central1" },
		responses: [{ status: 200, headers: { "content-type": "text/plain" }, chunks: ["subject-from-url-text"] }, sts("at-universe"), done],
	},
	{
		name: "wif/custom-token-url-workforce-with-client",
		model: v25,
		context: simple(),
		files: {
			"token.txt": "t",
			[ADC]: wif({ audience: WORKFORCE, workforce_pool_user_project: "wf", client_id: "cid", token_url: "https://sts.custom.test/v1/token", credential_source: { file: "<dir>/token.txt" } }),
		},
		options: { project: "w-5", location: "us-central1" },
		responses: [sts("at-custom"), done],
	},
	{
		name: "wif/sts-error",
		model: v25,
		context: simple(),
		files: { "token.txt": "t", [ADC]: wif({ credential_source: { file: "<dir>/token.txt" } }) },
		options: { project: "w-6", location: "us-central1" },
		responses: [json(400, { error: "invalid_grant", error_description: "The subject token is invalid.", error_uri: "https://example.test/err" })],
	},
	{
		name: "wif/sts-error-without-description",
		model: v25,
		context: simple(),
		files: { "token.txt": "t", [ADC]: wif({ credential_source: { file: "<dir>/token.txt" } }) },
		options: { project: "w-6", location: "us-central1" },
		responses: [json(401, { message: "nope" })],
	},
	{
		name: "wif/impersonation-denied",
		model: v25,
		context: simple(),
		files: { "token.txt": "t", [ADC]: wif({ service_account_impersonation_url: IMPERSONATE, credential_source: { file: "<dir>/token.txt" } }) },
		options: { project: "w-7", location: "us-central1" },
		responses: [sts("at-sts"), json(403, { error: { code: 403, message: "Permission denied", status: "PERMISSION_DENIED" } })],
	},
	{
		name: "wif/token-file-missing",
		model: v25,
		context: simple(),
		files: { [ADC]: wif({ credential_source: { file: "<dir>/absent.txt" } }) },
		options: { project: "w-8", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/token-file-empty",
		model: v25,
		context: simple(),
		files: { "token.txt": "", [ADC]: wif({ credential_source: { file: "<dir>/token.txt" } }) },
		options: { project: "w-8", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/token-json-field-missing",
		model: v25,
		context: simple(),
		files: { "token.json": { other: "x" }, [ADC]: wif({ credential_source: { file: "<dir>/token.json", format: { type: "json", subject_token_field_name: "id_token" } } }) },
		options: { project: "w-8", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/url-token-missing",
		model: v25,
		context: simple(),
		files: { [ADC]: wif({ credential_source: { url: "http://token.test/token", format: { type: "json", subject_token_field_name: "id_token" } } }) },
		options: { project: "w-8", location: "us-central1" },
		responses: [json(200, { nope: 1 })],
	},
	{
		name: "wif/no-credential-source",
		model: v25,
		context: simple(),
		files: { [ADC]: wif({ credential_source: undefined }) },
		options: { project: "w-9", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/file-and-url",
		model: v25,
		context: simple(),
		files: { [ADC]: wif({ credential_source: { file: "/x", url: "http://token.test/" } }) },
		options: { project: "w-9", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/bad-format-type",
		model: v25,
		context: simple(),
		files: { [ADC]: wif({ credential_source: { file: "/x", format: { type: "xml" } } }) },
		options: { project: "w-9", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/json-format-without-field",
		model: v25,
		context: simple(),
		files: { [ADC]: wif({ credential_source: { file: "/x", format: { type: "json" } } }) },
		options: { project: "w-9", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/workforce-project-on-workload-audience",
		model: v25,
		context: simple(),
		files: { [ADC]: wif({ workforce_pool_user_project: "p", credential_source: { file: "/x" } }) },
		options: { project: "w-9", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/empty-credential-source",
		model: v25,
		context: simple(),
		files: { [ADC]: wif({ credential_source: {} }) },
		options: { project: "w-9", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/key-file-constructor-error-falls-back",
		model: v25,
		context: simple(),
		files: { "wif.json": wif({ credential_source: undefined }) },
		options: { project: "w-10", location: "us-central1", env: { GOOGLE_APPLICATION_CREDENTIALS: "<dir>/wif.json" } },
		responses: [],
	},
	{
		name: "wif/aws-env-credentials",
		model: v25,
		context: simple(),
		files: { [ADC]: wif({ subject_token_type: "urn:ietf:params:aws:token-type:aws4_request", credential_source: AWS_SOURCE() }) },
		processEnv: { AWS_REGION: "us-east-2", AWS_ACCESS_KEY_ID: "AKIDEXAMPLE", AWS_SECRET_ACCESS_KEY: "fake-secret-key", AWS_SESSION_TOKEN: "fake-session" },
		options: { project: "w-11", location: "us-central1" },
		responses: [sts("at-aws"), done],
	},
	{
		name: "wif/aws-env-default-region-no-token",
		model: v25,
		context: simple(),
		files: { [ADC]: wif({ subject_token_type: "urn:ietf:params:aws:token-type:aws4_request", credential_source: AWS_SOURCE() }) },
		processEnv: { AWS_DEFAULT_REGION: "eu-west-1", AWS_ACCESS_KEY_ID: "AKIDEXAMPLE", AWS_SECRET_ACCESS_KEY: "fake-secret-key" },
		options: { project: "w-11", location: "us-central1" },
		responses: [sts("at-aws2"), done],
	},
	{
		name: "wif/aws-metadata-imdsv2",
		model: v25,
		context: simple(),
		files: { [ADC]: wif({ subject_token_type: "urn:ietf:params:aws:token-type:aws4_request", credential_source: AWS_SOURCE({ imdsv2_session_token_url: "http://imds.test/latest/api/token" }) }) },
		options: { project: "w-12", location: "us-central1" },
		responses: [
			text("imds-session"),
			text("us-west-2b"),
			text("imds-session"),
			text("fake-role"),
			json(200, { Code: "Success", AccessKeyId: "AKIDMETA", SecretAccessKey: "meta-secret", Token: "meta-token" }),
			sts("at-aws-meta"),
			done,
		],
	},
	{
		name: "wif/aws-bad-environment-id",
		model: v25,
		context: simple(),
		files: { [ADC]: wif({ credential_source: AWS_SOURCE({ environment_id: "aws2" }) }) },
		options: { project: "w-13", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/aws-missing-verification-url",
		model: v25,
		context: simple(),
		files: { [ADC]: wif({ credential_source: AWS_SOURCE({ regional_cred_verification_url: undefined }) }) },
		options: { project: "w-13", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/aws-missing-region-url",
		model: v25,
		context: simple(),
		files: { [ADC]: wif({ credential_source: AWS_SOURCE({ region_url: undefined }) }) },
		options: { project: "w-13", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/aws-missing-role-url",
		model: v25,
		context: simple(),
		files: { [ADC]: wif({ credential_source: AWS_SOURCE({ url: undefined }) }) },
		processEnv: { AWS_REGION: "us-east-2" },
		options: { project: "w-13", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/aws-metadata-error-not-retried",
		model: v25,
		context: simple(),
		files: { [ADC]: wif({ credential_source: AWS_SOURCE() }) },
		options: { project: "w-13", location: "us-central1" },
		responses: [{ status: 503, headers: { "content-type": "text/plain" }, chunks: ["imds down"] }],
	},
	{
		name: "wif/certificate-neither-option",
		model: v25,
		context: simple(),
		files: { [ADC]: wif({ credential_source: { certificate: { trust_chain_path: "/x" } } }) },
		options: { project: "w-13", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/certificate-both-options",
		model: v25,
		context: simple(),
		files: { [ADC]: wif({ credential_source: { certificate: { use_default_certificate_config: true, certificate_config_location: "/x" } } }) },
		options: { project: "w-13", location: "us-central1" },
		responses: [],
	},
	...certificateScenarios(),
	{
		name: "wif/executable-not-allowed",
		model: v25,
		context: simple(),
		files: { [ADC]: wif({ credential_source: { executable: { command: "<dir>/never.sh" } } }) },
		options: { project: "w-14", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/executable-oidc-env",
		model: v25,
		context: simple(),
		files: {
			"exec.sh": script(`printf '{"version":1,"success":true,"token_type":"urn:ietf:params:oauth:token-type:jwt","expiration_time":4102444800,"id_token":"%s|%s|%s|%s|%s|%s"}' "$GOOGLE_EXTERNAL_ACCOUNT_AUDIENCE" "$GOOGLE_EXTERNAL_ACCOUNT_TOKEN_TYPE" "$GOOGLE_EXTERNAL_ACCOUNT_INTERACTIVE" "\${GOOGLE_EXTERNAL_ACCOUNT_IMPERSONATED_EMAIL-unset}" "\${GOOGLE_EXTERNAL_ACCOUNT_OUTPUT_FILE-unset}" "$1"`),
			[ADC]: wif({ service_account_impersonation_url: IMPERSONATE, credential_source: { executable: { command: '<dir>/exec.sh "two words"', timeout_millis: 10000 } } }),
		},
		executables: ["exec.sh"],
		processEnv: { GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES: "1" },
		options: { project: "w-15", location: "us-central1" },
		responses: [sts("at-exec"), json(200, { accessToken: "at-exec-imp", expireTime: "2026-01-01T01:00:00Z" }), done],
	},
	{
		name: "wif/executable-saml-output-file-written",
		model: v25,
		context: simple(),
		files: {
			"exec.sh": script(`printf '{"version":1,"success":true,"token_type":"urn:ietf:params:oauth:token-type:saml2","expiration_time":4102444800,"saml_response":"saml-%s"}' "$GOOGLE_EXTERNAL_ACCOUNT_OUTPUT_FILE"`),
			[ADC]: wif({ subject_token_type: "urn:ietf:params:oauth:token-type:saml2", credential_source: { executable: { command: "<dir>/exec.sh", output_file: "<dir>/out.json" } } }),
		},
		executables: ["exec.sh"],
		processEnv: { GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES: "1" },
		options: { project: "w-16", location: "us-central1" },
		responses: [sts("at-saml"), done],
	},
	{
		name: "wif/executable-cached-output-file",
		model: v25,
		context: simple(),
		files: {
			"never.sh": script("exit 7"),
			"out.json": { version: 1, success: true, token_type: "urn:ietf:params:oauth:token-type:id_token", expiration_time: 4102444800, id_token: "cached-token" },
			[ADC]: wif({ credential_source: { executable: { command: "<dir>/never.sh", output_file: "<dir>/out.json" } } }),
		},
		executables: ["never.sh"],
		processEnv: { GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES: "1" },
		options: { project: "w-17", location: "us-central1" },
		responses: [sts("at-cached"), done],
	},
	{
		name: "wif/executable-expired-cache-runs-executable",
		model: v25,
		context: simple(),
		files: {
			"exec.sh": script(`printf '{"version":1,"success":true,"token_type":"urn:ietf:params:oauth:token-type:id_token","expiration_time":4102444800,"id_token":"fresh"}'`),
			"out.json": { version: 1, success: true, token_type: "urn:ietf:params:oauth:token-type:id_token", expiration_time: 1000, id_token: "stale" },
			[ADC]: wif({ credential_source: { executable: { command: "<dir>/exec.sh", output_file: "<dir>/out.json" } } }),
		},
		executables: ["exec.sh"],
		processEnv: { GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES: "1" },
		options: { project: "w-17", location: "us-central1" },
		responses: [sts("at-fresh"), done],
	},
	{
		name: "wif/executable-cached-output-file-invalid",
		model: v25,
		context: simple(),
		files: { "never.sh": script("exit 7"), "out.json": "{ nope", [ADC]: wif({ credential_source: { executable: { command: "<dir>/never.sh", output_file: "<dir>/out.json" } } }) },
		executables: ["never.sh"],
		processEnv: { GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES: "1" },
		options: { project: "w-17", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/executable-exit-code",
		model: v25,
		context: simple(),
		files: { "exec.sh": script("echo out; echo err >&2; exit 3"), [ADC]: wif({ credential_source: { executable: { command: "<dir>/exec.sh" } } }) },
		executables: ["exec.sh"],
		processEnv: { GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES: "1" },
		options: { project: "w-18", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/executable-unsuccessful",
		model: v25,
		context: simple(),
		files: { "exec.sh": script(`printf '{"version":1,"success":false,"code":"401","message":"Caller not authorized."}'`), [ADC]: wif({ credential_source: { executable: { command: "<dir>/exec.sh" } } }) },
		executables: ["exec.sh"],
		processEnv: { GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES: "1" },
		options: { project: "w-18", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/executable-invalid-json",
		model: v25,
		context: simple(),
		files: { "exec.sh": script("echo not-json"), [ADC]: wif({ credential_source: { executable: { command: "<dir>/exec.sh" } } }) },
		executables: ["exec.sh"],
		processEnv: { GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES: "1" },
		options: { project: "w-18", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/executable-bad-token-type",
		model: v25,
		context: simple(),
		files: { "exec.sh": script(`printf '{"version":1,"success":true,"token_type":"other","id_token":"x"}'`), [ADC]: wif({ credential_source: { executable: { command: "<dir>/exec.sh" } } }) },
		executables: ["exec.sh"],
		processEnv: { GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES: "1" },
		options: { project: "w-18", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/executable-version-too-new",
		model: v25,
		context: simple(),
		files: { "exec.sh": script(`printf '{"version":2,"success":true,"token_type":"urn:ietf:params:oauth:token-type:jwt","id_token":"x"}'`), [ADC]: wif({ credential_source: { executable: { command: "<dir>/exec.sh" } } }) },
		executables: ["exec.sh"],
		processEnv: { GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES: "1" },
		options: { project: "w-18", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/executable-expired-response",
		model: v25,
		context: simple(),
		files: { "exec.sh": script(`printf '{"version":1,"success":true,"token_type":"urn:ietf:params:oauth:token-type:jwt","expiration_time":1000,"id_token":"x"}'`), [ADC]: wif({ credential_source: { executable: { command: "<dir>/exec.sh" } } }) },
		executables: ["exec.sh"],
		processEnv: { GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES: "1" },
		options: { project: "w-18", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/executable-output-file-needs-expiration",
		model: v25,
		context: simple(),
		files: { "exec.sh": script(`printf '{"version":1,"success":true,"token_type":"urn:ietf:params:oauth:token-type:jwt","id_token":"x"}'`), [ADC]: wif({ credential_source: { executable: { command: "<dir>/exec.sh", output_file: "<dir>/missing-out.json" } } }) },
		executables: ["exec.sh"],
		processEnv: { GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES: "1" },
		options: { project: "w-18", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/executable-timeout-out-of-range",
		model: v25,
		context: simple(),
		files: { [ADC]: wif({ credential_source: { executable: { command: "<dir>/exec.sh", timeout_millis: 1000 } } }) },
		options: { project: "w-18", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/executable-timeout",
		model: v25,
		context: simple(),
		files: { "slow.sh": script("exec sleep 600"), [ADC]: wif({ credential_source: { executable: { command: "<dir>/slow.sh", timeout_millis: 5000 } } }) },
		executables: ["slow.sh"],
		processEnv: { GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES: "1" },
		options: { project: "w-18", location: "us-central1" },
		responses: [],
	},
	{
		name: "wif/authorized-user",
		model: v25,
		context: simple(),
		files: { "eau.json": externalUser({ quota_project_id: "eau-quota" }) },
		options: { project: "w-19", location: "us-central1", env: { GOOGLE_APPLICATION_CREDENTIALS: "<dir>/eau.json" } },
		responses: [json(200, { access_token: "at-eau", expires_in: 3600 }), done],
	},
	{
		name: "wif/authorized-user-universe-and-token-url",
		model: v25,
		context: simple(),
		files: { [ADC]: externalUser({ universe_domain: "example-universe.com", client_secret: undefined }) },
		options: { project: "w-20", location: "us-central1" },
		responses: [json(200, { access_token: "at-eau-u", expires_in: 3600 }), done],
	},
	{
		name: "wif/authorized-user-custom-token-url",
		model: v25,
		context: simple(),
		files: { [ADC]: externalUser({ token_url: "https://sts.custom.test/v1/oauthtoken" }) },
		options: { project: "w-20", location: "us-central1" },
		responses: [json(200, { access_token: "at-eau-c", expires_in: 3600 }), done],
	},
	{
		name: "wif/authorized-user-refresh-error",
		model: v25,
		context: simple(),
		files: { [ADC]: externalUser() },
		options: { project: "w-21", location: "us-central1" },
		responses: [json(400, { error: "invalid_grant", error_description: "Token has been revoked." })],
	},
	{
		name: "wif/impersonated-external-source",
		model: v25,
		context: simple(),
		files: {
			"token.txt": "t",
			[ADC]: {
				type: "impersonated_service_account",
				service_account_impersonation_url: IMPERSONATE,
				source_credentials: wif({ credential_source: { file: "<dir>/token.txt" }, universe_domain: "example-universe.com" }),
			},
		},
		options: { project: "w-22", location: "us-central1" },
		responses: [sts("at-src"), json(200, { accessToken: "at-imp", expireTime: "2026-01-01T01:00:00Z" }), done],
	},
	{
		name: "universe/service-account-self-signed-jwt",
		model: v25,
		context: simple(),
		files: { "sa.json": serviceAccount({ universe_domain: "example-universe.com", quota_project_id: "sa-quota" }) },
		options: { project: "u-1", location: "us-central1", env: { GOOGLE_APPLICATION_CREDENTIALS: "<dir>/sa.json" } },
		responses: [done],
	},
	{
		name: "universe/service-account-self-signed-jwt-adc-no-key-id",
		model: v25,
		context: simple(),
		files: { [ADC]: serviceAccount({ universe_domain: "example-universe.com", private_key_id: undefined }) },
		options: { project: "u-2", location: "us-central1" },
		responses: [done],
	},
	{
		name: "universe/authorized-user-unchanged",
		model: v25,
		context: simple(),
		files: { [ADC]: userCreds({ universe_domain: "example-universe.com" }) },
		options: { project: "u-3", location: "us-central1" },
		responses: [token("at-u"), done],
	},
	{
		name: "universe/impersonated-explicit-matching",
		model: v25,
		context: simple(),
		files: { [ADC]: { type: "impersonated_service_account", universe_domain: "example-universe.com", service_account_impersonation_url: IMPERSONATE, source_credentials: userCreds({ universe_domain: "example-universe.com" }) } },
		options: { project: "u-4", location: "us-central1" },
		responses: [token("at-src"), json(200, { accessToken: "at-imp-u", expireTime: "2026-01-01T01:00:00Z" }), done],
	},
	{
		name: "universe/impersonated-explicit-mismatch",
		model: v25,
		context: simple(),
		files: { [ADC]: { type: "impersonated_service_account", universe_domain: "example-universe.com", service_account_impersonation_url: IMPERSONATE, source_credentials: userCreds() } },
		options: { project: "u-5", location: "us-central1" },
		responses: [],
	},
	{
		name: "base-url/env-override-adc",
		model: v25,
		context: simple(),
		files: { [ADC]: userCreds() },
		processEnv: { GOOGLE_VERTEX_BASE_URL: " https://vertex-proxy.test/root/ " },
		options: { project: "b-1", location: "europe-west4" },
		responses: [token("at-b"), done],
	},
	{
		name: "base-url/env-override-api-key",
		model: v25,
		context: simple(),
		processEnv: { GOOGLE_VERTEX_BASE_URL: "https://vertex-proxy.test" },
		options: { apiKey: "vertex-key" },
		responses: [done],
	},
	{
		name: "base-url/model-base-wins-over-env",
		model: proxied,
		context: simple(),
		files: { [ADC]: userCreds() },
		processEnv: { GOOGLE_VERTEX_BASE_URL: "https://ignored.test" },
		options: { project: "b-3", location: "us-central1" },
		responses: [token("at-b3"), done],
	},
];

const results = [];
for (const scenario of scenarios) results.push(await run(scenario));
globalThis.Date = RealDate;
assertOffline();
const out = join(here, "google-vertex-adc.json");
writeFileSync(
	out,
	`${JSON.stringify({ generator: "crates/ai/fixtures/generate-vertex-adc.mjs", now: NOW, privateKeyDer: PRIVATE_KEY_DER, certificates: CERTIFICATES, scenarios: results }, null, "\t")}\n`,
);
console.log(`wrote ${results.length} scenarios to ${out}`);
