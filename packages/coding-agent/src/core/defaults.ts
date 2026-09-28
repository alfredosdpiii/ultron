import type { ThinkingLevel } from "@ultron/agent-core";

/** Ultron: the Hindsight memory server used when neither ULTRON_HINDSIGHT_URL nor the `hindsightUrl` setting is set. */
export const DEFAULT_HINDSIGHT_URL = "http://localhost:8888";

export const DEFAULT_THINKING_LEVEL: ThinkingLevel = "medium";
export const THINKING_LEVEL_OPTIONS: readonly ThinkingLevel[] = [
	"off",
	"minimal",
	"low",
	"medium",
	"high",
	"xhigh",
	"max",
];
