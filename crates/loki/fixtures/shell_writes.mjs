// Records loki.py `shell-writes` (the before-shell check Ultron runs on commands that may write files) over a corpus
// of commands in a temp Git repository, for crates/loki/tests/shell_writes.rs.
// Run: node crates/loki/fixtures/shell_writes.mjs  (writes shell-writes-corpus.json next to this file)
// Python: $ULTRON_PYTHON, else /usr/bin/python3. Deterministic: run twice and compare sha256.
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const engine = join(here, "../../../packages/coding-agent/src/ultron/loki-engine/loki.py");
const python = process.env.ULTRON_PYTHON || "/usr/bin/python3";

/** Every case runs in a fresh repository holding these files (plus the case's own). */
const base = {
	".gitignore": "build/\n*.tmp\n",
	"src/main.c": "int main(void) { return 0; }\n",
	"a.txt": "a\n",
	"docs/.keep": "",
};
const cfg = (value) => ({ ".loki/loki.json": JSON.stringify(value) });

const commands = [
	// No write at all.
	"ls -la",
	"",
	"   ",
	"cat a.txt | grep a",
	"echo hi 2>&1",
	"echo hi >/dev/null",
	"echo hi > /dev/null 2>&1",
	"echo hi >&2",
	"echo hi > /dev/stderr",
	"echo hi > /dev/fd/3",
	"echo hi > /proc/self/fd/1",
	"git status && npm test",
	// Redirects into source and configuration.
	"echo hi > out.py",
	"echo hi >> src/main.c",
	"echo hi >| src/x.h",
	"make &> build.cfg",
	"make &>> build.json",
	"echo a;>x.c",
	"echo a 2> err.ts",
	"echo a 2>> err.rs",
	"cat <<EOF > script.sh\necho $HOME\nEOF",
	"cat <<'EOF' > gen.py\nprint('x') > y.c\nEOF\necho done",
	"cat <<-\"END\" >notes.md\n\tbody > z.c\n\tEND",
	"cat << EOF\nbody > z.c\nEOF",
	"(echo a) > sub.ts",
	"{ echo a; } > brace.ts",
	"echo 'a > b.c'",
	"echo \"x\" > \"quoted name.yml\"",
	"echo x > 'single.toml'",
	"echo x > esc\\ aped.ini",
	"echo x > \"dq\\\"q.c\"",
	"printf '%s' x>tight.go",
	"echo x >> ./src/../src/up.c",
	// Data and notes are fine; hidden directories are not.
	"echo hi > notes.md",
	"echo hi > NOTES.TXT",
	"echo hi > run.log",
	"git diff > change.patch",
	"echo x > report.csv",
	"echo x > .notes.md",
	"echo x > .github/notes.md",
	"echo x > docs/guide.rst",
	"echo x > a.",
	"echo x > noext",
	// Ignored output, outside the project, unknown targets.
	"make > build/out.js",
	"echo x > cache.tmp",
	"echo x > ../outside.c",
	"echo x > /tmp/elsewhere.c",
	"echo x > $OUT",
	"echo x > `pwd`/a.c",
	"echo x > *.c",
	"echo x > a?.c",
	"echo x > ~/home.c",
	// Protected files.
	"echo '{}' > .loki/loki.json",
	"cp a.txt .loki/x",
	"echo x > .ruff.toml",
	"echo x > .github/workflows/loki.yml",
	"echo x > src",
	// tee, sed -i, perl -i, cp/mv/install, dd.
	"cat a.txt | tee copy.cfg",
	"cat a.txt | tee -a log.txt other.c",
	"sed -i s/a/b/ src/main.c",
	"sed -i.bak 's/a/b/' x.txt y.c",
	"sed --in-place -e s/a/b/ conf.yaml",
	"sed -e s/a/b/ src/main.c",
	"sed -n -i -f script.sed target.c",
	"perl -pi -e 's/a/b/' lib.pm",
	"perl -i.bak -pe s/a/b/ notes.txt",
	"perl -e 'print 1' foo.c",
	"perl -pe s/a/b/ src/main.c",
	"cp a.txt b.c",
	"cp a.txt docs",
	"cp a.txt docs/",
	"cp a.txt b.txt dest",
	"mv -f a.txt moved.json",
	"install -m 644 a.txt bin.sh",
	"cp -t docs a.txt b.c",
	"cp -t docs",
	"cp a.txt",
	"/bin/cp a.txt viapath.c",
	"./tee x.c",
	"dd if=/dev/zero of=disk.img bs=1 count=1",
	"dd of=README.md",
	// Prefixes and assignments.
	"sudo tee /etc/hosts",
	"FOO=1 BAR=2 tee env.c",
	"env -i tee envi.c",
	"nohup nice cp a.txt n.c",
	"time mv a.txt t.c",
	"echo ok && cp a.txt and.c || echo no",
	"echo ok; sed -i s/x/y/ semi.c | cat",
	// Parse failures.
	"echo 'unterminated > a.c",
	"echo x > a.c \\",
	// Targets follow a literal cd/pushd; a ( ) subshell restores the directory; popd, bare cd and cd - are unknown.
	"cd /tmp && echo x > Cargo.toml",
	"(cd /tmp; echo x > a.rs)",
	"(cd /tmp && echo 1 > a.rs); echo 2 > b.c",
	"cd $WORK && echo x > a.c",
	"cd; echo x > a.c",
	"cd - && echo x > a.c",
	"popd && echo x > a.c",
	"cd src && echo x > x.c",
	"pushd src && cp ../a.txt b.c",
	"cd -P -- src; tee t.c",
	"echo x > a.c; cd /tmp",
];

const cases = [
	...commands.map((command, i) => ({ name: `command-${i}`, command })),
	{ name: "cwd-subdir", command: "echo x > here.c", cwd: "src" },
	{ name: "cwd-subdir-parent", command: "echo x > ../top.c", cwd: "src" },
	{ name: "cwd-outside-root", command: "echo x > out.c", cwd: ".." },
	{ name: "protected-extra", files: cfg({ protected_extra: ["secrets/**"] }), command: "echo x > secrets/a.txt" },
	{ name: "invalid-config-no-write", files: cfg({ zeta: 1 }), command: "ls" },
	{ name: "invalid-config-write", files: cfg({ zeta: 1 }), command: "echo x > a.c" },
	{ name: "not-a-string", payload: { command: 1 } },
	{ name: "not-an-object", payload: [] },
	{ name: "no-git", git: false, command: "make > build/out.js" },
];

function run(testCase) {
	const tmp = mkdtempSync(join(tmpdir(), "loki-shell-fixture-"));
	const root = join(tmp, "repo");
	try {
		for (const [path, content] of Object.entries({ ...base, ...(testCase.files ?? {}) })) {
			mkdirSync(dirname(join(root, path)), { recursive: true });
			writeFileSync(join(root, path), content);
		}
		if (testCase.git !== false) spawnSync("git", ["init", "-q"], { cwd: root });
		const payload =
			testCase.payload ?? { command: testCase.command, ...(testCase.cwd ? { cwd: join(root, testCase.cwd) } : {}) };
		const env = { PATH: process.env.PATH, HOME: tmp, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1" };
		const result = spawnSync(python, [engine, "--root", root, "shell-writes", "--harness", "ultron"], {
			cwd: root,
			env,
			input: JSON.stringify(payload),
			encoding: "utf8",
		});
		const scrub = (text) => text.split(root).join("<ROOT>");
		return { status: result.status, stderr: scrub(result.stderr) };
	} finally {
		rmSync(tmp, { recursive: true, force: true });
	}
}

const recorded = cases.map((testCase) => ({ ...testCase, expected: run(testCase) }));
writeFileSync(
	join(here, "shell-writes-corpus.json"),
	`${JSON.stringify({ engine: "packages/coding-agent/src/ultron/loki-engine/loki.py", base, cases: recorded }, null, "\t")}\n`,
);
console.log(`recorded ${recorded.length} cases`);
