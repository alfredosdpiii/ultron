#!/usr/bin/env node

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { basename, dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { getPublicWorkspacePackages } from "./release-packages.mjs";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const codingAgentDirectory = join(repoRoot, "packages", "coding-agent");
const codingAgentPackage = JSON.parse(readFileSync(join(codingAgentDirectory, "package.json"), "utf8"));

function run(command, args, options = {}) {
	console.log(`$ ${command} ${args.join(" ")}`);
	const result = spawnSync(command, args, {
		cwd: repoRoot,
		encoding: "utf8",
		shell: process.platform === "win32",
		timeout: 300_000,
		...options,
	});
	if (result.status !== 0) {
		throw new Error(
			`Command failed: ${command} ${args.join(" ")}\n${result.stdout ?? ""}${result.stderr ?? ""}${result.error?.message ?? ""}`,
		);
	}
	return result.stdout ?? "";
}

function buildCodingAgent() {
	if (process.env.ULTRON_ACCEPTANCE_USE_EXISTING_BUILD === "1") {
		console.log("Using the existing coding-agent build (ULTRON_ACCEPTANCE_USE_EXISTING_BUILD=1).");
		return;
	}
	run("npm", ["run", "build"], { cwd: codingAgentDirectory });
}

function packWorkspacePackages(tarballDirectory) {
	mkdirSync(tarballDirectory, { recursive: true });
	const tarballs = new Map();
	for (const pkg of getPublicWorkspacePackages()) {
		const manifest = JSON.parse(readFileSync(join(pkg.directory, "package.json"), "utf8"));
		if (manifest.name !== pkg.name) throw new Error(`Unexpected package name in ${pkg.directory}`);
		const output = run("npm", ["pack", "--ignore-scripts", "--json", "--pack-destination", tarballDirectory], {
			cwd: pkg.directory,
		});
		const parsed = JSON.parse(output);
		const packed = Array.isArray(parsed) ? parsed[0] : Object.values(parsed)[0];
		if (!packed?.filename) throw new Error(`npm pack returned no filename for ${pkg.name}`);
		tarballs.set(pkg.name, join(tarballDirectory, basename(packed.filename)));
	}
	return tarballs;
}

function installConsumer(directory, tarballs) {
	mkdirSync(directory, { recursive: true });
	const overrides = Object.fromEntries(
		[...tarballs].map(([name, path]) => [name, `file:./${relative(directory, path).replaceAll("\\", "/")}`]),
	);
	const codingAgentTarball = tarballs.get(codingAgentPackage.name);
	if (!codingAgentTarball) throw new Error(`Missing ${codingAgentPackage.name} tarball`);
	writeFileSync(
		join(directory, "package.json"),
		`${JSON.stringify(
			{
				private: true,
				dependencies: {
					[codingAgentPackage.name]: `file:./${relative(directory, codingAgentTarball).replaceAll("\\", "/")}`,
				},
				overrides,
			},
			null,
			2,
		)}\n`,
	);
	run("npm", ["install", "--ignore-scripts", "--omit=dev", "--no-audit", "--no-fund"], { cwd: directory });
}

function assertShippedFiles(packageDirectory) {
	const requiredFiles = [
		"dist/bundle/cli.js",
		"dist/bundle/coordinator-entry.js",
		"dist/bundle/server-entry.js",
		"dist/bundle/session-worker-entry.js",
		"dist/ultron/rlm/runtime.py",
		"dist/modes/interactive/theme/dark.json",
		"dist/modes/interactive/theme/light.json",
		"dist/modes/interactive/assets/clankolas.png",
		"dist/core/export-html/template.html",
	];
	for (const path of requiredFiles) {
		const file = join(packageDirectory, path);
		if (!existsSync(file)) throw new Error(`Packed package is missing ${path}`);
		if (statSync(file).size === 0) throw new Error(`Packed package contains an empty ${path}`);
	}
	if (existsSync(join(packageDirectory, "src"))) {
		throw new Error("Packed package unexpectedly contains the source tree");
	}
}

async function startMockProvider() {
	const requests = [];
	const server = createServer(async (request, response) => {
		const chunks = [];
		for await (const chunk of request) chunks.push(chunk);
		const body = Buffer.concat(chunks).toString("utf8");
		requests.push({ method: request.method, url: request.url, body: JSON.parse(body) });
		if (request.method !== "POST" || request.url !== "/v1/chat/completions") {
			response.writeHead(404).end();
			return;
		}
		response.writeHead(200, {
			"content-type": "text/event-stream",
			"cache-control": "no-cache",
			connection: "keep-alive",
		});
		const chunk = {
			id: "mock-completion",
			object: "chat.completion.chunk",
			created: 0,
			model: "mock",
			choices: [{ index: 0, delta: { role: "assistant", content: "ULTRON_PACKED_MOCK_OK" }, finish_reason: null }],
		};
		const done = {
			id: "mock-completion",
			object: "chat.completion.chunk",
			created: 0,
			model: "mock",
			choices: [{ index: 0, delta: {}, finish_reason: "stop" }],
			usage: { prompt_tokens: 7, completion_tokens: 1, total_tokens: 8 },
		};
		response.write(`data: ${JSON.stringify(chunk)}\n\n`);
		response.write(`data: ${JSON.stringify(done)}\n\n`);
		response.end("data: [DONE]\n\n");
	});
	await new Promise((resolveServer, reject) => {
		server.once("error", reject);
		server.listen(0, "127.0.0.1", resolveServer);
	});
	const address = server.address();
	if (!address || typeof address === "string") throw new Error("Mock provider did not bind to a TCP port");
	return {
		baseUrl: `http://127.0.0.1:${address.port}/v1`,
		requests,
		close: () => new Promise((resolveClose, reject) => server.close((error) => (error ? reject(error) : resolveClose()))),
	};
}

function isolatedCliEnvironment(root, consumerDirectory, mockBaseUrl) {
	const home = join(root, "home");
	const agentDirectory = join(home, ".ultron", "agent");
	// Unix socket paths have a platform limit. Keep the server socket directory
	// short even though the packed consumer lives under a long temporary path.
	const serverDirectory = join(tmpdir(), `u-s-${process.pid}`);
	const projectDirectory = join(root, "project");
	mkdirSync(agentDirectory, { recursive: true });
	mkdirSync(serverDirectory, { recursive: true });
	mkdirSync(projectDirectory, { recursive: true });
	writeFileSync(
		join(agentDirectory, "models.json"),
		`${JSON.stringify(
			{
				providers: {
					mock: {
						name: "Deterministic local mock",
						baseUrl: mockBaseUrl,
						api: "openai-completions",
						apiKey: "mock-key",
						models: [
							{
								id: "mock",
								name: "mock",
								api: "openai-completions",
								reasoning: false,
								input: ["text"],
								contextWindow: 32_000,
								maxTokens: 256,
							},
						],
					},
				},
			},
			null,
			2,
		)}\n`,
	);
	return {
		cwd: projectDirectory,
		env: {
			PATH: `${join(consumerDirectory, "node_modules", ".bin")}${process.platform === "win32" ? ";" : ":"}${process.env.PATH ?? ""}`,
			HOME: home,
			USERPROFILE: home,
			APPDATA: home,
			LOCALAPPDATA: home,
			XDG_CONFIG_HOME: join(home, ".config"),
			XDG_CACHE_HOME: join(home, ".cache"),
			XDG_DATA_HOME: join(home, ".local", "share"),
			XDG_STATE_HOME: join(home, ".local", "state"),
			ULTRON_CODING_AGENT_DIR: agentDirectory,
			ULTRON_CODING_AGENT_SESSION_DIR: join(home, ".ultron", "sessions"),
			ULTRON_SERVER_DIR: serverDirectory,
			ULTRON_ACCEPTANCE_SERVER_DIRECTORY: serverDirectory,
			ULTRON_DEBUG_INTERNAL: "1",
			PI_OFFLINE: "1",
			PI_SKIP_VERSION_CHECK: "1",
			PI_TELEMETRY: "0",
			NO_COLOR: "1",
		},
	};
}

export async function runPackedNativeCliAcceptance() {
	const root = mkdtempSync(join(tmpdir(), "ultron-native-cli-consumer-"));
	const mock = await startMockProvider();
	let serverDirectory;
	try {
		buildCodingAgent();
		const tarballs = packWorkspacePackages(join(root, "tarballs"));
		const consumer = join(root, "consumer");
		installConsumer(consumer, tarballs);
		const packageDirectory = join(consumer, "node_modules", ...codingAgentPackage.name.split("/"));
		assertShippedFiles(packageDirectory);

		const isolated = isolatedCliEnvironment(root, consumer, mock.baseUrl);
		serverDirectory = isolated.env.ULTRON_SERVER_DIR;
		const command = join(consumer, "node_modules", ".bin", process.platform === "win32" ? "ultron.cmd" : "ultron");
		const args = [
			"--provider",
			"mock",
			"--model",
			"mock",
			"--print",
			"--no-session",
			"Return the deterministic acceptance marker.",
		];
		console.log(`$ ${command} ${args.join(" ")}`);
		const result = await new Promise((resolveResult, rejectResult) => {
			const child = spawn(command, args, {
				cwd: isolated.cwd,
				env: isolated.env,
				windowsHide: true,
				stdio: ["ignore", "pipe", "pipe"],
			});
			let stdout = "";
			let stderr = "";
			const timer = setTimeout(() => {
				child.kill("SIGKILL");
				rejectResult(new Error(`Packed ultron CLI timed out. stdout: ${stdout} stderr: ${stderr}`));
			}, 120_000);
			child.stdout.setEncoding("utf8");
			child.stderr.setEncoding("utf8");
			child.stdout.on("data", (chunk) => {
				stdout += chunk;
			});
			child.stderr.on("data", (chunk) => {
				stderr += chunk;
			});
			child.once("error", (error) => {
				clearTimeout(timer);
				rejectResult(error);
			});
			child.once("close", (status, signal) => {
				clearTimeout(timer);
				resolveResult({ status, signal, stdout, stderr });
			});
		});
		if (result.status !== 0) {
			throw new Error(
				`Packed ultron CLI failed with status ${result.status}\nstdout:\n${result.stdout}\nstderr:\n${result.stderr}`,
			);
		}
		if (!result.stdout.includes("ULTRON_PACKED_MOCK_OK")) {
			throw new Error(
				`Packed ultron CLI returned unexpected output (status=${result.status}, signal=${result.signal ?? "none"}):\nstdout:\n${result.stdout}\nstderr:\n${result.stderr}\nmock requests: ${JSON.stringify(mock.requests)}`,
			);
		}
		if (mock.requests.length !== 1) throw new Error(`Expected one mock request, got ${mock.requests.length}`);
		const request = mock.requests[0];
		if (request.method !== "POST" || request.url !== "/v1/chat/completions") {
			throw new Error(`Unexpected mock request route: ${request.method} ${request.url}`);
		}
		if (request.body.model !== "mock" || request.body.stream !== true) {
			throw new Error(`Unexpected mock request body: ${JSON.stringify(request.body)}`);
		}
		return { consumer, packageDirectory, output: result.stdout, requests: mock.requests };
	} finally {
		await mock.close();
		if (serverDirectory) rmSync(serverDirectory, { recursive: true, force: true });
		if (process.env.ULTRON_ACCEPTANCE_KEEP !== "1") {
			try {
				rmSync(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
			} catch (error) {
				if (!(error instanceof Error && "code" in error && error.code === "ENOTEMPTY")) throw error;
			}
		}
		else console.error(`Kept acceptance root: ${root}`);
	}
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
	await runPackedNativeCliAcceptance();
	console.log("Packed Ultron native CLI acceptance passed.");
}
