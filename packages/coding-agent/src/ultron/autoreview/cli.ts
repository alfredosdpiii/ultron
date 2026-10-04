/**
 * `ultron autoreview`: the automated pull-request reviewer.
 *
 *   ultron autoreview run                       poll and review until stopped
 *   ultron autoreview once                      one poll cycle, then exit
 *   ultron autoreview review <owner/repo#N|URL> review one pull request now
 *   ultron autoreview review --repo-dir <dir> --base <sha> --head <sha> --json --dry-run
 *                                               review a local diff, no GitHub access (the benchmark entry)
 *   ultron autoreview status                    accounts, last poll, queue, recent reviews
 *   ultron autoreview install | uninstall       write or remove the user service
 */
import { resolve } from "node:path";
import { APP_NAME, getAgentDir } from "../../config.ts";
import { FRAME_THINKING_LEVELS, type FrameThinkingLevel, SettingsManager } from "../../core/settings-manager.ts";
import { selfCommand } from "../claude/self.ts";
import { type Account, accountKey, listAccounts, TokenStore } from "./accounts.ts";
import { CheckoutManager } from "./checkout.ts";
import { type AutoreviewConfig, autoreviewPaths, engineSettings, MIN_BUDGET_TOKENS, resolveConfig } from "./config.ts";
import { createLogger, Daemon } from "./daemon.ts";
import { RuntimeReviewEngine } from "./engine.ts";
import { parsePullTarget, pullKey } from "./github.ts";
import { decideVerdict, planReview } from "./plan.ts";
import { type Outcome, reviewPull } from "./reviewer.ts";
import { type Runner, runProcess } from "./runner.ts";
import { installService, serviceFile, uninstallService } from "./service.ts";
import { acquireDaemonLock, DaemonRunningError, StateStore } from "./state.ts";
import type { EngineResult, ReviewEngine } from "./types.ts";

export interface AutoreviewIo {
	stdout(text: string): void;
	stderr(text: string): void;
}

export interface AutoreviewEnvironment {
	readonly agentDir?: string;
	readonly cwd?: string;
	readonly env?: NodeJS.ProcessEnv;
	readonly runner?: Runner;
	/** Replaces the runtime-backed engine (tests). */
	readonly engine?: (options: { dir: string; model?: string; log: (line: string) => void }) => ReviewEngine & {
		start?: () => Promise<void>;
		startMs?: number;
	};
	readonly io?: AutoreviewIo;
	readonly signal?: AbortSignal;
	readonly platform?: NodeJS.Platform;
	readonly home?: string;
}

class UsageError extends Error {}

const USAGE = `Usage: ${APP_NAME} autoreview <command> [options]

Review pull requests automatically, as the accounts gh is logged in to: a review starts when one of them is
requested as reviewer or @mentioned on a pull request, and is posted under that account.

Commands:
  run                          Poll and review until stopped (Ctrl+C, or stop the service)
  once                         One poll cycle: review what is due, then exit
  review <owner/repo#N | URL>  Review one pull request now
  review --repo-dir <dir> --base <sha> --head <sha>
                               Review a local diff with no GitHub access; with --json, one JSON object on stdout
  status                       Accounts, last poll, queue and recent reviews
  install | uninstall          Write or remove the user service that runs "${APP_NAME} autoreview run"

Options:
  --dry-run                    Write the would-be review to <agent dir>/autoreview/dry-run/ instead of posting
  --json                       Machine-readable output (review, once, status)
  --account <login>            review: the account to review as (default: the first logged-in account of the host)
  --model <provider/model>     review: the model of the finder frames (default: autoreview.model, then review.model,
                               then rlm.frameModel, then the default model)
  --verify-model <p/m>         review: the model of the verifier frames (default: the finder model)
  --budget <tokens>            review: a token cap, e.g. 300000 or 300k (default: autoreview.budget, none)
  --thinking <level>           review: thinking level of the finder frames (off, minimal, low, medium, high, ...;
                               default: autoreview.thinking, low)
  --verify-thinking <level>    review: thinking level of the verifier frames (default: autoreview.verifyThinking, low)
  --deadline <seconds>         review: give up unfinished passes after this long and report the rest (default:
                               autoreview.deadlineSeconds, none)

Settings (global settings.json): autoreview.accounts, pollSeconds, concurrency, model, verifyModel, budget, dryRun,
frameConcurrency, thinking, verifyThinking, deadlineSeconds, frameTimeoutSeconds, ack, ackLines, ackArt, signature.
See docs/autoreview.md.`;

interface Parsed {
	command: string;
	target?: string;
	dryRun: boolean;
	json: boolean;
	account?: string;
	model?: string;
	verifyModel?: string;
	budget?: number;
	thinking?: FrameThinkingLevel;
	verifyThinking?: FrameThinkingLevel;
	deadlineSeconds?: number;
	repoDir?: string;
	base?: string;
	head?: string;
	help: boolean;
}

function parseTokens(value: string): number {
	const match = /^(\d+(?:\.\d+)?)([km]?)$/.exec(value.trim().toLowerCase().replace(/[,_]/g, ""));
	if (!match) throw new UsageError(`--budget takes a token count such as 300000 or 300k, not ${value}`);
	const amount = Math.floor(Number(match[1]) * { "": 1, k: 1_000, m: 1_000_000 }[match[2] as "" | "k" | "m"]);
	if (amount < MIN_BUDGET_TOKENS) throw new UsageError(`--budget must be at least ${MIN_BUDGET_TOKENS} tokens`);
	return amount;
}

const MODEL_REF = /^[^/\s]+\/[^/\s]+(?:\/[^/\s]+)*$/;

export function parseAutoreviewArgs(args: readonly string[]): Parsed {
	const parsed: Parsed = { command: args[0] ?? "", dryRun: false, json: false, help: false };
	if (parsed.command === "" || parsed.command === "--help" || parsed.command === "-h" || parsed.command === "help") {
		parsed.help = true;
		return parsed;
	}
	for (let index = 1; index < args.length; index += 1) {
		const arg = args[index]!;
		const value = (): string => {
			const next = args[++index];
			if (next === undefined) throw new UsageError(`${arg} needs a value`);
			return next;
		};
		const model = (): string => {
			const ref = value();
			if (!MODEL_REF.test(ref)) throw new UsageError(`${arg} takes provider/model, not ${ref}`);
			return ref;
		};
		const thinking = (): FrameThinkingLevel => {
			const name = value();
			const level = FRAME_THINKING_LEVELS.find((item) => item === name);
			if (level === undefined)
				throw new UsageError(`${arg} takes one of ${FRAME_THINKING_LEVELS.join(", ")}, not ${name}`);
			return level;
		};
		if (arg === "--help" || arg === "-h") parsed.help = true;
		else if (arg === "--dry-run") parsed.dryRun = true;
		else if (arg === "--json") parsed.json = true;
		else if (arg === "--account") parsed.account = value();
		else if (arg === "--model") parsed.model = model();
		else if (arg === "--verify-model") parsed.verifyModel = model();
		else if (arg === "--budget") parsed.budget = parseTokens(value());
		else if (arg === "--thinking") parsed.thinking = thinking();
		else if (arg === "--verify-thinking") parsed.verifyThinking = thinking();
		else if (arg === "--deadline") {
			const seconds = Number(value());
			if (!Number.isInteger(seconds) || seconds < 0)
				throw new UsageError("--deadline takes whole seconds (0 for no deadline)");
			parsed.deadlineSeconds = seconds;
		} else if (arg === "--repo-dir") parsed.repoDir = value();
		else if (arg === "--base") parsed.base = value();
		else if (arg === "--head") parsed.head = value();
		else if (arg.startsWith("-")) throw new UsageError(`unknown option for ${APP_NAME} autoreview: ${arg}`);
		else if (parsed.target === undefined && parsed.command === "review") parsed.target = arg;
		else throw new UsageError(`unexpected argument: ${arg}`);
	}
	return parsed;
}

/** The JSON object `review --repo-dir ... --json` prints. */
export function offlineJson(result: EngineResult, startupMs: number | undefined): Record<string, unknown> {
	const { verdict } = decideVerdict(result, { selfAuthored: false, state: "open" });
	return {
		verdict,
		complete: result.complete,
		findings: result.findings.map((finding) => ({
			file: finding.file,
			line: finding.line,
			...(finding.endLine === undefined ? {} : { endLine: finding.endLine }),
			severity: finding.severity,
			finderSeverity: finding.finderSeverity ?? finding.severity,
			scenario: finding.scenario ?? "",
			category: finding.category,
			claim: finding.claim,
			why: finding.why,
			...(finding.suggestedFix === undefined ? {} : { suggestedFix: finding.suggestedFix }),
			verification: finding.verification,
			confidence: finding.confidence,
		})),
		dropped: { rejected: result.dropped.rejected, duplicates: result.dropped.duplicates },
		timing: {
			totalMs: result.timing.totalMs,
			scopeMs: result.timing.scopeMs,
			findMs: result.timing.findMs,
			verifyMs: result.timing.verifyMs,
			...(startupMs === undefined ? {} : { startupMs }),
			frames: [...(result.timing.frames ?? [])],
		},
		usage: {
			inputTokens: result.usage.inputTokens,
			outputTokens: result.usage.outputTokens,
			costUsd: result.usage.costUsd,
			frames: result.usage.frames,
		},
		model: result.model,
		verifyModel: result.verifyModel,
		thinking: result.thinking ?? null,
		verifyThinking: result.verifyThinking ?? null,
		notChecked: [...result.notChecked],
	};
}

function age(iso: string | undefined, now: number): string {
	if (!iso) return "never";
	const seconds = Math.max(0, Math.round((now - Date.parse(iso)) / 1000));
	if (seconds < 90) return `${seconds} s ago`;
	if (seconds < 5400) return `${Math.round(seconds / 60)} min ago`;
	return `${Math.round(seconds / 3600)} h ago`;
}

/** Run `ultron autoreview <args>`; returns the exit code. */
export async function runAutoreviewCommand(
	args: readonly string[],
	environment: AutoreviewEnvironment = {},
): Promise<number> {
	const io: AutoreviewIo = environment.io ?? {
		stdout: (text) => process.stdout.write(text),
		stderr: (text) => process.stderr.write(text),
	};
	let parsed: Parsed;
	try {
		parsed = parseAutoreviewArgs(args);
	} catch (error) {
		io.stderr(`Error: ${(error as Error).message}\n`);
		return 2;
	}
	if (parsed.help) {
		io.stdout(`${USAGE}\n`);
		return 0;
	}
	const env = environment.env ?? process.env;
	const agentDir = environment.agentDir ?? getAgentDir();
	const cwd = environment.cwd ?? process.cwd();
	const runner = environment.runner ?? runProcess;
	const paths = autoreviewPaths(agentDir, env);
	const settings = SettingsManager.create(cwd, agentDir, { projectTrusted: false });
	const saved = settings.getAutoreviewSettings();
	const reviewModel = settings.getReviewModel();
	const defaultProvider = settings.getDefaultProvider();
	const defaultModel = settings.getDefaultModel();
	const base = resolveConfig(
		{
			...saved,
			...(parsed.model === undefined ? {} : { model: parsed.model }),
			...(parsed.verifyModel === undefined ? {} : { verifyModel: parsed.verifyModel }),
			...(parsed.budget === undefined ? {} : { budget: parsed.budget }),
			...(parsed.thinking === undefined ? {} : { thinking: parsed.thinking }),
			...(parsed.verifyThinking === undefined ? {} : { verifyThinking: parsed.verifyThinking }),
			...(parsed.deadlineSeconds === undefined ? {} : { deadlineSeconds: parsed.deadlineSeconds }),
		},
		{
			...(reviewModel === undefined ? {} : { reviewModel }),
			rlm: settings.getRlmModelSettings(),
			...(defaultProvider === undefined ? {} : { defaultProvider }),
			...(defaultModel === undefined ? {} : { defaultModel }),
		},
	);
	const config: AutoreviewConfig = { ...base, dryRun: parsed.dryRun || base.dryRun };
	const store = new StateStore(paths.state);

	try {
		switch (parsed.command) {
			case "install":
			case "uninstall": {
				const file = serviceFile(selfCommand(env), {
					platform: environment.platform ?? process.platform,
					...(environment.home === undefined ? {} : { home: environment.home }),
					env,
				});
				if (!file)
					throw new Error(
						`no user service on ${environment.platform ?? process.platform}; run "${APP_NAME} autoreview run" under your own supervisor`,
					);
				if (parsed.command === "install") {
					installService(file);
					io.stdout(
						`Wrote ${file.path}\nNot enabled. To start it now and at login:\n${file.enable.map((line) => `  ${line}`).join("\n")}\n`,
					);
				} else if (uninstallService(file))
					io.stdout(
						`Removed ${file.path}\nIf it is still running:\n${file.disable.map((line) => `  ${line}`).join("\n")}\n`,
					);
				else io.stdout(`No service file at ${file.path}\n`);
				return 0;
			}
			case "status": {
				const state = store.read();
				const accounts = await listAccounts(runner, config.accounts).catch(() => [] as Account[]);
				const now = Date.now();
				if (parsed.json) {
					io.stdout(
						`${JSON.stringify({ accounts: accounts.map((account) => ({ ...account, ...state.accounts[accountKey(account)] })), queue: state.queue ?? [], recent: state.recent, daemon: state.daemon ?? null, dryRun: config.dryRun }, null, 1)}\n`,
					);
					return 0;
				}
				const lines = [
					`Posting: ${config.dryRun ? "off (dry run)" : "on"}; poll every ${config.pollSeconds} s; up to ${config.concurrency} reviews at once; model ${config.model ?? "the default model"}`,
				];
				if (state.daemon)
					lines.push(
						`Daemon: pid ${state.daemon.pid}, started ${age(state.daemon.startedAt, now)}${state.daemon.engineStartMs === undefined ? "" : `, engine start ${state.daemon.engineStartMs} ms`}`,
					);
				lines.push("", "Accounts:");
				if (accounts.length === 0) lines.push("  none (gh auth login)");
				for (const account of accounts) {
					const entry = state.accounts[accountKey(account)] ?? {};
					const paused =
						entry.pausedUntil && Date.parse(entry.pausedUntil) > now ? `, paused until ${entry.pausedUntil}` : "";
					lines.push(
						`  ${account.login} (${account.host}): last poll ${age(entry.lastPollAt, now)}${paused}${entry.lastError ? `, last error: ${entry.lastError}` : ""}`,
					);
				}
				lines.push("", `Queue: ${(state.queue ?? []).length === 0 ? "empty" : ""}`);
				for (const item of state.queue ?? []) lines.push(`  ${item}`);
				lines.push("", "Recent reviews:");
				if (state.recent.length === 0) lines.push("  none");
				for (const record of state.recent.slice(-15).reverse())
					lines.push(
						`  ${record.at}  ${record.pull} ${record.sha.slice(0, 7)} as ${record.account.split("/").pop()}: ${record.outcome}${record.verdict ? ` ${record.verdict}` : ""}, ${record.findings ?? 0} findings, pipeline ${Math.round((record.totalMs ?? 0) / 1000)} s, ${record.tagToAckMs === undefined ? "" : `tag to ack ${(record.tagToAckMs / 1000).toFixed(1)} s, `}${record.ackToPostMs === undefined ? "" : `ack to review ${Math.round(record.ackToPostMs / 1000)} s, `}pickup to post ${Math.round((record.pickupToPostMs ?? 0) / 1000)} s${record.costUsd ? `, $${record.costUsd.toFixed(2)}` : ""}`,
					);
				io.stdout(`${lines.join("\n")}\n`);
				return 0;
			}
			case "run":
			case "once":
			case "review":
				break;
			default:
				throw new UsageError(`unknown command: ${parsed.command}\n\n${USAGE}`);
		}

		const offline = parsed.command === "review" && parsed.repoDir !== undefined;
		const log = createLogger(offline ? undefined : paths.logs);
		const engine = (environment.engine ?? ((options) => new RuntimeReviewEngine(options)))({
			dir: paths.dir,
			...(config.model === undefined ? {} : { model: config.model }),
			log,
		});
		try {
			if (offline) {
				if (parsed.target !== undefined) throw new UsageError("give a pull request or --repo-dir, not both");
				if (!parsed.base || !parsed.head) throw new UsageError("--repo-dir needs --base and --head");
				await engine.start?.();
				const result = await engine.review({
					repoDir: resolve(cwd, parsed.repoDir!),
					base: parsed.base,
					head: parsed.head,
					...engineSettings(config),
				});
				if (parsed.json) io.stdout(`${JSON.stringify(offlineJson(result, engine.startMs))}\n`);
				else
					io.stdout(
						`${planReview(result, { selfAuthored: false, state: "open", headSha: parsed.head, signature: config.signature }).body}\n`,
					);
				return 0;
			}

			const tokens = new TokenStore(runner);
			const checkouts = new CheckoutManager(runner, paths.cache);
			const deps = { runner, tokens, engine, store, checkouts, config, paths, log };
			if (parsed.command === "review") {
				if (parsed.target === undefined)
					throw new UsageError("review needs a pull request (owner/repo#N or its URL) or --repo-dir");
				const ref = parsePullTarget(parsed.target);
				if (!ref)
					throw new UsageError(`not a pull request: ${parsed.target} (use owner/repo#N or the pull request URL)`);
				const accounts = (
					await listAccounts(runner, parsed.account === undefined ? config.accounts : [parsed.account])
				).filter((account) => account.host === ref.host);
				const account = accounts.find((item) => item.active) ?? accounts[0];
				if (!account)
					throw new Error(`gh is not logged in to ${ref.host}${parsed.account ? ` as ${parsed.account}` : ""}`);
				const outcome = await reviewPull(
					deps,
					{ account, ref, reasons: ["manual"], pickedAt: Date.now() },
					{ force: true, dryRun: config.dryRun },
				);
				io.stdout(
					parsed.json ? `${JSON.stringify(outcomeJson(outcome))}\n` : `${outcomeText(pullKey(ref), outcome)}\n`,
				);
				return outcome.kind === "posted" || outcome.kind === "dry-run" ? 0 : 1;
			}

			const accounts = await listAccounts(runner, config.accounts);
			if (accounts.length === 0) throw new Error("gh is not logged in to any account (gh auth login)");
			const outcomes: Array<Record<string, unknown>> = [];
			const daemon = new Daemon({
				...deps,
				accounts,
				onOutcome: (candidate, outcome) => {
					outcomes.push({
						pull: pullKey(candidate.ref),
						account: candidate.account.login,
						...("verdict" in outcome ? outcomeJson(outcome) : outcome),
					});
				},
			});
			let release: (() => Promise<void>) | undefined;
			try {
				release = await acquireDaemonLock(paths.dir);
			} catch (error) {
				if (error instanceof DaemonRunningError) throw error;
				throw new Error(`cannot lock ${paths.dir}: ${(error as Error).message}`);
			}
			try {
				log(
					`autoreview ${parsed.command}: ${accounts.map((account) => `${account.login}@${account.host}`).join(", ")}; posting ${config.dryRun ? "off (dry run)" : "on"}`,
				);
				if (parsed.command === "once") {
					await daemon.once();
					if (parsed.json) io.stdout(`${JSON.stringify({ reviews: outcomes })}\n`);
					return 0;
				}
				// A warm engine: the first review does not pay for opening the runtime.
				await engine.start?.();
				await store.update((state) => {
					state.daemon = {
						pid: process.pid,
						startedAt: new Date().toISOString(),
						...(engine.startMs === undefined ? {} : { engineStartMs: engine.startMs }),
					};
				});
				const stop = new AbortController();
				const onSignal = () => stop.abort();
				process.once("SIGINT", onSignal);
				process.once("SIGTERM", onSignal);
				environment.signal?.addEventListener("abort", onSignal, { once: true });
				if (environment.signal?.aborted) stop.abort();
				try {
					await daemon.run(stop.signal);
				} finally {
					process.off("SIGINT", onSignal);
					process.off("SIGTERM", onSignal);
					await store
						.update((state) => {
							delete state.daemon;
							delete state.queue;
						})
						.catch(() => {});
				}
				log("autoreview stopped");
				return 0;
			} finally {
				await release().catch(() => {});
			}
		} finally {
			await engine.close().catch(() => {});
		}
	} catch (error) {
		io.stderr(`Error: ${error instanceof Error ? error.message : String(error)}\n`);
		return error instanceof UsageError ? 2 : 1;
	}
}

function outcomeJson(outcome: Outcome): Record<string, unknown> {
	if (outcome.kind === "posted" || outcome.kind === "dry-run")
		return {
			kind: outcome.kind,
			sha: outcome.sha,
			event: outcome.plan.event,
			...offlineJson(outcome.result, undefined),
			verdict: outcome.verdict,
			inlineComments: outcome.plan.comments.length,
			...(outcome.reviewId === undefined ? {} : { reviewId: outcome.reviewId }),
			...(outcome.path === undefined ? {} : { files: [`${outcome.path}.json`, `${outcome.path}.md`] }),
		};
	return { ...outcome };
}

function outcomeText(name: string, outcome: Outcome): string {
	if ("reason" in outcome) return `${name}: ${outcome.kind} (${outcome.reason})`;
	if (outcome.kind === "posted")
		return `Posted ${outcome.plan.event} on ${name} at ${outcome.sha.slice(0, 7)}: ${outcome.result.findings.length} findings, ${outcome.plan.comments.length} inline.`;
	return `Dry run of ${name} at ${outcome.sha.slice(0, 7)}: ${outcome.plan.event}, ${outcome.result.findings.length} findings. Written to ${outcome.path}.md and .json`;
}

/**
 * The process entry: stdout carries only the command's own output (the runtime's prints go to stderr), so
 * `--json` output is one clean JSON object.
 */
export async function runAutoreviewMain(args: readonly string[]): Promise<number> {
	const write = process.stdout.write.bind(process.stdout);
	process.stdout.write = process.stderr.write.bind(process.stderr) as typeof process.stdout.write;
	console.log = console.error;
	console.info = console.error;
	const code = await runAutoreviewCommand(args, {
		io: { stdout: (text) => void write(text), stderr: (text) => void process.stderr.write(text) },
	});
	// Let the JSON reach a pipe before main() exits the process.
	await new Promise<void>((done) => write("", () => done()));
	return code;
}
