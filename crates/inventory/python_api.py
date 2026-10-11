# Lists the Python kernel's model-facing API: every name the runtime binds in a cell's namespace,
# and the public members of each bound object (what help() shows). Prints TSV: name, path, line.
# Usage: python3 -I python_api.py <packages/coding-agent/src/ultron/rlm>
import ast
import inspect
import os
import sys
import warnings

warnings.simplefilter("ignore")
root = os.path.realpath(sys.argv[1])
sys.path.insert(0, root)
import runtime  # noqa: E402

STDLIB = {"__name__", "asyncio", "re", "json", "os", "Path", "state"}
state = runtime.RuntimeState()
runtime_path = os.path.join(root, "runtime.py")
sources = [runtime_path] + sorted(os.path.join(root, f) for f in os.listdir(root) if f.endswith(".py") and f != "runtime.py")
trees = {}


def tree(path):
    if path not in trees:
        trees[path] = ast.parse(open(path, encoding="utf-8").read())
    return trees[path]


def binding(name):
    """File and line that bind `name` in the cell namespace."""
    for path in sources:
        for i, text in enumerate(open(path, encoding="utf-8").read().splitlines(), 1):
            if f'namespace["{name}"] =' in text:
                return path, i
    raise SystemExit(f"no binding line for {name}")


def where(obj):
    obj = inspect.unwrap(obj)
    path = os.path.realpath(inspect.getsourcefile(obj))
    lines, line = inspect.getsourcelines(obj)
    # Point at the `def` line, past any decorators.
    while lines and lines[0].lstrip().startswith("@"):
        lines, line = lines[1:], line + 1
    return path, line


def member_line(cls, attr):
    """Line of a non-function member: `self.attr = ...` or a class-level assignment in cls or a base."""
    for klass in cls.__mro__:
        try:
            path = os.path.realpath(inspect.getsourcefile(klass))
        except TypeError:
            continue
        for node in ast.walk(tree(path)):
            if isinstance(node, ast.ClassDef) and node.name == klass.__name__:
                for sub in ast.walk(node):
                    targets = []
                    if isinstance(sub, ast.Assign):
                        targets = sub.targets
                    elif isinstance(sub, ast.AnnAssign):
                        targets = [sub.target]
                    for t in targets:
                        if (isinstance(t, ast.Attribute) and t.attr == attr) or (isinstance(t, ast.Name) and t.id == attr):
                            return path, sub.lineno
    return None


def member(obj, attr):
    cls = type(obj)
    static = inspect.getattr_static(obj, attr)
    if isinstance(static, property):
        return where(static.fget)
    value = getattr(obj, attr)
    if inspect.ismethod(value) or inspect.isfunction(value):
        return where(value)
    return member_line(cls, attr)


rows = []
for name, value in state.namespace.items():
    if name in STDLIB:
        continue
    rows.append((name, *binding(name)))
    if inspect.isfunction(value) or inspect.isclass(value) or inspect.ismodule(value):
        continue
    for attr in sorted(a for a in dir(value) if not a.startswith("_")):
        loc = member(value, attr)
        if loc is None:
            raise SystemExit(f"no source line for {name}.{attr}")
        rows.append((f"{name}.{attr}", loc[0], loc[1]))
for name, path, line in rows:
    print(f"{name}\t{path}\t{line}")
