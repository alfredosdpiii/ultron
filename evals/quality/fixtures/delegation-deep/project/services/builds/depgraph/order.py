"""Build order and dependency cycles."""
import heapq


class CycleError(ValueError):
    """The targets to order contain a dependency cycle; `cycle` is the canonical one (see find_cycle)."""

    def __init__(self, cycle):
        self.cycle = list(cycle)
        super().__init__("dependency cycle: " + " -> ".join(self.cycle + self.cycle[:1]))


def closure(graph, targets=None):
    """The given targets (default: all) plus everything they depend on, transitively, as a set."""
    if targets is None:
        roots = graph.targets()
    else:
        roots = list(targets)
        for name in roots:
            if name not in graph:
                raise KeyError(name)
    seen = set()
    stack = list(roots)
    while stack:
        name = stack.pop()
        if name in seen:
            continue
        seen.add(name)
        for dep in graph.deps(name):
            if dep not in graph:
                raise ValueError(f"{name!r} depends on undefined target {dep!r}")
            if dep not in seen:
                stack.append(dep)
    return seen


def topo_order(graph, targets=None):
    """The order to build `targets` (default: all) and their dependencies in.

    Every target comes after all of its dependencies; whenever several targets are ready, the smallest name goes
    first. Raises CycleError when the targets to order contain a cycle.
    """
    included = closure(graph, targets)
    waiting = {name: len(graph.deps(name)) for name in included}
    released_by = {name: [] for name in included}
    for name, dep in graph.edges():
        if name in included:
            released_by[dep].append(name)
    ready = [name for name, count in waiting.items() if count == 0]
    heapq.heapify(ready)
    order = []
    while ready:
        name = heapq.heappop(ready)
        order.append(name)
        for dependent in released_by[name]:
            waiting[dependent] -= 1
            if waiting[dependent] == 0:
                heapq.heappush(ready, dependent)
    if len(order) != len(included):
        raise CycleError(find_cycle(graph, targets))
    return order


def _reaches(graph, start, goal, blocked):
    """Whether `goal` can be reached from `start` along dependencies without passing through `blocked`."""
    seen = {start}
    stack = [start]
    while stack:
        name = stack.pop()
        for dep in graph.deps(name):
            if dep == goal:
                return True
            if dep in blocked or dep in seen:
                continue
            seen.add(dep)
            stack.append(dep)
    return False


def find_cycle(graph, targets=None):
    """The canonical dependency cycle among `targets` (default: all) and their dependencies, or None.

    A cycle is a list of distinct targets, each depending on the next and the last on the first, rotated to start
    at its smallest name. The canonical cycle is the smallest such list (Python list order).
    """
    included = closure(graph, targets)
    on_cycle = sorted(name for name in included if _reaches(graph, name, name, frozenset()))
    if not on_cycle:
        return None
    start = on_cycle[0]
    path = [start]
    while True:
        current = path[-1]
        deps = graph.deps(current)
        if start in deps:
            return path
        blocked = set(path)
        for dep in deps:
            if dep not in blocked and _reaches(graph, dep, start, blocked):
                path.append(dep)
                break
        else:
            raise RuntimeError(f"cycle through {start!r} lost at {current!r}")
