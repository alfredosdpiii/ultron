/**
 * Reference solutions for tasks-hard.mjs, used only by `eval-quality.mjs --tasks hard --self-check`
 * to prove every task is solvable and every hidden check accepts a correct answer (and, with
 * several solution files, that each file's fix is required). Never shown to the agents.
 *
 * Each entry: `files` to write over the task files, `remove` paths to delete, `run` a shell command
 * executed in the task directory after writing (data tasks compute their answers with an
 * independent Python implementation, which cross-checks the generator's expected values).
 */

const code = (strings, ...values) => String.raw(strings, ...values).replace(/^\n/, "");

const python = (source) => ({ files: { "_solution.py": source }, run: "python3 _solution.py" });

export const solutions = {
	"bugs-invoicing": {
		files: {
			"invoicing/money.py": code`
from decimal import ROUND_HALF_UP, Decimal


def to_cents(amount):
    """Round a Decimal amount in EUR to integer cents, half-up."""
    return int((Decimal(amount) * 100).quantize(Decimal(1), rounding=ROUND_HALF_UP))


def fmt(cents):
    """Render integer cents as 1,234.56 (negative: -1,234.56)."""
    sign = "-" if cents < 0 else ""
    whole, frac = divmod(abs(cents), 100)
    return f"{sign}{whole:,}.{frac:02d}"
`,
			"invoicing/parse.py": code`
import re
from dataclasses import dataclass
from decimal import Decimal

LINE = re.compile(r"^\s*([A-Za-z0-9-]+)\s+x(\d+(?:_\d+)*)\s+@(\d+(?:\.\d{1,4})?)(?:\s+([A-Za-z]+))?\s*$")
UNITS = {"ea", "kg", "g", "lb"}


@dataclass
class Line:
    sku: str
    qty: int
    price: Decimal
    unit: str

    @property
    def category(self):
        return self.sku.split("-", 1)[0]


def parse_line(text):
    match = LINE.match(text)
    if not match:
        raise ValueError(f"bad order line: {text!r}")
    sku, qty, price, unit = match.groups()
    unit = (unit or "ea").lower()
    if unit not in UNITS:
        raise ValueError(f"unknown unit: {unit}")
    qty = int(qty.replace("_", ""))
    if qty <= 0:
        raise ValueError("quantity must be positive")
    return Line(sku.upper(), qty, Decimal(price), unit)


def parse_order(text):
    lines = []
    for raw in text.splitlines():
        stripped = raw.strip()
        if not stripped or stripped.startswith("#"):
            continue
        lines.append(parse_line(raw))
    return lines
`,
			"invoicing/units.py": code`
from decimal import Decimal

KG_PER_UNIT = {"ea": Decimal(0), "kg": Decimal(1), "g": Decimal("0.001"), "lb": Decimal("0.45359237")}


def weight_kg(line):
    return line.qty * KG_PER_UNIT[line.unit]
`,
			"invoicing/pricing.py": code`
from decimal import Decimal

TIERS = [(100000, Decimal("0.15")), (50000, Decimal("0.10")), (10000, Decimal("0.05"))]


def discount_rate(subtotal_cents):
    for threshold, rate in TIERS:
        if subtotal_cents >= threshold:
            return rate
    return Decimal(0)
`,
			"invoicing/tax.py": code`
from decimal import Decimal

from .money import to_cents

EXEMPT = {"BOOK", "FOOD"}
VAT = Decimal("0.20")


def tax_cents(lines, amounts, rate):
    taxable = sum(amount for line, amount in zip(lines, amounts) if line.category not in EXEMPT)
    return to_cents(Decimal(taxable) / 100 * (1 - rate) * VAT)
`,
			"invoicing/shipping.py": code`
import math


def shipping_cents(weight_kg):
    if weight_kg == 0:
        return 0
    if weight_kg <= 1:
        return 490
    if weight_kg <= 10:
        return 990
    return 990 + 50 * math.ceil(weight_kg - 10)
`,
		},
	},
	"bugs-scheduler": {
		files: {
			"sched/days.js": code`
const NAMES = {
	sun: 0, mon: 1, tue: 2, wed: 3, thu: 4, fri: 5, sat: 6,
	sunday: 0, monday: 1, tuesday: 2, wednesday: 3, thursday: 4, friday: 5, saturday: 6,
};

function dayNumber(name) {
	const key = name.trim().toLowerCase();
	if (!Object.hasOwn(NAMES, key)) throw new RangeError("unknown day: " + name);
	return NAMES[key];
}

function parseDays(text) {
	if (text === "*") return new Set([0, 1, 2, 3, 4, 5, 6]);
	const days = new Set();
	for (const item of text.split(",")) {
		const parts = item.split("-");
		if (parts.length > 2) throw new RangeError("bad day range: " + item);
		const first = dayNumber(parts[0]);
		if (parts.length === 1) {
			days.add(first);
			continue;
		}
		const last = dayNumber(parts[1]);
		for (let day = first; ; day = (day + 1) % 7) {
			days.add(day);
			if (day === last) break;
		}
	}
	return days;
}

module.exports = { parseDays };
`,
			"sched/clock.js": code`
function parseClock(text) {
	const match = /^(\d{1,2}):(\d{2})$/.exec(text);
	if (!match) throw new RangeError("bad time: " + text);
	const hours = Number(match[1]);
	const minutes = Number(match[2]);
	if (hours > 23 || minutes > 59) throw new RangeError("bad time: " + text);
	return hours * 60 + minutes;
}

module.exports = { parseClock };
`,
			"sched/offset.js": code`
function parseOffset(text) {
	const match = /^([+-])(\d{2}):(\d{2})$/.exec(text);
	if (!match) throw new RangeError("bad offset: " + text);
	const sign = match[1] === "-" ? -1 : 1;
	return sign * (Number(match[2]) * 60 + Number(match[3]));
}

module.exports = { parseOffset };
`,
			"sched/next.js": code`
const { parseClock } = require("./clock");
const { parseDays } = require("./days");
const { parseOffset } = require("./offset");

const DAY = 86400000;

function nextRun(spec, from, offset = "+00:00") {
	const [time, days] = spec.trim().split(/\s+/);
	const minutes = parseClock(time);
	const allowed = parseDays(days);
	const shift = parseOffset(offset) * 60000;
	const local = new Date(from).getTime() + shift;
	const midnight = Math.floor(local / DAY) * DAY;
	for (let day = 0; day <= 7; day += 1) {
		const candidate = midnight + day * DAY + minutes * 60000;
		if (candidate > local && allowed.has(new Date(candidate).getUTCDay())) return new Date(candidate - shift).toISOString();
	}
	throw new Error("unreachable");
}

module.exports = { nextRun };
`,
			"sched/duration.js": code`
const UNITS = { d: 86400000, h: 3600000, m: 60000, s: 1000 };

function parseDuration(text) {
	const match = /^(?:(\d+)d)?(?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s)?$/.exec(text);
	if (!match || text === "") throw new RangeError("bad duration: " + text);
	const [, d, h, m, s] = match;
	return Number(d ?? 0) * UNITS.d + Number(h ?? 0) * UNITS.h + Number(m ?? 0) * UNITS.m + Number(s ?? 0) * UNITS.s;
}

module.exports = { parseDuration };
`,
		},
	},
	"bugs-ttl-cache": {
		files: {
			"cache.py": code`
"""A TTL + LRU cache (see the task's original docstring for the specification)."""

import time
from collections import OrderedDict


class TTLCache:
    def __init__(self, capacity, ttl, clock=time.monotonic):
        if capacity < 1:
            raise ValueError("capacity must be at least 1")
        self.capacity = capacity
        self.ttl = ttl
        self.clock = clock
        self._items = OrderedDict()
        self._stats = {"hits": 0, "misses": 0, "evictions": 0, "expirations": 0}

    def _expired(self, inserted_at, now):
        return now - inserted_at >= self.ttl

    def get(self, key, default=None):
        now = self.clock()
        if key not in self._items:
            self._stats["misses"] += 1
            return default
        value, inserted_at = self._items[key]
        if self._expired(inserted_at, now):
            del self._items[key]
            self._stats["expirations"] += 1
            self._stats["misses"] += 1
            return default
        self._items.move_to_end(key)
        self._stats["hits"] += 1
        return value

    def put(self, key, value):
        now = self.clock()
        if key in self._items:
            self._items[key] = (value, now)
            self._items.move_to_end(key)
            return
        if len(self._items) >= self.capacity:
            for stale in [k for k, (_, at) in self._items.items() if self._expired(at, now)]:
                del self._items[stale]
                self._stats["expirations"] += 1
        if len(self._items) >= self.capacity:
            self._items.popitem(last=False)
            self._stats["evictions"] += 1
        self._items[key] = (value, now)

    def delete(self, key):
        entry = self._items.pop(key, None)
        return entry is not None and not self._expired(entry[1], self.clock())

    def __len__(self):
        now = self.clock()
        return sum(1 for _, at in self._items.values() if not self._expired(at, now))

    def __contains__(self, key):
        return key in self._items and not self._expired(self._items[key][1], self.clock())

    def stats(self):
        return dict(self._stats)
`,
		},
	},
	"refactor-intervals-fast": {
		files: {
			"intervals.py": code`
"""Interval utilities. Endpoints are numbers (ints or floats) and may be given in either order."""

from bisect import bisect_left, bisect_right


def normalize(intervals):
    """Merge intervals that overlap or lie within 1 of each other; return sorted (start, end) tuples."""
    merged = []
    for a, b in sorted((min(a, b), max(a, b)) for a, b in intervals):
        if merged and a <= merged[-1][1] + 1:
            if b > merged[-1][1]:
                merged[-1][1] = b
        else:
            merged.append([a, b])
    return [(a, b) for a, b in merged]


def covered(intervals):
    """Total length of normalize(intervals)."""
    return sum(b - a for a, b in normalize(intervals))


def overlaps(intervals, points):
    """For each point, how many of the given intervals contain it (endpoints inclusive)."""
    spans = [(min(a, b), max(a, b)) for a, b in intervals]
    starts = sorted(a for a, _ in spans)
    ends = sorted(b for _, b in spans)
    return [bisect_right(starts, p) - bisect_left(ends, p) for p in points]
`,
		},
	},
	"refactor-shop-package": {
		remove: ["shop.py"],
		files: {
			"shop/__init__.py": code`
"""Shop domain logic: money helpers, carts, promotions and receipts."""

from .cart import Cart, Item
from .money import CURRENCY, cents, format_cents
from .promotions import PROMOTIONS, bulk_three, free_shipping, promotion, ten_off
from .receipts import receipt

__all__ = [
    "CURRENCY",
    "PROMOTIONS",
    "cents",
    "format_cents",
    "promotion",
    "Item",
    "Cart",
    "ten_off",
    "bulk_three",
    "free_shipping",
    "receipt",
]
`,
			"shop/money.py": code`
from decimal import ROUND_HALF_EVEN, Decimal

CURRENCY = "EUR"


def cents(amount):
    """Convert an amount in currency units (number or string) to integer cents, half-even."""
    return int((Decimal(str(amount)) * 100).quantize(Decimal(1), rounding=ROUND_HALF_EVEN))


def format_cents(value):
    sign = "-" if value < 0 else ""
    whole, frac = divmod(abs(value), 100)
    return f"{sign}{CURRENCY} {whole}.{frac:02d}"
`,
			"shop/promotions.py": code`
PROMOTIONS = {}


def promotion(name):
    """Register a promotion: a function cart -> discount in cents."""

    def register(function):
        PROMOTIONS[name] = function
        return function

    return register


@promotion("TENOFF")
def ten_off(cart):
    return cart.subtotal() // 10


@promotion("BULK3")
def bulk_three(cart):
    return sum(item.price_cents for item in cart.items if item.qty >= 3)


@promotion("FREESHIP")
def free_shipping(cart):
    return 499 if cart.subtotal() >= 5000 else 0
`,
			"shop/cart.py": code`
from dataclasses import dataclass, field

from .money import cents
from .promotions import PROMOTIONS


@dataclass
class Item:
    sku: str
    price_cents: int
    qty: int = 1


@dataclass
class Cart:
    items: list = field(default_factory=list)
    codes: list = field(default_factory=list)

    def add(self, sku, price, qty=1):
        for item in self.items:
            if item.sku == sku and item.price_cents == cents(price):
                item.qty += qty
                return self
        self.items.append(Item(sku, cents(price), qty))
        return self

    def apply(self, code):
        if code not in PROMOTIONS:
            raise KeyError(code)
        if code not in self.codes:
            self.codes.append(code)
        return self

    def subtotal(self):
        return sum(item.price_cents * item.qty for item in self.items)

    def discount(self):
        return min(self.subtotal(), sum(PROMOTIONS[code](self) for code in self.codes))

    def total(self):
        return self.subtotal() - self.discount()
`,
			"shop/receipts.py": code`
from .money import format_cents
from .promotions import PROMOTIONS


def receipt(cart):
    lines = [f"{item.qty} x {item.sku} @ {format_cents(item.price_cents)}" for item in sorted(cart.items, key=lambda i: i.sku)]
    lines.append(f"subtotal {format_cents(cart.subtotal())}")
    for code in cart.codes:
        lines.append(f"promo {code} -{format_cents(PROMOTIONS[code](cart))}")
    lines.append(f"total {format_cents(cart.total())}")
    return "\n".join(lines)
`,
		},
	},
	"refactor-streaming-records": {
		files: {
			"records.py": code`
"""Reader for the .rec format (streaming). See the original docstring for the format."""


def _lines(path):
    # Same text-mode (universal newline) reading as the original, one line at a time.
    with open(path, encoding="utf-8") as handle:
        for raw in handle:
            yield raw[:-1] if raw.endswith("\n") else raw


def iter_records(path):
    current = None
    last_key = None
    for line in _lines(path):
        if line.startswith("%% record"):
            if current is not None:
                yield current
            current = {}
            last_key = None
            continue
        if line.startswith("%% end"):
            if current is not None:
                yield current
            current = None
            continue
        if current is None or not line.strip() or line.startswith("#"):
            continue
        if line[0] in " \t":
            if last_key is not None:
                value = current[last_key]
                if isinstance(value, list):
                    value[-1] += "\n" + line.lstrip()
                else:
                    current[last_key] = value + "\n" + line.lstrip()
            continue
        key, sep, value = line.partition(":")
        if not sep:
            continue
        key = key.strip()
        value = value.strip()
        if key in current:
            existing = current[key]
            current[key] = existing + [value] if isinstance(existing, list) else [existing, value]
        else:
            current[key] = value
        last_key = key
    if current is not None:
        yield current


def parse_file(path):
    return list(iter_records(path))


def summarize(path):
    """Return (number of records, {key: number of records containing key})."""
    total = 0
    counts = {}
    for record in iter_records(path):
        total += 1
        for key in record:
            counts[key] = counts.get(key, 0) + 1
    return total, counts
`,
		},
	},
	"data-sessions": python(code`
import json
import statistics
from collections import defaultdict
from datetime import datetime

seen = set()
times = defaultdict(list)
hours = [0] * 24
with open("events.jsonl", encoding="utf-8") as handle:
    for line in handle:
        event = json.loads(line)
        if event["event_id"] in seen:
            continue
        seen.add(event["event_id"])
        instant = int(datetime.fromisoformat(event["ts"]).timestamp())
        times[event["user"]].append(instant)
        hours[instant // 3600 % 24] += 1
durations = []
sessions = {}
for user, stamps in times.items():
    stamps.sort()
    start = stamps[0]
    count = 1
    for previous, current in zip(stamps, stamps[1:]):
        if current - previous > 1800:
            durations.append(previous - start)
            start = current
            count += 1
    durations.append(stamps[-1] - start)
    sessions[user] = count
top = min(sessions, key=lambda user: (-sessions[user], user))
busiest = min(range(24), key=lambda hour: (-hours[hour], hour))
json.dump(
    {"total_sessions": len(durations), "top_user": top, "median_session_seconds": statistics.median(durations), "busiest_hour_utc": busiest},
    open("answer.json", "w"),
)
`),
	"data-ledger-reconcile": python(code`
import bisect
import csv
import json
from collections import defaultdict

rates = defaultdict(list)
with open("rates.csv", newline="") as handle:
    for row in csv.DictReader(handle):
        rates[row["currency"]].append((row["date"], float(row["eur_per_unit"])))
for series in rates.values():
    series.sort()
dates = {currency: [date for date, _ in series] for currency, series in rates.items()}


def rate(currency, date):
    if currency == "EUR":
        return 1.0
    index = bisect.bisect_right(dates[currency], date) - 1
    return rates[currency][index][1]


ledger = {}
with open("ledger.csv", newline="") as handle:
    for row in csv.DictReader(handle):
        ledger[row["ref"].strip().upper()] = float(row["amount_eur"])
matched = unmatched_bank = 0
total = 0.0
largest = (-1.0, None)
used = set()
with open("bank.csv", newline="") as handle:
    for row in csv.DictReader(handle):
        ref = row["reference"].strip().upper()
        eur = float(row["amount"]) * rate(row["currency"], row["date"])
        if ref in ledger and abs(eur - ledger[ref]) <= 0.01:
            matched += 1
            used.add(ref)
            continue
        unmatched_bank += 1
        total += eur
        if ref in ledger:
            diff = abs(eur - ledger[ref])
            if diff > largest[0]:
                largest = (diff, row["reference"])
json.dump(
    {
        "matched": matched,
        "unmatched_bank": unmatched_bank,
        "unmatched_ledger": len(ledger) - len(used),
        "unmatched_bank_total_eur": round(total, 2),
        "largest_mismatch_ref": largest[1],
    },
    open("answer.json", "w"),
)
`),
	"data-sensors": python(code`
import csv
import glob
import json
from collections import defaultdict
from datetime import datetime

with open("meta.csv", newline="") as handle:
    site_of = {row["sensor_id"]: row["site"] for row in csv.DictReader(handle)}
sums = defaultdict(float)
counts = defaultdict(int)
above = 0
times = defaultdict(list)
for path in sorted(glob.glob("sensors/*.csv")):
    with open(path, newline="") as handle:
        header = handle.readline()
        handle.seek(0)
        for row in csv.DictReader(handle, delimiter=";" if ";" in header else ","):
            sensor = row["sensor"]
            if "epoch" in row:
                times[sensor].append(int(row["epoch"]))
                raw = row["temp_f"]
                celsius = None if raw in ("", "NA") else (float(raw) - 32) * 5 / 9
            else:
                times[sensor].append(int(datetime.fromisoformat(row["timestamp"]).timestamp()))
                raw = row["temp_c"]
                celsius = None if raw in ("", "NA") else float(raw)
            if celsius is None:
                continue
            sums[site_of[sensor]] += celsius
            counts[site_of[sensor]] += 1
            if celsius > 40.0:
                above += 1
hottest = max(sums, key=lambda site: sums[site] / counts[site])
gaps = {}
for sensor, stamps in times.items():
    stamps.sort()
    gaps[sensor] = max(b - a for a, b in zip(stamps, stamps[1:]))
worst = min(gaps, key=lambda sensor: (-gaps[sensor], sensor))
json.dump(
    {
        "hottest_site": hottest,
        "hottest_site_mean_c": sums[hottest] / counts[hottest],
        "readings_above_40c": above,
        "max_gap_sensor": worst,
        "max_gap_seconds": gaps[worst],
    },
    open("answer.json", "w"),
)
`),
	"data-dependencies": python(code`
import json
from collections import Counter, deque

latest = {}
with open("packages.jsonl", encoding="utf-8") as handle:
    for line in handle:
        package = json.loads(line)
        version = tuple(int(part) for part in package["version"].split("."))
        if package["name"] not in latest or version > latest[package["name"]][0]:
            latest[package["name"]] = (version, package["deps"])
graph = {name: sorted(set(deps)) for name, (_, deps) in latest.items()}

depth = {"app-root": 0}
queue = deque(["app-root"])
while queue:
    node = queue.popleft()
    for dep in graph.get(node, ()):
        if dep not in depth:
            depth[dep] = depth[node] + 1
            queue.append(dep)

# Kosaraju, iteratively: finish order on the graph, then components on the reverse graph.
order, visited = [], set()
for start in graph:
    if start in visited:
        continue
    visited.add(start)
    stack = [(start, iter(graph[start]))]
    while stack:
        node, edges = stack[-1]
        for dep in edges:
            if dep not in visited:
                visited.add(dep)
                stack.append((dep, iter(graph.get(dep, ()))))
                break
        else:
            stack.pop()
            order.append(node)
reverse = {name: [] for name in graph}
for name, deps in graph.items():
    for dep in deps:
        reverse.setdefault(dep, []).append(name)
assigned, cyclic = set(), 0
for start in reversed(order):
    if start in assigned:
        continue
    component, stack = [], [start]
    assigned.add(start)
    while stack:
        node = stack.pop()
        component.append(node)
        for parent in reverse.get(node, ()):
            if parent not in assigned:
                assigned.add(parent)
                stack.append(parent)
    if len(component) > 1 or start in graph.get(start, ()):
        cyclic += len(component)

dependents = Counter(dep for deps in graph.values() for dep in deps)
most = min(dependents, key=lambda name: (-dependents[name], name))
json.dump(
    {"closure_size": len(depth) - 1, "max_depth": max(depth.values()), "cycle_packages": cyclic, "most_depended_on": most},
    open("answer.json", "w"),
)
`),
	"logs-revoked-tokens": python(code`
import csv
import json
import re
from collections import Counter
from datetime import datetime

revoked = {}
with open("logs/revocations.csv", newline="") as handle:
    for row in csv.DictReader(handle):
        at = datetime.fromisoformat(row["revoked_at"]).timestamp() * 1000
        revoked[row["token"]] = min(at, revoked.get(row["token"], at))
gateway_time = {}
with open("logs/gateway.log") as handle:
    for line in handle:
        stamp, _, request = line.split()[:3]
        gateway_time[request[len("req="):]] = datetime.fromisoformat(stamp).timestamp()
failed = Counter()
pattern = re.compile(r" write req=(\S+) .*status=FAILED")
with open("logs/db.log") as handle:
    for line in handle:
        match = pattern.search(line)
        if match:
            failed[match.group(1)] += 1
hits = []
with open("logs/auth.jsonl") as handle:
    for line in handle:
        entry = json.loads(line)
        at = revoked.get(entry["token"])
        if entry["result"] == "ok" and at is not None and at <= entry["t"]:
            hits.append(entry)
by_user = Counter()
for entry in hits:
    if failed[entry["request"]]:
        by_user[entry["user"]] += failed[entry["request"]]
json.dump(
    {
        "revoked_requests": len(hits),
        "failed_writes": sum(failed[entry["request"]] for entry in hits),
        "top_user": min(by_user, key=lambda user: (-by_user[user], user)),
        "first_request": min(hits, key=lambda entry: gateway_time[entry["request"]])["request"],
    },
    open("answer.json", "w"),
)
`),
	"logs-deploy-asof": python(code`
import bisect
import glob
import json
import os
import re
from collections import Counter, defaultdict
from datetime import datetime

events = defaultdict(list)
with open("deploys.log") as handle:
    for line in handle:
        stamp, rest = line.split(" ", 1)
        fields = dict(re.findall(r"(\w+)=(\S+)", rest))
        events[fields["host"]].append((datetime.fromisoformat(stamp).timestamp(), fields["action"], fields.get("version")))
served, errors = Counter(), Counter()
unversioned = total_5xx = 0
for path in sorted(glob.glob("access/*.log")):
    host = os.path.basename(path)[: -len(".log")]
    history, times, states = [], [], []
    for at, action, version in sorted(events[host], key=lambda event: event[0]):
        if action == "deploy":
            history.append(version)
        else:
            history.pop()
        times.append(at)
        states.append(history[-1] if history else None)
    with open(path) as handle:
        for line in handle:
            stamp, _, _, status, _ = line.split()
            index = bisect.bisect_right(times, float(stamp)) - 1
            version = states[index] if index >= 0 else None
            bad = 500 <= int(status) <= 599
            total_5xx += bad
            if version is None:
                unversioned += 1
            else:
                served[version] += 1
                errors[version] += bad
worst = max(served, key=lambda version: errors[version] / served[version])
json.dump(
    {
        "worst_version": worst,
        "worst_version_error_rate": errors[worst] / served[worst],
        "most_served_version": max(served, key=served.get),
        "unversioned_requests": unversioned,
        "total_5xx": total_5xx,
    },
    open("answer.json", "w"),
)
`),
	"logs-bruteforce": python(code`
import bisect
import glob
import gzip
import json
import re
from collections import defaultdict
from datetime import datetime, timezone

line_re = re.compile(r'^(\S+) \S+ \S+ \[([^\]]+)\] "\S+ (\S+) [^"]*" (\d{3}) ')
events = []
for path in glob.glob("logs/access.log*"):
    opener = gzip.open if path.endswith(".gz") else open
    with opener(path, "rt") as handle:
        for line in handle:
            match = line_re.match(line)
            ip, stamp, target, status = match.groups()
            path_only = target.split("?", 1)[0]
            if path_only != "/admin" and not path_only.startswith("/admin/"):
                continue
            events.append((datetime.strptime(stamp, "%d/%b/%Y:%H:%M:%S %z").timestamp(), ip, int(status)))
events.sort()
failures = defaultdict(list)
for at, ip, status in events:
    if status == 401:
        failures[ip].append(at)
for stamps in failures.values():
    stamps.sort()
breached, count, first = set(), 0, None
for at, ip, status in events:
    if status != 200:
        continue
    stamps = failures.get(ip, [])
    if bisect.bisect_left(stamps, at) - bisect.bisect_left(stamps, at - 600) >= 5:
        breached.add(ip)
        count += 1
        first = at if first is None else min(first, at)
json.dump(
    {
        "breached_ips": sorted(breached),
        "breach_events": count,
        "first_breach_utc": datetime.fromtimestamp(first, timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    },
    open("answer.json", "w"),
)
`),
	"huge-customer-dedupe": python(code`
import csv
from datetime import datetime

best = {}
with open("customers.csv", newline="", encoding="utf-8") as handle:
    reader = csv.reader(handle)
    header = next(reader)
    for row in reader:
        key = row[1].strip().lower()
        instant = datetime.fromisoformat(row[4]).timestamp()
        if key not in best or instant >= best[key][0]:
            best[key] = (instant, row)
with open("dedup.csv", "w", newline="", encoding="utf-8") as handle:
    writer = csv.writer(handle)
    writer.writerow(header)
    for key in sorted(best):
        row = list(best[key][1])
        row[1] = key
        writer.writerow(row)
`),
	"huge-catalog-diff": python(code`
import json

old = {item["id"]: item for item in json.load(open("catalog/old.json"))["items"]}
new = {item["id"]: item for item in json.load(open("catalog/new.json"))["items"]}


def value(item, field):
    data = item.get(field)
    return sorted(data) if field == "tags" and isinstance(data, list) else data


changed = {}
for key in old.keys() & new.keys():
    fields = sorted(field for field in set(old[key]) | set(new[key]) if value(old[key], field) != value(new[key], field))
    if fields:
        changed[key] = fields
json.dump({"added": sorted(new.keys() - old.keys()), "removed": sorted(old.keys() - new.keys()), "changed": changed}, open("diff.json", "w"))
`),
};
