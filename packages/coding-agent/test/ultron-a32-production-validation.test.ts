import { describe, expect, test } from "vitest";
import { createWorkerServices } from "../src/ultron/worker-services.ts";

function sessionValues() {
	const values = new Map<string, unknown>();
	const key = (address: { namespace: string; key: string }) => `${address.namespace}\0${address.key}`;
	return {
		getValue: async (address: { namespace: string; key: string }) =>
			values.has(key(address)) ? { address, value: structuredClone(values.get(key(address))) } : undefined,
		setValue: async (address: { namespace: string; key: string }, value: unknown) => {
			values.set(key(address), structuredClone(value));
		},
		scanValues: async (prefix: { namespace: string; key: string }) =>
			[...values]
				.filter(([stored]) => stored.startsWith(`${prefix.namespace}\0${prefix.key}`))
				.map(([stored, value]) => ({
					address: { namespace: prefix.namespace, key: stored.split("\0")[1] },
					value,
				})),
	};
}

const context = {} as never;

describe("A32 production skill validation", () => {
	test("the worker's refinement service refuses malformed skills and accepts a loadable SKILL.md", async () => {
		const services = createWorkerServices({ session: sessionValues() as never, sessionId: "s", cwd: process.cwd() });
		const propose = async (content: string, target: string) =>
			(
				(await services.handle(
					"refinements.propose",
					{ kind: "skill", target, baseVersion: 0, content, evidence: [{ repeated: 3 }] },
					context,
				)) as { id: string }
			).id;
		const activate = (id: string) => services.handle("refinements.activate", { id }, context);

		await expect(activate(await propose("just prose, no frontmatter", "skill:bad-one"))).rejects.toThrow(
			"frontmatter",
		);
		await expect(
			activate(await propose("---\nname: Bad Name\ndescription: x\n---\nbody", "skill:bad-two")),
		).rejects.toThrow("kebab-case");
		await expect(
			activate(await propose("---\nname: empty-body\ndescription: x\n---\n   ", "skill:bad-three")),
		).rejects.toThrow("body is empty");
		await expect(
			activate(
				await propose(
					"---\nname: release-notes\ndescription: Draft release notes from merged changes\n---\nCollect merged PRs, then group them.",
					"skill:release-notes",
				),
			),
		).resolves.toMatchObject({ state: "active", approval: "not_required" });
	});
});
