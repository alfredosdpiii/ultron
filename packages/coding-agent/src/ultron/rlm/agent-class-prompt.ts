/**
 * Prompt fragment for agents as Python classes (agent_class_api.py). It is one bullet of the rlm tool
 * description today and belongs in the system prompt's Runtime section once that exists.
 */
export const AGENT_CLASS_PROMPT = [
	"- Agents as classes: `@agent` on a class (or subclass `Agent`) makes it an agent. The class docstring is its role; an `async def` method whose body is only a docstring and `...` is done by a model as a typed `agents.invoke` (docstring = task, annotated arguments = input data, return annotation = contract checked by the host; a bad return raises `AgentCallError` with the schema error). Methods with real bodies run here as ordinary Python.",
	'  Annotated fields (`seen: int = 0`, `notes: list[str] = []`) are durable state shown to the model: `t = Triage("main")` reattaches to key "main" after a scratch reset or snapshot restore, and the class is defined again from its stored source. Types: str, int, float, bool, None, list, dict[str, T], tuple, set, Optional/Union, Literal, Enum, dataclass, TypedDict, pydantic.',
	"  Redefining a class makes a new generation: `Triage.generations()`, `Triage.at(1)` calls generation 1, `await Triage.rollback(1)` makes it current; `agent.schema(T)` gives the JSON schema for any annotation.",
].join("\n");
