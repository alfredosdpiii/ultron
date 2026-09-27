/**
 * `view_image` in the RLM REPL: an image the kernel names reaches the model as an image content block on the rlm
 * tool result (Pi's read-tool shape), never as printed base64.
 * - paths, bytes, PIL images and matplotlib figures; the type comes from magic bytes, not the extension;
 * - at most 8 per cell; oversized images are downscaled by Pi's image pipeline;
 * - a model without image input gets a plain note; `read` on an image points at `view_image`;
 * - through the real CLI, the next provider request carries the image, RPC events and `get_messages` show it in
 *   Pi's shape, and it survives a session restore.
 */
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { crc32, deflateSync } from "node:zlib";
import { NodeExecutionEnv } from "@ultron/agent-core/node";
import type { Api, ImageContent, Model, TextContent } from "@ultron/ai";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { getImageDimensions } from "@ultron/tui";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { createUltronRlmTool } from "../src/experimental/session-worker.ts";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";
import { rlmRuntimePrompt } from "../src/ultron/rlm/prompt.ts";
import { ScriptedProvider, type ScriptedRequest, scriptedModelsJson } from "./support/scripted-provider.ts";
import { tempServerDir } from "./support/server-dir.ts";

/** A solid-color RGB PNG, encoded by hand so the tests need no image library. */
function png(width: number, height: number, rgb: [number, number, number] = [220, 30, 30]): Buffer {
	const chunk = (type: string, data: Buffer) => {
		const length = Buffer.alloc(4);
		length.writeUInt32BE(data.length);
		const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
		const crc = Buffer.alloc(4);
		crc.writeUInt32BE(crc32(body));
		return Buffer.concat([length, body, crc]);
	};
	const header = Buffer.alloc(13);
	header.writeUInt32BE(width, 0);
	header.writeUInt32BE(height, 4);
	header[8] = 8; // bit depth
	header[9] = 2; // truecolor
	const row = Buffer.alloc(1 + width * 3);
	for (let x = 0; x < width; x++) row.set(rgb, 1 + x * 3);
	const raw = Buffer.concat(Array.from({ length: height }, () => row));
	return Buffer.concat([
		Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
		chunk("IHDR", header),
		chunk("IDAT", deflateSync(raw)),
		chunk("IEND", Buffer.alloc(0)),
	]);
}

function pythonHas(module: string): boolean {
	try {
		execFileSync("/usr/bin/python3", ["-c", `import ${module}`], { stdio: "ignore" });
		return true;
	} catch {
		return false;
	}
}

const model = (input: ("text" | "image")[]): Model<Api> =>
	({ provider: "test", id: input.includes("image") ? "vision" : "text-only", input }) as unknown as Model<Api>;

type Content = (TextContent | ImageContent)[];

describe("view_image in the rlm tool", () => {
	let cwd: string;
	let tool: ReturnType<typeof createUltronRlmTool>;
	let current: Model<Api> | undefined;
	const invocation = {
		invocationId: "view-image",
		operationId: "operation-view-image",
		turnId: "turn-view-image",
		getMemo: async () => undefined,
		setMemo: async () => undefined,
	};
	const run = async (code: string): Promise<Content> =>
		(
			await tool.execute(
				"call",
				{ code },
				() => {},
				{ env: new NodeExecutionEnv({ cwd }) },
				invocation,
				BACKGROUND_CONTEXT,
			)
		).content as Content;
	const text = (content: Content) => (content[0] as TextContent).text;
	const images = (content: Content) => content.filter((part): part is ImageContent => part.type === "image");

	beforeEach(() => {
		cwd = mkdtempSync(join(tmpdir(), "ultron-view-image-"));
		current = model(["text", "image"]);
		tool = createUltronRlmTool(
			cwd,
			async (type) => {
				throw new Error(`unexpected host request ${type}`);
			},
			undefined,
			{ resolveModel: async () => current },
		);
		writeFileSync(join(cwd, "dot.png"), png(4, 3));
	});

	afterEach(async () => {
		await tool.close();
		rmSync(cwd, { recursive: true, force: true });
	});

	test("a path attaches an image block after the text, and printed output carries no base64", async () => {
		const content = await run("print(await view_image('dot.png'))");
		expect(content).toHaveLength(2);
		expect(text(content)).toMatch(/^image 1: 4x3 PNG, \d+ B$/);
		const [image] = images(content);
		expect(image).toMatchObject({ type: "image", mimeType: "image/png" });
		expect(getImageDimensions(image!.data, image!.mimeType)).toEqual({ widthPx: 4, heightPx: 3 });
		expect(text(content)).not.toContain(image!.data.slice(0, 16));
		// A cell without view_image keeps the plain text result.
		expect(await run("1 + 1")).toEqual([{ type: "text", text: "2" }]);
	});

	test("raw bytes attach too, and the type comes from magic bytes, not the extension", async () => {
		writeFileSync(join(cwd, "photo.jpg"), png(5, 5));
		writeFileSync(join(cwd, "fake.png"), "not an image at all");
		const content = await run(
			[
				"print(await view_image(open('dot.png', 'rb').read()))",
				"from pathlib import Path",
				"print(await view_image(Path('photo.jpg')))",
				"try:\n    await view_image('fake.png')\nexcept ValueError as error:\n    print('refused:', error)",
				"try:\n    await view_image(b'GIF8 but not really')\nexcept ValueError as error:\n    print('refused:', error)",
				"try:\n    await view_image(42)\nexcept TypeError as error:\n    print('refused:', error)",
			].join("\n"),
		);
		expect(text(content)).toContain("image 1: 4x3 PNG");
		expect(text(content)).toContain("image 2: 5x5 PNG");
		expect(text(content)).toContain("refused: fake.png is not a png, jpeg, gif or webp image");
		expect(text(content)).toContain("refused: view_image bytes are not a png, jpeg, gif or webp image");
		expect(text(content)).toContain("refused: view_image takes a path, image bytes");
		expect(images(content).map((image) => image.mimeType)).toEqual(["image/png", "image/png"]);
		// The temporary file written for bytes is gone.
		expect(
			readdirSync(tmpdir()).filter((name) => name.startsWith("ultron-view-image-") && name.includes(".")),
		).toEqual([]);
	}, 30_000);

	test("an object with savefig (a matplotlib figure) is rendered to PNG", async () => {
		const content = await run(
			[
				"class Figure:",
				"    def savefig(self, buffer, format=None, bbox_inches=None):",
				"        assert format == 'png'",
				"        buffer.write(open('dot.png', 'rb').read())",
				"await view_image(Figure())",
			].join("\n"),
		);
		expect(text(content)).toContain("image 1: 4x3 PNG");
		expect(images(content)).toHaveLength(1);
	});

	test.skipIf(!pythonHas("matplotlib"))(
		"a real matplotlib figure is attached as a PNG",
		async () => {
			const content = await run(
				[
					"import matplotlib",
					"matplotlib.use('Agg')",
					"import matplotlib.pyplot as plt",
					"fig, ax = plt.subplots(figsize=(2, 2))",
					"ax.plot([0, 1], [0, 1])",
					"await view_image(fig)",
				].join("\n"),
			);
			expect(text(content)).toMatch(/image 1: \d+x\d+ PNG/);
			expect(images(content)[0]?.mimeType).toBe("image/png");
		},
		60_000,
	);

	test.skipIf(!pythonHas("PIL"))(
		"a PIL image is attached as a PNG",
		async () => {
			const content = await run(
				"from PIL import Image\nawait view_image(Image.new('RGB', (30, 20), (0, 128, 255)))",
			);
			expect(text(content)).toContain("image 1: 30x20 PNG");
			expect(images(content)).toHaveLength(1);
		},
		30_000,
	);

	test("a cell attaches at most 8 images; the ninth raises and the eight are kept", async () => {
		const content = await run(
			[
				"for i in range(9):",
				"    try:",
				"        await view_image('dot.png')",
				"    except RuntimeError as error:",
				"        print(i, error)",
			].join("\n"),
		);
		expect(images(content)).toHaveLength(8);
		expect(text(content)).toContain("8 view_image: at most 8 images per cell");
		// The cap is per cell.
		expect(images(await run("await view_image('dot.png')"))).toHaveLength(1);
	}, 30_000);

	test("an oversized image is downscaled by Pi's image pipeline, and detail='low' makes a small preview", async () => {
		writeFileSync(join(cwd, "big.png"), png(3000, 2400, [10, 200, 10]));
		const content = await run("print(await view_image('big.png'))\nprint(await view_image('big.png', detail='low'))");
		expect(text(content)).toContain("(downscaled from 3000x2400)");
		const [large, small] = images(content).map((image) => getImageDimensions(image.data, image.mimeType));
		expect(Math.max(large!.widthPx, large!.heightPx)).toBeLessThanOrEqual(2000);
		expect(Math.max(small!.widthPx, small!.heightPx)).toBeLessThanOrEqual(512);
	}, 60_000);

	test("a model without image input gets a plain note instead of a silent drop", async () => {
		current = model(["text"]);
		const content = await run("print(await view_image('dot.png'))");
		expect(text(content)).toContain("image 1: 4x3 PNG");
		expect(text(content)).toContain("the current model (test/text-only) does not accept image input");
		// The block is still kept (as Pi's read tool keeps it) so the transcript shows it and a vision model can use it.
		expect(images(content)).toHaveLength(1);
	});

	test("read on an image file returns a note pointing at view_image", async () => {
		const content = await run("await read('dot.png')");
		expect(text(content)).toContain("dot.png is a PNG image");
		expect(text(content)).toContain("Use `await view_image('dot.png')`");
		expect(images(content)).toHaveLength(0);
	});

	test("the runtime guide lists view_image", () => {
		expect(rlmRuntimePrompt(["rlm"])).toContain("await view_image(path)");
	});
});

describe("view_image through the CLI", () => {
	const cliPath = resolve(__dirname, "../src/cli.ts");
	const sourceResolverPath = resolve(__dirname, "../src/experimental/source-resolver.ts");
	let root: string;
	let agentDir: string;
	let projectDir: string;
	let sessionDir: string;
	let provider: ScriptedProvider;
	const clients: RpcClient[] = [];

	function script(request: ScriptedRequest) {
		if (request.firstUser.includes("look at chart.png")) {
			// The tool's image follows its tool message as a user message, so the tool message is not the last one.
			const tool = [...request.body.messages].reverse().find((message) => message.role === "tool");
			return tool === undefined
				? { tool: "rlm", args: { code: "print(await view_image('chart.png'))" } }
				: { text: `seen: ${typeof tool.content === "string" ? tool.content : JSON.stringify(tool.content)}` };
		}
		return { text: "ok" };
	}

	beforeEach(async () => {
		root = mkdtempSync(join(tmpdir(), "ultron-view-image-cli-"));
		agentDir = join(root, "agent");
		projectDir = join(root, "project");
		sessionDir = join(root, "sessions");
		mkdirSync(agentDir, { recursive: true });
		mkdirSync(projectDir, { recursive: true });
		writeFileSync(join(projectDir, "chart.png"), png(6, 4));
		provider = new ScriptedProvider(script);
		await provider.start();
		writeFileSync(join(agentDir, "models.json"), scriptedModelsJson(provider.baseUrl, { input: ["text", "image"] }));
	});

	afterEach(async () => {
		for (const client of clients.splice(0)) await client.stop().catch(() => {});
		await provider.stop();
		rmSync(root, { recursive: true, force: true });
	});

	function startClient(args: string[]): RpcClient {
		const client = new RpcClient({
			cliPath,
			cwd: projectDir,
			provider: "scripted",
			model: "scripted",
			args,
			env: {
				NODE_OPTIONS: `--import ${sourceResolverPath}`,
				ULTRON_CODING_AGENT_DIR: agentDir,
				ULTRON_HINDSIGHT_URL: "off",
				ULTRON_SERVER_DIR: tempServerDir("u-img-"),
				PI_OFFLINE: "1",
			},
		});
		clients.push(client);
		return client;
	}

	test("the next provider request carries the image, and RPC events, get_messages and a restore keep it", async () => {
		const expected = png(6, 4).toString("base64");
		const client = startClient(["--session-dir", sessionDir]);
		await client.start();
		const events = await client.promptAndWait("look at chart.png", undefined, 120_000);
		expect(await client.getLastAssistantText()).toBe(`seen: image 1: 6x4 PNG, ${png(6, 4).length} B`);

		// The request after the cell: the tool result text, then the image as an image part (OpenAI chat
		// completions carries tool images in a user message right after the tool message).
		const followUp = provider.requests.at(-1)!;
		const messages = followUp.body.messages;
		const toolIndex = messages.findIndex((message) => message.role === "tool");
		expect(toolIndex).toBeGreaterThan(0);
		expect(followUp.raw).toContain(`data:image/png;base64,${expected}`);
		const imageMessage = messages
			.slice(toolIndex + 1)
			.find((message) => JSON.stringify(message.content).includes("image_url"));
		expect(imageMessage?.role).toBe("user");
		// The image is not in any text: not in the tool message.
		expect(JSON.stringify(messages[toolIndex]!.content)).not.toContain(expected);

		// RPC tool events and get_messages carry Pi's shape: [text, {type: "image", data, mimeType}].
		const end = events.find((event) => event.type === "tool_execution_end") as
			| { result: { content: Content } }
			| undefined;
		expect(end?.result.content.map((part) => part.type)).toEqual(["text", "image"]);
		expect(end?.result.content[1]).toEqual({ type: "image", data: expected, mimeType: "image/png" });
		const toolResult = (await client.getMessages()).find((message) => message.role === "toolResult");
		expect(toolResult && "content" in toolResult ? toolResult.content : undefined).toEqual([
			{ type: "text", text: `image 1: 6x4 PNG, ${png(6, 4).length} B` },
			{ type: "image", data: expected, mimeType: "image/png" },
		]);
		await client.stop();

		// Saved and restored with the session.
		const resumed = startClient(["--session-dir", sessionDir, "--continue"]);
		await resumed.start();
		const restored = (await resumed.getMessages()).find((message) => message.role === "toolResult");
		expect(restored && "content" in restored ? restored.content : undefined).toContainEqual({
			type: "image",
			data: expected,
			mimeType: "image/png",
		});
		expect(readFileSync(join(projectDir, "chart.png")).toString("base64")).toBe(expected);
	}, 240_000);
});
