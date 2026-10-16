// Generates crates/ai/fixtures/bedrock-credentials.json from this worktree's TypeScript Bedrock provider
// (`packages/ai/src/api/bedrock-converse-stream.ts`) and the AWS SDK default credential chain it relies on: SSO
// (sso-session with token refresh, and legacy), credential_process, web identity (env and profile), assume role with
// a source profile, container credentials and IMDSv2/v1. Every AWS endpoint is one loopback HTTP server (the SDK's
// endpoint overrides AWS_ENDPOINT_URL_SSO/_SSO_OIDC/_STS, AWS_EC2_METADATA_SERVICE_ENDPOINT,
// AWS_CONTAINER_CREDENTIALS_FULL_URI, and the model's base URL for Bedrock itself); offline-guard.mjs refuses anything
// else. Each scenario writes files into a temp HOME, sets the process env the SDK reads, and records the credential
// requests (method, path, query, chosen headers, body), the Bedrock request's signing key and session token, files
// after the run, and the final message. Paths are recorded as <home>, the port as <port>; `<now+N>` in a file is the
// ISO time N seconds from now, written at run time.
// Usage: node crates/ai/fixtures/generate-bedrock-credentials.mjs
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync, existsSync } from "node:fs";
import { createHash } from "node:crypto";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
for (const name of Object.keys(process.env)) {
	if (/^(AWS_|PI_|ULTRON_CODING|ANTHROPIC|OPENAI|GOOGLE|GEMINI)/.test(name)) delete process.env[name];
}
const { EventStreamCodec } = await import("@smithy/core/event-streams");
const api = await import(pathToFileURL(join(root, "packages/ai/src/api/bedrock-converse-stream.ts")).href);

const encoder = new TextEncoder();
const decoder = new TextDecoder();
const codec = new EventStreamCodec((b) => decoder.decode(b), (t) => encoder.encode(t));
const header = (value) => ({ type: "string", value });
const event = (type, body) =>
	Buffer.from(codec.encode({ headers: { ":event-type": header(type), ":message-type": header("event"), ":content-type": header("application/json") }, body: encoder.encode(JSON.stringify(body)) }));
const converse = Buffer.concat([
	event("messageStart", { role: "assistant" }),
	event("contentBlockDelta", { contentBlockIndex: 0, delta: { text: "ok" } }),
	event("contentBlockStop", { contentBlockIndex: 0 }),
	event("messageStop", { stopReason: "end_turn" }),
	event("metadata", { usage: { inputTokens: 5, outputTokens: 1, totalTokens: 6 }, metrics: { latencyMs: 3 } }),
]);

const RECORDED_HEADERS = ["authorization", "content-type", "x-amz-sso_bearer_token", "x-aws-ec2-metadata-token", "x-aws-ec2-metadata-token-ttl-seconds"];

let current = null;
const server = createServer((req, res) => {
	const chunks = [];
	req.on("data", (c) => chunks.push(c));
	req.on("end", () => {
		const url = new URL(req.url, "http://127.0.0.1");
		const body = Buffer.concat(chunks).toString("utf8");
		if (req.method === "POST" && url.pathname.endsWith("/converse-stream")) {
			const auth = req.headers.authorization ?? "";
			current.bedrock = {
				credential: /Credential=([^/]+)\//.exec(auth)?.[1] ?? null,
				sessionToken: req.headers["x-amz-security-token"] ?? null,
			};
			res.writeHead(200, { "content-type": "application/vnd.amazon.eventstream", "x-amzn-requestid": "req-1" });
			res.end(converse);
			return;
		}
		const headers = {};
		for (const name of RECORDED_HEADERS) {
			const value = req.headers[name];
			if (value === undefined) continue;
			headers[name] = name === "authorization" && value.startsWith("AWS4-HMAC-SHA256") ? `AWS4-HMAC-SHA256 Credential=${/Credential=([^,]+),/.exec(value)?.[1].replace(/\/\d{8}\//, "/<date>/")}` : value;
		}
		const query = [...url.searchParams.entries()].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
		current.requests.push({ method: req.method, path: url.pathname, query, headers, body: normalize(body) });
		const key = `${req.method} ${url.pathname}`;
		const list = current.routes[key];
		const canned = list?.[Math.min((current.used[key] = (current.used[key] ?? -1) + 1), list.length - 1)];
		if (!canned) {
			res.writeHead(404, { "content-type": "text/plain" });
			res.end("no route");
			return;
		}
		res.writeHead(canned.status ?? 200, canned.headers ?? { "content-type": "application/json" });
		res.end(typeof canned.body === "string" ? canned.body : JSON.stringify(canned.body));
	});
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;
const origin = `http://127.0.0.1:${port}`;

let home = "";
const normalize = (text) =>
	text
		.replaceAll(home, "<home>")
		.replaceAll(String(port), "<port>")
		.replace(/aws-sdk-js-session-\d+/g, "aws-sdk-js-session-<now>")
		.replace(/aws-sdk-js-\d+/g, "aws-sdk-js-<now>")
		.replace(/\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z/g, (iso) => (iso.startsWith("20") && iso < "2090" && iso > "2010" ? "<iso>" : iso));
const expand = (text) =>
	text
		.replaceAll("<home>", home)
		.replaceAll("<port>", String(port))
		.replace(/<now\+(-?\d+)>/g, (_, s) => new Date(Date.now() + Number(s) * 1000).toISOString());

const sha1 = (text) => createHash("sha1").update(text).digest("hex");
const model = {
	id: "amazon.nova-pro-v1:0",
	name: "Nova Pro",
	api: "bedrock-converse-stream",
	provider: "amazon-bedrock",
	baseUrl: "<origin>",
	reasoning: false,
	input: ["text"],
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 200000,
	maxTokens: 8192,
};
const context = { messages: [{ role: "user", content: "hi", timestamp: 2 }] };

async function run(scenario) {
	home = mkdtempSync(join(tmpdir(), "bedrock-creds-"));
	const files = scenario.files ?? {};
	for (const [path, content] of Object.entries(files)) {
		const full = join(home, path);
		mkdirSync(dirname(full), { recursive: true });
		writeFileSync(full, expand(content));
		if (path.endsWith(".sh")) chmodSync(full, 0o755);
	}
	const processEnv = { HOME: "<home>", AWS_CONFIG_FILE: "<home>/.aws/config", AWS_SHARED_CREDENTIALS_FILE: "<home>/.aws/credentials", AWS_EC2_METADATA_DISABLED: "true", AWS_BEDROCK_FORCE_HTTP1: "1", ...scenario.processEnv };
	for (const [name, value] of Object.entries(processEnv)) if (value !== null) process.env[name] = expand(value);
	current = { requests: [], routes: scenario.routes ?? {}, used: {}, bedrock: null };
	const m = { ...model, baseUrl: origin };
	if (scenario.warm) {
		// A first call in the same process (upstream's SSO refresh throttle is process-wide); only the second is recorded.
		await api.stream(m, context, { ...(scenario.options ?? {}) }).result();
		current = { requests: [], routes: scenario.routes ?? {}, used: {}, bedrock: null };
	}
	let result;
	try {
		const s = api.stream(m, context, { ...(scenario.options ?? {}) });
		result = await s.result();
	} catch (e) {
		result = { thrown: e.message };
	}
	for (const name of Object.keys(processEnv)) delete process.env[name];
	const after = {};
	for (const path of scenario.after ?? []) {
		const full = join(home, path);
		after[path] = existsSync(full) ? normalize(readFileSync(full, "utf8")) : null;
	}
	const record = {
		name: scenario.name,
		warm: scenario.warm ?? false,
		options: scenario.options ?? {},
		processEnv,
		files,
		routes: scenario.routes ?? {},
		requests: current.requests,
		bedrock: current.bedrock,
		after,
		stopReason: result.stopReason ?? null,
		errorMessage: result.errorMessage ? normalize(result.errorMessage) : null,
		diagnostics: result.diagnostics ? result.diagnostics.map(({ timestamp, ...rest }) => rest) : null,
	};
	rmSync(home, { recursive: true, force: true });
	return record;
}

const xml = (body) => ({ status: 200, headers: { "content-type": "text/xml" }, body });
const stsWebIdentity = xml(
	`<AssumeRoleWithWebIdentityResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/"><AssumeRoleWithWebIdentityResult><Credentials><AccessKeyId>ASIAWEBID</AccessKeyId><SecretAccessKey>web-secret</SecretAccessKey><SessionToken>web-session</SessionToken><Expiration>2099-01-01T00:00:00Z</Expiration></Credentials><AssumedRoleUser><Arn>arn:aws:sts::123456789012:assumed-role/r/s</Arn><AssumedRoleId>AROA:s</AssumedRoleId></AssumedRoleUser></AssumeRoleWithWebIdentityResult><ResponseMetadata><RequestId>r1</RequestId></ResponseMetadata></AssumeRoleWithWebIdentityResponse>`,
);
const stsAssumeRole = xml(
	`<AssumeRoleResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/"><AssumeRoleResult><Credentials><AccessKeyId>ASIAROLE</AccessKeyId><SecretAccessKey>role-secret</SecretAccessKey><SessionToken>role-session</SessionToken><Expiration>2099-01-01T00:00:00Z</Expiration></Credentials><AssumedRoleUser><Arn>arn:aws:sts::123456789012:assumed-role/r/s</Arn><AssumedRoleId>AROA:s</AssumedRoleId></AssumedRoleUser></AssumeRoleResult><ResponseMetadata><RequestId>r2</RequestId></ResponseMetadata></AssumeRoleResponse>`,
);
const stsError = {
	status: 400,
	headers: { "content-type": "text/xml" },
	body: `<ErrorResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/"><Error><Type>Sender</Type><Code>InvalidIdentityToken</Code><Message>Token is not valid</Message></Error><RequestId>r3</RequestId></ErrorResponse>`,
};
const ssoCreds = { status: 200, body: { roleCredentials: { accessKeyId: "ASIASSO", secretAccessKey: "sso-secret", sessionToken: "sso-session", expiration: 4070908800000 } } };
const ssoSessionConfig = (session = "corp") => `[profile sso]\nsso_session = ${session}\nsso_account_id = 111122223333\nsso_role_name = Dev\n\n[sso-session ${session}]\nsso_start_url = https://example.awsapps.com/start\nsso_region = us-east-1\n`;
const legacyConfig = `[profile legacy]\nsso_start_url = https://legacy.example.com/start\nsso_account_id = 111122223333\nsso_region = eu-west-1\nsso_role_name = Dev\n`;
const ssoEnv = { AWS_ENDPOINT_URL_SSO: "http://127.0.0.1:<port>", AWS_ENDPOINT_URL_SSO_OIDC: "http://127.0.0.1:<port>" };
const stsEnv = { AWS_ENDPOINT_URL_STS: "http://127.0.0.1:<port>" };
const procJson = (fields) => `#!/bin/sh\necho run >> "<home>/runs"\necho '${JSON.stringify(fields)}'\n`;
const imdsCreds = { status: 200, body: { Code: "Success", AccessKeyId: "ASIAIMDS", SecretAccessKey: "imds-secret", Token: "imds-session", Expiration: "2099-01-01T00:00:00Z" } };

const scenarios = [
	{
		name: "sso-session",
		options: { profile: "sso" },
		processEnv: ssoEnv,
		files: { ".aws/config": ssoSessionConfig("corp"), [`.aws/sso/cache/${sha1("corp")}.json`]: JSON.stringify({ accessToken: "sso-at", expiresAt: "2099-01-01T00:00:00Z" }) },
		routes: { "GET /federation/credentials": [ssoCreds] },
	},
	{
		name: "sso-session-refresh",
		options: { profile: "sso" },
		processEnv: ssoEnv,
		files: {
			".aws/config": ssoSessionConfig("corp"),
			[`.aws/sso/cache/${sha1("corp")}.json`]: JSON.stringify({ accessToken: "old-at", expiresAt: "<now+60>", clientId: "cid", clientSecret: "csecret", refreshToken: "rt-1", region: "us-east-1" }),
		},
		routes: { "POST /token": [{ status: 200, body: { accessToken: "new-at", expiresIn: 3600, refreshToken: "rt-2", tokenType: "Bearer" } }], "GET /federation/credentials": [ssoCreds] },
		after: [`.aws/sso/cache/${sha1("corp")}.json`],
	},
	{
		name: "sso-session-refresh-fails-uses-valid-token",
		options: { profile: "sso" },
		processEnv: ssoEnv,
		files: {
			".aws/config": ssoSessionConfig("corp2"),
			[`.aws/sso/cache/${sha1("corp2")}.json`]: JSON.stringify({ accessToken: "old-at", expiresAt: "<now+60>", clientId: "cid", clientSecret: "csecret", refreshToken: "rt-1" }),
		},
		routes: { "POST /token": [{ status: 400, body: { error: "invalid_grant" } }], "GET /federation/credentials": [ssoCreds] },
		after: [`.aws/sso/cache/${sha1("corp2")}.json`],
	},
	{
		name: "sso-session-refresh-drops-refresh-token",
		options: { profile: "sso" },
		processEnv: ssoEnv,
		files: {
			".aws/config": ssoSessionConfig("corp6"),
			[`.aws/sso/cache/${sha1("corp6")}.json`]: JSON.stringify({ accessToken: "old-at", expiresAt: "<now+60>", clientId: "cid", clientSecret: "csecret", refreshToken: "rt-1", region: "us-east-1" }),
		},
		routes: { "POST /token": [{ status: 200, body: { accessToken: "new-at", expiresIn: 3600, tokenType: "Bearer" } }], "GET /federation/credentials": [ssoCreds] },
		after: [`.aws/sso/cache/${sha1("corp6")}.json`],
	},
	{
		name: "sso-session-refresh-throttled",
		warm: true,
		options: { profile: "sso" },
		processEnv: ssoEnv,
		files: {
			".aws/config": ssoSessionConfig("corp7"),
			[`.aws/sso/cache/${sha1("corp7")}.json`]: JSON.stringify({ accessToken: "old-at", expiresAt: "<now+60>", clientId: "cid", clientSecret: "csecret", refreshToken: "rt-1" }),
		},
		routes: { "POST /token": [{ status: 400, body: { error: "invalid_grant" } }], "GET /federation/credentials": [ssoCreds] },
		after: [`.aws/sso/cache/${sha1("corp7")}.json`],
	},
	{
		name: "sso-session-expired-no-refresh-token",
		options: { profile: "sso" },
		processEnv: ssoEnv,
		files: { ".aws/config": ssoSessionConfig("corp3"), [`.aws/sso/cache/${sha1("corp3")}.json`]: JSON.stringify({ accessToken: "old-at", expiresAt: "<now+-60>" }) },
	},
	{
		name: "sso-session-missing-cache",
		options: { profile: "sso" },
		processEnv: ssoEnv,
		files: { ".aws/config": ssoSessionConfig("corp4") },
	},
	{
		name: "sso-session-unknown-session",
		options: { profile: "sso" },
		processEnv: ssoEnv,
		files: { ".aws/config": `[profile sso]\nsso_session = nowhere\nsso_account_id = 1\nsso_role_name = Dev\n` },
	},
	{
		name: "sso-legacy",
		options: { profile: "legacy" },
		processEnv: ssoEnv,
		files: { ".aws/config": legacyConfig, [`.aws/sso/cache/${sha1("https://legacy.example.com/start")}.json`]: JSON.stringify({ accessToken: "legacy-at", expiresAt: "2099-01-01T00:00:00Z" }) },
		routes: { "GET /federation/credentials": [ssoCreds] },
	},
	{
		name: "sso-legacy-expired",
		options: { profile: "legacy" },
		processEnv: ssoEnv,
		files: { ".aws/config": legacyConfig, [`.aws/sso/cache/${sha1("https://legacy.example.com/start")}.json`]: JSON.stringify({ accessToken: "legacy-at", expiresAt: "2001-01-01T00:00:00Z" }) },
	},
	{
		name: "sso-legacy-missing-fields",
		options: { profile: "legacy" },
		processEnv: ssoEnv,
		files: { ".aws/config": `[profile legacy]\nsso_start_url = https://legacy.example.com/start\nsso_region = eu-west-1\n` },
	},
	{
		name: "sso-get-role-credentials-denied",
		options: { profile: "sso" },
		processEnv: ssoEnv,
		files: { ".aws/config": ssoSessionConfig("corp5"), [`.aws/sso/cache/${sha1("corp5")}.json`]: JSON.stringify({ accessToken: "sso-at", expiresAt: "2099-01-01T00:00:00Z" }) },
		routes: { "GET /federation/credentials": [{ status: 401, headers: { "content-type": "application/json", "x-amzn-errortype": "UnauthorizedException" }, body: { message: "Session token not found or invalid" } }] },
	},
	{
		name: "sso-invalid-role-credentials",
		options: { profile: "sso" },
		processEnv: ssoEnv,
		files: { ".aws/config": ssoSessionConfig("corp6"), [`.aws/sso/cache/${sha1("corp6")}.json`]: JSON.stringify({ accessToken: "sso-at", expiresAt: "2099-01-01T00:00:00Z" }) },
		routes: { "GET /federation/credentials": [{ status: 200, body: { roleCredentials: { accessKeyId: "ASIASSO", secretAccessKey: "sso-secret" } } }] },
	},
	{
		name: "credential-process",
		options: { profile: "proc" },
		files: { ".aws/config": `[profile proc]\ncredential_process = <home>/proc.sh\n`, "proc.sh": procJson({ Version: 1, AccessKeyId: "AKIDPROC", SecretAccessKey: "proc-secret", SessionToken: "proc-session", Expiration: "2099-01-01T00:00:00Z" }) },
		after: ["runs"],
	},
	{
		name: "credential-process-default-profile-no-token",
		files: { ".aws/config": `[default]\ncredential_process = <home>/proc.sh --flag\n`, "proc.sh": procJson({ Version: 1, AccessKeyId: "AKIDPROC", SecretAccessKey: "proc-secret" }) },
		after: ["runs"],
	},
	{
		name: "credential-process-bad-version-falls-through",
		options: { profile: "proc" },
		files: { ".aws/config": `[profile proc]\ncredential_process = <home>/proc.sh\n`, "proc.sh": procJson({ Version: 2, AccessKeyId: "AKIDPROC", SecretAccessKey: "proc-secret" }) },
		after: ["runs"],
	},
	{
		name: "credential-process-expired",
		options: { profile: "proc" },
		files: { ".aws/config": `[profile proc]\ncredential_process = <home>/proc.sh\n`, "proc.sh": procJson({ Version: 1, AccessKeyId: "AKIDPROC", SecretAccessKey: "proc-secret", Expiration: "2001-01-01T00:00:00Z" }) },
		after: ["runs"],
	},
	{
		name: "credential-process-fails",
		options: { profile: "proc" },
		files: { ".aws/config": `[profile proc]\ncredential_process = <home>/proc.sh\n`, "proc.sh": `#!/bin/sh\necho run >> "<home>/runs"\necho nope >&2\nexit 3\n` },
		after: ["runs"],
	},
	{
		name: "credential-process-static-keys-win",
		options: { profile: "both" },
		files: {
			".aws/config": `[profile both]\ncredential_process = <home>/proc.sh\n`,
			".aws/credentials": `[both]\naws_access_key_id = AKIDSTATIC\naws_secret_access_key = static-secret\n`,
			"proc.sh": procJson({ Version: 1, AccessKeyId: "AKIDPROC", SecretAccessKey: "proc-secret" }),
		},
		after: ["runs"],
	},
	{
		name: "web-identity-env",
		processEnv: { ...stsEnv, AWS_WEB_IDENTITY_TOKEN_FILE: "<home>/token", AWS_ROLE_ARN: "arn:aws:iam::123456789012:role/web", AWS_ROLE_SESSION_NAME: "my-session" },
		files: { token: "web-token-1" },
		routes: { "POST /": [stsWebIdentity] },
	},
	{
		name: "web-identity-env-default-session-name",
		processEnv: { ...stsEnv, AWS_WEB_IDENTITY_TOKEN_FILE: "<home>/token", AWS_ROLE_ARN: "arn:aws:iam::123456789012:role/web" },
		files: { token: "web-token-2\n" },
		routes: { "POST /": [stsWebIdentity] },
	},
	{
		name: "web-identity-profile",
		options: { profile: "web" },
		processEnv: stsEnv,
		files: { ".aws/config": `[profile web]\nweb_identity_token_file = <home>/token\nrole_arn = arn:aws:iam::123456789012:role/web\nrole_session_name = prof-session\n`, token: "web-token-3" },
		routes: { "POST /": [stsWebIdentity] },
	},
	{
		name: "web-identity-sts-error",
		processEnv: { ...stsEnv, AWS_WEB_IDENTITY_TOKEN_FILE: "<home>/token", AWS_ROLE_ARN: "arn:aws:iam::123456789012:role/web" },
		files: { token: "bad" },
		routes: { "POST /": [stsError] },
	},
	{
		name: "web-identity-missing-token-file",
		processEnv: { ...stsEnv, AWS_WEB_IDENTITY_TOKEN_FILE: "<home>/missing", AWS_ROLE_ARN: "arn:aws:iam::123456789012:role/web" },
	},
	{
		name: "assume-role-source-profile",
		options: { profile: "role" },
		processEnv: stsEnv,
		files: {
			".aws/config": `[profile role]\nrole_arn = arn:aws:iam::123456789012:role/admin\nsource_profile = base\nrole_session_name = s1\nexternal_id = ext-1\nduration_seconds = 900\n`,
			".aws/credentials": `[base]\naws_access_key_id = AKIDBASE\naws_secret_access_key = base-secret\n`,
		},
		routes: { "POST /": [stsAssumeRole] },
	},
	{
		name: "assume-role-credential-source-imds",
		options: { profile: "role" },
		processEnv: { ...stsEnv, AWS_EC2_METADATA_DISABLED: null, AWS_EC2_METADATA_SERVICE_ENDPOINT: "http://127.0.0.1:<port>" },
		files: { ".aws/config": `[profile role]\nrole_arn = arn:aws:iam::123456789012:role/admin\ncredential_source = Ec2InstanceMetadata\nrole_session_name = s2\n` },
		routes: {
			"PUT /latest/api/token": [{ status: 200, headers: { "content-type": "text/plain" }, body: "imds-token" }],
			"GET /latest/meta-data/iam/security-credentials/": [{ status: 200, headers: { "content-type": "text/plain" }, body: "my-role" }],
			"GET /latest/meta-data/iam/security-credentials/my-role": [imdsCreds],
			"POST /": [stsAssumeRole],
		},
	},
	{
		name: "assume-role-cycle",
		options: { profile: "a" },
		processEnv: stsEnv,
		files: { ".aws/config": `[profile a]\nrole_arn = arn:aws:iam::1:role/a\nsource_profile = b\n\n[profile b]\nrole_arn = arn:aws:iam::1:role/b\nsource_profile = a\n` },
	},
	{
		name: "imds-v2",
		processEnv: { AWS_EC2_METADATA_DISABLED: null, AWS_EC2_METADATA_SERVICE_ENDPOINT: "http://127.0.0.1:<port>" },
		routes: {
			"PUT /latest/api/token": [{ status: 200, headers: { "content-type": "text/plain" }, body: "imds-token" }],
			"GET /latest/meta-data/iam/security-credentials/": [{ status: 200, headers: { "content-type": "text/plain" }, body: "my-role\n" }],
			"GET /latest/meta-data/iam/security-credentials/my-role": [imdsCreds],
		},
	},
	{
		name: "imds-v1-fallback",
		processEnv: { AWS_EC2_METADATA_DISABLED: null, AWS_EC2_METADATA_SERVICE_ENDPOINT: "http://127.0.0.1:<port>" },
		routes: {
			"PUT /latest/api/token": [{ status: 403, headers: { "content-type": "text/plain" }, body: "forbidden" }],
			"GET /latest/meta-data/iam/security-credentials/": [{ status: 200, headers: { "content-type": "text/plain" }, body: "my-role" }],
			"GET /latest/meta-data/iam/security-credentials/my-role": [imdsCreds],
		},
	},
	{
		name: "imds-v1-fallback-blocked",
		processEnv: { AWS_EC2_METADATA_DISABLED: null, AWS_EC2_METADATA_V1_DISABLED: "true", AWS_EC2_METADATA_SERVICE_ENDPOINT: "http://127.0.0.1:<port>" },
		routes: { "PUT /latest/api/token": [{ status: 404, headers: { "content-type": "text/plain" }, body: "nope" }] },
	},
	{
		name: "imds-token-400",
		processEnv: { AWS_EC2_METADATA_DISABLED: null, AWS_EC2_METADATA_SERVICE_ENDPOINT: "http://127.0.0.1:<port>" },
		routes: { "PUT /latest/api/token": [{ status: 400, headers: { "content-type": "text/plain" }, body: "bad" }] },
	},
	{
		name: "imds-invalid-credentials",
		processEnv: { AWS_EC2_METADATA_DISABLED: null, AWS_EC2_METADATA_SERVICE_ENDPOINT: "http://127.0.0.1:<port>" },
		routes: {
			"PUT /latest/api/token": [{ status: 200, headers: { "content-type": "text/plain" }, body: "imds-token" }],
			"GET /latest/meta-data/iam/security-credentials/": [{ status: 200, headers: { "content-type": "text/plain" }, body: "my-role" }],
			"GET /latest/meta-data/iam/security-credentials/my-role": [{ status: 200, body: { Code: "Success", AccessKeyId: "ASIAIMDS" } }],
		},
	},
	{
		name: "container-full-uri",
		processEnv: { AWS_CONTAINER_CREDENTIALS_FULL_URI: "http://127.0.0.1:<port>/creds?x=1", AWS_CONTAINER_AUTHORIZATION_TOKEN: "ctr-token" },
		routes: { "GET /creds": [{ status: 200, body: { AccessKeyId: "ASIACTR", SecretAccessKey: "ctr-secret", Token: "ctr-session", Expiration: "2099-01-01T00:00:00Z" } }] },
	},
	{
		name: "container-token-file",
		processEnv: { AWS_CONTAINER_CREDENTIALS_FULL_URI: "http://127.0.0.1:<port>/creds", AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE: "<home>/ctr-token" },
		files: { "ctr-token": "file-token" },
		routes: { "GET /creds": [{ status: 200, body: { AccessKeyId: "ASIACTR", SecretAccessKey: "ctr-secret", Token: "ctr-session", Expiration: "2099-01-01T00:00:00Z" } }] },
	},
	{
		name: "container-host-not-allowed",
		processEnv: { AWS_CONTAINER_CREDENTIALS_FULL_URI: "http://example.com/creds" },
	},
	{
		name: "profile-skips-env-keys-then-nothing",
		options: { profile: "empty" },
		processEnv: { AWS_ACCESS_KEY_ID: "AKIDENV", AWS_SECRET_ACCESS_KEY: "env-secret" },
		files: { ".aws/config": `[profile empty]\nregion = us-west-2\n` },
	},
	{
		name: "ini-subsections-and-comments",
		options: { profile: "x" },
		files: {
			".aws/config": `[profile x] ; trailing comment\ns3 =\n  max_concurrent_requests = 20\naws_access_key_id = AKIDINI # comment\naws_secret_access_key = ini#secret\n[profile other]\naws_access_key_id = AKIDOTHER\n`,
		},
	},
];

const results = [];
for (const scenario of scenarios) results.push(await run(scenario));
server.close();
assertOffline();
const out = join(here, "bedrock-credentials.json");
writeFileSync(out, `${JSON.stringify({ generator: "crates/ai/fixtures/generate-bedrock-credentials.mjs", scenarios: results }, null, "\t")}\n`);
console.log(`wrote ${results.length} scenarios to ${out}`);
