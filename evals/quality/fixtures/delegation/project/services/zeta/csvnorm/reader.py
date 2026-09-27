"""CSV text to a normalized header and raw rows."""
import csv
import io


def normalize_header(name):
    return "_".join(name.strip().lower().split())


def read_rows(text):
    rows = [row for row in csv.reader(io.StringIO(text)) if any(cell.strip() for cell in row)]
    if not rows:
        return [], []
    header = [normalize_header(name) for name in rows[0]]
    body = [(row + [""] * len(header))[: len(header)] for row in rows[1:]]
    return header, body
