import { fileURLToPath } from "node:url";
import { describe, expect, test } from "vitest";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";

const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));

/** A kernel whose `bash` host request records the command it was given and echoes it back. */
function recordingKernel() {
	const commands: string[] = [];
	const kernel = new RlmKernel({ cwd: process.cwd(), runtimePath }, async (type, payload) => {
		if (type === "bash") {
			const command = String((payload as { command: string }).command);
			commands.push(command);
			return { output: `ran ${command.length}`, exit_code: 0 };
		}
		if (type === "shell.run") return { id: "job-1", status: "running", running: true };
		throw new Error(`Unknown request ${type}`);
	});
	return { kernel, commands };
}

// Each wasted cell here was a whole extra model turn in the hard eval set.
describe("RLM cell repairs", () => {
	test("a literal bash command reaches the shell as written, backslashes included", async () => {
		const { kernel, commands } = recordingKernel();
		try {
			const heredoc = "await bash('''python - <<'PY'\nprint('a\\nb', '\\t')\nPY''')";
			expect(await kernel.execute(heredoc)).toMatchObject({ status: "ok" });
			expect(commands.at(-1)).toBe("python - <<'PY'\nprint('a\\nb', '\\t')\nPY");

			expect(await kernel.execute("await bash(command='''grep -P '\\d+' f''')")).toMatchObject({ status: "ok" });
			expect(commands.at(-1)).toBe("grep -P '\\d+' f");

			// Already raw, already escaped, f-strings and concatenations keep Python's reading.
			await kernel.execute("await bash(r'''x\\ny''')");
			expect(commands.at(-1)).toBe("x\\ny");
			await kernel.execute("await bash('''sed 's/\\\\./x/' f''')");
			expect(commands.at(-1)).toBe("sed 's/\\./x/' f");
			await kernel.execute("n = 1\nawait bash(f'''echo {n}\\n''')");
			expect(commands.at(-1)).toBe("echo 1\n");
			await kernel.execute("await bash('''a\\n''' '''b''')");
			expect(commands.at(-1)).toBe("a\nb");
			await kernel.execute("cmd = 'echo \\\\n'\nawait bash(cmd)");
			expect(commands.at(-1)).toBe("echo \\n");
		} finally {
			await kernel.shutdown();
		}
	});

	test("brackets left open at the very end of a cell are closed, with a note", async () => {
		const { kernel, commands } = recordingKernel();
		try {
			const result = await kernel.execute(
				"print(await bash('''echo hi\ncat answer.json'''))\nprint((await bash('''ls''')",
			);
			expect(result).toMatchObject({ status: "ok" });
			expect(result.stdout).toContain("[note: added the missing '))' at the end of the cell]");
			expect(result.stdout).toContain("ran 2");
			expect(commands).toEqual(["echo hi\ncat answer.json", "ls"]);
			expect(await kernel.execute("x = [1, (2,\n")).toMatchObject({ status: "ok" });
			expect(await kernel.execute("x")).toMatchObject({ result: "[1, (2,)]" });
			// Any other syntax error is still reported as written.
			expect(await kernel.execute("print(1))")).toMatchObject({ status: "error", error: { ename: "SyntaxError" } });
			expect(await kernel.execute("def f(:\n  pass")).toMatchObject({
				status: "error",
				error: { ename: "SyntaxError" },
			});
		} finally {
			await kernel.shutdown();
		}
	});

	test("common result spellings and stdlib names work without an extra cell", async () => {
		const { kernel } = recordingKernel();
		try {
			expect(await kernel.execute("(await bash('''echo hi''')).text")).toMatchObject({ result: "'ran 7'" });
			expect(await kernel.execute("job = await bash('''sleep 60''', yield_after=0)\njob.job is job")).toMatchObject({
				result: "True",
			});
			expect(
				await kernel.execute(
					"SpawnHandle(rlm_child_id='child-1', name='n', session_dir='', model='', timeout_ms=1, parent_branch_anchor='').id",
				),
			).toMatchObject({ result: "'child-1'" });
			expect(
				await kernel.execute("(re.sub('a', 'b', 'aa'), json.dumps([1]), Path('x').name, os.sep)"),
			).toMatchObject({
				result: "('bb', '[1]', 'x', '/')",
			});
		} finally {
			await kernel.shutdown();
		}
	});
});
