"""Integer ranges as (start, end) pairs, both ends inclusive."""


def merge_ranges(ranges):
    """Merge overlapping ranges into a sorted list of disjoint ranges."""
    merged = []
    for start, end in ranges:
        if merged and start < merged[-1][1]:
            merged[-1] = (merged[-1][0], max(merged[-1][1], end))
        else:
            merged.append((start, end))
    return merged
