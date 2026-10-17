// Generates crates/tui/fixtures/keybindings-file.json: packages/coding-agent/src/core/keybindings.ts
// `KeybindingsManager.create(agentDir)` (load <agentDir>/keybindings.json, migrate legacy names, keep string and
// string-array bindings), `reload()` after the file changes, and `migrateKeybindingsConfig`, over file contents.
// crates/tui/tests/keybindings_file.rs replays it.
// Usage: node crates/tui/fixtures/generate-keybindings-file.mjs (linux, WSL_DISTRO_NAME/WSL_INTEROP unset)
import "../../cli/fixtures/offline-guard.mjs";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

if (process.platform !== "linux" || process.env.WSL_DISTRO_NAME || process.env.WSL_INTEROP) {
	throw new Error("run on linux without WSL variables: the default bindings depend on the platform");
}
const here = dirname(fileURLToPath(import.meta.url));
const { KeybindingsManager, migrateKeybindingsConfig } = await import(
	pathToFileURL(join(here, "../../../packages/coding-agent/src/core/keybindings.ts")).href
);

const FILES = [
	null,
	"",
	"not json",
	"null",
	"42",
	'"ctrl+x"',
	"[]",
	'["ctrl+x"]',
	"{}",
	'{"app.exit": "ctrl+q"}',
	'﻿{"app.exit": ["ctrl+q", "ctrl+w", "ctrl+q"]}',
	'{"app.exit": []}',
	'{"app.exit": 5, "app.clear": ["ctrl+k", 3], "app.interrupt": null, "app.suspend": {"a": 1}}',
	'{"exit": "ctrl+q", "cursorUp": ["ctrl+p", "up"], "submit": "ctrl+s"}',
	'{"exit": "ctrl+q", "app.exit": "ctrl+e"}',
	'{"app.exit": "ctrl+e", "exit": "ctrl+q"}',
	'{"zeta": "a", "app.clear": "ctrl+k", "alpha": "b", "tui.editor.undo": "ctrl+u", "Beta": "c"}',
	'{"app.exit": "ctrl+k", "app.clear": "ctrl+k", "selectModel": "ctrl+k", "app.retry": "ctrl+k"}',
	'{"app.rlm.pane": "alt+p", "dequeue": "alt+d", "followUp": "ctrl+enter", "app.history.search": "ctrl+r"}',
	'{"app.exit": "ctrl+q",}',
];

const resolved = (manager) => Object.entries(manager.getEffectiveConfig());
const cases = [];
for (const content of FILES) {
	const dir = mkdtempSync(join(tmpdir(), "ultron-keybindings-"));
	try {
		if (content !== null) writeFileSync(join(dir, "keybindings.json"), content);
		const manager = KeybindingsManager.create(dir);
		let migrated = null;
		try {
			const parsed = JSON.parse(content?.replace(/^﻿/, "") ?? "null");
			if (typeof parsed === "object" && parsed !== null && !Array.isArray(parsed)) {
				const result = migrateKeybindingsConfig(parsed);
				migrated = { config: Object.entries(result.config), migrated: result.migrated };
			}
		} catch {}
		cases.push({
			content,
			user: Object.entries(manager.getUserBindings()),
			resolved: resolved(manager),
			conflicts: manager.getConflicts(),
			migrated,
		});
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
}

// reload(): the file changes (or goes away) after create().
const reloads = [];
for (const [before, after] of [
	['{"app.exit": "ctrl+q"}', '{"exit": "ctrl+w", "app.clear": "ctrl+k"}'],
	['{"app.exit": "ctrl+q"}', null],
	[null, '{"app.exit": "ctrl+q"}'],
	['{"app.exit": "ctrl+q"}', "broken"],
]) {
	const dir = mkdtempSync(join(tmpdir(), "ultron-keybindings-"));
	try {
		const path = join(dir, "keybindings.json");
		if (before !== null) writeFileSync(path, before);
		const manager = KeybindingsManager.create(dir);
		if (after === null) rmSync(path);
		else writeFileSync(path, after);
		manager.reload();
		reloads.push({ before, after, user: Object.entries(manager.getUserBindings()), resolved: resolved(manager) });
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
}

writeFileSync(join(here, "keybindings-file.json"), `${JSON.stringify({ cases, reloads }, null, 1)}\n`);
console.log(`wrote ${cases.length} cases, ${reloads.length} reloads`);
