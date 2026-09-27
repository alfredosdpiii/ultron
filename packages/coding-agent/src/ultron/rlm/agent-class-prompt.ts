/**
 * Prompt line for agents as Python classes (agent_class_api.py) in the runtime guide's Other APIs section. The
 * details live in `help(agent)` (the decorator's docstring), so the prompt stays one line.
 */
export const AGENT_CLASS_PROMPT = [
	"- Agents as classes: `@agent` on a class makes each docstring-only `async def` method a typed model call (docstring = task, annotations = contract) and annotated fields durable state. `help(agent)`.",
].join("\n");
