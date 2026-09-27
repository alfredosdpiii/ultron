"""Hidden checks for services/zeta (CSV normalization): SPEC cases plus randomized comparison with a reference."""
import csv
import io
import random
import re
import unittest
from decimal import ROUND_HALF_UP, Decimal

from csvnorm import normalize
from csvnorm.fields import normalize_amount

AMOUNT = re.compile(r"^(\()?(-)?\$?(\d{1,3}(?:,\d{3})+|\d+)(\.\d+)?(?(1)\))$")


def ref_amount(cell):
    text = cell.strip()
    if not text:
        return ""
    match = AMOUNT.match(text)
    if not match:
        raise ValueError(cell)
    value = Decimal(match.group(3).replace(",", "") + (match.group(4) or "")).quantize(Decimal("0.01"), rounding=ROUND_HALF_UP)
    if (match.group(1) or match.group(2)) and value != 0:
        value = -value
    return str(value)


def reference(text):
    if text.startswith("﻿"):
        text = text[1:]
    rows = [row for row in csv.reader(io.StringIO(text)) if any(cell.strip() for cell in row)]
    if not rows:
        return ""
    header = ["_".join(name.strip().lower().split()) for name in rows[0]]
    records, seen = [], set()
    for row in rows[1:]:
        row = (row + [""] * len(header))[: len(header)]
        record = []
        for name, cell in zip(header, row):
            cell = cell.strip()
            if name == "email":
                cell = cell.lower()
            elif name == "name":
                cell = " ".join(cell.split())
            elif name == "amount":
                cell = ref_amount(cell)
            record.append(cell)
        if "email" in header:
            email = record[header.index("email")]
            if email and email in seen:
                continue
            seen.add(email)
        records.append(record)
    out = io.StringIO()
    writer = csv.writer(out, lineterminator="\n")
    writer.writerow(header)
    writer.writerows(records)
    return out.getvalue()


def random_amount(rnd):
    if rnd.random() < 0.08:
        return ""
    whole = rnd.choice([0, rnd.randint(0, 99), rnd.randint(100, 999999)])
    frac = rnd.choice(["", ".5", ".05", ".50", ".125", ".995", ".004"])
    body = f"{whole:,}" if rnd.random() < 0.5 else str(whole)
    if rnd.random() < 0.3:
        body = "$" + body
    body += frac
    style = rnd.random()
    if style < 0.2:
        body = f"({body})"
    elif style < 0.35:
        body = "-" + body
    return rnd.choice(["", " ", "  "]) + body + rnd.choice(["", " "])


def random_email(rnd):
    local = rnd.choice(["ada", "bob", "cy", "dee", "eve"])
    email = f"{local}@{rnd.choice(['x.io', 'Example.com'])}"
    return "".join(c.upper() if rnd.random() < 0.3 else c for c in email)


def random_csv(rnd):
    columns = rnd.sample(["name", "email", "amount", "note"], rnd.randint(1, 4))
    spelled = [rnd.choice(["", " "]) + "  ".join(w.capitalize() if rnd.random() < 0.5 else w.upper() for w in c.split()) + rnd.choice(["", "  "]) for c in columns]
    if "note" in columns and rnd.random() < 0.5:
        spelled[columns.index("note")] = " Internal   Note "
    out = io.StringIO()
    writer = csv.writer(out, lineterminator=rnd.choice(["\n", "\r\n"]))
    writer.writerow(spelled)
    for _ in range(rnd.randint(0, 12)):
        if rnd.random() < 0.1:
            writer.writerow([" "] * rnd.randint(0, len(columns)))
            continue
        row = []
        for column in columns:
            if column == "name":
                row.append(rnd.choice(["  Ada  Lovelace", "Bob\tBuilder ", "Cy", ""]))
            elif column == "email":
                row.append(random_email(rnd) if rnd.random() < 0.9 else " ")
            elif column == "amount":
                row.append(random_amount(rnd))
            else:
                row.append(rnd.choice(["vip", " late, twice ", ""]))
        if rnd.random() < 0.1:
            row = row[: rnd.randint(0, len(row))]
        elif rnd.random() < 0.1:
            row = row + ["extra"]
        writer.writerow(row)
    text = out.getvalue()
    return ("﻿" + text) if rnd.random() < 0.3 else text


class ZetaHidden(unittest.TestCase):
    def test_amounts(self):
        for cell, want in [("(12.00)", "-12.00"), ("($1,200.00)", "-1200.00"), ("-$3", "-3.00"), ("1,234.5", "1234.50"),
                           ("0.005", "0.01"), ("2.675", "2.68"), ("(0.00)", "0.00"), ("-0.001", "0.00"), (" 7 ", "7.00"), ("", "")]:
            self.assertEqual(normalize_amount(cell), want, cell)
        for bad in ["abc", "1.2.3", "--3", "$", "()"]:
            with self.assertRaises(ValueError, msg=bad):
                normalize_amount(bad)

    def test_bom(self):
        self.assertEqual(normalize("﻿Email,Name\nA@x.io,Ann\n"), "email,name\na@x.io,Ann\n")

    def test_duplicates(self):
        text = "email,amount\nb@x.io,1\n,2\nB@X.io,3\n,4\na@x.io,5\n"
        self.assertEqual(normalize(text), "email,amount\nb@x.io,1.00\n,2.00\n,4.00\na@x.io,5.00\n")

    def test_random_against_reference(self):
        for seed in range(1500):
            rnd = random.Random(seed)
            text = random_csv(rnd)
            self.assertEqual(normalize(text), reference(text), f"seed {seed}: {text!r}")


if __name__ == "__main__":
    unittest.main()
