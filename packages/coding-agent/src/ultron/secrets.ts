/**
 * Secret detection shared by every place Ultron looks for credentials. The rules are data
 * (`rlm/secret-patterns.json`), read by this module, by the Python runtime (`rlm/secret_patterns.py`) and by
 * `scripts/secret-scan.mjs`, so the masker, the runtime and the release scan cannot drift apart.
 *
 * A rule matches a credential shape and names the span that is the value (the whole match, or one group). A
 * finding is dropped when that value is an obvious test value, or when one safe span (an environment read, a
 * `$VAR`, a `<placeholder>`, a redaction marker) covers the value entirely: `os.environ.get("X", "<key literal>")`
 * still reports the literal, because the environment read covers only its own text.
 *
 * This file must stay erasable TypeScript (no enums, no parameter properties): the scan script imports it directly.
 */
import { readFileSync } from "node:fs";

export type SecretScope = "mask" | "scan";

export interface SecretRuleData {
	readonly id: string;
	readonly description?: string;
	readonly regex: string;
	/** JavaScript/Python flag letters: `i` and `m` only. */
	readonly flags?: string;
	/** The capture group holding the value; 0 (the default) is the whole match. */
	readonly group?: number;
	/** Where the rule applies; default both. */
	readonly scopes?: readonly SecretScope[];
	/** The rule runs only when the text contains one of these (case-insensitively when `flags` has `i`). */
	readonly keywords?: readonly string[];
	/** Minimum Shannon entropy of the value, in bits per character. */
	readonly minEntropy?: number;
	/** The value must contain a letter and a digit. */
	readonly requireLetterAndDigit?: boolean;
	/** A value matching this is not a secret (a path, a template). */
	readonly rejectValue?: string;
	readonly rejectValueFlags?: string;
	/** A value matching this is allowed for this rule (placeholder user names). */
	readonly allowValue?: string;
	readonly allowValueFlags?: string;
	/** Canary examples, each split into parts that the tests join. */
	readonly examples?: {
		readonly flag?: readonly (readonly string[])[];
		readonly pass?: readonly (readonly string[])[];
	};
}

export interface SecretPatternData {
	readonly version: number;
	readonly safeSpans: readonly { readonly id: string; readonly regex: string }[];
	readonly testValue: { readonly regex: string; readonly flags?: string };
	readonly rules: readonly SecretRuleData[];
}

export interface SecretFinding {
	/** The rule id, also the kind in `[REDACTED:<kind>]`. */
	readonly kind: string;
	/** Offsets of the value in the scanned text. */
	readonly start: number;
	readonly end: number;
	/** A few leading characters and the length, never the value. */
	readonly preview: string;
}

type CompiledRule = {
	readonly id: string;
	readonly regex: RegExp;
	readonly group: number;
	readonly scopes: ReadonlySet<SecretScope>;
	readonly keywords: readonly string[];
	readonly caseless: boolean;
	readonly minEntropy: number;
	readonly requireLetterAndDigit: boolean;
	readonly rejectValue?: RegExp;
	readonly allowValue?: RegExp;
};

export const REDACTION_MARKER = /\[REDACTED:[A-Za-z0-9_]+\]/;

function jsFlags(flags: string | undefined, extra: string): string {
	const letters = new Set((flags ?? "").split("").filter(Boolean));
	for (const letter of letters)
		if (letter !== "i" && letter !== "m") throw new Error(`secret pattern flag ${letter} is not portable`);
	return [...letters, ...extra].join("");
}

/** Bits of Shannon entropy per character. */
export function shannonEntropy(value: string): number {
	if (!value) return 0;
	const counts = new Map<string, number>();
	for (const char of value) counts.set(char, (counts.get(char) ?? 0) + 1);
	let bits = 0;
	for (const count of counts.values()) {
		const p = count / value.length;
		bits -= p * Math.log2(p);
	}
	return bits;
}

/** A redacted preview: up to 4 leading characters (a quarter of the value at most) and the length. */
export function previewSecret(value: string): string {
	return `${value.slice(0, Math.min(4, Math.floor(value.length / 4)))}…(${value.length} chars)`;
}

export class SecretDetector {
	readonly #rules: readonly CompiledRule[];
	readonly #safe: readonly RegExp[];
	readonly #testValue: RegExp;

	constructor(data: SecretPatternData) {
		this.#rules = data.rules.map((rule) => ({
			id: rule.id,
			// `d` gives group offsets; `g` iterates.
			regex: new RegExp(rule.regex, jsFlags(rule.flags, "gd")),
			group: rule.group ?? 0,
			scopes: new Set(rule.scopes ?? ["mask", "scan"]),
			keywords: (rule.flags ?? "").includes("i")
				? (rule.keywords ?? []).map((word) => word.toLowerCase())
				: (rule.keywords ?? []),
			caseless: (rule.flags ?? "").includes("i"),
			minEntropy: rule.minEntropy ?? 0,
			requireLetterAndDigit: rule.requireLetterAndDigit === true,
			...(rule.rejectValue === undefined
				? {}
				: { rejectValue: new RegExp(rule.rejectValue, jsFlags(rule.rejectValueFlags, "")) }),
			...(rule.allowValue === undefined
				? {}
				: { allowValue: new RegExp(rule.allowValue, jsFlags(rule.allowValueFlags, "")) }),
		}));
		this.#safe = data.safeSpans.map((span) => new RegExp(span.regex, "g"));
		this.#testValue = new RegExp(data.testValue.regex, jsFlags(data.testValue.flags, ""));
	}

	/** Rule ids, in priority order. */
	get kinds(): string[] {
		return this.#rules.map((rule) => rule.id);
	}

	/** Non-overlapping findings in `text`, in order. */
	scan(text: string, scope: SecretScope = "scan"): SecretFinding[] {
		if (!text) return [];
		let lower: string | undefined;
		let safe: Array<[number, number]> | undefined;
		const found: Array<SecretFinding & { order: number }> = [];
		this.#rules.forEach((rule, order) => {
			if (!rule.scopes.has(scope)) return;
			if (rule.keywords.length > 0) {
				if (rule.caseless && lower === undefined) lower = text.toLowerCase();
				const haystack = rule.caseless ? lower! : text;
				if (!rule.keywords.some((word) => haystack.includes(word))) return;
			}
			for (const match of text.matchAll(rule.regex)) {
				const span = match.indices?.[rule.group];
				if (!span) continue;
				const [start, end] = span;
				const value = text.slice(start, end);
				if (!this.#plausible(rule, value)) continue;
				safe ??= this.#safeSpans(text);
				if (safe.some(([from, to]) => from <= start && end <= to)) continue;
				found.push({ kind: rule.id, start, end, preview: previewSecret(value), order });
			}
		});
		// Earlier first; at the same start the longer match, then the earlier rule.
		found.sort((a, b) => a.start - b.start || b.end - b.start - (a.end - a.start) || a.order - b.order);
		const kept: SecretFinding[] = [];
		let reach = -1;
		for (const { order: _order, ...finding } of found) {
			if (finding.start < reach) continue;
			kept.push(finding);
			reach = finding.end;
		}
		return kept;
	}

	/** `text` with every masking finding replaced by `[REDACTED:<kind>]`. */
	redact(text: string): string {
		const findings = this.scan(text, "mask");
		if (findings.length === 0) return text;
		let out = "";
		let cursor = 0;
		for (const finding of findings) {
			out += `${text.slice(cursor, finding.start)}[REDACTED:${finding.kind}]`;
			cursor = finding.end;
		}
		return out + text.slice(cursor);
	}

	#plausible(rule: CompiledRule, value: string): boolean {
		if (!value) return false;
		if (rule.requireLetterAndDigit && !(/[A-Za-z]/.test(value) && /[0-9]/.test(value))) return false;
		if (rule.minEntropy > 0 && shannonEntropy(value) < rule.minEntropy) return false;
		if (rule.rejectValue?.test(value)) return false;
		if (rule.allowValue?.test(value)) return false;
		return !this.#testValue.test(value);
	}

	#safeSpans(text: string): Array<[number, number]> {
		const spans: Array<[number, number]> = [];
		for (const regex of this.#safe)
			for (const match of text.matchAll(regex)) spans.push([match.index, match.index + match[0].length]);
		return spans;
	}
}

export function loadSecretPatterns(path: string): SecretPatternData {
	return JSON.parse(readFileSync(path, "utf8")) as SecretPatternData;
}

/** Whether cell output is masked (`ULTRON_MASK_SECRETS=off` turns it off). */
export function maskSecretsEnabled(env: NodeJS.ProcessEnv = process.env): boolean {
	const value = env.ULTRON_MASK_SECRETS?.trim().toLowerCase();
	return !value || !["off", "0", "false", "no"].includes(value);
}
