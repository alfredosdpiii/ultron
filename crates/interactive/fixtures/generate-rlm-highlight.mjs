// Generates crates/interactive/fixtures/rlm-highlight.json: for each Python cell, highlight.js 10.7.3's HTML
// (`hljs.highlight(code, { language: "python", ignoreIllegals: true }).value`, the instance the TS client registers)
// and the TS `highlightCode(code, "python")` lines (theme.ts) with the dark truecolor and light 256color themes.
// The calls run in fixture order in one process, as highlight.js keeps matcher state between calls;
// crates/interactive/tests/rlm_highlight.rs replays them in the same order.
// Usage: node crates/interactive/fixtures/generate-rlm-highlight.mjs   (deterministic; output is written next to this file)
import "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

process.env.FORCE_COLOR = "3";
const here = dirname(fileURLToPath(import.meta.url));
const ca = (p) => import(join(here, "../../../packages/coding-agent/src", p));
const T = await ca("modes/interactive/theme/theme.ts");
const { validateThemeJson } = await ca("modes/interactive/theme/theme-json.ts");
T.setThemeJsonValidator(validateThemeJson);
const require = createRequire(join(here, "../../../packages/coding-agent/src/utils/syntax-highlight.ts"));
const hljs = (await import(pathToFileURL(require.resolve("highlight.js/lib/core.js")).href)).default;
const themeDir = join(here, "../../../packages/coding-agent/src/modes/interactive/theme");
const THEMES = [
	["dark", "truecolor"],
	["light", "256color"],
].map(([name, mode]) => T.loadThemeFromPath(join(themeDir, `${name}.json`), mode));

const CELLS = [
	"",
	"x",
	"print('hi')",
	"import os\nfrom pathlib import Path\nprint(os.getcwd(), Path('.').resolve())",
	"def f(x: int = 1, *args, **kw) -> str:\n    return f\"{x!r:>10} {kw}\"  # done\n",
	"class A(Base, metaclass=Meta):\n    \"\"\"Doc string.\n\n    >>> A().go()\n    1\n    \"\"\"\n    def go(self):\n        return self.value\n",
	"@decorator(1, key='v')\n@other  # note\ndef g(): pass",
	"  @indented.decorator\n  def h(a, b=(1, 2)): ...",
	"x = 0x1F + 0o17 + 0b1010 + 1_000_000 + 3.14 + .5 + 5. + 1e10 + 2.5E-3j + 10j + 0 + 00 + 0_0",
	"0l 1L 0xffL 1__2 1_ 07 1.2.3 1e 0b2 0o8 .5j 5.e3",
	"s = r'\\d+' + b\"bytes\" + u'uni' + rb'x' + Rb\"y\" + BR'z' + f'{a}' + F\"{b}\" + fr'{c}' + Rf'{d}'",
	"t = '''triple\nsingle''' + \"\"\"triple\ndouble\"\"\" + f'''{x}\n{{literal}}\n''' + rb'''raw\nbytes'''",
	"f\"{a + b}\" f'{x[\"k\"]}' f\"{ {'a': 1}['a'] }\" f'{{}}' f\"{f'{1}'}\"",
	"f'{x # not a comment}'\nprint(1)",
	"msg = 'unterminated\nnext = \"also\nend",
	"x = 1  # TODO: fix this and the other thing\n# FIXME: a doctag\n# NOTE: you should simply do it",
	"y = []  # type: List[int]\nz = {}  # type: Dict[str, Any]  # trailing comment",
	"def f(a,  # type: int\n      ):\n    # type: (...) -> None\n    pass",
	">>> print(1)\n... more\n>>>notprompt\n",
	"if x and not y or z is None:\n    pass\nelif w in v:\n    raise ValueError('bad')\nelse:\n    yield from g()",
	"async def main():\n    await asyncio.sleep(1)\n    async with lock:\n        async for i in it:\n            nonlocal q\n",
	"lambda x: x if x else None\nglobal G\ndel G\nassert True, False\nwith open('f') as fh: fh.read()",
	"try:\n    pass\nexcept (A, B) as e:\n    continue\nfinally:\n    break",
	"_print(1)\nprint_(2)\n__name__ == '__main__'\n__init__\n__debug__\nEllipsis NotImplemented",
	"obj.if obj.class obj.def x.if(1) self.class_ = 1\nclass_name = 'c'\ndefault = 1",
	"a.if(); a.if(); b.def; c.class; if x: pass",
	"self.x = self\nmyself = 1\nselfish(self)",
	"def f(self, x=(1, 'a', [2]), y=\"s\" # comment\n      , z=0x10):\n    pass",
	"def ():\nclass :\ndef f(:\nclass C(object) -> X:",
	"def g() -> Optional[List[Dict[str, Any]]]:\n    return None",
	"def broken(a, b\n    c = 1",
	"class C:\n    x: int = 1\n    def __init__(self) -> None: ...\n",
	"x = a ? b : c\ny => z\n</tag>\nw -> v",
	"result = {k: v for k, v in zip(keys, values) if v is not None}\nsorted(result, key=len, reverse=True)",
	"print(f\"{len(items)} items, {sum(x.size for x in items) / 1e6:.1f} MB\")",
	"isinstance(x, (int, float)) and issubclass(type(x), object) and callable(getattr(x, 'f', None))",
	"Any Callable Coroutine Dict List Literal Generic Optional Sequence Set Tuple Type Union",
	"from typing import Any, Callable, Coroutine, Dict\nT = Type[int]\nset() Set frozenset()",
	"\u65e5\u672c = '\u8a9e'\nprint(\u65e5\u672c)  # \u30b3\u30e1\u30f3\u30c8\nemoji = '\ud83d\ude00'",
	"caf\u00e9 = 1\nna\u00efve_if = 2\n\u00e9if = 3",
	"x = 1\r\ny = 2  # crlf comment\r\nprint(x)\r\n",
	"line1\u2028line2 # sep\u2029end",
	"    indented = True\n        deeper = 'yes'\n",
	"'\\'' \"\\\"\" '\\\\' \"\\n\\t\" '\\x41' '\\u00e9'",
	"'''unterminated triple\nkeeps going\nand going",
	"f'''unterminated {f\ncontinues {x}",
	"r'C:\\path\\' + 'next'",
	"b'\\x00\\xff' bytes([1, 2]) bytearray(b'ab') memoryview(b'')",
	"x = 1 if y else 2\nz = 'a' if q else 'b'\nifx = 1\nif_ = 2",
	"if'string'\nif\"other\"\nif f'x': pass\nelif'y': pass",
	"@property\ndef x(self): return self._x\n@x.setter\ndef x(self, v): self._x = v",
	"@dataclass(frozen=True)\nclass P:\n    x: float = 0.0\n    y: float = field(default=1.5)",
	"@app.route('/path', methods=['GET'])  # comment\n@a.b.c\n@d # e\n",
	"print('a', end='')\nprint(\"b\", sep=\"\\t\")",
	"rlm.query('summarize', ctx.read('file.py'))\nctx.write('out.txt', text)\nanswer = rlm.map(prompts)",
	"for i in range(10):\n\tprint(i)\n\twhile True:\n\t\tbreak",
	"def f(*, a, b): pass\ndef g(a, /, b): pass\ndef h(**kwargs): pass",
	"x = (\n    1,\n    2,  # two\n)\ny = [\n    'a',\n    \"b\",\n]",
	"# just a comment",
	"#",
	"#!/usr/bin/env python3\n# -*- coding: utf-8 -*-",
	"''",
	"\"\"",
	"'''",
	"\"\"\"",
	"f''",
	"{",
	"}",
	"{{}}",
	"(",
	")",
	"()",
	"def",
	"class",
	"if",
	"self",
	"@",
	"->",
	"?",
	"\n\n\n",
	"   ",
	"a.b.c.d(e=f, g=h)[i:j:k]",
	"print(*args, **{'sep': ''})",
	"match command:\n    case [x, y]:\n        pass\n    case _:\n        pass",
	"x := 5\nif (n := len(a)) > 10: pass",
	"del x[0], y.z\nnot_ = not x\nis_ = x is not y",
	"return\nyield\npass\nraise\nNone True False",
	"0.0 1.5e+10 1E5 1_0.0_1 .1e-1j 1.j 1.0J 1J",
	"x=1;y=2;z=x+y",
	"a = {'k': [1, 2, {'n': (3, 4)}]}\nb = a['k'][2]['n'][0]",
	"text = '''\n>>> not a prompt start\n>>> 1 + 1\n2\n'''",
	"def f():\n    '''Docstring with 'quotes' and \"doubles\".'''\n    return f'{1:{width}}'",
	"f\"{'#'}\" f\"{x:#x}\" f'{y!s}'",
	"print('unterminated)\nprint(\"ok\")",
	"class A: pass\nclass B(A): x = 1\nclass C(\n    A,\n    B,\n): ...",
	"def very_long_function_name_that_keeps_going(argument_one, argument_two, argument_three, argument_four):",
	"async\nawait\nnonlocal\nglobal",
	"print(abs(-1), all([]), any([]), ascii('x'), bin(1), bool(0), breakpoint, chr(65), classmethod, compile)",
	"complex delattr dict dir divmod enumerate eval exec filter float format getattr globals hasattr hash help hex id",
	"input int iter len list locals map max min next object oct open ord pow property range repr reversed round",
	"setattr slice sorted staticmethod str sum super tuple type vars zip __import__",
];

// Deterministic generated cells: fragments joined by a seeded PRNG, so many more token interactions are covered.
const FRAGMENTS = [
	"def ", "class ", "if ", "elif ", "else:", "return ", "self", ".", "if", "def", "class", "(", ")", "[", "]",
	"{", "}", ":", ",", " ", "\n", "\n    ", "#", " # c", "# type: int", "'", "\"", "'''", "\"\"\"", "f'", "f\"",
	"r'", "b\"", "{x}", "{{", "}}", "\\", "\\n", "1", "0x1f", "1.5", ".5", "1e3", "2j", "None", "True", "print",
	"len", "List", "Any", "x", "foo", "_bar", "__init__", "@", "@dec", ">>> ", "... ", "->", "?", "=>", "</", "=",
	"+", "*", "**", "lambda", "async ", "await ", "with ", "as ", "in ", "not ", "and ", "TODO:", "you", "\t", "é",
];
let seed = 0x5eed1234;
const rand = () => {
	seed ^= seed << 13;
	seed >>>= 0;
	seed ^= seed >>> 17;
	seed ^= seed << 5;
	seed >>>= 0;
	return seed / 0x100000000;
};
for (let i = 0; i < 300; i++) {
	const n = 1 + Math.floor(rand() * 40);
	let s = "";
	for (let j = 0; j < n; j++) s += FRAGMENTS[Math.floor(rand() * FRAGMENTS.length)];
	CELLS.push(s);
}

const cells = CELLS.map((code) => {
	const html = hljs.highlight(code, { language: "python", ignoreIllegals: true }).value;
	const lines = THEMES.map((theme) => {
		T.setThemeInstance(theme);
		return T.highlightCode(code, "python");
	});
	return { code, html, lines };
});
const out = { hljs: hljs.versionString, themes: ["dark-truecolor", "light-256color"], cells };
writeFileSync(join(here, "rlm-highlight.json"), `${JSON.stringify(out, null, "\t")}\n`);
console.log(`wrote ${cells.length} cells`);
