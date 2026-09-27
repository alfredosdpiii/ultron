"""Skill catalog client. Selection is explainable and version-pinned; skills never grant capabilities."""


class Skills:
    """Skill catalog and code skills.

    Code skills are tested Python kept across sessions. When a procedure worked and will recur, save it:
    `await skills.propose_code(name, source, test_source, evidence)` (name is a lowercase identifier;
    `test_source` defines `test_*` functions and imports the skill with `from code_skills import <name>`).
    The test runs in a fresh kernel; only a passing version becomes active. `await skills.rollback(name,
    version)` undoes the active version; `await skills.code_list()` and `await skills.code_history(name)`
    inspect them. Use an active code skill instead of re-deriving it: `from code_skills import <name>` works
    in every kernel.
    """

    def __init__(self, bridge):
        self._bridge = bridge

    async def refresh(self):
        return await self._bridge.request('skills.refresh', {})

    async def list(self):
        return await self._bridge.request('skills.list', {})

    async def select(self, query, *, limit=None):
        payload = {'query': query}
        if limit is not None:
            payload['limit'] = limit
        return await self._bridge.request('skills.select', payload)

    async def load(self, name, *, version=None):
        payload = {'name': name}
        if version is not None:
            payload['version'] = version
        return await self._bridge.request('skills.load', payload)

    async def why(self, decision_id):
        return await self._bridge.request('skills.why', {'decision_id': decision_id})

    async def invoke(self, name, version, input):
        return await self._bridge.request('skills.invoke', {'name': name, 'version': version, 'input': input})

    # --- Code skills (procedural memory as tested code) ---------------------------------------
    # Active code skills import as `from code_skills import <name>`; see CODE_SKILLS_PROMPT in
    # src/ultron/code-skills.ts. The package is named `code_skills` so it never shadows this
    # `skills` object.

    async def propose_code(self, name, source, test_source, evidence):
        """Write `name` as a new version, run its test in a fresh kernel, and activate it only on pass."""
        return await self._bridge.request('skills.propose_code', {
            'name': name,
            'source': _source_text(source, 'source'),
            'test_source': _source_text(test_source, 'test_source'),
            'evidence': evidence,
        })

    async def propose_policy(self, name, plan, test_source, evidence, *, family=None):
        """Store a decomposition plan (a module defining `plan(...)`, or a plan function) as a tested policy skill."""
        payload = {
            'name': name,
            'source': _plan_source(plan),
            'test_source': _source_text(test_source, 'test_source'),
            'evidence': evidence,
        }
        if family is not None:
            payload['family'] = family
        return await self._bridge.request('skills.propose_policy', payload)

    async def rollback(self, name, version):
        """Roll back the active version `version` of a code skill; the version it replaced becomes active again."""
        return await self._bridge.request('skills.rollback', {'name': name, 'version': version})

    async def code_list(self, *, kind=None, family=None):
        """Active code and policy skills with their docstrings and versions."""
        payload = {}
        if kind is not None:
            payload['kind'] = kind
        if family is not None:
            payload['family'] = family
        return await self._bridge.request('skills.code_list', payload)

    async def code_history(self, name):
        """Every version of a code skill with its state, test outcome, and Jev score."""
        return await self._bridge.request('skills.code_history', {'name': name})


def _source_text(value, label):
    if callable(value) and not isinstance(value, str):
        import inspect
        import textwrap
        try:
            return textwrap.dedent(inspect.getsource(value))
        except (OSError, TypeError) as error:
            raise ValueError(f'{label}: pass the source text; this callable has no retrievable source') from error
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f'{label} must be nonempty Python source text')
    return value


def _plan_source(plan):
    """A policy module must define `plan`; a plan function under another name is aliased to it."""
    source = _source_text(plan, 'plan')
    if callable(plan) and not isinstance(plan, str) and getattr(plan, '__name__', 'plan') != 'plan':
        source = source.rstrip() + f'\n\nplan = {plan.__name__}\n'
    return source


# --- `code_skills` package: active code skills, importable in every kernel ----------------------

import importlib.abc as _abc
import importlib.util as _util
import hashlib as _hashlib
import json as _json
import os as _os
import re as _re
import sys as _sys
import types as _types

CODE_SKILLS_PACKAGE = 'code_skills'
_NAME = _re.compile(r'^[a-z][a-z0-9_]{0,63}$')


def _skills_dir():
    value = _os.environ.get('ULTRON_CODE_SKILLS_DIR', '').strip()
    return value or None


def _active_entry(name):
    """(version, path) of the active version, verified against its recorded digest, or None."""
    root = _skills_dir()
    if root is None or not _NAME.match(name):
        return None
    base = _os.path.join(root, name)
    try:
        with open(_os.path.join(base, '.history.json'), encoding='utf-8') as handle:
            history = _json.load(handle)
    except (OSError, ValueError):
        return None
    active = history.get('active') if isinstance(history, dict) else None
    if not isinstance(active, int):
        return None
    record = next((item for item in history.get('versions', []) if isinstance(item, dict) and item.get('version') == active), None)
    if record is None or record.get('state') != 'active':
        return None
    path = _os.path.join(base, '.versions', str(active), 'skill.py')
    try:
        with open(path, 'rb') as handle:
            digest = _hashlib.sha256(handle.read()).hexdigest()
    except OSError:
        return None
    # Only the exact source whose test passed is importable.
    if digest != (record.get('sha256') or {}).get('skill'):
        return None
    return active, path


def _load(fullname, name, version, path):
    spec = _util.spec_from_file_location(fullname, path)
    module = _util.module_from_spec(spec)
    module.__skill_version__ = version
    module.__skill_name__ = name
    previous = _sys.modules.get(fullname)
    _sys.modules[fullname] = module
    try:
        spec.loader.exec_module(module)
    except BaseException:
        if previous is None:
            _sys.modules.pop(fullname, None)
        else:
            _sys.modules[fullname] = previous
        raise
    return module


class _CodeSkillsPackage(_types.ModuleType):
    """`from code_skills import name` always resolves to the active version at the time of import."""

    def __init__(self):
        super().__init__(CODE_SKILLS_PACKAGE, 'Tested Python code skills. `from code_skills import <name>`.')
        object.__setattr__(self, '__path__', [])
        object.__setattr__(self, '_candidates', {})

    def __setattr__(self, key, value):
        # The import system sets loaded submodules as attributes; keep resolution fresh instead.
        if _NAME.match(key):
            return
        object.__setattr__(self, key, value)

    def _use_candidate(self, name, path, version=None):
        """Test kernels only: resolve `name` to an unactivated candidate file."""
        self._candidates[name] = (version, path)

    def _resolve(self, name):
        if name in self._candidates:
            return self._candidates[name]
        return _active_entry(name)

    def __getattr__(self, name):
        if name.startswith('__') or not _NAME.match(name):
            raise AttributeError(name)
        entry = self._resolve(name)
        if entry is None:
            # Drop a module cached from an earlier active version, which `from ... import` would fall back to.
            _sys.modules.pop(f'{CODE_SKILLS_PACKAGE}.{name}', None)
            raise AttributeError(f'no active code skill named {name!r} (see `await skills.code_list()`)')
        version, path = entry
        fullname = f'{CODE_SKILLS_PACKAGE}.{name}'
        cached = _sys.modules.get(fullname)
        if cached is not None and getattr(cached, '__skill_version__', None) == version and getattr(cached, '__file__', None) == path:
            return cached
        return _load(fullname, name, version, path)

    def __dir__(self):
        root = _skills_dir()
        names = set(self._candidates)
        if root is not None:
            try:
                names.update(entry for entry in _os.listdir(root) if _NAME.match(entry) and _active_entry(entry))
            except OSError:
                pass
        return sorted(names)


class _CodeSkillsFinder(_abc.MetaPathFinder):
    """Makes `import code_skills.name` and `from code_skills.name import f` work too."""

    def find_spec(self, fullname, path=None, target=None):
        if fullname == CODE_SKILLS_PACKAGE or not fullname.startswith(CODE_SKILLS_PACKAGE + '.'):
            return None
        name = fullname[len(CODE_SKILLS_PACKAGE) + 1:]
        package = _sys.modules.get(CODE_SKILLS_PACKAGE)
        if not isinstance(package, _CodeSkillsPackage) or '.' in name or not _NAME.match(name):
            return None
        entry = package._resolve(name)
        if entry is None:
            return None
        version, file_path = entry
        spec = _util.spec_from_file_location(fullname, file_path)
        loader = spec.loader
        original = loader.exec_module

        def exec_module(module):
            module.__skill_version__ = version
            module.__skill_name__ = name
            original(module)

        loader.exec_module = exec_module
        return spec


def install_code_skills():
    if not isinstance(_sys.modules.get(CODE_SKILLS_PACKAGE), _CodeSkillsPackage):
        _sys.modules[CODE_SKILLS_PACKAGE] = _CodeSkillsPackage()
    if not any(isinstance(finder, _CodeSkillsFinder) for finder in _sys.meta_path):
        _sys.meta_path.append(_CodeSkillsFinder())


# Every kernel imports this module at startup, so every kernel can import active code skills.
install_code_skills()
