/**
 * A pass-through HTTP recorder in front of the model proxy.
 *
 * Every arm's base URL points at `http://127.0.0.1:<port>/run/<runId>/v1`; the recorder strips `/run/<runId>`,
 * forwards the request byte for byte to the upstream proxy and streams the response back unchanged. On the side it
 * keeps one ledger line per request: path, the `model` and reasoning effort the client asked for, the model the
 * upstream answered with, the HTTP status and the usage block of the response. That is the harness's evidence that
 * an arm really ran on the model it claims, and a token count taken the same way for every arm (the tools' own
 * usage reports stay the primary numbers).
 *
 * Headers are never recorded, so no key reaches a ledger.
 */

import { appendFileSync } from "node:fs";
import { createServer, request as httpRequest } from "node:http";

const RUN_PREFIX = /^\/run\/([A-Za-z0-9._-]+)(\/.*)$/;

/** `/run/<id>/v1/x` -> `{ runId, path: "/v1/x" }`; anything else belongs to no run. */
export function splitRunPath(url) {
	const match = RUN_PREFIX.exec(url);
	return match ? { runId: match[1], path: match[2] } : { runId: null, path: url };
}

/** The fields of a request body worth keeping: what the client asked for, never its content. */
export function requestFacts(bodyText) {
	let body;
	try {
		body = JSON.parse(bodyText);
	} catch {
		return {};
	}
	if (!body || typeof body !== "object") return {};
	const effort = body.reasoning_effort ?? body.reasoning?.effort ?? null;
	return {
		model: typeof body.model === "string" ? body.model : null,
		reasoningEffort: typeof effort === "string" ? effort : null,
		stream: body.stream === true,
		tools: Array.isArray(body.tools) ? body.tools.length : 0,
		items: Array.isArray(body.messages) ? body.messages.length : Array.isArray(body.input) ? body.input.length : null,
	};
}

function usageFrom(value) {
	if (!value || typeof value !== "object") return null;
	const number = (candidate) => (typeof candidate === "number" ? candidate : 0);
	const input = number(value.input_tokens ?? value.prompt_tokens);
	const output = number(value.output_tokens ?? value.completion_tokens);
	if (value.input_tokens === undefined && value.prompt_tokens === undefined) return null;
	const cached = number(value.input_tokens_details?.cached_tokens ?? value.prompt_tokens_details?.cached_tokens);
	const reasoning = number(
		value.output_tokens_details?.reasoning_tokens ?? value.completion_tokens_details?.reasoning_tokens,
	);
	return { input, cachedInput: cached, output, reasoning };
}

/**
 * The answering model and the usage block of a response body, which is either one JSON document or a server-sent
 * event stream (Chat Completions chunks, or Responses API events whose final `response.completed` carries usage).
 * Input tokens include cached ones, as both OpenAI APIs report them.
 */
export function responseFacts(bodyText) {
	let model = null;
	let usage = null;
	const visit = (payload) => {
		if (!payload || typeof payload !== "object") return;
		const response = payload.response && typeof payload.response === "object" ? payload.response : payload;
		if (typeof response.model === "string" && response.model) model = response.model;
		usage = usageFrom(response.usage) ?? usage;
	};
	const trimmed = bodyText.trimStart();
	if (trimmed.startsWith("{")) {
		try {
			visit(JSON.parse(trimmed));
			return { model, usage };
		} catch {
			// Not one JSON document: fall through and read it as an event stream.
		}
	}
	for (const line of bodyText.split("\n")) {
		if (!line.startsWith("data:")) continue;
		const data = line.slice(5).trim();
		if (!data || data === "[DONE]") continue;
		try {
			visit(JSON.parse(data));
		} catch {
			// A partial or non-JSON event carries nothing the ledger needs.
		}
	}
	return { model, usage };
}

/**
 * Start the recorder. `ledgerPath(runId)` names the JSONL file a run's requests are appended to (null drops them).
 * Resolves to `{ port, close }`.
 */
export function startRecorder({ upstream, ledgerPath, host = "127.0.0.1", port = 0 }) {
	const target = new URL(upstream);
	const server = createServer((incoming, outgoing) => {
		const { runId, path } = splitRunPath(incoming.url ?? "/");
		const started = Date.now();
		const requestChunks = [];
		const record = (entry) => {
			const file = runId ? ledgerPath(runId) : null;
			if (!file) return;
			try {
				appendFileSync(file, `${JSON.stringify({ at: new Date(started).toISOString(), method: incoming.method, path, ...entry })}\n`);
			} catch {
				// A ledger that cannot be written must not break the agent's request.
			}
		};
		// One ledger line per request, written when the exchange ends however it ends: Codex closes its connection
		// as soon as it has read `response.completed`, before the upstream stream ends.
		let recorded = false;
		let status = 0;
		const responseChunks = [];
		const finish = (extra = {}) => {
			if (recorded) return;
			recorded = true;
			const facts = responseFacts(Buffer.concat(responseChunks).toString("utf8"));
			record({
				status,
				ms: Date.now() - started,
				request: requestFacts(Buffer.concat(requestChunks).toString("utf8")),
				responseModel: facts.model,
				usage: facts.usage,
				...extra,
			});
		};
		const forward = httpRequest(
			{
				host: target.hostname,
				port: target.port,
				method: incoming.method,
				path,
				headers: { ...incoming.headers, host: target.host },
			},
			(upstreamResponse) => {
				status = upstreamResponse.statusCode ?? 0;
				outgoing.writeHead(upstreamResponse.statusCode ?? 502, upstreamResponse.headers);
				upstreamResponse.on("data", (chunk) => {
					responseChunks.push(chunk);
					outgoing.write(chunk);
				});
				upstreamResponse.on("end", () => {
					outgoing.end();
					finish();
				});
				upstreamResponse.on("error", () => {
					outgoing.destroy();
					finish({ error: "upstream stream error" });
				});
			},
		);
		forward.on("error", (error) => {
			const clientGone = outgoing.destroyed;
			finish(clientGone ? { clientClosed: true } : { error: String(error.message ?? error).slice(0, 200) });
			if (clientGone) return;
			if (!outgoing.headersSent) outgoing.writeHead(502, { "content-type": "application/json" });
			outgoing.end(JSON.stringify({ error: { message: "recorder: upstream request failed" } }));
		});
		incoming.on("data", (chunk) => {
			requestChunks.push(chunk);
			forward.write(chunk);
		});
		incoming.on("end", () => forward.end());
		// The client went away (agent killed at the wall-clock limit): stop the upstream request too.
		outgoing.on("close", () => {
			if (outgoing.writableEnded) return;
			finish({ clientClosed: true });
			forward.destroy();
		});
	});
	return new Promise((resolveStart, rejectStart) => {
		server.once("error", rejectStart);
		server.listen(port, host, () => {
			const address = server.address();
			resolveStart({
				port: typeof address === "object" && address ? address.port : port,
				close: () => new Promise((done) => server.close(() => done())),
			});
		});
	});
}
