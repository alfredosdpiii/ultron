"""Choosing versions of external packages."""
from .versions import Constraint, Version


class ResolutionError(Exception):
    """No choice of versions satisfies the requirements."""


def _releases(registry):
    """Every package's releases, highest version first, as (version, text, {dependency: Constraint})."""
    releases = {}
    for package, versions in registry.items():
        entries = []
        for text, deps in versions.items():
            parsed = {dep: Constraint(constraint) for dep, constraint in sorted(deps.items())}
            entries.append((Version.parse(text), text, parsed))
        entries.sort(key=lambda entry: entry[0], reverse=True)
        releases[package] = entries
    return releases


class _Search:
    """Depth-first search over packages in name order, trying each package's highest version first and leaving a
    package out last, so the first complete choice found is the best one."""

    def __init__(self, releases, roots):
        self.releases = releases
        self.roots = roots
        names = set(releases) | set(roots)
        for entries in releases.values():
            for _, _, deps in entries:
                names.update(deps)
        self.packages = sorted(names)
        self.rank = {name: index for index, name in enumerate(self.packages)}
        self.chosen = {}

    def run(self):
        required = {name: [constraint] for name, constraint in self.roots.items()}
        return self._step(0, required)

    def _fits_chosen(self, package, version, deps):
        for dep, constraint in deps.items():
            if dep == package:
                if not constraint.matches(version):
                    return False
            elif self.rank[dep] < self.rank[package]:
                entry = self.chosen.get(dep)
                if entry is None or not constraint.matches(entry[0]):
                    return False
        return True

    def _step(self, position, required):
        if position == len(self.packages):
            return self._complete()
        package = self.packages[position]
        constraints = required.get(package, [])
        for version, text, deps in self.releases.get(package, ()):
            if not all(constraint.matches(version) for constraint in constraints):
                continue
            if not self._fits_chosen(package, version, deps):
                continue
            extended = dict(required)
            for dep, constraint in deps.items():
                extended.setdefault(dep, []).append(constraint)
            self.chosen[package] = (version, text, deps)
            found = self._step(position + 1, extended)
            if found is not None:
                return found
            del self.chosen[package]
        if not constraints:
            return self._step(position + 1, required)
        return None

    def _complete(self):
        """The current choice, if every chosen package is needed: reachable from the roots through dependencies."""
        reached = set()
        stack = sorted(self.roots)
        while stack:
            name = stack.pop()
            if name in reached:
                continue
            entry = self.chosen.get(name)
            if entry is None:
                return None
            reached.add(name)
            stack.extend(entry[2])
        if reached != set(self.chosen):
            return None
        return {name: self.chosen[name][1] for name in sorted(self.chosen)}


def resolve(registry, requirements):
    """Choose versions for `requirements` ({package: constraint text}) from `registry`.

    `registry` maps each package to its releases: {version text: {dependency: constraint text}}. Returns
    {package: version text} for exactly the packages needed, sorted by name. Raises ResolutionError when no choice
    works and ValueError for a malformed version or constraint.
    """
    roots = {package: Constraint(text) for package, text in requirements.items()}
    found = _Search(_releases(registry), roots).run()
    if found is None:
        wanted = ", ".join(f"{name} {roots[name].text}" for name in sorted(roots))
        raise ResolutionError(f"no versions satisfy {wanted}")
    return found
