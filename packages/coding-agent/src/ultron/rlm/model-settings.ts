/**
 * Which model each kind of RLM work runs on, and why: the precedence behind `/settings → Models`.
 *
 * - Code-free `rlm.infer`/`rlm.map` frames: the call's `model=` > ULTRON_RLM_FRAME_MODEL > `rlm.frameModel` > the
 *   session's model.
 * - /review frames: `--model` > ULTRON_REVIEW_MODEL > `review.model` > the frame model above.
 * - `rlm.spawn` sub-agents: the call's `model=` > `rlm.childModel` > the session's model (`ultron claude` passes its
 *   own `claudeCode.ultronChildModel` first).
 *
 * The settings are global only (`~/.ultron/agent/settings.json`), and the worker reads them for every frame and
 * spawn, so a change in `/settings` applies to the next one without a restart.
 */
import type { RlmModelSettings } from "../../core/settings-manager.ts";

/** A `provider/model` reference (the settings manager checks saved values with the same pattern). */
const MODEL_REF_PATTERN = /^[^/\s]+\/[^/\s]+(?:\/[^/\s]+)*$/;

/** Environment variable naming the default model (provider/model) of code-free inference frames. */
export const FRAME_MODEL_ENV = "ULTRON_RLM_FRAME_MODEL";
/** Environment variable naming the model of /review frames (read by review_api.py as `--model`'s default). */
export const REVIEW_MODEL_ENV = "ULTRON_REVIEW_MODEL";

/** Where an effective model comes from. */
export type ModelSource = "env" | "setting" | "frame" | "session";

export interface EffectiveModel {
	/** `provider/model`, or undefined when the session's model applies. */
	readonly model?: string;
	readonly source: ModelSource;
	/** The environment variable, when `source` is `env`. */
	readonly env?: string;
}

/** A `provider/model` value of an environment variable, or undefined when unset or malformed. */
export function envModel(env: NodeJS.ProcessEnv, name: string): string | undefined {
	const value = env[name]?.trim();
	return value && MODEL_REF_PATTERN.test(value) ? value : undefined;
}

/** The model of a code-free frame that names none. */
export function effectiveFrameModel(env: NodeJS.ProcessEnv, settings: RlmModelSettings): EffectiveModel {
	const fromEnv = envModel(env, FRAME_MODEL_ENV);
	if (fromEnv !== undefined) return { model: fromEnv, source: "env", env: FRAME_MODEL_ENV };
	if (settings.frameModel !== undefined) return { model: settings.frameModel, source: "setting" };
	return { source: "session" };
}

/** The model of /review frames without `--model`. */
export function effectiveReviewModel(
	env: NodeJS.ProcessEnv,
	reviewModel: string | undefined,
	settings: RlmModelSettings,
): EffectiveModel {
	const fromEnv = envModel(env, REVIEW_MODEL_ENV);
	if (fromEnv !== undefined) return { model: fromEnv, source: "env", env: REVIEW_MODEL_ENV };
	if (reviewModel !== undefined) return { model: reviewModel, source: "setting" };
	const frame = effectiveFrameModel(env, settings);
	return frame.model === undefined ? frame : { model: frame.model, source: "frame" };
}

/** The model of an `rlm.spawn` sub-agent without `model=`. */
export function effectiveChildModel(settings: RlmModelSettings): EffectiveModel {
	return settings.childModel === undefined ? { source: "session" } : { model: settings.childModel, source: "setting" };
}

/** Whether `model` (provider/model) runs on the Claude Code CLI provider, which has no tool calling. */
export function isClaudeCodeModelRef(model: string): boolean {
	return model.startsWith("claude-code/");
}
