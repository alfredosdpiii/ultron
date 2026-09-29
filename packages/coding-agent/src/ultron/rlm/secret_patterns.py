"""Secret detection for the Python runtime, from the rules in ``secret-patterns.json`` beside this file.

The same JSON drives the host's cell-output masker (``src/ultron/secrets.ts``) and ``scripts/secret-scan.mjs``;
this module applies it the same way (a test compares the two), so the runtime, the masker and the release scan
agree on what a secret is. A finding names the rule, the value's offsets and a redacted preview, never the value.
"""

from __future__ import annotations

import json
import math
import re
from pathlib import Path
from typing import Any, NamedTuple

PATTERNS_PATH = Path(__file__).with_name("secret-patterns.json")

# What the host puts in cell output in place of a secret.
REDACTION_MARKER = re.compile(r"\[REDACTED:[A-Za-z0-9_]+\]")


class Finding(NamedTuple):
    kind: str
    start: int
    end: int
    preview: str


def _flags(letters: str | None) -> int:
    value = re.ASCII
    for letter in letters or "":
        if letter == "i":
            value |= re.IGNORECASE
        elif letter == "m":
            value |= re.MULTILINE
        else:
            raise ValueError(f"secret pattern flag {letter} is not portable")
    return value


def shannon_entropy(value: str) -> float:
    if not value:
        return 0.0
    counts: dict[str, int] = {}
    for char in value:
        counts[char] = counts.get(char, 0) + 1
    return -sum((n / len(value)) * math.log2(n / len(value)) for n in counts.values())


def preview(value: str) -> str:
    return f"{value[:min(4, len(value) // 4)]}…({len(value)} chars)"


_LETTER = re.compile(r"[A-Za-z]")
_DIGIT = re.compile(r"[0-9]")


class _Rule:
    def __init__(self, data: dict[str, Any]) -> None:
        self.id: str = data["id"]
        self.caseless = "i" in (data.get("flags") or "")
        self.regex = re.compile(data["regex"], _flags(data.get("flags")))
        self.group: int = data.get("group", 0)
        self.scopes = set(data.get("scopes") or ("mask", "scan"))
        keywords = data.get("keywords") or []
        self.keywords = [word.lower() for word in keywords] if self.caseless else list(keywords)
        self.min_entropy: float = data.get("minEntropy", 0)
        self.require_letter_and_digit = data.get("requireLetterAndDigit") is True
        self.reject_value = (
            re.compile(data["rejectValue"], _flags(data.get("rejectValueFlags"))) if data.get("rejectValue") else None
        )
        self.allow_value = (
            re.compile(data["allowValue"], _flags(data.get("allowValueFlags"))) if data.get("allowValue") else None
        )


class SecretDetector:
    def __init__(self, data: dict[str, Any]) -> None:
        self._rules = [_Rule(rule) for rule in data["rules"]]
        self._safe = [re.compile(span["regex"], re.ASCII) for span in data["safeSpans"]]
        test = data["testValue"]
        self._test_value = re.compile(test["regex"], _flags(test.get("flags")))

    @property
    def kinds(self) -> list[str]:
        return [rule.id for rule in self._rules]

    def scan(self, text: str, scope: str = "scan") -> list[Finding]:
        """Non-overlapping findings in ``text``, in order."""
        if not text:
            return []
        lower: str | None = None
        safe: list[tuple[int, int]] | None = None
        found: list[tuple[int, int, int, str]] = []
        for order, rule in enumerate(self._rules):
            if scope not in rule.scopes:
                continue
            if rule.keywords:
                if rule.caseless and lower is None:
                    lower = text.lower()
                haystack = lower if rule.caseless else text
                if not any(word in haystack for word in rule.keywords):
                    continue
            for match in rule.regex.finditer(text):
                start, end = match.span(rule.group)
                if start < 0:
                    continue
                value = text[start:end]
                if not self._plausible(rule, value):
                    continue
                if safe is None:
                    safe = [m.span() for pattern in self._safe for m in pattern.finditer(text)]
                if any(a <= start and end <= b for a, b in safe):
                    continue
                found.append((start, end, order, rule.id))
        found.sort(key=lambda f: (f[0], -(f[1] - f[0]), f[2]))
        kept: list[Finding] = []
        reach = -1
        for start, end, _order, kind in found:
            if start < reach:
                continue
            kept.append(Finding(kind, start, end, preview(text[start:end])))
            reach = end
        return kept

    def redact(self, text: str) -> str:
        """``text`` with every masking finding replaced by ``[REDACTED:<kind>]``."""
        findings = self.scan(text, "mask")
        if not findings:
            return text
        parts: list[str] = []
        cursor = 0
        for finding in findings:
            parts.append(text[cursor:finding.start])
            parts.append(f"[REDACTED:{finding.kind}]")
            cursor = finding.end
        parts.append(text[cursor:])
        return "".join(parts)

    def _plausible(self, rule: _Rule, value: str) -> bool:
        if not value:
            return False
        if rule.require_letter_and_digit and not (_LETTER.search(value) and _DIGIT.search(value)):
            return False
        if rule.min_entropy > 0 and shannon_entropy(value) < rule.min_entropy:
            return False
        if rule.reject_value is not None and rule.reject_value.search(value):
            return False
        if rule.allow_value is not None and rule.allow_value.search(value):
            return False
        return not self._test_value.search(value)


_DEFAULT: SecretDetector | None = None


def detector() -> SecretDetector:
    """The detector for the shipped rules, loaded on first use."""
    global _DEFAULT
    if _DEFAULT is None:
        _DEFAULT = SecretDetector(json.loads(PATTERNS_PATH.read_text(encoding="utf-8")))
    return _DEFAULT


if __name__ == "__main__":
    # `python3 secret_patterns.py [patterns.json] < text` prints the findings as JSON (the parity test uses it).
    import sys

    chosen = SecretDetector(json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))) if len(sys.argv) > 1 else detector()
    texts = json.loads(sys.stdin.read())
    print(json.dumps([{"findings": [f._asdict() for f in chosen.scan(text)], "redacted": chosen.redact(text)} for text in texts]))
