/**
 * A long session must not die from output that accumulates across cells. The kernel bounds protocol output
 * per frame (1 MiB) and per cell, never over the kernel's lifetime: 200 cells of 64 KB and hundreds of
 * bridge requests (far over 4 MiB in total) keep one kernel generation and its Python state, and a single
 * oversized output is cut in the cell instead of killing the kernel. Raw writes to fd 1 are not protocol at all
 * (frames travel on fd 3), so they are bounded per cell like any other output.
 */
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, test } from "vitest";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";

const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));
const KiB = 1024;
const MiB = 1024 * KiB;

describe("RLM kernel long sessions", () => {
	test("200 cells of 64 KB output keep the same kernel generation and its variables", async () => {
		// A 128 KiB capture budget lets each cell's 64 KB of stdout cross the protocol whole.
		const kernel = new RlmKernel(
			{ cwd: process.cwd(), runtimePath, env: { ULTRON_RLM_OUTPUT_BYTES: "131072" } },
			() => null,
		);
		try {
			const pid = (await kernel.execute("import os\nkept = {'answer': 42}\nos.getpid()")).result;
			let shipped = 0;
			for (let cell = 0; cell < 200; cell++) {
				const result = await kernel.execute(`print('${cell % 10}' * ${64 * KiB}, end='')\nkept['answer']`);
				expect(result.status).toBe("ok");
				expect(result.stdout).toHaveLength(64 * KiB);
				expect(result.result).toBe("42");
				shipped += result.stdout.length;
			}
			expect(shipped).toBeGreaterThan(12 * MiB);
			expect(await kernel.execute("os.getpid()")).toMatchObject({ status: "ok", result: pid });
			expect(await kernel.execute("kept")).toMatchObject({ result: "{'answer': 42}" });
		} finally {
			await kernel.shutdown();
		}
	}, 120_000);

	test("bridge requests totalling far over 4 MiB keep the same kernel generation", async () => {
		let requests = 0;
		const kernel = new RlmKernel({ cwd: process.cwd(), runtimePath }, async (_type, payload) => {
			requests++;
			// Echo a 64 KB reply, so bytes flow both ways.
			return { size: String(payload.text ?? "").length, echo: "r".repeat(64 * KiB) };
		});
		try {
			const pid = (await kernel.execute("import os\nstate_value = 'survives'\nos.getpid()")).result;
			// 4 cells x 64 requests x 64 KB payload = 16 MiB of host_request frames from the kernel.
			for (let cell = 0; cell < 4; cell++) {
				const result = await kernel.execute(
					[
						"sizes = []",
						"for _ in range(64):",
						`    reply = await rlm.host_request('test.echo', {'text': 'q' * ${64 * KiB}})`,
						"    sizes.append((reply['size'], len(reply['echo'])))",
						"set(sizes), len(sizes)",
					].join("\n"),
				);
				expect(result).toMatchObject({ status: "ok", result: `({(${64 * KiB}, ${64 * KiB})}, 64)` });
			}
			expect(requests).toBe(256);
			expect(await kernel.execute("os.getpid()")).toMatchObject({ status: "ok", result: pid });
			expect(await kernel.execute("state_value")).toMatchObject({ result: "'survives'" });
		} finally {
			await kernel.shutdown();
		}
	}, 120_000);

	test("a single output over the 1 MiB frame is cut in the cell, not by killing the kernel", async () => {
		// A capture budget over the frame (the host clamps ULTRON_RLM_OUTPUT_BYTES, an override does not).
		const kernel = new RlmKernel(
			{ cwd: process.cwd(), runtimePath, env: { ULTRON_RLM_OUTPUT_BYTES: String(4 * MiB) } },
			() => null,
		);
		try {
			const pid = (await kernel.execute("import os\nbefore = 'kept'\nos.getpid()")).result;
			const result = await kernel.execute(
				"import sys\nprint('h' + 'x' * (3 * 1024 * 1024) + 't')\nprint('\\u00e9' * (1024 * 1024), file=sys.stderr)\n7",
			);
			expect(result.status).toBe("ok");
			expect(result.result).toBe("7");
			expect(Buffer.byteLength(result.stdout)).toBeLessThan(MiB);
			expect(result.stdout.startsWith("hxxx")).toBe(true);
			expect(result.stdout.trimEnd().endsWith("xxxt")).toBe(true);
			expect(result.stdout).toMatch(/characters cut to fit the 1 MiB protocol frame/);
			// Escaped non-ASCII text grows up to six times in JSON; it is cut to fit as well.
			expect(result.stderr).toMatch(/characters cut to fit the 1 MiB protocol frame/);
			expect(await kernel.execute("os.getpid(), before")).toMatchObject({
				status: "ok",
				result: `(${pid}, 'kept')`,
			});
		} finally {
			await kernel.shutdown();
		}
	}, 60_000);

	test.skipIf(process.platform === "win32")(
		"raw fd 1 output across many cells (far over 4 MiB in total) keeps the same kernel generation",
		async () => {
			const kernel = new RlmKernel({ cwd: process.cwd(), runtimePath }, () => null);
			try {
				const pid = (await kernel.execute("import os\nkept = 'raw'\nos.getpid()")).result;
				for (let cell = 0; cell < 40; cell++) {
					const result = await kernel.execute(`os.write(1, b'${cell % 10}' * ${512 * KiB})\nkept`);
					expect(result).toMatchObject({ status: "ok", result: "'raw'" });
					expect(result.stdout).toBe(String(cell % 10).repeat(512 * KiB));
				}
				expect(await kernel.execute("os.getpid()")).toMatchObject({ status: "ok", result: pid });
			} finally {
				await kernel.shutdown();
			}
		},
		120_000,
	);

	test("the host bounds a cell's accumulated output without failing the kernel", async () => {
		const directory = await mkdtemp(join(tmpdir(), "ultron-kernel-long-"));
		try {
			// A runtime that answers each execute with twelve 900 KB stdout frames (about 10.5 MiB in one cell).
			const flooding = join(directory, "flooding.py");
			await writeFile(
				flooding,
				[
					"import json, os, sys",
					// The private-fd protocol (fd 3 out, fd 4 in), or stdout/stdin where the host uses stdio.
					"private = '--protocol-fds' in sys.argv",
					"out = os.fdopen(3, 'w') if private else sys.stdout",
					"def emit(**frame):",
					"    out.write(json.dumps(frame) + '\\n'); out.flush()",
					"emit(event='ready')",
					"for line in (os.fdopen(4) if private else sys.stdin):",
					"    request = json.loads(line)",
					"    if request.get('request') == 'execute':",
					"        for _ in range(12): emit(event='stdout', id=request['id'], text='s' * (900 * 1024))",
					"        emit(event='result', id=request['id'], result=str(len(request['code'])))",
					"    emit(event='done', id=request['id'], status='ok')",
				].join("\n"),
			);
			const kernel = new RlmKernel({ cwd: directory, runtimePath: flooding }, () => null);
			try {
				for (let cell = 0; cell < 3; cell++) {
					const result = await kernel.execute("1234");
					expect(result.status).toBe("ok");
					expect(result.result).toBe("4");
					expect(result.stdout.length).toBeLessThanOrEqual(4 * MiB + 256);
					expect(result.stdout).toMatch(/over the RLM kernel's 4 MiB per-cell output limit/);
				}
				expect(kernel.isRunning).toBe(true);
			} finally {
				await kernel.shutdown();
			}
		} finally {
			await rm(directory, { recursive: true, force: true });
		}
	}, 60_000);
});
