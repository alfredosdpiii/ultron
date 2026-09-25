import type { Api, Model, Models } from "@earendil-works/pi-ai";
import type { NativeDefinitionAdapter } from "./rlm/definition-registry.ts";

/**
 * Predict strategy: one model call with no tools and no Python kernel. The prompt carries the
 * definition's instructions, the JSON input, and the output schema; the reply must be JSON. The
 * host validates the value and asks again with the validation error, up to the definition's
 * maxRepairs, so repair stays bounded and visible.
 */
export function createPredictAdapter(options: {
	readonly models: Pick<Models, "completeSimple">;
	/** Model for predict calls; resolved per call so a model switch applies to later calls. */
	readonly model: () => Model<Api> | undefined | Promise<Model<Api> | undefined>;
}): NativeDefinitionAdapter {
	return async ({ definition, input, signal, repair }) => {
		const model = await options.model();
		if (!model) throw new Error("No model is configured for predict definitions");
		const repairNote =
			repair === undefined
				? ""
				: `\n\nYour previous answer was rejected (attempt ${repair.attempt}): ${repair.error}\nPrevious answer: ${JSON.stringify(repair.previous)}`;
		const message = await options.models.completeSimple(
			model,
			{
				systemPrompt:
					"You are a typed function. Reply with a single JSON value that satisfies the output schema. No prose, no code fences.",
				messages: [
					{
						role: "user",
						content: `${definition.instructions}\n\nInput:\n${JSON.stringify(input)}\n\nOutput schema:\n${JSON.stringify(definition.outputSchema)}\n\nOutput contract:\n${definition.outputDescription}${repairNote}`,
						timestamp: Date.now(),
					},
				],
			},
			{ signal },
		);
		if (message.stopReason === "error" || message.stopReason === "aborted")
			throw new Error(message.errorMessage ?? `Predict call ${message.stopReason}`);
		const text = message.content
			.filter((part) => part.type === "text")
			.map((part) => part.text)
			.join("")
			.trim()
			.replace(/^```(?:json)?\s*\n([\s\S]*?)\n```$/, "$1");
		try {
			return JSON.parse(text);
		} catch {
			// Returned as-is so the host's output validation reports it and drives a repair attempt.
			return text;
		}
	};
}
