# patch: line diffs and patches

Package `linediff`. Files are lists of lines: Python strings without newline characters. Nothing reads or writes
real files.

## Edit scripts

- `diff_lines(a, b)` returns a list of `Op(tag, i1, i2, j1, j2)` turning `a` into `b`. `tag` is `"equal"`
  (`a[i1:i2] == b[j1:j2]`), `"delete"` (`a[i1:i2]` removed, `j1 == j2`) or `"insert"` (`b[j1:j2]` added,
  `i1 == i2`). The ops cover `a` and `b` contiguously from index 0 to the end, no op is empty, and two adjacent ops
  never have the same tag. Between two equal ops (or before the first / after the last) the changes are at most
  one delete op followed by at most one insert op.
- The script is minimal: the equal ops cover exactly `lcs_length(a, b)` lines (the length of a longest common
  subsequence). Which common subsequence is used is unspecified.
- `similarity(a, b)` is `2 * common / (len(a) + len(b))`, and `1.0` when both are empty.

## Hunks

`Hunk(old_start, old_count, new_start, new_count, lines)`: `lines` is a list of `(tag, text)` with tag `" "`
(context, in both files), `"-"` (only in the old file) or `"+"` (only in the new file). `old_count` is the number of
`" "` and `"-"` lines, `new_count` the number of `" "` and `"+"` lines. Starts follow the unified convention:
when a side's count is positive its start is the 1-based number of its first line; when the count is 0 its start is
the number of lines before the hunk on that side (so a pure insertion at the top of a file has old start 0).

- `make_hunks(a, b, context=3)` groups the changes of a minimal edit script into hunks. Each hunk shows up to
  `context` unchanged lines before its first change and after its last change (fewer only at the start or end of
  the file). Two consecutive change regions separated by `g` unchanged lines belong to the same hunk when
  `g <= 2 * context` (their context would overlap or touch), and to different hunks otherwise; so the old-file line
  ranges of consecutive hunks never overlap or touch. Inside a hunk each change region lists its `"-"` lines before
  its `"+"` lines. No changes: `[]`. `context < 0` raises `ValueError`.
- `reverse_hunks(hunks)` swaps the old and new sides of every hunk (starts, counts, `"-"` and `"+"` tags); within
  each run of changed lines the `"-"` lines come first, then the `"+"` lines, each keeping its order.
  `reverse_hunks(reverse_hunks(h)) == h` for hunks made by `make_hunks`.
- `diffstat(hunks)` is `(lines added, lines removed)`.

## Unified text

- `format_hunks(hunks, from_name="a", to_name="b")` returns `""` for no hunks, else the lines `--- <from_name>`,
  `+++ <to_name>`, then for each hunk its header `@@ -<old_start>,<old_count> +<new_start>,<new_count> @@` (both
  counts always written) followed by one line per body line: the tag character, then the text. Every line ends
  with `\n`. `format_unified(a, b, context=3, from_name="a", to_name="b")` is
  `format_hunks(make_hunks(a, b, context), from_name, to_name)`.
- `parse_unified(text)` returns the hunks in order. A hunk's body is exactly the number of lines its header counts
  say, whatever those lines contain (a body line may itself start with `--- `, `+++ ` or `@@`). Outside hunk
  bodies, lines starting with `--- ` or `+++ ` are file headers and are skipped; any other line that is not a hunk
  header raises `ValueError`, as do a body line not starting with `" "`, `"-"` or `"+"`, and a body cut short by
  the end of the text. `parse_unified(format_hunks(h)) == h`.

## Applying

`apply_hunks(lines, hunks)` applies the hunks in order and returns the new list (the input is not modified). The
pieces of the input between hunks are copied unchanged. For each hunk:

- Its old lines (`" "` and `"-"`) must appear as a contiguous block of the input, starting at or after `floor`
  (the input index just past the block the previous hunk matched; 0 for the first hunk) and ending within the input.
- The expected position is the hunk's 0-based anchor (`old_start - 1` if `old_count > 0`, else `old_start`) plus
  the current offset (0 for the first hunk; afterwards the offset at which the previous hunk applied).
- The hunk applies at the matching position nearest to the expected one; of two matching positions equally near
  (one before and one after the expected position) the earlier wins. Its offset is that position minus its anchor.
- The block is replaced by the hunk's new lines (`" "` and `"+"`). If no position matches, `PatchError` is raised
  with `index` set to the hunk's 0-based index, and nothing is returned.

`offsets(lines, hunks)` returns each hunk's offset (raising `PatchError` like `apply_hunks`), `locate(source, old,
expected, floor=0)` the position rule above for one block (`None` if absent), `apply_patch(lines, text)` is
`apply_hunks(lines, parse_unified(text))`, and `reverse_patch(text)` formats the reversed hunks of `text` with the
two file names swapped.

For every `a`, `b` and `context >= 0`: `apply_patch(a, format_unified(a, b, context)) == b` and
`apply_hunks(b, reverse_hunks(make_hunks(a, b, context))) == a`.

## Check harness

`python3 harness.py` (from this directory) runs five stages of seeded checks, stops at the first failing one, and
logs every run to `.harness/runs.jsonl`.
