import * as bundledPiAgentCore from "@ultron/agent-core";
import * as bundledPiAiCompat from "@ultron/ai/compat";
import * as bundledPiAiOauth from "@ultron/ai/oauth";
import * as bundledPiAiProviders from "@ultron/ai/providers/all";
import * as bundledPiTui from "@ultron/tui";
import * as bundledTypebox from "typebox";
import * as bundledTypeboxCompile from "typebox/compile";
import * as bundledTypeboxValue from "typebox/value";
// This import is safe because loader.ts exports are not re-exported from index.ts.
// Extensions can therefore import from @ultron/coding-agent (or the legacy
// @earendil-works/pi-coding-agent and @mariozechner/pi-coding-agent names).
import * as bundledPiCodingAgent from "../../index.ts";

/** Modules available to extensions in source and compiled binary runtimes. */
export const VIRTUAL_MODULES: Record<string, unknown> = {
	typebox: bundledTypebox,
	"typebox/compile": bundledTypeboxCompile,
	"typebox/value": bundledTypeboxValue,
	"@sinclair/typebox": bundledTypebox,
	"@sinclair/typebox/compile": bundledTypeboxCompile,
	"@sinclair/typebox/value": bundledTypeboxValue,
	"@ultron/agent-core": bundledPiAgentCore,
	"@ultron/tui": bundledPiTui,
	"@ultron/ai": bundledPiAiCompat,
	"@ultron/ai/compat": bundledPiAiCompat,
	"@ultron/ai/oauth": bundledPiAiOauth,
	"@ultron/ai/providers/all": bundledPiAiProviders,
	"@ultron/coding-agent": bundledPiCodingAgent,
	// Legacy Pi package names stay available so existing extensions keep loading.
	"@earendil-works/pi-agent-core": bundledPiAgentCore,
	"@earendil-works/pi-tui": bundledPiTui,
	// Extensions resolve the pi-ai root to the compat entrypoint (a strict
	// superset of the core entrypoint): existing extensions using the old
	// global API keep working at runtime until compat is removed.
	"@earendil-works/pi-ai": bundledPiAiCompat,
	"@earendil-works/pi-ai/compat": bundledPiAiCompat,
	"@earendil-works/pi-ai/oauth": bundledPiAiOauth,
	"@earendil-works/pi-ai/providers/all": bundledPiAiProviders,
	"@earendil-works/pi-coding-agent": bundledPiCodingAgent,
	"@mariozechner/pi-agent-core": bundledPiAgentCore,
	"@mariozechner/pi-tui": bundledPiTui,
	"@mariozechner/pi-ai": bundledPiAiCompat,
	"@mariozechner/pi-ai/compat": bundledPiAiCompat,
	"@mariozechner/pi-ai/oauth": bundledPiAiOauth,
	"@mariozechner/pi-ai/providers/all": bundledPiAiProviders,
	"@mariozechner/pi-coding-agent": bundledPiCodingAgent,
};
