/**
 * The review engine: `autoreview_api.run` executed with frame inference available and no root model.
 *
 * `/review` costs a root-model turn just to run one REPL cell. Here the host is Ultron's runtime opened for an
 * external root (as `ultron mcp` opens it: harness, model registry, frame executor, usage ledger; no extensions,
 * skills or context files, and a working directory of its own, so nothing of a reviewed repository is loaded),
 * and each review runs in a Python kernel of its own whose host requests go straight to that runtime. Nothing
 * ever prompts a root lane: the only model calls are the pipeline's `rlm.map` frames, on the model the spec names.
 *
 * One runtime serves any number of concurrent reviews (a kernel is a short-lived Python process), so `ultron
 * autoreview run` keeps it warm; `startMs` is what opening it cost.
 */
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { type JsonlSessionMetadata, JsonlSessionRepo, type Session, TODO_CONTEXT } from "@ultron/agent-core";
import { NodeExecutionEnv } from "@ultron/agent-core/node";
import { BACKGROUND_CONTEXT, withAbortSignal } from "@ultron/chord/context";
import { getRlmRuntimePath } from "../../config.ts";
import { createUltronRuntime, type UltronRuntime } from "../../experimental/session-worker.ts";
import { RlmKernel } from "../rlm/kernel.ts";
import type { EngineResult, EngineSpec, ReviewEngine } from "./types.ts";

export interface EngineOptions {
	/** `<agentDir>/autoreview`: sessions and the kernels' working directory live under it. */
	readonly dir: string;
	/** `provider/model` the runtime should resolve as its model (frames that name none run on it). */
	readonly model?: string;
	readonly log?: (line: string) => void;
}

function splitModel(model: string | undefined): { provider: string; model: string } | undefined {
	if (model === undefined) return undefined;
	const slash = model.indexOf("/");
	return slash <= 0 || slash === model.length - 1
		? undefined
		: { provider: model.slice(0, slash), model: model.slice(slash + 1) };
}

/** The engine's session files (frame traces) are kept this long. */
const SESSION_RETENTION_MS = 7 * 24 * 60 * 60 * 1000;

/** Remove session files of earlier runs that are past retention. Never throws. */
export function pruneSessions(sessionDir: string, now = Date.now()): void {
	try {
		for (const group of readdirSync(sessionDir, { withFileTypes: true })) {
			if (!group.isDirectory()) continue;
			for (const entry of readdirSync(join(sessionDir, group.name), { withFileTypes: true })) {
				const path = join(sessionDir, group.name, entry.name);
				if (entry.isFile() && entry.name.endsWith(".jsonl") && now - statSync(path).mtimeMs > SESSION_RETENTION_MS)
					rmSync(path, { force: true });
			}
		}
	} catch {
		// Housekeeping only.
	}
}

export class RuntimeReviewEngine implements ReviewEngine {
	readonly #options: EngineOptions;
	#opened: Promise<{ runtime: UltronRuntime; close(): Promise<void> }> | undefined;
	#model: string | undefined;
	/** Milliseconds opening the runtime took (undefined until it is open). */
	startMs: number | undefined;

	constructor(options: EngineOptions) {
		this.#options = options;
	}

	get defaultModel(): string | undefined {
		return this.#model;
	}

	/** Open the runtime now (otherwise the first review opens it). */
	async start(): Promise<void> {
		await this.#runtime();
	}

	#runtime(): Promise<{ runtime: UltronRuntime; close(): Promise<void> }> {
		this.#opened ??= this.#open();
		return this.#opened;
	}

	async #open(): Promise<{ runtime: UltronRuntime; close(): Promise<void> }> {
		const started = Date.now();
		const cwd = join(this.#options.dir, "work");
		const sessionDir = join(this.#options.dir, "sessions");
		mkdirSync(cwd, { recursive: true, mode: 0o700 });
		mkdirSync(sessionDir, { recursive: true, mode: 0o700 });
		pruneSessions(sessionDir);
		const executionEnv = new NodeExecutionEnv({ cwd });
		const repo = new JsonlSessionRepo({ fileSystem: executionEnv, sessionsRoot: sessionDir });
		const session: Session<JsonlSessionMetadata> = await repo.create({ cwd }, TODO_CONTEXT);
		await session.setName("autoreview", TODO_CONTEXT).catch(() => {});
		const metadata = session.metadata;
		const preferred = splitModel(this.#options.model);
		let runtime: UltronRuntime;
		try {
			runtime = await createUltronRuntime(
				session,
				{
					sessionDir,
					metadata: {
						id: metadata.id,
						createdAt: metadata.createdAt,
						storageVersion: metadata.storageVersion,
						cwd: metadata.cwd,
						path: metadata.path,
						modifiedAt: metadata.modifiedAt,
					},
					extensionMode: "print",
					pluginManifestPaths: [],
					noExtensions: true,
					noSkills: true,
					noContextFiles: true,
					externalRoot: preferred === undefined ? {} : { preferredModel: preferred },
				},
				executionEnv,
			);
		} catch (error) {
			await session.close(TODO_CONTEXT).catch(() => {});
			await repo.close(TODO_CONTEXT).catch(() => {});
			throw error;
		}
		this.#model = runtime.model;
		this.startMs = Date.now() - started;
		this.#options.log?.(`engine ready in ${this.startMs} ms (model ${runtime.model})`);
		return {
			runtime,
			close: async () => {
				await runtime.closeRlm?.().catch(() => {});
				await runtime.harness.close(TODO_CONTEXT).catch(() => {});
				await repo.close(TODO_CONTEXT).catch(() => {});
				await executionEnv.cleanup(TODO_CONTEXT).catch(() => {});
			},
		};
	}

	async review(spec: EngineSpec, signal?: AbortSignal): Promise<EngineResult> {
		const { runtime } = await this.#runtime();
		const scratch = mkdtempSync(join(tmpdir(), "ultron-autoreview-"));
		const specPath = join(scratch, "spec.json");
		const resultPath = join(scratch, "result.json");
		writeFileSync(specPath, JSON.stringify(spec), { mode: 0o600 });
		const kernel = new RlmKernel(
			{
				cwd: join(this.#options.dir, "work"),
				runtimePath: getRlmRuntimePath(),
				// The kernel runs our own pipeline, and (sandboxed) the reviewed project's tests as its children:
				// the per-process address-space and CPU limits meant for model-written cells would break test
				// runners. The cap on the whole process tree stays.
				limits: { maxMemoryMb: 0, maxCpuSeconds: 0 },
			},
			(type, payload, requestSignal) =>
				runtime.hostRequest(
					type,
					payload,
					requestSignal === undefined ? BACKGROUND_CONTEXT : withAbortSignal(requestSignal, BACKGROUND_CONTEXT),
					{ lane: "main" },
				),
		);
		try {
			const cell = await kernel.execute(
				`import autoreview_api\nawait autoreview_api.run_file(rlm, ${JSON.stringify(specPath)}, ${JSON.stringify(resultPath)})`,
				signal,
			);
			if (cell.status === "error")
				throw new Error(
					`the review pipeline failed: ${cell.error?.ename ?? "error"}: ${cell.error?.evalue ?? ""}\n${(cell.error?.traceback ?? []).slice(-6).join("\n")}`.trim(),
				);
			const result = JSON.parse(readFileSync(resultPath, "utf8")) as EngineResult & { error?: string };
			if (typeof result.error === "string") throw new Error(result.error);
			return {
				...result,
				model: result.model ?? runtime.model,
				verifyModel: result.verifyModel ?? result.model ?? runtime.model,
			};
		} finally {
			await kernel.shutdown().catch(() => {});
			rmSync(scratch, { recursive: true, force: true });
		}
	}

	async close(): Promise<void> {
		const opened = this.#opened;
		this.#opened = undefined;
		if (opened) await (await opened.catch(() => undefined))?.close();
	}
}
