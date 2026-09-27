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


def carried_outputs(graph, previous):
    """The previous outputs of the graph's targets, without entries for targets the graph no longer has."""
    if len(previous) == len(graph) and all(name in graph for name in previous):
        return previous
    return {name: output for name, output in previous.items() if name in graph}


def rebuild(graph, changed_inputs, build, previous):
    """Rebuild what `changed_inputs` invalidated, given the `previous` outputs of every built target.

    `build(name, dep_outputs)` builds one target from its dependencies' current outputs and returns its output.
    A target is rebuilt when it is directly dirty (it reads a changed input, or has no previous output) or when
    the output of one of its dependencies changed in this build. A rebuilt target whose output equals its
    previous output does not dirty its dependents (early cutoff). Targets are built in topological order.
    """
    order = topo_order(graph)
    dirty = set(directly_dirty(graph, changed_inputs, previous))
    outputs = carried_outputs(graph, previous)
    rebuilt = []
    changed = set()
    for name in order:
        if name not in dirty and not any(dep in changed for dep in graph.deps(name)):
            continue
        output = build(name, {dep: outputs[dep] for dep in graph.deps(name)})
        rebuilt.append(name)
        outputs[name] = output
        if name not in previous or previous[name] != output:
            changed.add(name)
    return Rebuild(
        rebuilt=tuple(rebuilt),
        changed=tuple(name for name in order if name in changed),
        outputs={name: outputs[name] for name in order},
    )
