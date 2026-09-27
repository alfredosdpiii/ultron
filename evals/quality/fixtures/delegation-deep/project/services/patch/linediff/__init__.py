"""Line diffs and patches: minimal edit scripts, unified hunks, parsing, applying with offsets, reversing."""
from .apply import apply_hunks, apply_patch, locate, offsets, reverse_patch
from .hunks import diffstat, make_hunks, reverse_hunks
from .lcs import diff_lines, lcs_length, similarity
from .model import Hunk, Op, PatchError
from .unified import format_hunk, format_hunks, format_unified, parse_unified

__all__ = [
    "Hunk",
    "Op",
    "PatchError",
    "apply_hunks",
    "apply_patch",
    "diff_lines",
    "diffstat",
    "format_hunk",
    "format_hunks",
    "format_unified",
    "lcs_length",
    "locate",
    "make_hunks",
    "offsets",
    "parse_unified",
    "reverse_hunks",
    "reverse_patch",
    "similarity",
]
