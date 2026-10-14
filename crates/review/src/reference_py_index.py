"""The Python indexer of the review reference, run by the Rust host (crates/review src/reference.rs) with the
kernel's interpreter (`python -I`), once per reference build. Python files are indexed with `ast`, which Rust does
not reimplement: the visitor below is `autoreview_reference.py`'s (`_py_signature`, `_py_name`, `_py_family`,
`_PyVisitor`, `index_python`), copied verbatim; fixtures/record_reference.py records the module's own output and the
Rust tests compare this script's against it.

Protocol: each stdin line is a JSON list of [path, text]; each reply is one stdout line, the JSON list of their
FileIndex records (`exports` sorted), or null when the request itself could not be read. Standard library only.
"""
from __future__ import annotations

import ast
import json
import sys
from dataclasses import dataclass, field

MAX_CALLS_PER_FILE = 2_000
MAX_FAMILIES_PER_FILE = 60
MIN_FAMILY_ITEMS = 2
MAX_LINE_CHARS = 160


def _clip(text: str, limit: int = MAX_LINE_CHARS) -> str:
    text = text.strip()
    return text if len(text) <= limit else text[: limit - 1] + "…"


@dataclass
class Symbol:
    path: str
    name: str
    qualname: str
    kind: str
    line: int
    end: int
    signature: str = ""
    parent: str | None = None
    bases: list[str] = field(default_factory=list)
    decorators: list[str] = field(default_factory=list)
    doc: str = ""
    exported: bool = True


@dataclass
class CallSite:
    path: str
    line: int
    callee: str
    text: str
    enclosing: str | None
    targets: list[str] = field(default_factory=list)


@dataclass
class Family:
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
    imports: dict[str, tuple[str | None, str]] = field(default_factory=dict)
    calls: list[CallSite] = field(default_factory=list)
    families: list[Family] = field(default_factory=list)
    exports: set[str] = field(default_factory=set)
    parsed: bool = True


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



def _record(index: FileIndex) -> dict:
    return {
        "path": index.path, "lang": index.lang,
        "symbols": [vars(symbol) for symbol in index.symbols],
        "imports": {local: list(value) for local, value in index.imports.items()},
        "calls": [vars(call) for call in index.calls],
        "families": [dict(vars(family), items=list(family.items)) for family in index.families],
        "exports": sorted(index.exports), "parsed": index.parsed,
    }


def main() -> None:
    for raw in sys.stdin.buffer:
        try:
            files = json.loads(raw.decode("utf-8"))
            reply = []
            for path, text in files:
                try:
                    reply.append(_record(index_python(path, text)))
                except Exception:  # one file never costs the others
                    reply.append(_record(FileIndex(path, "python", parsed=False)))
        except Exception:
            reply = None
        sys.stdout.buffer.write(json.dumps(reply).encode("ascii") + b"\n")
        sys.stdout.buffer.flush()


if __name__ == "__main__":
    main()
