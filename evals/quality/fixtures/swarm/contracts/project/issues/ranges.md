# ranges: overlapping ranges are not merged

`merge_ranges([(5, 8), (1, 6)])` returns both ranges unmerged. Expected behaviour:

- The result is sorted and disjoint whatever the input order: `merge_ranges([(5, 8), (1, 6)]) == [(1, 8)]`.
- Ends are inclusive, so ranges that touch or are adjacent merge: `[(1, 3), (3, 5)] -> [(1, 5)]` and
  `[(1, 3), (4, 5)] -> [(1, 5)]` (no integer lies between 3 and 4); `[(1, 3), (5, 6)]` stays two ranges.
- A range inside another disappears into it; the input list is not modified; `[]` gives `[]`.
- A range with start greater than end raises `ValueError`.
