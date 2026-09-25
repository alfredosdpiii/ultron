import { createServer, type Server } from "node:http";

/** One OpenAI chat-completions request as the scripted provider received it. */
export interface ScriptedRequest {
	readonly body: {
		readonly messages: ReadonlyArray<{
			role: string;
			content?: unknown;
			tool_calls?: ReadonlyArray<{ function: { name: string; arguments: string } }>;
		}>;
	};
	readonly raw: string;
	/** Text of the system message, if any. */
	readonly system: string;
	/** Text of the first user message: identifies which lane or call this is. */
	readonly firstUser: string;
	/** Text of the latest user message (a follow-up on a continued lane). */
	readonly lastUser: string;
	/** Number of assistant messages already in the conversation. */
	readonly turn: number;
	/** Text of the last tool result, if the last message is one. */
	readonly lastToolResult: string | undefined;
}

export type ScriptedReply =
	| { readonly text: string; readonly delayMs?: number }
	| { readonly tool: string; readonly args: Record<string, unknown>; readonly delayMs?: number };

/**
 * Local OpenAI-compatible provider whose replies come from a script, so end-to-end runs of the
 * real CLI are deterministic. Every request is recorded for assertions about what reached a model.
 */
export class ScriptedProvider {
	readonly requests: ScriptedRequest[] = [];
	#server: Server | undefined;
	#calls = 0;

	constructor(private readonly script: (request: ScriptedRequest) => ScriptedReply) {}

	get baseUrl(): string {
		const address = this.#server?.address();
		if (!address || typeof address === "string") throw new Error("Scripted provider is not listening");
		return `http://127.0.0.1:${address.port}/v1`;
	}

	async start(): Promise<void> {
		this.#server = createServer(async (request, response) => {
			const chunks: Buffer[] = [];
			for await (const chunk of request) chunks.push(chunk as Buffer);
			const raw = Buffer.concat(chunks).toString("utf8");
			const body = JSON.parse(raw) as ScriptedRequest["body"];
			const text = (content: unknown): string =>
				typeof content === "string"
					? content
					: Array.isArray(content)
						? content.map((part) => (part as { text?: string }).text ?? "").join("")
						: "";
			const last = body.messages.at(-1);
			const scripted: ScriptedRequest = {
				body,
				raw,
				system: text(body.messages.find((message) => message.role === "system")?.content),
				firstUser: text(body.messages.find((message) => message.role === "user")?.content),
				lastUser: text(body.messages.findLast((message) => message.role === "user")?.content),
				turn: body.messages.filter((message) => message.role === "assistant").length,
				lastToolResult: last?.role === "tool" ? text(last.content) : undefined,
			};
			this.requests.push(scripted);
			let reply: ScriptedReply;
			try {
				reply = this.script(scripted);
			} catch (error) {
				response.writeHead(500).end(String(error));
				return;
			}
			if (reply.delayMs) await new Promise((resolve) => setTimeout(resolve, reply.delayMs));
			response.writeHead(200, { "content-type": "text/event-stream" });
			const id = `scripted-${++this.#calls}`;
			const chunk = (delta: object, finishReason: string | null, usage?: object) =>
				`data: ${JSON.stringify({
					id,
					object: "chat.completion.chunk",
					created: 0,
					model: "scripted",
					choices: [{ index: 0, delta, finish_reason: finishReason }],
					...(usage === undefined ? {} : { usage }),
				})}\n\n`;
			const usage = { prompt_tokens: Math.ceil(raw.length / 4), completion_tokens: 8, total_tokens: 0 };
			if ("text" in reply) {
				response.write(chunk({ role: "assistant", content: reply.text }, null));
				response.write(chunk({}, "stop", usage));
			} else {
				response.write(
					chunk(
						{
							role: "assistant",
							tool_calls: [
								{
									index: 0,
									id: `call_${this.#calls}`,
									type: "function",
									function: { name: reply.tool, arguments: JSON.stringify(reply.args) },
								},
							],
						},
						null,
					),
				);
				response.write(chunk({}, "tool_calls", usage));
			}
			response.end("data: [DONE]\n\n");
		});
		await new Promise<void>((resolve) => this.#server!.listen(0, "127.0.0.1", resolve));
	}

	async stop(): Promise<void> {
		const server = this.#server;
		if (!server) return;
		server.closeAllConnections();
		await new Promise<void>((resolve) => server.close(() => resolve()));
	}
}

/** models.json content pointing a `scripted` provider at this server. */
export function scriptedModelsJson(baseUrl: string): string {
	return JSON.stringify({
		providers: {
			scripted: {
				baseUrl,
				api: "openai-completions",
				apiKey: "scripted-key",
				models: [
					{
						id: "scripted",
						name: "scripted",
						reasoning: false,
						input: ["text"],
						contextWindow: 128000,
						maxTokens: 4096,
					},
				],
			},
		},
	});
}
