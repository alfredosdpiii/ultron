/**
 * Prompt line for agents as Python classes (agent_class_api.py) in the runtime guide's Other APIs section. The
 * details live in `help(agent)` (the decorator's docstring), so the prompt stays one line.
 */
export const AGENT_CLASS_PROMPT = [
	"- `@agent` classes: docstring-only async methods are typed model calls, annotated fields durable state. `help(agent)`.",
].join("\n");
