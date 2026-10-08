import { defaultSecretDetector } from "./rlm/output-secrets.ts";

/**
 * Local checks that keep secrets out of what Ultron stores: memory written to Hindsight and code skills written to
 * disk. Both also apply the shared credential rules (rlm/secret-patterns.json, as masked in cell output).
 */

/** Memory text: token prefixes, private keys, credential assignments, bearer tokens, SSNs. */
const sensitivePatterns = [
	/\b(?:sk|ghp|github_pat|xox[baprs]|AIza|AKIA)[A-Za-z0-9_-]{8,}\b/i,
	/-----BEGIN [A-Z ]*PRIVATE KEY-----/i,
	/\b(?:api[_ -]?key|access[_ -]?token|auth[_ -]?token|password|passwd|secret|private[_ -]?key)\s*[:=]\s*\S+/i,
	/\bbearer\s+[A-Za-z0-9._~+/=-]{16,}\b/i,
	/\b\d{3}-\d{2}-\d{4}\b/,
];

/**
 * Secrets in source code. Narrower than the memory patterns, which would flag ordinary identifiers such as
 * `skill_version`: token prefixes are case-sensitive with their separator, and a credential name counts only
 * when it is assigned a string literal.
 */
const codeSecretPatterns = [
	/\b(?:sk-(?:proj-|live-|test-)?|sk_live_|sk_test_|ghp_|gho_|github_pat_|xox[baprs]-|AIza|AKIA|ASIA)[A-Za-z0-9_-]{12,}/,
	/-----BEGIN [A-Z ]*PRIVATE KEY-----/,
	/\b(?:api[_-]?key|access[_-]?token|auth[_-]?token|password|passwd|secret|private[_-]?key)\s*[:=]\s*["'][^"'\s]{8,}["']/i,
	/\bbearer\s+[A-Za-z0-9._~+/=-]{20,}/i,
	/\b\d{3}-\d{2}-\d{4}\b/,
];

const sharedRulesMatch = (text: string): boolean => defaultSecretDetector().scan(text, "mask").length > 0;

/** Whether memory text holds a secret or personal identifier; such text is never written to Hindsight. */
export function containsSensitiveMemory(text: string): boolean {
	return sensitivePatterns.some((pattern) => pattern.test(text)) || sharedRulesMatch(text);
}

/** Whether a code skill proposal (name, source, test, evidence) holds a secret; such a proposal is never written. */
export function containsCodeSecret(text: string): boolean {
	return codeSecretPatterns.some((pattern) => pattern.test(text)) || sharedRulesMatch(text);
}
