// Generates crates/host/fixtures/secrets.json: the TypeScript secret masking (ultron/secrets.ts SecretDetector over
// rlm/secret-patterns.json, rlm/output-secrets.ts maskCellOutputCounted with known secrets, ultron/sensitive.ts)
// over a corpus: every rule's joined canary examples plus hand-written edge cases. Deterministic.
// Usage: node crates/host/fixtures/secrets.mjs
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const src = join(resolve(here, "../../.."), "packages/coding-agent/src");
const load = (path) => import(pathToFileURL(join(src, path)).href);
const { defaultSecretDetector, maskCellOutputCounted, registerSecretValues } = await load("ultron/rlm/output-secrets.ts");
const { containsSensitiveMemory, containsCodeSecret } = await load("ultron/sensitive.ts");
const { shannonEntropy, previewSecret } = await load("ultron/secrets.ts");
const data = JSON.parse((await import("node:fs")).readFileSync(join(src, "ultron/rlm/secret-patterns.json"), "utf8"));

const j = (...parts) => parts.join("");
const corpus = [];
for (const rule of data.rules)
	for (const kind of ["flag", "pass"])
		for (const parts of rule.examples?.[kind] ?? []) corpus.push(parts.join(""));
const key = j("sk-a", "nt-api03-", "Zq8Rw2Lm4Np6Tv0Xb3Cd5Fg7Hj9Kk1Mm");
const gh = j("gh", "p_", "Ab1Cd2Ef3Gh4Ij5Kl6Mn7Op8Qr9St0Uv1Wx2Y");
corpus.push(
	"",
	"nothing to see here",
	`two keys ${key} and ${gh} done`,
	`é${key}`,
	`日本 ${key} 語`,
	`😀 password = "Hunter2Correct9Horse"`,
	`export OPENAI_API_KEY=${j("Qw8Er7Ty6Ui5Op4As3Df2")}\nexport HOME_TOKEN=$HOME`,
	`os.environ.get("ANTHROPIC_API_KEY", "${key}")`,
	"os.environ['GITHUB_TOKEN']",
	"token = ${GITHUB_TOKEN}",
	"api_key: <your api key>",
	"password=[REDACTED:generic_secret]",
	`url = "postgres://admin:${j("S3cr3tPa", "ssW0rd")}@db.internal:5432/app"`,
	"https://user:example@host.com/x",
	`aws_secret_access_key = ${j("wJalrXUtnFEMI/K7MDENG/", "bPxRfiCYzzQ1a2b3c4d5")}`,
	j("AKIA", "Z7Q2W3E4R5T6Y7U8"),
	j("eyJhbGciOiJIUzI1", ".eyJzdWIiOiIxMjM0NTY3ODkw", ".SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"),
	"secret = aaaaaaaaaaaaaaaaaaaa",
	"see /home/bryan/x and /Users/alice/y and /home/runner/z",
	`TOKEN=${j("abcdefABCDEF0123", "456789xyz")} # tail`,
	"MY_SECRET_KEY = 0123456789abcdef0123456789abcdef01234567",
	`stripe ${j("sk_li", "ve_", "4eC39HqLyjWDarjtT1zdp7dc")}`,
	`${j("xox", "b-", "1234567890-abcdefghij")}`,
	`npm ${j("npm_", "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8")}`,
	`google ${j("AIza", "SyA1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q")}`,
	`gh ${j("github_pat_", "11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOP")}`,
	`${key}${key}`,
	`bearer ${j("abcdefghijklmnop", "qrstuvwx")}`,
	"ssn 123-45-6789",
	"skill_version = 3",
	`api_key = "${j("abcd", "efgh12")}"`,
	`sk-${j("proj-", "Ab1Cd2Ef3Gh4Ij5Kl6Mn7Op8Qr9St0Uv1Wx2")}`,
	`sk-ant-${"x".repeat(40)}`,
	`private_key: ${j("Zm9vYmFyYmF6cXV4", "MTIzNDU2Nzg5MA")}`,
	`export ANTHROPIC_API_KEY=${key}.trailing-part`,
	`api_key =\ufeff${j("Zq8Rw2Lm4N", "p6Tv0Xb3Cd5")}`,
	`api_key =\u0085${j("Zq8Rw2Lm4N", "p6Tv0Xb3Cd5")}`,
	`password:\u00a0"${j("Hunter2Corr", "ect9Horse")}"`,
	`cr\r\nTOKEN=${j("Qw8Er7Ty6Ui5Op4As3Df2")}\r\n`,
);
const unique = [...new Set(corpus)];
const detector = defaultSecretDetector();
// Secret-looking strings are stored as 8-character pieces so the fixture itself never holds a whole match.
const pieces = (text) => (Array.from(text).join("").match(/[\s\S]{1,8}/gu) ?? []);
const cases = unique.map((text) => {
	const masked = detector.redactCounted(text);
	const scan = detector.scan(text, "scan").map((f) => ({ kind: f.kind, value: pieces(text.slice(f.start, f.end)), preview: f.preview }));
	return {
		text: pieces(text),
		redacted: masked.text,
		masked: masked.masked,
		scan,
		sensitive_memory: containsSensitiveMemory(text),
		code_secret: containsCodeSecret(text),
	};
});
const entropy = ["", "a", "ab", "aabb", "Zq8Rw2Lm4Np6", "😀😀😀ab", "é"].map((value) => ({ value, bits: shannonEntropy(value), preview: previewSecret(value) }));
const off = ["off", "0", "FALSE", " no ", "on", ""].map((value) => {
	const result = maskCellOutputCounted(`k ${key}`, { ULTRON_MASK_SECRETS: value });
	return { value, text: pieces(result.text), masked: result.masked };
});
const known = ["opaqueTokenValue", "  spaced-known-1  ", "short", 42, "Ab3dEf7h", "Zy9xWv8"];
registerSecretValues(known);
const knownCases = [
	"x opaqueTokenValue y opaqueTokenValue",
	"spaced-known-1 and opaqueTokenValue",
	"short is fine",
	`both opaqueTokenValue ${key}`,
	"partial opaqueToken",
	"eight Ab3dEf7h seven Zy9xWv8",
].map((text) => {
	const result = maskCellOutputCounted(text, {});
	return { text: pieces(text), redacted: result.text, masked: result.masked };
});
writeFileSync(join(here, "secrets.json"), `${JSON.stringify({ cases, entropy, off, known, known_key: pieces(key), known_cases: knownCases }, null, "\t")}\n`);
