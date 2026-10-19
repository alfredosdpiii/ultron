"""`rlm.images`: image generation for the Rust host (OpenRouter's images API through the host's provider auth).

The host makes the request and saves each image to a file; the cell gets back file paths, never image bytes.
Hosts without these requests (the TypeScript host) answer them with an error.
"""

from __future__ import annotations

import os
from typing import Any

MAX_IMAGES = 4


class Images:
    """Image generation: `r = await rlm.images.generate("a lighthouse at dusk, watercolor")` saves the image to a
    file and returns `{"paths": [...], "model": "openrouter/<id>"}`; `await view_image(r["paths"][0])` shows it to
    you. `model=` picks an image model (default: the `images.model` setting, else
    `openrouter/google/gemini-2.5-flash-image`); `await rlm.images.models()` lists them. `size=` is an aspect ratio
    (`"16:9"`) or a resolution (`"1K"`, `"2K"`, `"4K"`); `n=` (1-4) asks for several images; `out=` is the file path
    (several images: `<stem>-1.png`, ...) or an existing directory (default: the session's output directory).
    Needs an OpenRouter API key (`OPENROUTER_API_KEY` or `/login`)."""

    def __init__(self, bridge: Any) -> None:
        self._bridge = bridge

    async def generate(self, prompt: str, model: str | None = None, size: str | None = None, n: int = 1,
                       out: Any = None) -> dict[str, Any]:
        """Generate `n` images for `prompt` and save them: `{"paths": [absolute file paths], "model": "provider/id"}`.
        Raises ValueError for invalid arguments and RuntimeError when the host or the provider fails."""
        if not isinstance(prompt, str) or not prompt.strip():
            raise ValueError("rlm.images.generate prompt must be a non-empty string")
        if model is not None and (not isinstance(model, str) or not model.strip()):
            raise ValueError("rlm.images.generate model must be a non-empty string")
        if size is not None and (not isinstance(size, str) or not size.strip()):
            raise ValueError("rlm.images.generate size must be a string such as '16:9' or '2K'")
        if isinstance(n, bool) or not isinstance(n, int) or not 1 <= n <= MAX_IMAGES:
            raise ValueError(f"rlm.images.generate n must be an integer from 1 to {MAX_IMAGES}")
        payload: dict[str, Any] = {"prompt": prompt, "n": n}
        if model is not None:
            payload["model"] = model
        if size is not None:
            payload["size"] = size
        if out is not None:
            payload["out"] = os.path.abspath(os.fspath(out))
        return await self._bridge.request("images.generate", payload)

    async def models(self) -> list[dict[str, Any]]:
        """The image models: `[{"model": "provider/id", "name": ...}]`."""
        return await self._bridge.request("images.models", {})

    def __repr__(self) -> str:
        return "<rlm.images: generate(prompt, model=None, size=None, n=1, out=None), models()>"


def install(rlm_class: Any) -> None:
    """Add `rlm.images` to the `rlm` class."""

    def images(self: Any) -> Images:
        return Images(self._bridge)

    images.__doc__ = Images.__doc__
    rlm_class.images = property(images)
