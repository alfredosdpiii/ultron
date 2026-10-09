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
import { existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { resolveClaudeCli } from "@ultron/ai/api/claude-code-cli";
import { APP_NAME, getAgentDir, getRlmRuntimePath } from "../../config.ts";
import { DEFAULT_HINDSIGHT_URL } from "../../core/defaults.ts";
import { ModelConfig } from "../../core/model-config.ts";
import { FRAME_THINKING_LEVELS, type FrameThinkingLevel, SettingsManager } from "../../core/settings-manager.ts";
import { selfCommand } from "../claude/self.ts";
import { type Account, accountKey, listAccounts, TokenStore } from "./accounts.ts";
import { CheckoutManager } from "./checkout.ts";
import {
	type AutoreviewConfig,
	autoreviewPaths,
	engineSettings,
	MIN_BUDGET_TOKENS,
	PLAN_STYLES,
	type PlanStyle,
	RECOMMENDED_MODELS,
	REVIEW_MODES,
	type ReviewMode,
	resolveConfig,
} from "./config.ts";
import { createLogger, Daemon } from "./daemon.ts";
import { RuntimeReviewEngine } from "./engine.ts";
import { parsePullTarget, pullKey } from "./github.ts";
import { HindsightLessons, hindsightUrlFrom } from "./lessons.ts";
import { expandPath, findCheckout } from "./local.ts";
import { decideVerdict, planReview, rankFindings } from "./plan.ts";
import { cachedEnvironment, describeToolchain, prepareEnvironment } from "./prepare.ts";
import { type Outcome, reviewPull } from "./reviewer.ts";
import { type Runner, runProcess } from "./runner.ts";
import { installService, serviceFile, uninstallService } from "./service.ts";
import { acquireDaemonLock, DaemonRunningError, StateStore } from "./state.ts";
import type { EngineResult, Level, ReviewEngine } from "./types.ts";
import { LEVELS, levelOf, severityOf } from "./types.ts";

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
  doctor [--repo owner/name]   The sandbox test execution would use, and a self-check of its isolation; with --repo,
                               the local checkout whose environments would be bound into it
  prepare <repo-dir> [--python 3.x]
                               Build the repository's test environment now, with the network (uv virtualenv, npm ci,
                               mise toolchains), into the cache the sandbox binds read-only during reviews
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
  --mode <fast|deep|both|compiled|hybrid>
                               review: fast reviews the diff; deep investigates beyond it by read-only lookups in the
                               repository (nothing is executed); both (default) does one after the other, each
                               finding verified by a verifier frame. Experimental: hybrid runs both as discovery and
                               verifies each candidate with a host-written check program; compiled has one planner
                               write the whole program
  --verify-candidates <n>      review --mode hybrid (experimental): candidates verified at most (default:
                               autoreview.verifyCandidates, 12)
  --deep-model <p/m>           review: the model of the deep pass's investigators (default: the finder model)
  --deep-thinking <level>      review: their thinking level (default: autoreview.deepThinking, high; medium was
                               measured to find a third as much). Every --*-thinking default is per model: the
                               recommended model thinks high at every stage
  --verify-batch <n>           review: findings of one file judged by one verifier frame (default:
                               autoreview.verifyBatch, 4; 1 is one frame per finding)
  --plan-model <p/m>           review --mode compiled: the planner's model (default: autoreview.planModel, the finder model)
  --plan-thinking <level>      review --mode compiled: its thinking level (default: autoreview.planThinking; medium for
                               cells, high for a frame)
  --ask-model <p/m>            review --mode compiled: the small model the program's questions go to (default:
                               autoreview.askModel, the finder model)
  --ask-thinking <level>       review --mode compiled: its thinking level (default: autoreview.askThinking, low)
  --plan-style <cell|frame>    review --mode compiled: the planner as sandboxed Python cells over the rv API (default)
                               or as one JSON-program frame
  --plan-cells <n>             review --mode compiled: cells the planner may run (default: autoreview.planCells, 4)
  --dump-program <path>        review --mode compiled: save the validated program as JSON for inspection
  --program <path>             review --mode compiled: execute this saved program instead of calling the planner
  --run-tests | --no-run-tests review --repo-dir: let the deep pass run the project's tests in a sandbox (default:
                               autoreview.runTests, on); never without a sandbox
  --prepare                    review --repo-dir: prepare the environment first (as "prepare" does), then review
  --python <3.x>               prepare, review --prepare: the Python version to build the virtualenv with
  --test-env <dir>             review --repo-dir: a pre-built environment (virtualenv, node_modules) to bind read-only
  --checkout-roots <dir,...>   review: directories of local checkouts whose environments may serve the tests
                               (with --repo owner/name, whose remote the checkout must have)
  --guides <path,...>          review: private review guides (markdown files or directories)
  --block-at <level>           review: request changes from this level up (critical, high, medium, low, nit)
  --max-comments <n>           review: inline comments at most
  --deadline <seconds>         review: give up unfinished passes after this long and report the rest (default:
                               autoreview.deadlineSeconds, none)

Settings (global settings.json): autoreview.accounts, pollSeconds, concurrency, model, verifyModel, budget, dryRun,
frameConcurrency, modelConcurrency, mode, deepModel, deepThinking, deepRounds, referenceView, verifyBatch, planModel, planThinking, askModel, askThinking, planStyle, planCells, verifyCandidates, prepareEnvs, mise, blockAt, maxComments, runTests, testOwners, testRuns, testTimeoutSeconds, testEnv, testImage, checkoutRoots, guides, thinking, verifyThinking, deadlineSeconds, frameTimeoutSeconds, ack, ackLines, ackArt, signature, learn, skipBots.
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
	mode?: ReviewMode;
	deepModel?: string;
	deepThinking?: FrameThinkingLevel;
	planModel?: string;
	planThinking?: FrameThinkingLevel;
	askModel?: string;
	askThinking?: FrameThinkingLevel;
	planStyle?: PlanStyle;
	planCells?: number;
	verifyCandidates?: number;
	verifyBatch?: number;
	programPath?: string;
	dumpProgramPath?: string;
	deadlineSeconds?: number;
	runTests?: boolean;
	testEnv?: string;
	prepare: boolean;
	python?: string;
	repo?: string;
	guides?: string[];
	checkoutRoots?: string[];
	blockAt?: Level;
	maxComments?: number;
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
	const parsed: Parsed = { command: args[0] ?? "", dryRun: false, json: false, help: false, prepare: false };
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
		const list = (): string[] =>
			value()
				.split(",")
				.map((item) => item.trim())
				.filter(Boolean);
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
		else if (arg === "--deep-thinking") parsed.deepThinking = thinking();
		else if (arg === "--deep-model") parsed.deepModel = model();
		else if (arg === "--plan-model") parsed.planModel = model();
		else if (arg === "--plan-thinking") parsed.planThinking = thinking();
		else if (arg === "--ask-model") parsed.askModel = model();
		else if (arg === "--ask-thinking") parsed.askThinking = thinking();
		else if (arg === "--plan-style") {
			const name = value();
			const style = PLAN_STYLES.find((item) => item === name);
			if (style === undefined) throw new UsageError(`--plan-style takes cell or frame, not ${name}`);
			parsed.planStyle = style;
		} else if (arg === "--plan-cells") {
			const count = Number(value());
			if (!Number.isInteger(count) || count < 1) throw new UsageError("--plan-cells takes a whole number of cells");
			parsed.planCells = count;
		} else if (arg === "--verify-candidates") {
			const count = Number(value());
			if (!Number.isInteger(count) || count < 1) throw new UsageError("--verify-candidates takes a whole number");
			parsed.verifyCandidates = count;
		} else if (arg === "--verify-batch") {
			const count = Number(value());
			if (!Number.isInteger(count) || count < 1) throw new UsageError("--verify-batch takes a whole number");
			parsed.verifyBatch = count;
		} else if (arg === "--program") parsed.programPath = value();
		else if (arg === "--dump-program") parsed.dumpProgramPath = value();
		else if (arg === "--run-tests") parsed.runTests = true;
		else if (arg === "--no-run-tests") parsed.runTests = false;
		else if (arg === "--test-env") parsed.testEnv = value();
		else if (arg === "--prepare") parsed.prepare = true;
		else if (arg === "--python") {
			const version = value();
			if (!/^\d+(\.\d+){0,2}$/.test(version))
				throw new UsageError(`--python takes a version such as 3.12, not ${version}`);
			parsed.python = version;
		} else if (arg === "--repo") parsed.repo = value();
		else if (arg === "--guides") parsed.guides = list();
		else if (arg === "--checkout-roots") parsed.checkoutRoots = list();
		else if (arg === "--block-at") {
			const name = value();
			const level = LEVELS.find((item) => item === name);
			if (level === undefined) throw new UsageError(`--block-at takes one of ${LEVELS.join(", ")}, not ${name}`);
			parsed.blockAt = level;
		} else if (arg === "--max-comments") {
			const count = Number(value());
			if (!Number.isInteger(count) || count < 0) throw new UsageError("--max-comments takes a whole number");
			parsed.maxComments = count;
		} else if (arg === "--mode") {
			const name = value();
			const mode = REVIEW_MODES.find((item) => item === name);
			if (mode === undefined) throw new UsageError(`--mode takes fast, deep, both, compiled or hybrid, not ${name}`);
			parsed.mode = mode;
		} else if (arg === "--deadline") {
			const seconds = Number(value());
			if (!Number.isInteger(seconds) || seconds < 0)
				throw new UsageError("--deadline takes whole seconds (0 for no deadline)");
			parsed.deadlineSeconds = seconds;
		} else if (arg === "--repo-dir") parsed.repoDir = value();
		else if (arg === "--base") parsed.base = value();
		else if (arg === "--head") parsed.head = value();
		else if (arg.startsWith("-")) throw new UsageError(`unknown option for ${APP_NAME} autoreview: ${arg}`);
		else if (parsed.target === undefined && (parsed.command === "review" || parsed.command === "prepare"))
			parsed.target = arg;
		else throw new UsageError(`unexpected argument: ${arg}`);
	}
	return parsed;
}

/** The JSON object `review --repo-dir ... --json` prints. */
/** The model each stage runs on, with its thinking level, for `doctor` and `install`. */
export function describeModels(config: AutoreviewConfig): string[] {
	const finder = config.model ?? "the session's default model";
	const stage = (name: string, model: string | undefined, thinking: string): string =>
		`  ${name}: ${model ?? finder} (thinking ${thinking})`;
	return [
		`Models${config.model !== undefined && RECOMMENDED_MODELS.includes(config.model) ? " (the recommended set; it thinks high at every stage)" : ""}:`,
		stage("finders", config.model, config.thinking),
		stage("investigators", config.deepModel, config.deepThinking),
		stage("verifier", config.verifyModel, config.verifyThinking),
		stage("planner (compiled)", config.planModel, config.planThinking),
		stage("questions (compiled)", config.askModel, config.askThinking),
	];
}

/** The per-stage durations of a review for the status line: "; map 1 s, tests 20 s, find 31 s, deep 58 s, verify 19 s, post 3 s". */
export function describeStages(stages: Readonly<Record<string, number>> | undefined): string {
	if (stages === undefined) return "";
	const parts = Object.entries(stages)
		.filter(([, ms]) => typeof ms === "number" && ms > 0)
		.map(([name, ms]) => `${name.replace(/Ms$/, "")} ${Math.round(ms / 1000)} s`);
	return parts.length === 0 ? "" : `; ${parts.join(", ")}`;
}

export function offlineJson(
	result: EngineResult,
	startupMs: number | undefined,
	blockAt?: Level,
	maxComments?: number,
): Record<string, unknown> {
	const options = {
		selfAuthored: false,
		state: "open" as const,
		...(blockAt === undefined ? {} : { blockAt }),
		...(maxComments === undefined ? {} : { maxComments }),
	};
	const { verdict } = decideVerdict(result, options);
	// What the poster would do with each finding: the same ranking and cap, computed here too.
	const plan = planReview(result, { ...options, headSha: "0".repeat(40), signature: false });
	const inline = new Set(plan.comments.map((comment) => comment.finding));
	const inBody = new Set(plan.inSummary);
	const ranks = new Map(rankFindings(result).map((index, position) => [index, position + 1]));
	return {
		verdict,
		complete: result.complete,
		findings: result.findings.map((finding, index) => ({
			// Where the finding would go: an inline comment, named in the body, or only counted.
			posted: inline.has(index) ? "inline" : inBody.has(index) ? "body" : "counted",
			rank: ranks.get(index) ?? null,
			...(finding.unpinned === undefined ? {} : { unpinned: finding.unpinned }),
			...(finding.consequence === undefined ? {} : { consequence: finding.consequence }),
			...(finding.unclear === true ? { unclear: true } : {}),
			file: finding.file,
			line: finding.line,
			...(finding.endLine === undefined ? {} : { endLine: finding.endLine }),
			// `severity` stays on the old four-name scale; `level` is the five-level one the verdict uses.
			severity: severityOf(levelOf(finding)),
			level: levelOf(finding),
			finderSeverity: finding.finderSeverity ?? severityOf(levelOf(finding)),
			finderLevel: finding.finderLevel ?? levelOf(finding),
			...(finding.verifierLevel === undefined ? {} : { verifierLevel: finding.verifierLevel }),
			...(finding.verifierScenarioHolds === undefined
				? {}
				: { verifierScenarioHolds: finding.verifierScenarioHolds }),
			scenario: finding.scenario ?? "",
			category: finding.category,
			claim: finding.claim,
			why: finding.why,
			...(finding.suggestedFix === undefined ? {} : { suggestedFix: finding.suggestedFix }),
			verification: finding.verification,
			confidence: finding.confidence,
			...(finding.alsoAt === undefined ? {} : { alsoAt: finding.alsoAt }),
			source: finding.source ?? "fast",
			...(finding.verifiedBy === undefined ? {} : { verifiedBy: finding.verifiedBy }),
			evidence: finding.evidence ?? "",
			howVerified: finding.howVerified ?? "",
		})),
		dropped: {
			rejected: result.dropped.rejected,
			duplicates: result.dropped.duplicates,
			generic: result.dropped.generic ?? 0,
			refutedByTest: result.dropped.refutedByTest ?? 0,
			duplicateOf: result.dropped.duplicateOf ?? [],
		},
		timing: {
			totalMs: result.timing.totalMs,
			scopeMs: result.timing.scopeMs,
			findMs: result.timing.findMs,
			verifyMs: result.timing.verifyMs,
			...(startupMs === undefined ? {} : { startupMs }),
			...(result.timing.stages === undefined ? {} : { stages: { ...result.timing.stages } }),
			frames: [...(result.timing.frames ?? [])],
			investigators: [...(result.timing.investigators ?? [])],
			...(result.timing.reference ? { reference: { ...result.timing.reference } } : {}),
		},
		usage: {
			inputTokens: result.usage.inputTokens,
			outputTokens: result.usage.outputTokens,
			costUsd: result.usage.costUsd,
			frames: result.usage.frames,
			...(result.usage.byPhase === undefined ? {} : { byPhase: result.usage.byPhase }),
		},
		model: result.model,
		verifyModel: result.verifyModel,
		mode: result.mode ?? "fast",
		assurance: (result.assurance ?? []).join(" "),
		tests: result.tests ?? { enabled: false, mechanism: null, note: null, runs: [] },
		deepModel: result.deepModel ?? null,
		deepThinking: result.deepThinking ?? null,
		thinking: result.thinking ?? null,
		verifyThinking: result.verifyThinking ?? null,
		// The compiled mode: the planner's and the small model's settings, and the program's stats and steps.
		planModel: result.planModel ?? null,
		planThinking: result.planThinking ?? null,
		askModel: result.askModel ?? null,
		askThinking: result.askThinking ?? null,
		planStyle: result.planStyle ?? null,
		verification: result.verification ?? null,
		program:
			result.program === undefined || result.program === null
				? null
				: { ...result.program, steps: [...(result.timing.program ?? [])] },
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
	// The user's own catalog (models.json): the recommended model leads the fallback chain when it is there.
	const catalog = await ModelConfig.load(join(agentDir, "models.json")).catch(() => undefined);
	const hasModel = (ref: string): boolean => {
		const slash = ref.indexOf("/");
		// Claude Code's models are served by the `claude` CLI, not listed in the catalog: available when the provider
		// would find it (ULTRON_CLAUDE_CODE_BIN, else `claude` on PATH).
		if (ref.startsWith("claude-code/")) {
			try {
				resolveClaudeCli(env);
				return true;
			} catch {
				return false;
			}
		}
		if (slash <= 0 || catalog === undefined) return false;
		const models = catalog.getProvider(ref.slice(0, slash))?.models ?? [];
		return models.some((model) => model.id === ref.slice(slash + 1));
	};
	const base = resolveConfig(
		{
			...saved,
			...(parsed.model === undefined ? {} : { model: parsed.model }),
			...(parsed.verifyModel === undefined ? {} : { verifyModel: parsed.verifyModel }),
			...(parsed.budget === undefined ? {} : { budget: parsed.budget }),
			...(parsed.thinking === undefined ? {} : { thinking: parsed.thinking }),
			...(parsed.verifyThinking === undefined ? {} : { verifyThinking: parsed.verifyThinking }),
			...(parsed.mode === undefined ? {} : { mode: parsed.mode }),
			...(parsed.guides === undefined ? {} : { guides: parsed.guides }),
			...(parsed.checkoutRoots === undefined ? {} : { checkoutRoots: parsed.checkoutRoots }),
			...(parsed.blockAt === undefined ? {} : { blockAt: parsed.blockAt }),
			...(parsed.maxComments === undefined ? {} : { maxComments: parsed.maxComments }),
			...(parsed.deepModel === undefined ? {} : { deepModel: parsed.deepModel }),
			...(parsed.deepThinking === undefined ? {} : { deepThinking: parsed.deepThinking }),
			...(parsed.planModel === undefined ? {} : { planModel: parsed.planModel }),
			...(parsed.planThinking === undefined ? {} : { planThinking: parsed.planThinking }),
			...(parsed.askModel === undefined ? {} : { askModel: parsed.askModel }),
			...(parsed.askThinking === undefined ? {} : { askThinking: parsed.askThinking }),
			...(parsed.planStyle === undefined ? {} : { planStyle: parsed.planStyle }),
			...(parsed.planCells === undefined ? {} : { planCells: parsed.planCells }),
			...(parsed.verifyCandidates === undefined ? {} : { verifyCandidates: parsed.verifyCandidates }),
			...(parsed.verifyBatch === undefined ? {} : { verifyBatch: parsed.verifyBatch }),
			...(parsed.deadlineSeconds === undefined ? {} : { deadlineSeconds: parsed.deadlineSeconds }),
		},
		{
			...(reviewModel === undefined ? {} : { reviewModel }),
			rlm: settings.getRlmModelSettings(),
			...(defaultProvider === undefined ? {} : { defaultProvider }),
			...(defaultModel === undefined ? {} : { defaultModel }),
			hasModel,
		},
	);
	const config: AutoreviewConfig = { ...base, dryRun: parsed.dryRun || base.dryRun };
	const modelLines = describeModels(config);
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
						`Wrote ${file.path}\n${modelLines.join("\n")}\nNot enabled. To start it now and at login:\n${file.enable.map((line) => `  ${line}`).join("\n")}\n`,
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
						`  ${record.at}  ${record.pull} ${record.sha.slice(0, 7)} as ${record.account.split("/").pop()}: ${record.outcome}${record.verdict ? ` ${record.verdict}` : ""}, ${record.findings ?? 0} findings, pipeline ${Math.round((record.totalMs ?? 0) / 1000)} s, ${record.tagToAckMs === undefined ? "" : `tag to ack ${(record.tagToAckMs / 1000).toFixed(1)} s, `}${record.ackToPostMs === undefined ? "" : `ack to review ${Math.round(record.ackToPostMs / 1000)} s, `}pickup to post ${Math.round((record.pickupToPostMs ?? 0) / 1000)} s${record.costUsd ? `, $${record.costUsd.toFixed(2)}` : ""}${describeStages(record.stages)}`,
					);
				io.stdout(`${lines.join("\n")}\n`);
				return 0;
			}
			case "prepare": {
				if (parsed.target === undefined) throw new UsageError("prepare needs the repository directory");
				const prepared = await prepareEnvironment(resolve(cwd, parsed.target), {
					runner,
					cacheDir: paths.cache,
					mise: config.mise,
					env,
					...(parsed.python === undefined ? {} : { python: parsed.python }),
					log: (line) => io.stderr(`${line}\n`),
					force: true,
				});
				if (parsed.json) io.stdout(`${JSON.stringify(prepared)}\n`);
				else
					io.stdout(
						`${[
							`Environment ${prepared.hash} at ${prepared.dir}${prepared.cached ? " (cached)" : ""}`,
							...prepared.prepared.map((item) => `  prepared: ${item}`),
							...describeToolchain(prepared.toolchain).map((line) => `  toolchain: ${line}`),
							...prepared.failures.map((item) => `  failed: ${item}`),
							`  ${Math.round(prepared.ms / 1000)} s`,
						].join("\n")}\n`,
					);
				return prepared.failures.length === 0 ? 0 : 1;
			}
			case "doctor": {
				const python =
					env.ULTRON_PYTHON ??
					(process.platform === "linux" && existsSync("/usr/bin/python3") ? "/usr/bin/python3" : "python3");
				const script = join(dirname(getRlmRuntimePath()), "autoreview_tests.py");
				const result = await runner([python, script, "doctor", ...(config.testImage ? [config.testImage] : [])], {
					timeoutMs: 180_000,
				});
				let report: {
					mechanism: string | null;
					isolation?: string;
					ok: boolean;
					message?: string;
					selfCheck?: Record<string, unknown>;
				};
				try {
					report = JSON.parse(result.stdout.trim().split("\n").at(-1) ?? "");
				} catch {
					throw new Error(
						`the sandbox check did not run: ${(result.stderr || result.stdout).trim().slice(0, 300)}`,
					);
				}
				// `--repo owner/name`: the local checkout that would lend its environments, and what would be bound.
				let lent: { repo: string; checkout: string | null; environments: unknown[] } | undefined;
				if (parsed.repo !== undefined) {
					const ref = parsePullTarget(`${parsed.repo}#1`);
					if (!ref) throw new UsageError(`--repo takes owner/name, not ${parsed.repo}`);
					const checkout = await findCheckout(runner, config.checkoutRoots, ref);
					let environments: unknown[] = [];
					if (checkout !== undefined) {
						const listed = await runner([python, script, "environments", checkout], { timeoutMs: 60_000 });
						try {
							environments = JSON.parse(listed.stdout.trim().split("\n").at(-1) ?? "[]") as unknown[];
						} catch {
							environments = [];
						}
					}
					lent = { repo: parsed.repo, checkout: checkout ?? null, environments };
				}
				// The prepared environment of that checkout, when there is one: its toolchain and where it came from.
				const prepared = lent?.checkout ? cachedEnvironment(lent.checkout, paths.cache) : undefined;
				const preparedLines =
					lent === undefined
						? []
						: prepared === undefined
							? ["Prepared environment: none (run `ultron autoreview prepare <repo-dir>`)"]
							: [
									`Prepared environment: ${prepared.dir}`,
									...prepared.prepared.map((item) => `  ${item}`),
									...describeToolchain(prepared.toolchain).map((line) => `  toolchain: ${line}`),
								];
				const lentLines =
					lent === undefined
						? []
						: lent.checkout === null
							? [`Local checkout of ${lent.repo}: none under autoreview.checkoutRoots with a matching remote`]
							: [
									`Local checkout of ${lent.repo}: ${lent.checkout}`,
									...(lent.environments.length === 0
										? ["  no prepared environment (.venv, venv, node_modules) found: nothing would be bound"]
										: (lent.environments as Array<{ path: string; kind: string; interpreter?: string }>).map(
												(item) =>
													`  bound read-only: ${item.path} (${item.kind}${item.interpreter && item.interpreter !== "system" ? `, with its interpreter ${item.interpreter}` : ""})`,
											)),
									"  never bound: the checkout's source, .git, .env files",
								];
				if (parsed.json)
					io.stdout(
						`${JSON.stringify({ ...report, runTests: config.runTests, ...(lent ? { lent } : {}), ...(prepared ? { prepared } : {}) })}\n`,
					);
				else if (report.mechanism === null)
					io.stdout(
						`${[...modelLines, `Sandbox: none. ${report.message ?? ""}`, ...lentLines, ...preparedLines].join("\n")}\n`,
					);
				else {
					const check = report.selfCheck ?? {};
					const owners = config.testOwners.length ? ` and owners ${config.testOwners.join(", ")}` : "";
					const testsLine = !config.runTests
						? "off (autoreview.runTests)"
						: report.ok
							? `run for repositories the account can push to${owners}`
							: "not run (the self-check failed)";
					io.stdout(
						`${[
							...modelLines,
							`Sandbox: ${report.isolation}`,
							`Self-check: ${report.ok ? "passed" : "FAILED"}`,
							`  network: ${String(check.network)}`,
							`  canary file in the real home: ${String(check.canary)}`,
							`  token-like variable of the caller: ${String(check.token)}`,
							`  home inside the sandbox: ${String(check.home)}`,
							`  exported commit: ${String(check.workdir)}; system directories: ${String(check.system)}`,
							`  Docker socket: ${String(check.dockerSocket)}`,
							`Tests in reviews: ${testsLine}`,
							...lentLines,
							...preparedLines,
						].join("\n")}\n`,
					);
				}
				return report.ok ? 0 : 1;
			}
			case "run":
			case "once":
				// `/autoreview off` (autoreview.enabled: false): the loop and a one-off poll do nothing, so a service left
				// installed cannot review; `review` of one pull request still works.
				if (saved.enabled === false) {
					io.stderr(
						`autoreview is off (autoreview.enabled: false; /autoreview on or ${APP_NAME} autoreview is not needed for one review)\n`,
					);
					return 0;
				}
				break;
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
				if (
					(parsed.programPath !== undefined || parsed.dumpProgramPath !== undefined) &&
					config.mode !== "compiled"
				)
					throw new UsageError("--program and --dump-program need --mode compiled");
				const named = parsed.repo === undefined ? undefined : parsePullTarget(`${parsed.repo}#1`);
				const lent =
					named !== undefined && config.checkoutRoots.length > 0
						? await findCheckout(runner, config.checkoutRoots, named)
						: undefined;
				// --prepare: build (or reuse) the environment first, with the network; the review then binds it read-only.
				const prepared = parsed.prepare
					? await prepareEnvironment(resolve(cwd, parsed.repoDir!), {
							runner,
							cacheDir: paths.cache,
							mise: config.mise,
							env,
							...(parsed.python === undefined ? {} : { python: parsed.python }),
							log: (line) => io.stderr(`${line}\n`),
						})
					: undefined;
				if (prepared !== undefined)
					for (const line of [
						`prepared environment ${prepared.hash}${prepared.cached ? " (cached)" : ""}: ${prepared.prepared.join("; ") || "nothing"}`,
						...prepared.failures.map((item) => `prepare: ${item}`),
					])
						io.stderr(`${line}\n`);
				const usePrepared =
					prepared !== undefined && prepared.failures.length === 0 && prepared.prepared.length > 0;
				await engine.start?.();
				const result = await engine.review({
					repoDir: resolve(cwd, parsed.repoDir!),
					base: parsed.base,
					head: parsed.head,
					...engineSettings(config),
					// A local repository is the user's own: its tests may run (sandboxed) unless turned off.
					runTests: config.mode !== "fast" && (parsed.runTests ?? config.runTests),
					...(parsed.testEnv !== undefined
						? { testEnv: resolve(cwd, parsed.testEnv), testEnvKind: "testEnv" as const }
						: usePrepared
							? {
									testCheckout: prepared.dir,
									testToolchain: prepared.toolchain.map((entry) => entry.path),
									testEnvKind: "prepared" as const,
								}
							: // A checkout of the named repository under --checkout-roots, else the repository given: it is
								// itself a local checkout, and its own prepared environments serve the tests.
								{ testCheckout: lent ?? resolve(cwd, parsed.repoDir!), testEnvKind: "checkout" as const }),
					...(config.guides.length === 0 ? {} : { guides: config.guides.map((path) => expandPath(path)) }),
					...(parsed.repo === undefined ? {} : { repo: parsed.repo }),
					...(parsed.programPath === undefined ? {} : { programPath: resolve(cwd, parsed.programPath) }),
					...(parsed.dumpProgramPath === undefined
						? {}
						: { dumpProgramPath: resolve(cwd, parsed.dumpProgramPath) }),
				});
				if (parsed.json)
					io.stdout(
						`${JSON.stringify(offlineJson(result, engine.startMs, config.blockAt, config.maxComments))}\n`,
					);
				else
					io.stdout(
						`${planReview(result, { selfAuthored: false, state: "open", headSha: parsed.head, signature: config.signature, blockAt: config.blockAt, maxComments: config.maxComments }).body}\n`,
					);
				return 0;
			}

			const tokens = new TokenStore(runner);
			const checkouts = new CheckoutManager(runner, paths.cache);
			// Fates are retained in Hindsight when memory is on (the same server the REPL's memory uses).
			const hindsight = config.learn
				? hindsightUrlFrom(env.ULTRON_HINDSIGHT_URL, settings.getHindsightUrl(), DEFAULT_HINDSIGHT_URL)
				: undefined;
			const lessons = hindsight === undefined ? undefined : new HindsightLessons({ baseUrl: hindsight });
			const deps = { runner, tokens, engine, store, checkouts, config, paths, log, ...(lessons ? { lessons } : {}) };
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
