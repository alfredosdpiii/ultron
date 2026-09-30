"""Sub-agent worktrees in the kernel: path mapping for a worktree child, and `rlm.worktrees`.

A child started with `rlm.spawn(..., worktree=True)` runs in its own Git worktree; the host sets ULTRON_WORKTREE
(the worktree's root) and ULTRON_PARENT_REPO (the parent's checkout) in its kernel. Briefs often name files by
their absolute path in the parent's checkout: `read`, `edit`, `write`, `view_image` and `rlm.load` map such a path
to the same file in the worktree (and say so once per path), so a child never edits its parent's files by accident.
Shell commands are not rewritten; the host reports files a worktree child changed in its parent's tree.
"""
from __future__ import annotations

import os
from pathlib import Path
from typing import Any

_ANNOUNCED: set[str] = set()


def worktree_path(path: Path) -> Path:
    """`path` itself, or its counterpart in this kernel's worktree when it points into the parent's checkout."""
    worktree = os.environ.get("ULTRON_WORKTREE", "").strip()
    parent = os.environ.get("ULTRON_PARENT_REPO", "").strip()
    if not worktree or not parent or not path.is_absolute():
        return path
    try:
        resolved = Path(os.path.abspath(path))
        root = Path(os.path.abspath(worktree))
        repo = Path(os.path.abspath(parent))
        if resolved == root or root in resolved.parents:
            return path
        if resolved != repo and repo not in resolved.parents:
            return path
        relative = resolved.relative_to(repo)
        if relative.parts[:1] == (".git",):
            return path
    except (OSError, ValueError):
        return path
    mapped = root / relative
    key = str(resolved)
    if key not in _ANNOUNCED:
        _ANNOUNCED.add(key)
        print(f"[worktree] {path} is in your parent's checkout; using your worktree's copy {mapped}")
    return mapped


class Worktrees:
    """This session's sub-agent worktrees (`rlm.spawn(..., worktree=True)`).

    `await rlm.worktrees.list(all=False)` lists them (branch, path, state, running); `await
    rlm.worktrees.cleanup(branches=False)` removes the worktrees of this session's finished children (their
    branches stay when they hold unmerged work; `branches=True` deletes those too) and those of crashed sessions.
    Merged and empty children are cleaned up already; `ULTRON_KEEP_WORKTREES=1` keeps everything.
    """

    def __init__(self, bridge: Any) -> None:
        self._bridge = bridge

    async def list(self, all: bool = False) -> list[dict[str, Any]]:
        result = await self._bridge.request("rlm.worktrees.list", {"all": bool(all)})
        return result.get("worktrees", []) if isinstance(result, dict) else []

    async def cleanup(self, branches: bool = False) -> dict[str, Any]:
        result = await self._bridge.request("rlm.worktrees.cleanup", {"branches": bool(branches)})
        return result if isinstance(result, dict) else {"result": result}

    def __repr__(self) -> str:
        return "<rlm.worktrees: await .list(), await .cleanup(branches=False)>"
