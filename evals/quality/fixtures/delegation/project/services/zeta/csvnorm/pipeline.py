"""normalize(): read, clean every cell, de-duplicate, write."""
import csv
import io

from .dedupe import dedupe
from .fields import NORMALIZERS
from .reader import read_rows


def normalize(text):
    header, rows = read_rows(text)
    records = []
    for row in rows:
        record = {}
        for name, cell in zip(header, row):
            clean = NORMALIZERS.get(name)
            record[name] = clean(cell) if clean else cell.strip()
        records.append(record)
    if "email" in header:
        records = dedupe(records, "email")
    out = io.StringIO()
    writer = csv.writer(out, lineterminator="\n")
    if header:
        writer.writerow(header)
    for record in records:
        writer.writerow([record[name] for name in header])
    return out.getvalue()
