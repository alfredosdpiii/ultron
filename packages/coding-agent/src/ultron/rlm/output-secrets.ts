/**
 * Secrets in RLM cell output are masked before the result reaches the model or the transcript: a credential the
 * cell printed (a `bash` result, a file it read, stdout, a traceback) becomes `[REDACTED:<kind>]` and everything
 * else is left as it was. Only the text a cell returns is scanned; values in the kernel keep their real content,
 * so `text = await read(path)` followed by a programmatic edit never loses a secret. `ULTRON_MASK_SECRETS=off`
 * turns masking off. The rules are `secret-patterns.json` beside runtime.py (see ../secrets.ts).
 */
import { dirname, join } from "node:path";
import { getRlmRuntimePath } from "../../config.ts";
import { loadSecretPatterns, maskSecretsEnabled, SecretDetector } from "../secrets.ts";

let detector: SecretDetector | undefined;

/** Values known to be secret whatever they look like: MCP server env values, headers and OAuth tokens. */
const knownSecrets = new Set<string>();
const KNOWN_SECRET_MIN_LENGTH = 8;

/**
 * Mask these exact values in cell output from now on. Values a pattern cannot recognize (a short API key, an
 * opaque OAuth token) still must not reach the model or the transcript. Short values are skipped: masking a
 * 3-character string would mangle ordinary output.
 */
export function registerSecretValues(values: readonly unknown[]): void {
	for (const value of values) {
		if (typeof value === "string" && value.trim().length >= KNOWN_SECRET_MIN_LENGTH) knownSecrets.add(value.trim());
	}
}

function maskKnownSecrets(text: string): { text: string; masked: number } {
	let masked = 0;
	let out = text;
	for (const secret of knownSecrets) {
		if (!out.includes(secret)) continue;
		masked += out.split(secret).length - 1;
		out = out.replaceAll(secret, "[REDACTED:known_secret]");
	}
	return { text: out, masked };
}

/** The detector for the shipped rules, loaded once. */
export function defaultSecretDetector(): SecretDetector {
	detector ??= new SecretDetector(loadSecretPatterns(join(dirname(getRlmRuntimePath()), "secret-patterns.json")));
	return detector;
}

/** `text` with secrets masked, unless ULTRON_MASK_SECRETS turns masking off. */
export function maskCellOutput(text: string, env: NodeJS.ProcessEnv = process.env): string {
	return maskCellOutputCounted(text, env).text;
}

/** {@link maskCellOutput}, with how many secrets were masked (for the session report's count). */
export function maskCellOutputCounted(
	text: string,
	env: NodeJS.ProcessEnv = process.env,
): { text: string; masked: number } {
	if (!text || !maskSecretsEnabled(env)) return { text, masked: 0 };
	const known = maskKnownSecrets(text);
	const detected = defaultSecretDetector().redactCounted(known.text);
	return { text: detected.text, masked: known.masked + detected.masked };
}
