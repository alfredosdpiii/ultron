"""Build targets and the dependencies between them."""


def _check_name(value, what):
    if not isinstance(value, str) or not value or value != value.strip():
        raise ValueError(f"{what} must be a non-empty string without surrounding spaces, got {value!r}")
    return value


class Graph:
    """Named build targets. Each target lists the targets it depends on and the source inputs it reads.

    Dependencies may name targets that are added later; they are only checked when the graph is ordered.
    """

    def __init__(self):
        self._declared = {}
        self._inputs = {}

    def add(self, name, deps=(), inputs=()):
        """Define target `name`, depending on the targets in `deps` and reading the source inputs in `inputs`."""
        _check_name(name, "a target name")
        if name in self._declared:
            raise ValueError(f"target {name!r} is already defined")
        declared = [_check_name(dep, f"a dependency of {name!r}") for dep in deps]
        reads = [_check_name(item, f"an input of {name!r}") for item in inputs]
        self._declared[name] = declared
        self._inputs[name] = tuple(sorted(set(reads)))
        return self

    def __contains__(self, name):
        return name in self._declared

    def __len__(self):
        return len(self._declared)

    def __iter__(self):
        return iter(self.targets())

    def targets(self):
        """Every target name, sorted."""
        return sorted(self._declared)

    def deps(self, name):
        """The targets `name` depends on: sorted, each once."""
        return tuple(sorted(set(self._declared[name])))

    def inputs(self, name):
        """The source inputs `name` reads: sorted, each once."""
        return self._inputs[name]

    def dependents(self, name):
        """The defined targets that depend on `name` directly: sorted, each once."""
        if name not in self._declared:
            raise KeyError(name)
        return tuple(sorted(target for target, declared in self._declared.items() if name in declared))

    def readers(self, item):
        """The targets that read source input `item`, sorted."""
        return tuple(sorted(target for target, reads in self._inputs.items() if item in reads))

    def edges(self):
        """Every (target, dependency) pair in declaration order, targets in name order."""
        for name in sorted(self._declared):
            for dep in self._declared[name]:
                yield name, dep

    def undefined(self):
        """Dependencies that name no defined target, as sorted (target, dependency) pairs."""
        return sorted({(name, dep) for name, dep in self.edges() if dep not in self._declared})
