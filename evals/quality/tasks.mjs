/**
 * Frozen quality tasks for the baseline (stock Pi) vs candidate (Ultron) comparison.
 *
 * Each task: `files` seed the working copy, `prompts` are sent in order (later prompts model a
 * user correction), `hidden` files are copied in only after the agent finishes, and `verify` is
 * the shell command whose exit code decides pass/fail. Generated data is deterministic.
 * Do not edit a task after measurements exist: add a new id instead (see FROZEN_AT).
 */

export const FROZEN_AT = "2026-09-25";

function rng(seed) {
	let state = seed >>> 0;
	return () => {
		state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
		return state / 2 ** 32;
	};
}

function salesCsv() {
	const random = rng(11);
	const regions = ["north", "south", "east", "west", "central"];
	const totals = Object.fromEntries(regions.map((region) => [region, 0]));
	const rows = ["order_id,region,amount_cents"];
	for (let index = 0; index < 300_000; index += 1) {
		const region = regions[Math.floor(random() * regions.length)];
		const amount = 100 + Math.floor(random() * 50_000) + (region === "east" ? 40 : 0);
		totals[region] += amount;
		rows.push(`${index},${region},${amount}`);
	}
	const [top, cents] = Object.entries(totals).sort((a, b) => b[1] - a[1])[0];
	return { csv: `${rows.join("\n")}\n`, answer: `${top} ${cents}` };
}

function eventsJsonl() {
	const random = rng(22);
	const counts = new Map();
	const lines = [];
	for (let index = 0; index < 200_000; index += 1) {
		const user = `u${Math.floor(random() * 40_000)}`;
		counts.set(user, (counts.get(user) ?? 0) + 1);
		lines.push(JSON.stringify({ ts: 1_700_000_000 + index, user, kind: random() < 0.5 ? "view" : "click" }));
	}
	const heavy = [...counts.values()].filter((count) => count > 5).length;
	return { jsonl: `${lines.join("\n")}\n`, answer: String(heavy) };
}

function errorLog() {
	const random = rng(33);
	const lines = [];
	let run = 0;
	let best = 0;
	for (let index = 0; index < 150_000; index += 1) {
		const error = random() < (index > 90_000 && index < 90_040 ? 0.97 : 0.3);
		run = error ? run + 1 : 0;
		best = Math.max(best, run);
		lines.push(`2026-01-01T00:00:${String(index % 60).padStart(2, "0")} ${error ? "ERROR" : "INFO"} request ${index}`);
	}
	return { log: `${lines.join("\n")}\n`, answer: String(best) };
}

function latencyCsv() {
	const random = rng(44);
	const endpoints = ["/login", "/search", "/checkout", "/profile"];
	const samples = [];
	const rows = ["endpoint,latency_ms"];
	for (let index = 0; index < 250_000; index += 1) {
		const endpoint = endpoints[Math.floor(random() * endpoints.length)];
		const latency = Math.floor(5 + random() * 900 + (endpoint === "/checkout" ? 120 : 0));
		if (endpoint === "/checkout") samples.push(latency);
		rows.push(`${endpoint},${latency}`);
	}
	samples.sort((a, b) => a - b);
	const middle = samples.length / 2;
	const median = samples.length % 2 ? samples[Math.floor(middle)] : (samples[middle - 1] + samples[middle]) / 2;
	return { csv: `${rows.join("\n")}\n`, answer: String(median) };
}

function configFiles() {
	const random = rng(55);
	const keys = ["retries", "timeout", "region", "verbose", "cache", "workers", "endpoint"];
	const counts = Object.fromEntries(keys.map((key) => [key, 0]));
	const files = {};
	for (let index = 0; index < 500; index += 1) {
		const config = {};
		for (const key of keys) {
			const weight = key === "workers" ? 0.83 : 0.6;
			if (random() < weight) {
				config[key] = Math.floor(random() * 100);
				counts[key] += 1;
			}
		}
		files[`configs/service-${index}.json`] = `${JSON.stringify(config)}\n`;
	}
	const [top] = Object.entries(counts).sort((a, b) => b[1] - a[1])[0];
	return { files, answer: top };
}

const py = (source) => `${source.trim()}\n`;

export function tasks() {
	const sales = salesCsv();
	const events = eventsJsonl();
	const errors = errorLog();
	const latency = latencyCsv();
	const configs = configFiles();
	const answerCheck = (expected) =>
		`python3 -c "import sys; got=open('answer.txt').read().split(); exp='${expected}'.split(); sys.exit(0 if got==exp else 1)"`;
	return [
		{
			id: "edit-average",
			category: "small-edit",
			files: { "calc.py": py("def average(values):\n    return sum(values) / (len(values) - 1)") },
			prompts: ["calc.py has a bug in average(). Fix it."],
			hidden: { "test_hidden.py": py("from calc import average\nassert average([2, 4, 6]) == 4\nassert average([5]) == 5") },
			verify: "python3 test_hidden.py",
		},
		{
			id: "edit-slugify",
			category: "small-edit",
			files: {
				"text.py": py(
					'def slugify(title):\n    """Lowercase, trim, replace runs of non-alphanumeric characters with a single hyphen, strip leading/trailing hyphens."""\n    raise NotImplementedError',
				),
			},
			prompts: ["Implement slugify in text.py as its docstring describes."],
			hidden: {
				"test_hidden.py": py(
					'from text import slugify\nassert slugify("  Hello, World!  ") == "hello-world"\nassert slugify("a--b__c") == "a-b-c"\nassert slugify("---") == ""',
				),
			},
			verify: "python3 test_hidden.py",
		},
		{
			id: "edit-rename",
			category: "small-edit",
			files: {
				"users.py": py("def get_usr(user_id):\n    return {'id': user_id}"),
				"api.py": py("from users import get_usr\n\ndef handler(i):\n    return get_usr(i)"),
				"jobs.py": py("import users\n\ndef nightly():\n    return users.get_usr(0)"),
			},
			prompts: ["Rename get_usr to get_user everywhere in this project. No alias for the old name."],
			hidden: {
				"test_hidden.py": py(
					"import users, api, jobs, pathlib\nassert users.get_user(3) == {'id': 3}\nassert api.handler(1) == {'id': 1}\nassert jobs.nightly() == {'id': 0}\nassert not hasattr(users, 'get_usr')\nassert not any('get_usr' in p.read_text() for p in pathlib.Path('.').glob('*.py') if p.name != 'test_hidden.py')",
				),
			},
			verify: "python3 test_hidden.py",
		},
		{
			id: "edit-config-default",
			category: "small-edit",
			files: {
				"config.py": py(
					'DEFAULT_TIMEOUT = 10\n\ndef timeout(value=None):\n    """Return the timeout in seconds (default 10)."""\n    return DEFAULT_TIMEOUT if value is None else value',
				),
			},
			prompts: ["Change the default timeout to 30 seconds, keeping the docstring accurate."],
			hidden: {
				"test_hidden.py": py(
					"import config\nassert config.timeout() == 30\nassert config.timeout(5) == 5\nassert '30' in config.timeout.__doc__ and '10' not in config.timeout.__doc__",
				),
			},
			verify: "python3 test_hidden.py",
		},
		{
			id: "edit-js-sum",
			category: "small-edit",
			files: { "sum.js": "exports.sum = (values) => values.reduce((a, b) => a + b);\n" },
			prompts: ["sum.js throws for an empty array. Make sum([]) return 0 without changing other results."],
			hidden: {
				"test_hidden.js":
					"const assert = require('assert');\nconst { sum } = require('./sum.js');\nassert.strictEqual(sum([]), 0);\nassert.strictEqual(sum([1, 2, 3]), 6);\n",
			},
			verify: "node test_hidden.js",
		},
		{
			id: "multi-csv-quotes",
			category: "multi-file",
			files: {
				"parse.py": py("def parse_line(line):\n    return line.rstrip('\\n').split(',')"),
				"report.py": py(
					"from parse import parse_line\n\ndef names(lines):\n    return [parse_line(line)[0] for line in lines]",
				),
			},
			prompts: [
				'report.names() returns broken names when a field is quoted and contains a comma, e.g. \'"Smith, J",42\'. Fix the root cause.',
			],
			hidden: {
				"test_hidden.py": py(
					"from report import names\nfrom parse import parse_line\nassert names(['\"Smith, J\",42\\n', 'Lee,7\\n']) == ['Smith, J', 'Lee']\nassert parse_line('\"a,b\",\"c\"') == ['a,b', 'c']",
				),
			},
			verify: "python3 test_hidden.py",
		},
		{
			id: "multi-cache",
			category: "multi-file",
			files: {
				"store.py": py("_data = {}\n\ndef put(key, value):\n    _data[key] = value\n\ndef get(key):\n    return _data.get(key)"),
				"service.py": py(
					"import store\n_cache = {}\n\ndef lookup(key):\n    if key not in _cache:\n        _cache[key] = store.get(key)\n    return _cache[key]\n\ndef update(key, value):\n    store.put(key, value)",
				),
			},
			prompts: ["service.lookup returns stale values after service.update. Fix it."],
			hidden: {
				"test_hidden.py": py(
					"import service\nservice.update('a', 1)\nassert service.lookup('a') == 1\nservice.update('a', 2)\nassert service.lookup('a') == 2",
				),
			},
			verify: "python3 test_hidden.py",
		},
		{
			id: "multi-units",
			category: "multi-file",
			files: {
				"geo.py": py("def distance_km(a, b):\n    return abs(a - b) * 1.609344"),
				"trip.py": py("from geo import distance_km\n\ndef trip_km(stops_km):\n    return sum(distance_km(stops_km[i], stops_km[i + 1]) for i in range(len(stops_km) - 1))"),
			},
			prompts: ["trip_km gives wrong totals. Inputs are already kilometres. Fix the unit bug where it belongs."],
			hidden: {
				"test_hidden.py": py(
					"from trip import trip_km\nfrom geo import distance_km\nassert trip_km([0, 10, 25]) == 25\nassert distance_km(3, 1) == 2",
				),
			},
			verify: "python3 test_hidden.py",
		},
		{
			id: "multi-pagination",
			category: "multi-file",
			files: {
				"api.py": py("ITEMS = list(range(1, 26))\n\ndef page(number, size=10):\n    start = number * size\n    return ITEMS[start:start + size]"),
				"client.py": py(
					"from api import page\n\ndef all_items():\n    out, number = [], 1\n    while True:\n        chunk = page(number)\n        if not chunk:\n            return out\n        out += chunk\n        number += 1",
				),
			},
			prompts: ["client.all_items() misses items. Pages are meant to be 1-based. Fix it."],
			hidden: {
				"test_hidden.py": py(
					"from client import all_items\nfrom api import page\nassert all_items() == list(range(1, 26))\nassert page(1) == list(range(1, 11))",
				),
			},
			verify: "python3 test_hidden.py",
		},
		{
			id: "multi-timezone",
			category: "multi-file",
			files: {
				"times.py": py(
					"from datetime import datetime\n\ndef parse(stamp):\n    return datetime.strptime(stamp, '%Y-%m-%dT%H:%M:%S')",
				),
				"schedule.py": py(
					"from times import parse\n\ndef hours_between(a, b):\n    return (parse(b) - parse(a)).total_seconds() / 3600",
				),
			},
			prompts: [
				"Timestamps now carry offsets like 2026-01-01T10:00:00+02:00. Make schedule.hours_between correct for such inputs, with parse() returning timezone-aware datetimes.",
			],
			hidden: {
				"test_hidden.py": py(
					"from schedule import hours_between\nfrom times import parse\nassert hours_between('2026-01-01T10:00:00+02:00', '2026-01-01T10:00:00+00:00') == 2\nassert parse('2026-01-01T00:00:00+00:00').tzinfo is not None",
				),
			},
			verify: "python3 test_hidden.py",
		},
		{
			id: "data-top-region",
			category: "large-data",
			files: { "sales.csv": sales.csv },
			prompts: [
				"sales.csv has 300,000 orders. Find the region with the highest total amount_cents. Write '<region> <total_cents>' to answer.txt.",
			],
			hidden: {},
			verify: answerCheck(sales.answer),
		},
		{
			id: "data-heavy-users",
			category: "large-data",
			files: { "events.jsonl": events.jsonl },
			prompts: ["events.jsonl has 200,000 events. How many distinct users have more than 5 events? Write just the number to answer.txt."],
			hidden: {},
			verify: answerCheck(events.answer),
		},
		{
			id: "data-error-streak",
			category: "large-data",
			files: { "app.log": errors.log },
			prompts: ["In app.log, what is the longest run of consecutive ERROR lines? Write just the number to answer.txt."],
			hidden: {},
			verify: answerCheck(errors.answer),
		},
		{
			id: "data-median-latency",
			category: "large-data",
			files: { "latency.csv": latency.csv },
			prompts: ["What is the median latency_ms for the /checkout endpoint in latency.csv? Write just the number to answer.txt."],
			hidden: {},
			verify: answerCheck(latency.answer),
		},
		{
			id: "data-config-keys",
			category: "large-data",
			files: configs.files,
			prompts: ["Across the 500 JSON files in configs/, which top-level key appears in the most files? Write just the key to answer.txt."],
			hidden: {},
			verify: answerCheck(configs.answer),
		},
		{
			id: "review-sql-injection",
			category: "review",
			files: {
				"db.py": py(
					"import sqlite3\n\ndef connect():\n    return sqlite3.connect(':memory:')\n\ndef find_user(conn, name):\n    query = \"SELECT id FROM users WHERE name = '\" + name + \"'\"\n    return conn.execute(query).fetchall()\n\ndef count(conn):\n    return conn.execute('SELECT COUNT(*) FROM users').fetchone()[0]",
				),
			},
			prompts: [
				"Review db.py for security issues. Do not change code. Write review.md listing each finding as `db.py:<line>: <issue>`.",
			],
			hidden: {},
			verify: "grep -Eq 'db\\.py:7\\b' review.md && grep -qi 'inject' review.md",
		},
		{
			id: "review-race",
			category: "review",
			files: {
				"counter.py": py(
					"import threading\n\ncount = 0\nlock = threading.Lock()\n\ndef increment():\n    global count\n    value = count\n    count = value + 1\n\ndef reset():\n    global count\n    with lock:\n        count = 0",
				),
			},
			prompts: [
				"Review counter.py for concurrency bugs. Do not change code. Write review.md listing each finding as `counter.py:<line>: <issue>`.",
			],
			hidden: {},
			verify: "grep -Eq 'counter\\.py:(8|9)\\b' review.md && grep -Eiq 'race|lock|atomic' review.md",
		},
		{
			id: "review-leak",
			category: "review",
			files: {
				"loader.py": py(
					"import json\n\ndef load_all(paths):\n    out = []\n    for path in paths:\n        handle = open(path)\n        out.append(json.load(handle))\n    return out",
				),
			},
			prompts: [
				"Review loader.py for resource-handling bugs. Do not change code. Write review.md listing each finding as `loader.py:<line>: <issue>`.",
			],
			hidden: {},
			verify: "grep -Eq 'loader\\.py:6\\b' review.md && grep -Eiq 'close|leak|with' review.md",
		},
		{
			id: "learn-type-hints",
			category: "correction",
			files: { "mathx.py": py("def add(a, b):\n    return a + b") },
			prompts: [
				"Add a function mul(a, b) to mathx.py.",
				"In this repository every function must have type hints and a docstring. Please follow that from now on, and fix what you just added.",
				"Now add a function sub(a, b) to mathx.py.",
			],
			hidden: {
				"test_hidden.py": py(
					"import inspect, mathx\nfor name in ('mul', 'sub'):\n    fn = getattr(mathx, name)\n    sig = inspect.signature(fn)\n    assert fn.__doc__, name\n    assert all(p.annotation is not inspect.Parameter.empty for p in sig.parameters.values()), name\n    assert sig.return_annotation is not inspect.Signature.empty, name\nassert mathx.sub(5, 3) == 2 and mathx.mul(2, 3) == 6",
				),
			},
			verify: "python3 test_hidden.py",
		},
		{
			id: "learn-output-convention",
			category: "correction",
			files: { "data.txt": "3\n1\n2\n" },
			prompts: [
				"Write the numbers in data.txt, sorted, to sorted.txt.",
				"Convention here: output files go under results/, are UTF-8, and end with a trailing newline. Move sorted.txt accordingly and remember this.",
				"Now write the sum of the numbers in data.txt to total.txt.",
			],
			hidden: {},
			verify:
				"python3 -c \"import pathlib,sys; t=pathlib.Path('results/total.txt'); s=pathlib.Path('results/sorted.txt'); ok=t.exists() and t.read_bytes().endswith(b'\\n') and t.read_text().strip()=='6' and s.exists() and s.read_text()=='1\\n2\\n3\\n' and not pathlib.Path('total.txt').exists(); sys.exit(0 if ok else 1)\"",
		},
	];
}
