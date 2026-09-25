import type { JsonValue } from "@ultron/chord";
import type { RefinementKind } from "./local-services.ts";

const MAX_SKILL_BYTES = 64 * 1024;
const MAX_INSTRUCTION_BYTES = 16 * 1024;

/**
 * Content checks run when a refinement is activated (A32). A skill must be a loadable SKILL.md: YAML
 * frontmatter with a kebab-case name and a description, then a nonempty body. Instructions are bounded
 * plain text. Capability requests are refused separately by the refinement service itself.
 */
export function validateRefinementContent(kind: RefinementKind, content: JsonValue): true | undefined {
	if (kind === "skill") {
		if (typeof content !== "string") throw new Error("A skill refinement must be SKILL.md text");
		if (Buffer.byteLength(content) > MAX_SKILL_BYTES) throw new Error("Skill content exceeds 64 KiB");
		const match = /^---\r?\n([\s\S]*?)\r?\n---\r?\n([\s\S]*)$/.exec(content.trimStart());
		if (!match) throw new Error("Skill content needs YAML frontmatter");
		const [, frontmatter = "", body = ""] = match;
		const field = (key: string) => new RegExp(`^${key}:\\s*(.+)$`, "m").exec(frontmatter)?.[1]?.trim();
		const name = field("name");
		if (!name || !/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(name))
			throw new Error("Skill frontmatter needs a kebab-case name");
		if (!field("description")) throw new Error("Skill frontmatter needs a description");
		if (!body.trim()) throw new Error("Skill body is empty");
		return true;
	}
	if (kind === "instruction") {
		if (typeof content !== "string" || !content.trim()) throw new Error("An instruction refinement must be text");
		if (Buffer.byteLength(content) > MAX_INSTRUCTION_BYTES) throw new Error("Instruction exceeds 16 KiB");
		return true;
	}
	return undefined;
}
