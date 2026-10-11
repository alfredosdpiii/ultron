// Generates crates/inventory/inventory.tsv: the behavior surfaces of every packages/* package and of the
// pinned upstream Pi packages (durable, chord, ai), one row per surface with the source line that defines it.
// Usage: node crates/inventory/generate.mjs [<pi-reference-checkout>]   (run from the repository root)
// Deterministic: sorted output, no timestamps; paths are repo-relative, upstream paths are prefixed "upstream:".
import { execFileSync } from "node:child_process";
import { existsSync, readdirSync, readFileSync, realpathSync, statSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repo = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const reference = resolve(process.argv[2] ?? join(repo, "../ultron-rust-reference"));
const ts = createRequire(join(repo, "package.json"))("typescript");
const python = process.env.ULTRON_PYTHON ?? "/usr/bin/python3";

const PACKAGES = [
	...["agent", "ai", "chord", "client", "coding-agent", "durable", "evals", "mcp", "protocol", "server"].map((p) => [
		p,
		join(repo, "packages", p),
	]),
	["session-backends-sqlite-node", join(repo, "packages/session-backends/sqlite-node")],
	["telemetry", join(repo, "packages/telemetry")],
	["tui", join(repo, "packages/tui")],
	...["durable", "chord", "ai"].map((p) => [`upstream-${p}`, join(reference, "packages", p)]),
];

const rows = [];
const added = new Set();
const lineOf = (node) => node.getSourceFile().getLineAndCharacterOfPosition(node.getStart()).line + 1;
const location = (file, line) => {
	const real = realpathSync(file);
	const ref = realpathSync(reference);
	return real.startsWith(`${ref}/`) ? `upstream:${relative(ref, real)}:${line}` : `${relative(repo, real)}:${line}`;
};
// Records a surface once per (package, kind, name); the first occurrence in scan order wins.
function add(pkg, kind, name, file, line) {
	const key = `${pkg}\t${kind}\t${name}`;
	if (added.has(key)) return;
	added.add(key);
	if (/[\t\n]/.test(name)) throw new Error(`bad name ${JSON.stringify(name)}`);
	const token = name.split(/[:./ ]/).pop();
	const text = readFileSync(file, "utf8").split("\n")[line - 1] ?? "";
	// The row's last name token must appear on its source line (or the line re-exports a whole module);
	// the Rust test re-checks this.
	if (!text.includes(token) && !/^export \* from /.test(text)) throw new Error(`${pkg} ${kind} ${name}: "${token}" not on ${file}:${line}`);
	rows.push({ pkg, kind, name, at: location(file, line) });
}

const walk = (dir) =>
	readdirSync(dir, { withFileTypes: true })
		.flatMap((e) => (e.isDirectory() ? (e.name === "node_modules" ? [] : walk(join(dir, e.name))) : [join(dir, e.name)]))
		.sort();
const sourceFiles = (root, ext = /\.tsx?$/) =>
	existsSync(join(root, "src"))
		? walk(join(root, "src")).filter((f) => ext.test(f) && !/\.d\.ts$|\.test\.tsx?$/.test(f))
		: [];
const parse = (file) => ts.createSourceFile(file, readFileSync(file, "utf8"), ts.ScriptTarget.Latest, true);
function visit(node, fn) {
	fn(node);
	ts.forEachChild(node, (child) => visit(child, fn));
}
const literal = (node) => (node && (ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) ? node.text : undefined);

// ─── Public exports (package.json "exports", following re-exports through the checker) ─────────
function entryFiles(root) {
	const pkg = JSON.parse(readFileSync(join(root, "package.json"), "utf8"));
	const exportsField = pkg.exports ?? (pkg.main ? { ".": pkg.main } : {});
	const toSource = (target) => {
		const spec = typeof target === "string" ? target : (target.source ?? target.types ?? target.import);
		if (!spec || !/\.(ts|js)$/.test(spec)) return undefined;
		const src = spec.replace(/^\.?\/?dist\//, "src/").replace(/(\.d)?\.(ts|js)$/, ".ts");
		if (src.includes("*")) return join(root, src);
		for (const candidate of [src, src.replace("src/bundle/", "src/")]) {
			if (existsSync(join(root, candidate))) return join(root, candidate);
		}
		throw new Error(`no source for ${root} ${spec}`);
	};
	const entries = [];
	for (const [subpath, target] of Object.entries(exportsField)) {
		if (subpath === "./package.json") continue;
		const file = toSource(target);
		if (!file) continue;
		const label = subpath === "." ? "" : subpath.replace(/^\.\//, "");
		if (label.includes("*")) {
			const [dir, after] = file.split("*");
			for (const f of readdirSync(dir).sort()) {
				const full = join(dir, f);
				if (full.endsWith(after) && !f.endsWith(".d.ts") && !f.endsWith(".test.ts") && statSync(full).isFile()) {
					entries.push([label.replace("*", f.slice(0, -after.length)), full]);
				}
			}
		} else entries.push([label, file]);
	}
	return entries;
}

function exportsOf(pkg, root) {
	const entries = entryFiles(root);
	if (entries.length === 0) return;
	const program = ts.createProgram(
		entries.map(([, f]) => f),
		{
			target: ts.ScriptTarget.ESNext,
			module: ts.ModuleKind.NodeNext,
			moduleResolution: ts.ModuleResolutionKind.NodeNext,
			allowImportingTsExtensions: true,
			resolveJsonModule: true,
			noEmit: true,
			skipLibCheck: true,
			types: [],
		},
	);
	const checker = program.getTypeChecker();
	const inside = (node) => realpathSync(node.getSourceFile().fileName).startsWith(`${realpathSync(root)}/`);
	// `export * from "<other package>"` statements in this package, for symbols they pass through.
	const stars = program
		.getSourceFiles()
		.filter(inside)
		.flatMap((sf) => sf.statements.filter((st) => ts.isExportDeclaration(st) && !st.exportClause && st.moduleSpecifier));
	const starFor = (target) =>
		stars.find((st) => {
			const mod = checker.getSymbolAtLocation(st.moduleSpecifier);
			return mod && checker.getExportsOfModule(mod).some((e) => e === target || (e.flags & ts.SymbolFlags.Alias && checker.getAliasedSymbol(e) === target));
		});
	for (const [label, file] of entries) {
		const moduleSymbol = checker.getSymbolAtLocation(program.getSourceFile(file));
		if (!moduleSymbol) continue;
		for (const symbol of checker.getExportsOfModule(moduleSymbol)) {
			const name = symbol.escapedName.toString();
			const target = symbol.flags & ts.SymbolFlags.Alias ? checker.getAliasedSymbol(symbol) : symbol;
			const decl = target.declarations?.[0];
			const named = decl?.name && decl.name.getText?.() === name;
			const inPackage = decl && inside(decl);
			// Renamed re-exports and re-exports of another package point at this package's export statement.
			const at = decl && (named || name === "default") && inPackage ? decl : (symbol.declarations.find(inside) ?? starFor(target));
			if (!at || !inside(at)) throw new Error(`${pkg} ${name}: no declaration inside the package`);
			const value = (target.flags & ts.SymbolFlags.Value) !== 0;
			add(pkg, value ? "export" : "type", label ? `${label}:${name}` : name, at.getSourceFile().fileName, lineOf(at));
		}
	}
}

// ─── Coding-agent CLI: commands, subcommands and flags ──────────────────────────────────────────
const CLI_FLAG_FILES = {
	"src/cli/args.ts": "",
	"src/package-manager-cli.ts": "package",
	"src/ultron/usage-cli.ts": "usage",
	"src/ultron/migration-cli.ts": "migrate",
	"src/cli/auth-command.ts": "auth",
	"src/ultron/autoreview/cli.ts": "autoreview",
	"src/extensions/mcp/cli.ts": "mcp",
};
function cli(pkg, root) {
	const flagFiles = pkg === "coding-agent" ? CLI_FLAG_FILES : { "src/cli.ts": "" };
	for (const [rel, command] of Object.entries(flagFiles)) {
		const file = join(root, rel);
		if (!existsSync(file)) continue;
		visit(parse(file), (node) => {
			// `x === "--flag"` / `x === "-f"` comparisons, and `args.includes("--flag")`.
			const flag =
				ts.isBinaryExpression(node) && node.operatorToken.kind === ts.SyntaxKind.EqualsEqualsEqualsToken
					? literal(node.right)
					: ts.isCallExpression(node) && node.expression.name?.text === "includes"
						? literal(node.arguments[0])
						: undefined;
			if (flag && /^--?[a-z]/.test(flag)) add(pkg, "cli-flag", command ? `${command} ${flag}` : flag, file, lineOf(node));
		});
	}
	if (pkg !== "coding-agent") return;
	const lines = (rel) => readFileSync(join(root, rel), "utf8").split("\n");
	const grep = (rel, re, fn) =>
		lines(rel).forEach((text, i) => {
			for (const m of text.matchAll(re)) fn(m[1], join(root, rel), i + 1);
		});
	const command = (name, file, line) => add(pkg, "cli-command", name, file, line);
	grep("src/main.ts", /args\[0\] === "([a-z][\w-]*)"/g, command);
	for (const rel of ["src/cli/auth-command.ts", "src/ultron/usage-cli.ts", "src/ultron/migration-cli.ts"]) {
		grep(rel, /args\[0\] !== "([a-z][\w-]*)"/g, command);
	}
	grep("src/package-manager-cli.ts", /rawCommand === "([a-z][\w-]*)"/g, command);
	grep("src/cli/auth-command.ts", /args\[1\] === "([a-z][\w-]*)"/g, (n, f, l) => command(`auth ${n}`, f, l));
	grep("src/ultron/migration-cli.ts", /^\s*case "([a-z][\w-]*)":/g, (n, f, l) => command(`migrate ${n}`, f, l));
	grep("src/ultron/autoreview/cli.ts", /parsed\.command === "([a-z][\w-]*)"/g, (n, f, l) => command(`autoreview ${n}`, f, l));
	grep("src/extensions/mcp/cli.lazy.ts", /MCP_MANAGEMENT_COMMANDS[^=]*= \[(.*)\]/g, (list, f, l) => {
		for (const [, n] of list.matchAll(/"([a-z-]+)"/g)) command(`mcp ${n}`, f, l);
	});
}

// ─── Settings keys (nested interfaces of Settings in settings-manager.ts) ───────────────────────
function settings(pkg, root) {
	const file = join(root, "src/core/settings-manager.ts");
	if (!existsSync(file)) return;
	const program = ts.createProgram([file], { noResolve: true, noEmit: true, types: [] });
	const checker = program.getTypeChecker();
	const source = program.getSourceFile(file);
	const settingsDecl = source.statements.find((s) => ts.isInterfaceDeclaration(s) && s.name.text === "Settings");
	const walkType = (type, prefix) => {
		for (const prop of checker.getPropertiesOfType(type)) {
			const decl = prop.declarations?.[0];
			if (!decl || decl.getSourceFile() !== source) continue;
			const key = `${prefix}${prop.name}`;
			add(pkg, "setting", key, file, lineOf(decl));
			const propType = checker.getNonNullableType(checker.getTypeOfSymbolAtLocation(prop, decl));
			const nested = propType.getSymbol()?.declarations?.[0];
			if (nested && ts.isInterfaceDeclaration(nested) && nested.getSourceFile() === source) walkType(propType, `${key}.`);
		}
	};
	walkType(checker.getTypeAtLocation(settingsDecl), "");
}

// ─── Slash commands ──────────────────────────────────────────────────────────────────────────────
function slashCommands(pkg, root) {
	if (pkg !== "coding-agent") return;
	const slash = (name, file, line) => add(pkg, "slash-command", name, file, line);
	const builtins = join(root, "src/core/slash-commands.ts");
	visit(parse(builtins), (node) => {
		if (ts.isPropertyAssignment(node) && node.name.getText() === "name" && literal(node.initializer)) {
			slash(literal(node.initializer), builtins, lineOf(node));
		}
	});
	for (const file of sourceFiles(root)) {
		readFileSync(file, "utf8")
			.split("\n")
			.forEach((text, i) => {
				for (const m of text.matchAll(/registerCommand\(\s*"([a-z][\w-]*)"|text === "\/([a-z][\w-]*)"|startsWith\("\/([a-z][\w-]*):"\)/g)) {
					slash(m[1] ?? m[2] ?? m[3], file, i + 1);
				}
			});
	}
}

// ─── Keybinding ids ──────────────────────────────────────────────────────────────────────────────
function keybindings(pkg, root) {
	const file = join(root, pkg === "tui" ? "src/keybindings.ts" : "src/core/keybindings.ts");
	if (!["tui", "coding-agent"].includes(pkg)) return;
	visit(parse(file), (node) => {
		if (ts.isVariableDeclaration(node) && /^(TUI_)?KEYBINDINGS$/.test(node.name.getText())) {
			let init = node.initializer;
			while (init && !ts.isObjectLiteralExpression(init)) init = init.expression;
			for (const prop of init.properties) {
				if (ts.isPropertyAssignment(prop)) add(pkg, "keybinding", literal(prop.name) ?? prop.name.getText(), file, lineOf(prop));
			}
		}
	});
}

// ─── RPC and wire protocol message types ─────────────────────────────────────────────────────────
function rpc(pkg, root) {
	if (pkg === "coding-agent") {
		const file = join(root, "src/modes/rpc/rpc-types.ts");
		visit(parse(file), (node) => {
			if (!ts.isTypeAliasDeclaration(node) || !ts.isUnionTypeNode(node.type)) return;
			for (const member of node.type.types) {
				if (!ts.isTypeLiteralNode(member)) continue;
				const field = (n) => member.members.find((m) => m.name?.getText() === n);
				// The most specific literal discriminant: a response's command, a UI request's method, else type.
				const disc = ["command", "method", "type"]
					.map(field)
					.find((f) => f?.type && ts.isLiteralTypeNode(f.type) && literal(f.type.literal));
				if (disc) add(pkg, "rpc", `${node.name.text}:${literal(disc.type.literal)}`, file, lineOf(disc));
			}
		});
	}
	if (pkg === "protocol") {
		const file = join(root, "src/protocol.ts");
		visit(parse(file), (node) => {
			if (!ts.isVariableDeclaration(node) || !node.initializer) return;
			visit(node.initializer, (inner) => {
				if (ts.isPropertyAssignment(inner) && inner.name.getText() === "type" && ts.isCallExpression(inner.initializer)) {
					const value = literal(inner.initializer.arguments[0]);
					if (value) add(pkg, "protocol-message", `${node.name.getText()}:${value}`, file, lineOf(inner));
				}
			});
		});
	}
}

// ─── MCP JSON-RPC methods ────────────────────────────────────────────────────────────────────────
function mcp(pkg, root) {
	if (pkg !== "mcp") return;
	for (const file of sourceFiles(root)) {
		visit(parse(file), (node) => {
			const value = literal(node);
			const method = /^(initialize|ping|(notifications\/)?[a-z]+\/[a-zA-Z_/]+)$/.test(value ?? "");
			if (method && !/^(application|text|image|audio)\//.test(value)) add(pkg, "mcp-method", value, file, lineOf(node));
		});
	}
}

// ─── Extension API (ExtensionAPI members and its event subscriptions) ────────────────────────────
function extensionApi(pkg, root) {
	if (pkg !== "coding-agent") return;
	const file = join(root, "src/core/extensions/types.ts");
	visit(parse(file), (node) => {
		if (!ts.isInterfaceDeclaration(node) || node.name.text !== "ExtensionAPI") return;
		for (const member of node.members) {
			const name = member.name?.getText();
			if (!name) continue;
			const event = ts.isMethodSignature(member) && member.parameters[0]?.type;
			const lit = event && ts.isLiteralTypeNode(event) ? literal(event.literal) : undefined;
			add(pkg, "extension-api", lit ? `${name}:${lit}` : name, file, lineOf(lit ? event : member));
		}
	});
}

// ─── Provider and API registrations ──────────────────────────────────────────────────────────────
function providers(pkg, root) {
	if (!pkg.endsWith("ai")) return;
	const compat = join(root, "src/compat.ts");
	visit(parse(compat), (node) => {
		if (ts.isVariableDeclaration(node) && node.name.getText() === "BUILTIN_APIS") {
			for (const el of node.initializer.elements) add(pkg, "api", literal(el.elements[0]), compat, lineOf(el));
		}
	});
	for (const file of walk(join(root, "src/providers")).filter((f) => f.endsWith(".ts") && !f.endsWith(".models.ts"))) {
		visit(parse(file), (node) => {
			if (!ts.isCallExpression(node) || !/^create\w*Provider$/.test(node.expression.getText())) return;
			const arg = node.arguments[0];
			const id = arg && ts.isObjectLiteralExpression(arg) && arg.properties.find((p) => p.name?.getText() === "id");
			// createProvider -> "provider", createImagesProvider -> "images-provider".
			const kind = node.expression.getText().slice(6).replace(/([a-z])([A-Z])/g, "$1-$2").toLowerCase();
			if (id && literal(id.initializer)) add(pkg, kind, literal(id.initializer), file, lineOf(id));
		});
	}
}

// ─── Environment variables read ──────────────────────────────────────────────────────────────────
function env(pkg, root) {
	const record = (name, file, line, pattern = /^[A-Z][A-Z0-9_]*$/) => {
		if (pattern.test(name)) add(pkg, "env", name, file, line);
	};
	const anyName = /^[A-Za-z_][\w()]*$/;
	for (const file of sourceFiles(root)) {
		visit(parse(file), (node) => {
			const isEnv = (e) => e && /(^|\.)(process|Bun)\.env$/.test(e.getText());
			if (ts.isPropertyAccessExpression(node) && isEnv(node.expression)) record(node.name.text, file, lineOf(node), anyName);
			if (ts.isElementAccessExpression(node) && isEnv(node.expression) && literal(node.argumentExpression)) {
				record(literal(node.argumentExpression), file, lineOf(node), anyName);
			}
			// Provider credentials read through a helper: `ctx.env("X")`, `getEnvApiKey("X")`, `*_ENV = "X"`.
			if (ts.isCallExpression(node) && /(^|\.)(env|getEnv\w*)$/.test(node.expression.getText()) && literal(node.arguments[0])) {
				record(literal(node.arguments[0]), file, lineOf(node));
			}
			if (ts.isVariableDeclaration(node) && /_ENV$/.test(node.name.getText()) && literal(node.initializer)) {
				record(literal(node.initializer), file, lineOf(node));
			}
		});
	}
	for (const file of sourceFiles(root, /\.py$/)) {
		readFileSync(file, "utf8")
			.split("\n")
			.forEach((text, i) => {
				for (const m of text.matchAll(/(?:environ\.get\(|environ\[|getenv\(|_env\w*\()\s*"([A-Z][A-Z0-9_]+)"/g)) record(m[1], file, i + 1);
			});
	}
}

// ─── Python runtime model-facing API ─────────────────────────────────────────────────────────────
function pythonApi(pkg, root) {
	if (pkg !== "coding-agent") return;
	const script = join(repo, "crates/inventory/python_api.py");
	const out = execFileSync(python, ["-I", "-B", script, join(root, "src/ultron/rlm")], { encoding: "utf8", cwd: "/" });
	for (const line of out.trim().split("\n")) {
		const [name, file, at] = line.split("\t");
		add(pkg, "python-api", name, file, Number(at));
	}
}

// ─── Executables and (for the private evals package) npm scripts ─────────────────────────────────
function binaries(pkg, root) {
	const file = join(root, "package.json");
	const text = readFileSync(file, "utf8");
	const manifest = JSON.parse(text);
	const lineFor = (key, section) => {
		const lines = text.split("\n");
		const start = lines.findIndex((l) => l.includes(`"${section}"`));
		return start + 1 + lines.slice(start).findIndex((l) => l.includes(`"${key}"`));
	};
	for (const name of Object.keys(manifest.bin ?? {})) add(pkg, "bin", name, file, lineFor(name, "bin"));
	if (pkg === "evals") for (const name of Object.keys(manifest.scripts ?? {})) add(pkg, "npm-script", name, file, lineFor(name, "scripts"));
}

for (const [pkg, root] of PACKAGES) {
	if (!existsSync(join(root, "package.json"))) throw new Error(`missing package ${root}`);
	for (const extract of [exportsOf, binaries, cli, settings, slashCommands, keybindings, rpc, mcp, extensionApi, providers, env, pythonApi]) {
		extract(pkg, root);
	}
}

const ids = new Set();
const byKey = (a, b) => (a < b ? -1 : a > b ? 1 : 0);
const lines = rows
	.map((r) => ({ ...r, id: `INV-${r.pkg}-${r.kind}-${r.name.replace(/ /g, "_")}` }))
	.sort((a, b) => byKey(a.id, b.id))
	.map((r) => {
		if (ids.has(r.id)) throw new Error(`duplicate id ${r.id}`);
		ids.add(r.id);
		return [r.id, r.pkg, r.kind, r.name, r.at].join("\t");
	});
writeFileSync(join(repo, "crates/inventory/inventory.tsv"), `id\tpackage\tkind\tname\tsource\n${lines.join("\n")}\n`);
const counts = {};
for (const r of rows) counts[`${r.pkg}\t${r.kind}`] = (counts[`${r.pkg}\t${r.kind}`] ?? 0) + 1;
for (const key of Object.keys(counts).sort()) console.log(`${key}\t${counts[key]}`);
console.log(`total\t${rows.length}`);
