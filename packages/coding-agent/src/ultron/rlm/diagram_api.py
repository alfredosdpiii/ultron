"""`diagram`: Mermaid diagrams rendered by the Rust host (node with the vendored Pele renderer, Ultron's theme colours,
an SVG and a 2x PNG saved to files). Hosts without the request (the TypeScript host) answer it with an error.
"""

from __future__ import annotations

import os
from typing import Any

_THEMES = ("auto", "dark", "light")


class Diagram:
    """Mermaid diagrams: `d = await diagram.mermaid("flowchart LR\\n  a --> b")` renders the text, saves
    `<slug>-<n>.svg` and `.png` (the session's `diagrams/` output directory, or `path=`: a file stem or an
    existing directory) and shows the PNG inline to the user; it returns `{"svg_path", "png_path", "width",
    "height", "type"}` (PNG pixels). The image stays out of your context (you get one line `diagram: <path> (WxH)`)
    unless `to_model=True`; `show=False` saves without showing. `theme=` is "auto" (Ultron's current theme),
    "dark" or "light". A Mermaid syntax error raises RuntimeError with the renderer's message. Needs Node.js on
    PATH."""

    def __init__(self, bridge: Any) -> None:
        self._bridge = bridge

    async def mermaid(self, text: str, *, theme: str = "auto", show: bool = True, to_model: bool = False,
                      path: Any = None) -> dict[str, Any]:
        """Render Mermaid `text` to an SVG and a PNG and save both: `{"svg_path", "png_path", "width", "height",
        "type"}` (the PNG's pixel size, Pele's diagram type). `show=True` puts the PNG in this cell's result for the
        user; `to_model=True` also lets you see it (otherwise only a `diagram: <path> (WxH)` line reaches you).
        `theme`: "auto", "dark" or "light". `path`: where to save (a stem such as "docs/flow", or an existing
        directory). Raises ValueError for invalid arguments, RuntimeError for syntax errors and host failures."""
        if not isinstance(text, str) or not text.strip():
            raise ValueError("diagram.mermaid text must be a non-empty string")
        if theme not in _THEMES:
            raise ValueError('diagram.mermaid theme must be "auto", "dark" or "light"')
        if not isinstance(show, bool) or not isinstance(to_model, bool):
            raise ValueError("diagram.mermaid show and to_model must be True or False")
        payload: dict[str, Any] = {"text": text, "theme": theme, "show": show, "to_model": to_model}
        if path is not None:
            path = os.fspath(path)
            if not path:
                raise ValueError("diagram.mermaid path must be a non-empty string")
            payload["path"] = os.path.abspath(path)
        return await self._bridge.request("diagram.mermaid", payload)

    def __repr__(self) -> str:
        return "<diagram: mermaid(text, *, theme='auto', show=True, to_model=False, path=None)>"
