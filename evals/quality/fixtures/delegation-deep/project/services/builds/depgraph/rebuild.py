"""Incremental rebuilds with early cutoff."""
from dataclasses import dataclass, field

from .order import topo_order


@dataclass(frozen=True)
class Rebuild:
    """What an incremental build did: the targets rebuilt and the ones whose output changed, both in build order,
    and every target's output after the build."""

    rebuilt: tuple = ()
    changed: tuple = ()
    outputs: dict = field(default_factory=dict)


def directly_dirty(graph, changed_inputs, previous):
    """Targets that must be rebuilt whatever their dependencies do, in build order."""
    changed_inputs = set(changed_inputs)
    return [
        name
        for name in topo_order(graph)
        if name not in previous or any(item in changed_inputs for item in graph.inputs(name))
    ]


def affected(graph, changed_inputs, previous):
    """Every target that could need a rebuild (the directly dirty targets and all their dependents), sorted."""
    reached = set()
    stack = directly_dirty(graph, changed_inputs, previous)
    while stack:
        name = stack.pop()
        if name in reached:
            continue
        reached.add(name)
        stack.extend(graph.dependents(name))
    return sorted(reached)


def rebuild(graph, changed_inputs, build, previous):
    """Rebuild what `changed_inputs` invalidated, given the `previous` outputs of every built target.

    `build(name, dep_outputs)` builds one target from its dependencies' current outputs and returns its output.
    A target is rebuilt when it is directly dirty (it reads a changed input, or has no previous output) or when
    the output of one of its dependencies changed in this build. A rebuilt target whose output equals its
    previous output does not dirty its dependents (early cutoff). Targets are built in topological order.
    """
    order = topo_order(graph)
    position = {name: index for index, name in enumerate(order)}
    outputs = {name: previous[name] for name in order if name in previous}
    pending = directly_dirty(graph, changed_inputs, previous)
    queued = set(pending)
    rebuilt = []
    changed = []
    while pending:
        name = pending.pop(0)
        output = build(name, {dep: outputs[dep] for dep in graph.deps(name)})
        rebuilt.append(name)
        outputs[name] = output
        if name in previous and previous[name] == output:
            continue
        changed.append(name)
        for dependent in graph.dependents(name):
            if dependent not in queued:
                queued.add(dependent)
                pending.append(dependent)
    rebuilt.sort(key=position.__getitem__)
    changed.sort(key=position.__getitem__)
    return Rebuild(rebuilt=tuple(rebuilt), changed=tuple(changed), outputs={name: outputs[name] for name in order})
