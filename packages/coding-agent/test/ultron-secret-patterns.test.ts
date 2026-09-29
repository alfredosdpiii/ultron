/**
 * The shared secret rules (src/ultron/rlm/secret-patterns.json):
 * - canary: every rule has examples that must be flagged and examples that must not, and each behaves as expected;
 * - safe forms (environment reads, `$VAR`, `<placeholder>`, redaction markers, obvious test values) excuse only
 *   the span they cover, so an environment read with a literal default still reports the literal;
 * - findings carry a kind, offsets and a redacted preview, never the value; masking is idempotent;
 * - the Python runtime's copy of the engine (secret_patterns.py) agrees with this one on every example;
 * - the code-skill secret gate uses the shared rules too.
 *
 * Fake credentials are built from parts at run time, so this file never holds a whole one.
 */
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, test } from "vitest";
import { containsCodeSecret } from "../src/ultron/jev.ts";
import { defaultSecretDetector, maskCellOutput } from "../src/ultron/rlm/output-secrets.ts";
import {
	loadSecretPatterns,
	maskSecretsEnabled,
	previewSecret,
	SecretDetector,
	shannonEntropy,
} from "../src/ultron/secrets.ts";

const patternsPath = fileURLToPath(new URL("../src/ultron/rlm/secret-patterns.json", import.meta.url));
const pythonModule = fileURLToPath(new URL("../src/ultron/rlm/secret_patterns.py", import.meta.url));
const data = loadSecretPatterns(patternsPath);
const detector = new SecretDetector(data);

const FAKE = "FAKE0a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8s9T0u";
const fake = {
	openai: ["s", "k-proj-", FAKE].join(""),
	anthropic: ["s", "k-ant-api03-", FAKE].join(""),
	github: ["g", "hp_", FAKE.slice(0, 36)].join(""),
	stripe: ["s", "k_live_", FAKE.slice(0, 24)].join(""),
	aws: ["AK", "IA", "FAKE0A1B2C3D4E5F"].join(""),
};

const joined = (parts: readonly string[]) => parts.join("");

describe("canary: every rule still fires, and still stays quiet", () => {
	for (const rule of data.rules) {
		test(rule.id, () => {
			const flag = rule.examples?.flag ?? [];
			const pass = rule.examples?.pass ?? [];
			expect(flag.length, `${rule.id} needs a should-flag example`).toBeGreaterThan(0);
			expect(pass.length, `${rule.id} needs a should-not-flag example`).toBeGreaterThan(0);
			for (const parts of flag) {
				const text = joined(parts);
				const findings = detector.scan(text);
				expect(
					findings.map((f) => f.kind),
					text,
				).toContain(rule.id);
				// The redacted text keeps none of the value.
				const finding = findings.find((f) => f.kind === rule.id)!;
				const value = text.slice(finding.start, finding.end);
				if ((rule.scopes ?? ["mask", "scan"]).includes("mask")) {
					expect(detector.redact(text)).not.toContain(value);
					expect(detector.redact(text)).toContain(`[REDACTED:${rule.id}]`);
				}
				expect(finding.preview).not.toContain(value);
			}
			for (const parts of pass) expect(detector.scan(joined(parts)), joined(parts)).toEqual([]);
		});
	}

	test("no example is whole in the rules file itself", () => {
		// The file is scanned in CI like any other: it must not trip its own rules.
		expect(detector.scan(readFileSync(patternsPath, "utf8"))).toEqual([]);
	});
});

describe("safe forms excuse only their own span", () => {
	test("an environment read with a literal default reports the literal", () => {
		const python = `key = os.environ.get("OPENAI_API_KEY", "${fake.openai}")`;
		expect(detector.scan(python).map((f) => f.kind)).toEqual(["openai_key"]);
		expect(detector.redact(python)).toBe('key = os.environ.get("OPENAI_API_KEY", "[REDACTED:openai_key]")');
		const node = `const key = process.env.STRIPE_KEY ?? "${fake.stripe}";`;
		expect(detector.redact(node)).toBe('const key = process.env.STRIPE_KEY ?? "[REDACTED:stripe_key]";');
		// The reads alone are fine.
		expect(detector.scan('key = os.environ["OPENAI_API_KEY"]; t = process.env.GITHUB_TOKEN')).toEqual([]);
	});

	test("placeholders, variables, markers and test values are not secrets", () => {
		for (const text of [
			'api_key = "<your api key>"',
			`token: "\${GITHUB_TOKEN}"`,
			"password = $DB_PASSWORD_FROM_THE_VAULT",
			'secret = "{{ vault.secret_value }}"',
			'client_secret = "[REDACTED]"',
			'api_key = "[REDACTED:generic_secret]"',
			'api_key = "xxxxxxxxxxxxxxxxxxxxxxxx"',
			'api_key = "sk-EXAMPLE-0000000000000000000000000000000000"',
			`AWS_SECRET_ACCESS_KEY = "${"A".repeat(40)}"`,
			"curl -H 'Authorization: Bearer $TOKEN' https://api.example.com",
		])
			expect(detector.scan(text), text).toEqual([]);
	});

	test("PEM private keys, including JSON-escaped ones, but not public keys", () => {
		const body = "MIIEFAKE0a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8s9T0u1V2w3X4y5Z6ab";
		const header = ["-----BEGIN ", "RSA PRIV", "ATE KEY-----"].join("");
		const footer = ["-----END ", "RSA PRIV", "ATE KEY-----"].join("");
		const pem = `before\n${header}\n${body}\n${body}\n${footer}\nafter`;
		expect(detector.redact(pem)).toBe("before\n[REDACTED:private_key]\nafter");
		const escaped = JSON.stringify({ private_key: `${header}\n${body}\n${footer}\n`, other: 1 });
		expect(detector.redact(escaped)).toBe('{"private_key":"[REDACTED:private_key]\\n","other":1}');
		// A header cut from its footer (a truncated print) is still masked from the header on.
		expect(detector.redact(`${header}\n${body}\n${body.slice(0, 20)}`)).toBe("[REDACTED:private_key]");
		const publicKey = `-----BEGIN PUBLIC KEY-----\n${body}\n-----END PUBLIC KEY-----`;
		expect(detector.scan(publicKey)).toEqual([]);
	});

	test("a generic assignment needs a long, mixed, high-entropy literal", () => {
		expect(detector.scan(`api_key = "${FAKE.slice(0, 24)}"`).map((f) => f.kind)).toEqual(["generic_secret"]);
		expect(detector.scan('api_key = "short1"')).toEqual([]);
		expect(detector.scan('password = "onlylettershereforalongtime"')).toEqual([]);
		expect(detector.scan('secret_path = "/etc/app/secret-1234.json"')).toEqual([]);
		expect(detector.scan('max_tokens = 4096; tokenizer = "cl100k"')).toEqual([]);
		// The whole assignment stays readable; only the literal is masked.
		expect(detector.redact(`DB_PASSWORD=${FAKE.slice(0, 20)}\nDEBUG=1`)).toBe(
			"DB_PASSWORD=[REDACTED:env_secret]\nDEBUG=1",
		);
	});
});

describe("findings and masking", () => {
	test("findings report kind, offsets and a preview that never holds the value", () => {
		const text = `first ${fake.github} then ${fake.aws}`;
		const findings = detector.scan(text);
		expect(findings.map((f) => [f.kind, text.slice(f.start, f.end)])).toEqual([
			["github_token", fake.github],
			["aws_access_key_id", fake.aws],
		]);
		expect(findings[0]!.preview).toBe(`${fake.github.slice(0, 4)}…(40 chars)`);
		expect(previewSecret("abcdefgh")).toBe("ab…(8 chars)");
		expect(previewSecret("abc")).toBe("…(3 chars)");
	});

	test("overlapping rules report one finding; masking is idempotent and leaves the rest intact", () => {
		const url = `git remote add origin https://x-access-token:${fake.github}@github.com/o/r.git`;
		expect(detector.scan(url).map((f) => f.kind)).toEqual(["github_token"]);
		const text = `ok: 1\nkey=${fake.anthropic}\nlist = [1, 2, 3]\n${fake.openai}`;
		const once = detector.redact(text);
		expect(once).toBe("ok: 1\nkey=[REDACTED:anthropic_key]\nlist = [1, 2, 3]\n[REDACTED:openai_key]");
		expect(detector.redact(once)).toBe(once);
	});

	test("home paths are a release-scan rule, never masked in cell output", () => {
		const text = ["/ho", "me/jdoe-fake/project/a.py"].join("");
		expect(detector.scan(text, "scan").map((f) => f.kind)).toEqual(["home_path"]);
		expect(detector.redact(text)).toBe(text);
	});

	test("the default detector reads the shipped rules; ULTRON_MASK_SECRETS=off disables masking", () => {
		expect(defaultSecretDetector().kinds).toEqual(detector.kinds);
		const text = `token ${fake.github}`;
		expect(maskCellOutput(text, {})).toBe("token [REDACTED:github_token]");
		expect(maskCellOutput(text, { ULTRON_MASK_SECRETS: "off" })).toBe(text);
		expect(maskSecretsEnabled({})).toBe(true);
		expect(maskSecretsEnabled({ ULTRON_MASK_SECRETS: " OFF " })).toBe(false);
		expect(maskSecretsEnabled({ ULTRON_MASK_SECRETS: "on" })).toBe(true);
	});

	test("entropy", () => {
		expect(shannonEntropy("aaaa")).toBe(0);
		expect(shannonEntropy("abcd")).toBe(2);
	});

	test("the code-skill secret gate uses the shared rules", () => {
		const jwt = ["ey", "JhbGciOiJIUzI1NiJ9.ey", "JzdWIiOiJGQUtFIn0.", FAKE].join("");
		expect(containsCodeSecret(`TOKEN = "${jwt}"`)).toBe(true);
		expect(containsCodeSecret('TOKEN = os.environ["TOKEN"]')).toBe(false);
	});
});

describe("Python parity", () => {
	test("secret_patterns.py finds and masks exactly what secrets.ts does", () => {
		const texts = data.rules.flatMap((rule) =>
			[...(rule.examples?.flag ?? []), ...(rule.examples?.pass ?? [])].map(joined),
		);
		texts.push(
			`mixed ${fake.openai} and ${fake.github}\nAPI_TOKEN=${FAKE.slice(0, 30)}\nos.getenv("K", "${fake.stripe}")`,
			["/Us", "ers/fakeperson/x and /home/user/y"].join(""),
			"nothing to see here",
		);
		const run = spawnSync("python3", [pythonModule, patternsPath], {
			input: JSON.stringify(texts),
			encoding: "utf8",
		});
		expect(run.status, run.stderr).toBe(0);
		const python = JSON.parse(run.stdout) as Array<{
			findings: Array<{ kind: string; start: number; end: number; preview: string }>;
			redacted: string;
		}>;
		expect(python).toHaveLength(texts.length);
		texts.forEach((text, i) => {
			expect(python[i]!.findings, text).toEqual(detector.scan(text));
			expect(python[i]!.redacted, text).toBe(detector.redact(text));
		});
	});
});
