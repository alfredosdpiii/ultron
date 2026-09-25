/**
 * Frozen HARD quality tasks for the baseline (stock Pi) vs candidate (Ultron) comparison.
 *
 * The original set (tasks.mjs) is saturated: the baseline passes all of it. These tasks are meant to
 * separate strong agents: several interacting bugs behind one symptom, refactors checked by hidden
 * property tests against a frozen copy of the original code, and research over deterministic
 * multi-file datasets (each task at most ~50 MB) whose multi-part answers must all be exact. Data is
 * far too large to read into context, so the agent must compute over it.
 *
 * Same contract as tasks.mjs, except that `build()` generates the task on demand (the data is large)
 * and returns `{ files, hidden }`. Values in `files`/`hidden` are strings or Buffers. Reference
 * solutions live in tasks-hard-solutions.mjs and are exercised by `eval-quality.mjs --self-check`.
 * Prompts are agent-neutral: nothing in them favors one runtime.
 * Do not edit a task after measurements exist: add a new id instead (see FROZEN_AT).
 */
import { createHash } from "node:crypto";
import { gzipSync } from "node:zlib";

export const FROZEN_AT = "2026-09-25-hard";

function rng(seed) {
	let state = seed >>> 0;
	return () => {
		state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
		return state / 2 ** 32;
	};
}

const pick = (random, values) => values[Math.floor(random() * values.length)];
const pad = (value, width = 2) => String(value).padStart(width, "0");
const int = (random, low, high) => low + Math.floor(random() * (high - low + 1));

function shuffle(random, values) {
	for (let index = values.length - 1; index > 0; index -= 1) {
		const other = Math.floor(random() * (index + 1));
		[values[index], values[other]] = [values[other], values[index]];
	}
	return values;
}

/** ISO-8601 local time for epoch seconds at a UTC offset in minutes; offset 0 renders as `Z` or `+00:00`. */
function isoAt(seconds, offsetMinutes, zulu = false) {
	const local = new Date((seconds + offsetMinutes * 60) * 1000).toISOString().slice(0, 19);
	if (offsetMinutes === 0 && zulu) return `${local}Z`;
	const sign = offsetMinutes < 0 ? "-" : "+";
	const magnitude = Math.abs(offsetMinutes);
	return `${local}${sign}${pad(Math.floor(magnitude / 60))}:${pad(magnitude % 60)}`;
}

const isoUtc = (seconds) => `${new Date(seconds * 1000).toISOString().slice(0, 19)}Z`;

function csvField(value) {
	const text = String(value);
	return /[",\n\r]/.test(text) ? `"${text.replaceAll('"', '""')}"` : text;
}

const csvRow = (fields) => fields.map(csvField).join(",");

function median(values) {
	const sorted = [...values].sort((a, b) => a - b);
	const middle = Math.floor(sorted.length / 2);
	return sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2;
}

/** Deterministic filler words so records have realistic width. */
const WORDS =
	"alpha bravo cobalt delta ember fjord granite harbor indigo juniper kelvin lumen meadow nimbus onyx prairie quartz raven sierra tundra umber violet willow xenon yarrow zephyr".split(
		" ",
	);
const words = (random, count) => Array.from({ length: count }, () => pick(random, WORDS)).join(" ");

const code = (strings, ...values) => String.raw(strings, ...values).replace(/^\n/, "");

/**
 * Hidden answer checker: expected.json maps key -> {value, tol?, set?}. Numbers compare with the
 * tolerance (default exact), lists compare in order unless `set`, everything else compares as JSON.
 */
const ANSWER_CHECK = code`
import json, sys

expected = json.load(open("expected_hidden.json"))
try:
    got = json.load(open("answer.json"))
except Exception as error:
    sys.exit(f"answer.json missing or not JSON: {error}")
if not isinstance(got, dict):
    sys.exit("answer.json must hold a JSON object")

def number(value):
    if isinstance(value, bool):
        raise ValueError("bool")
    return float(value)

problems = []
for key, spec in expected.items():
    want = spec["value"]
    if key not in got:
        problems.append(f"missing key {key}")
        continue
    have = got[key]
    if isinstance(want, (int, float)) and not isinstance(want, bool):
        try:
            ok = abs(number(have) - want) <= spec.get("tol", 0)
        except (TypeError, ValueError):
            ok = False
    elif isinstance(want, list) and spec.get("set"):
        ok = isinstance(have, list) and sorted(map(str, have)) == sorted(map(str, want))
    else:
        ok = have == want
    if not ok:
        problems.append(f"{key}: wrong value")
if problems:
    sys.exit("; ".join(problems))
print("ok")
`;

const answerHidden = (expected) => ({
	"check_answer_hidden.py": ANSWER_CHECK,
	"expected_hidden.json": `${JSON.stringify(expected)}\n`,
});
const ANSWER_VERIFY = "python3 check_answer_hidden.py";

// ---------------------------------------------------------------------------------------------
// Large-data research
// ---------------------------------------------------------------------------------------------

function buildSessions() {
	const random = rng(101);
	const offsets = [0, 0, -300, 120, 330, -210, 540];
	const types = ["view", "click", "scroll", "purchase"];
	const base = Date.UTC(2026, 2, 1) / 1000;
	const lines = [];
	const sessionCounts = [];
	const durations = [];
	const hours = new Array(24).fill(0);
	let eventNumber = 0;
	for (let userIndex = 0; userIndex < 5000; userIndex += 1) {
		const user = `u${pad(userIndex, 5)}`;
		let time = base + Math.floor(random() * 3 * 86400);
		let sessionStart = time;
		let previous = time;
		let sessions = 1;
		const count = 10 + Math.floor(random() * 120);
		for (let event = 0; event < count; event += 1) {
			if (event > 0) {
				const roll = random();
				const gap =
					roll < 0.6
						? int(random, 5, 600)
						: roll < 0.7
							? 1800
							: roll < 0.75
								? 1801
								: roll < 0.85
									? int(random, 900, 1799)
									: int(random, 1802, 50_000);
				time += gap;
				if (gap > 1800) {
					durations.push(previous - sessionStart);
					sessionStart = time;
					sessions += 1;
				}
			}
			previous = time;
			hours[new Date(time * 1000).getUTCHours()] += 1;
			const offset = pick(random, offsets);
			const id = `ev-${(eventNumber++).toString(36)}`;
			const type = pick(random, types);
			const record = (ingest) => JSON.stringify({ event_id: id, user, ts: isoAt(time, offset, random() < 0.5), type, ingest });
			lines.push(record(int(random, 1, 40)));
			if (random() < 0.03) lines.push(record(int(random, 41, 80)));
		}
		durations.push(previous - sessionStart);
		sessionCounts.push([user, sessions]);
	}
	shuffle(random, lines);
	let top = sessionCounts[0];
	for (const entry of sessionCounts) if (entry[1] > top[1]) top = entry;
	let busiest = 0;
	for (let hour = 1; hour < 24; hour += 1) if (hours[hour] > hours[busiest]) busiest = hour;
	return {
		files: { "events.jsonl": `${lines.join("\n")}\n` },
		hidden: answerHidden({
			total_sessions: { value: durations.length },
			top_user: { value: top[0] },
			median_session_seconds: { value: median(durations), tol: 0.001 },
			busiest_hour_utc: { value: busiest },
		}),
	};
}

function buildLedger() {
	const random = rng(202);
	const currencies = [
		["EUR", 1, 0.5],
		["USD", 0.92, 0.2],
		["GBP", 1.17, 0.12],
		["JPY", 0.0062, 0.1],
		["CHF", 1.04, 0.08],
	];
	const startDay = Date.UTC(2026, 0, 1) / 86_400_000;
	const days = 181;
	const dateOf = (day) => new Date((startDay + day) * 86_400_000).toISOString().slice(0, 10);
	const rateRows = ["date,currency,eur_per_unit"];
	const rateOn = currencies.map(() => new Array(days));
	const current = currencies.map((currency) => currency[1]);
	for (let day = 0; day < days; day += 1) {
		const weekday = new Date((startDay + day) * 86_400_000).getUTCDay();
		currencies.forEach(([name], index) => {
			if (name === "EUR") {
				rateOn[index][day] = 1;
				return;
			}
			if (weekday === 0 || weekday === 6) {
				rateOn[index][day] = rateOn[index][day - 1];
				return;
			}
			current[index] *= 1 + (random() - 0.5) * 0.01;
			const text = current[index].toFixed(name === "JPY" ? 8 : 6);
			rateOn[index][day] = Number(text);
			rateRows.push(`${dateOf(day)},${name},${text}`);
		});
	}
	const descriptions = [
		"Card payment",
		"SEPA transfer",
		"Refund, partial",
		'Invoice "Q2" settlement',
		"Fees, FX, misc",
		"Payroll",
		"Subscription",
	];
	const bankRows = ["date,reference,description,amount,currency"];
	const ledgerRows = [];
	let matched = 0;
	let unmatchedBank = 0;
	let unmatchedLedger = 0;
	let unmatchedTotal = 0;
	let largest = { diff: -1, ref: "" };
	for (let index = 0; index < 140_000; index += 1) {
		const ref = `TX${(1_000_003 + index * 7919).toString(36).toUpperCase()}`;
		const day = Math.floor(random() * days);
		let roll = random();
		let currencyIndex = 0;
		for (; currencyIndex < currencies.length - 1; currencyIndex += 1) {
			roll -= currencies[currencyIndex][2];
			if (roll < 0) break;
		}
		const [currency] = currencies[currencyIndex];
		const negative = random() < 0.25;
		const amountText =
			currency === "JPY"
				? String((negative ? -1 : 1) * int(random, 100, 900_000))
				: `${negative ? "-" : ""}${(int(random, 100, 2_000_000) / 100).toFixed(2)}`;
		const eur = Number(amountText) * rateOn[currencyIndex][day];
		bankRows.push(csvRow([dateOf(day), ref, `${pick(random, descriptions)} ${index}`, amountText, currency]));
		const kind = random();
		const ledgerRef = () => {
			const style = random();
			return style < 0.6 ? ref : style < 0.8 ? ref.toLowerCase() : ` ${ref} `;
		};
		const entryId = `L${pad(index, 7)}`;
		if (kind < 0.87) {
			matched += 1;
			ledgerRows.push(csvRow([entryId, dateOf(Math.min(days - 1, day + int(random, 0, 3))), ledgerRef(), (Math.round(eur * 100) / 100).toFixed(2)]));
		} else if (kind < 0.91) {
			const booked = Number((eur + (random() < 0.5 ? -1 : 1) * (1 + random() * 499)).toFixed(2));
			ledgerRows.push(csvRow([entryId, dateOf(day), ledgerRef(), booked.toFixed(2)]));
			unmatchedBank += 1;
			unmatchedLedger += 1;
			unmatchedTotal += eur;
			const diff = Math.abs(eur - booked);
			if (diff > largest.diff) largest = { diff, ref };
		} else {
			unmatchedBank += 1;
			unmatchedTotal += eur;
		}
	}
	for (let index = 0; index < 4000; index += 1) {
		unmatchedLedger += 1;
		ledgerRows.push(
			csvRow([`M${pad(index, 7)}`, dateOf(Math.floor(random() * days)), `LX${(77_777 + index * 131).toString(36).toUpperCase()}`, (int(random, -500_000, 500_000) / 100).toFixed(2)]),
		);
	}
	shuffle(random, ledgerRows);
	return {
		files: {
			"bank.csv": `${bankRows.join("\n")}\n`,
			"ledger.csv": `entry_id,posted_date,ref,amount_eur\n${ledgerRows.join("\n")}\n`,
			"rates.csv": `${rateRows.join("\n")}\n`,
		},
		hidden: answerHidden({
			matched: { value: matched },
			unmatched_bank: { value: unmatchedBank },
			unmatched_ledger: { value: unmatchedLedger },
			unmatched_bank_total_eur: { value: Math.round(unmatchedTotal * 100) / 100, tol: 0.015 },
			largest_mismatch_ref: { value: largest.ref },
		}),
	};
}

function buildSensors() {
	const random = rng(303);
	const sites = ["north-yard", "south-yard", "dock-1", "dock-2", "roof-a", "roof-b", "basement", "lab", "server-room", "atrium", "garage", "annex"];
	const siteBase = sites.map((_, index) => 21 + (index % 4) * 0.004 + Math.floor(index / 4) * 0.003);
	const meta = ["sensor_id,site,model"];
	const files = {};
	const siteSum = sites.map(() => 0);
	const siteCount = sites.map(() => 0);
	let above = 0;
	let gapBest = { gap: -1, sensor: "" };
	const start = Date.UTC(2026, 4, 1) / 1000;
	for (let sensorIndex = 0; sensorIndex < 330; sensorIndex += 1) {
		const sensor = `S${pad(sensorIndex + 1, 4)}`;
		const siteIndex = Math.floor(random() * sites.length);
		meta.push(`${sensor},${sites[siteIndex]},${pick(random, ["TX-100", "TX-200", "HX-9"])}`);
		if (sensorIndex >= 300) continue; // listed in meta.csv but never reported
		const fahrenheit = random() < 0.4;
		const offset = (random() - 0.5) * 4;
		const rows = [];
		let time = start + int(random, 0, 3600);
		let previous = null;
		for (let reading = 0; reading < 3000; reading += 1) {
			if (reading > 0) {
				time += random() < 0.02 ? 60 * int(random, 2, 90) : 60;
				if (sensorIndex === 137 && reading === 1500) time += 5 * 3600 + 17 * 60;
			}
			if (previous !== null && time - previous > gapBest.gap) gapBest = { gap: time - previous, sensor };
			previous = time;
			let tenths = Math.round((siteBase[siteIndex] + offset + 5 * Math.sin(reading / 240) + (random() - 0.5) * 3) * 10);
			if (random() < 0.003) tenths = int(random, 401, 480);
			if (tenths === 400) tenths = 401;
			const missing = random() < 0.015;
			if (!missing) {
				siteSum[siteIndex] += tenths;
				siteCount[siteIndex] += 1;
				if (tenths > 400) above += 1;
			}
			const humidity = int(random, 20, 90);
			if (fahrenheit) {
				const value = missing ? "" : ((tenths * 18 + 3200) / 100).toFixed(2);
				rows.push(`${sensor};${time};${value};${humidity}`);
			} else {
				const value = missing ? "NA" : (tenths / 10).toFixed(1);
				rows.push(`${isoUtc(time)},${sensor},${value},${humidity}`);
			}
		}
		if (random() < 0.25) shuffle(random, rows);
		const header = fahrenheit ? "sensor;epoch;temp_f;humidity" : "timestamp,sensor,temp_c,humidity";
		const name = `sensors/dump-${(0x10000 + Math.floor(random() * 0xeffff)).toString(16)}-${sensorIndex}.csv`;
		files[name] = `${header}\n${rows.join("\n")}\n`;
	}
	files["meta.csv"] = `${meta.join("\n")}\n`;
	let hottest = 0;
	for (let index = 1; index < sites.length; index += 1)
		if (siteSum[index] / siteCount[index] > siteSum[hottest] / siteCount[hottest]) hottest = index;
	return {
		files,
		hidden: answerHidden({
			hottest_site: { value: sites[hottest] },
			hottest_site_mean_c: { value: siteSum[hottest] / siteCount[hottest] / 10, tol: 0.0005 },
			readings_above_40c: { value: above },
			max_gap_sensor: { value: gapBest.sensor },
			max_gap_seconds: { value: gapBest.gap },
		}),
	};
}

function buildDependencies() {
	const random = rng(404);
	const count = 60_000;
	const chain = 2500;
	const names = Array.from({ length: count }, (_, index) => `pkg-${pad(index, 5)}`);
	const chainNames = Array.from({ length: chain }, (_, index) => `chain-${pad(index, 4)}`);
	const latestDeps = new Map();
	const lines = [];
	const describe = () => words(random, int(random, 6, 16));
	const emit = (name, version, deps) => lines.push(JSON.stringify({ name, version, deps, description: describe() }));
	const versionsFor = () => {
		const total = int(random, 1, 4);
		const seen = new Set();
		while (seen.size < total) seen.add([int(random, 0, 2), int(random, 0, 12), int(random, 0, 12)].join("."));
		const sorted = [...seen].sort((a, b) => {
			const x = a.split(".").map(Number);
			const y = b.split(".").map(Number);
			return x[0] - y[0] || x[1] - y[1] || x[2] - y[2];
		});
		return sorted;
	};
	for (let index = 0; index < count; index += 1) {
		const deps = [];
		const fanout = random() < 0.3 ? 0 : int(random, 1, 5);
		for (let edge = 0; edge < fanout; edge += 1) {
			const target = index + int(random, 1, 2000);
			if (target < count) deps.push(names[target]);
			if (random() < 0.05 && deps.length) deps.push(deps[0]);
		}
		latestDeps.set(names[index], deps);
	}
	for (let cycle = 0; cycle < 50; cycle += 1) {
		const first = int(random, 0, count - 10);
		const size = int(random, 2, 6);
		for (let step = 0; step < size - 1; step += 1) latestDeps.get(names[first + step]).push(names[first + step + 1]);
		latestDeps.get(names[first + size - 1]).push(names[first]);
	}
	const selfLoop = names[int(random, 0, count - 1)];
	latestDeps.get(selfLoop).push(selfLoop);
	for (let index = 0; index < chain; index += 1)
		latestDeps.set(chainNames[index], index + 1 < chain ? [chainNames[index + 1]] : [names[int(random, 0, count - 1)]]);
	const rootDeps = [chainNames[0]];
	for (let edge = 0; edge < 25; edge += 1) rootDeps.push(names[int(random, 0, 3000)]);
	latestDeps.set("app-root", rootDeps);
	for (const name of [...names, ...chainNames, "app-root"]) {
		const versions = versionsFor();
		versions.forEach((version, position) => {
			if (position === versions.length - 1) emit(name, version, latestDeps.get(name));
			else emit(name, version, Array.from({ length: int(random, 0, 4) }, () => pick(random, names)));
		});
	}
	shuffle(random, lines);
	// Truth over the latest-version graph.
	const all = [...latestDeps.keys()];
	const adjacency = new Map(all.map((name) => [name, [...new Set(latestDeps.get(name))]]));
	const depth = new Map([["app-root", 0]]);
	const queue = ["app-root"];
	for (let head = 0; head < queue.length; head += 1)
		for (const next of adjacency.get(queue[head]))
			if (!depth.has(next)) {
				depth.set(next, depth.get(queue[head]) + 1);
				queue.push(next);
			}
	const indegree = new Map();
	for (const deps of adjacency.values()) for (const dep of deps) indegree.set(dep, (indegree.get(dep) ?? 0) + 1);
	let mostDepended = null;
	for (const [name, degree] of indegree)
		if (!mostDepended || degree > mostDepended[1] || (degree === mostDepended[1] && name < mostDepended[0])) mostDepended = [name, degree];
	return {
		files: { "packages.jsonl": `${lines.join("\n")}\n` },
		hidden: answerHidden({
			closure_size: { value: depth.size - 1 },
			max_depth: { value: [...depth.values()].reduce((a, b) => Math.max(a, b), 0) },
			cycle_packages: { value: cyclicCount(all, adjacency) },
			most_depended_on: { value: mostDepended[0] },
		}),
	};
}

/** Number of nodes on some cycle (SCC larger than one node, or a self-loop), via iterative Tarjan. */
function cyclicCount(nodes, adjacency) {
	const index = new Map();
	const low = new Map();
	const onStack = new Set();
	const stack = [];
	let counter = 0;
	let cyclic = 0;
	for (const start of nodes) {
		if (index.has(start)) continue;
		const work = [[start, 0]];
		index.set(start, counter);
		low.set(start, counter++);
		stack.push(start);
		onStack.add(start);
		while (work.length) {
			const frame = work[work.length - 1];
			const [node, position] = frame;
			const edges = adjacency.get(node);
			if (position < edges.length) {
				frame[1] += 1;
				const next = edges[position];
				if (!index.has(next)) {
					index.set(next, counter);
					low.set(next, counter++);
					stack.push(next);
					onStack.add(next);
					work.push([next, 0]);
				} else if (onStack.has(next)) low.set(node, Math.min(low.get(node), index.get(next)));
				continue;
			}
			work.pop();
			if (work.length) {
				const parent = work[work.length - 1][0];
				low.set(parent, Math.min(low.get(parent), low.get(node)));
			}
			if (low.get(node) === index.get(node)) {
				const component = [];
				let member;
				do {
					member = stack.pop();
					onStack.delete(member);
					component.push(member);
				} while (member !== node);
				if (component.length > 1 || adjacency.get(node).includes(node)) cyclic += component.length;
			}
		}
	}
	return cyclic;
}

// ---------------------------------------------------------------------------------------------
// Log forensics
// ---------------------------------------------------------------------------------------------

const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

function buildRevokedTokens() {
	const random = rng(505);
	const users = Array.from({ length: 400 }, (_, index) => `user${pad(index, 3)}`);
	const tokens = users.flatMap((user) =>
		Array.from({ length: int(random, 1, 4) }, () => ({ user, token: `tok_${Math.floor(random() * 2 ** 40).toString(36)}` })),
	);
	const start = Date.UTC(2026, 3, 2, 0, 0, 0) * 1;
	const span = 2 * 86_400_000;
	const revoked = new Map();
	const revocationRows = ["token,revoked_at,reason"];
	for (const entry of tokens) {
		if (random() >= 0.2) continue;
		const at = start + Math.floor(random() * span);
		revoked.set(entry.token, Math.min(revoked.get(entry.token) ?? Number.POSITIVE_INFINITY, at));
		revocationRows.push(`${entry.token},${isoAt(at / 1000, pick(random, [0, 120, -240, 330]))},${pick(random, ["leaked", "rotation", "offboarding"])}`);
		if (random() < 0.3) {
			// A later duplicate revocation must not move the effective time.
			const later = at + int(random, 60, 86_400) * 1000;
			revocationRows.push(`${entry.token},${isoAt(later / 1000, 0, true)},audit`);
		}
	}
	// Revocation times are whole seconds; keep the true instants in seconds.
	for (const [token, at] of revoked) revoked.set(token, Math.floor(at / 1000) * 1000);
	const gateway = [];
	const auth = [];
	const db = [];
	let time = start;
	let revokedRequests = 0;
	let failedWrites = 0;
	let first = null;
	const failuresByUser = new Map();
	const paths = ["/api/orders", "/api/cart", "/api/profile", "/api/payments"];
	for (let index = 0; index < 120_000; index += 1) {
		time += int(random, 1, 2800);
		const rid = `${Math.floor(random() * 2 ** 32).toString(16).padStart(8, "0")}${pad(index.toString(16), 5)}`;
		const stamp = new Date(time).toISOString();
		if (random() < 0.1) {
			gateway.push(`${stamp} INFO req=${rid} GET /healthz -> 200 1ms`);
			continue;
		}
		let entry = pick(random, tokens);
		const authTime = time + int(random, 1, 40);
		// Send some extra traffic with tokens that get revoked at some point.
		if (random() < 0.08) {
			const candidates = [...revoked.keys()];
			const token = pick(random, candidates);
			entry = tokens.find((candidate) => candidate.token === token);
		}
		const denied = random() < 0.05;
		auth.push(JSON.stringify({ t: authTime, request: rid, user: entry.user, token: entry.token, result: denied ? "denied" : "ok" }));
		const method = pick(random, ["GET", "POST", "PUT"]);
		const writes = denied || method === "GET" ? 0 : int(random, 0, 3);
		let failed = 0;
		for (let write = 0; write < writes; write += 1) {
			const writeTime = authTime + int(random, 1, 400);
			const local = new Date(writeTime + 2 * 3_600_000);
			const syslog = `${MONTHS[local.getUTCMonth()]} ${pad(local.getUTCDate())} ${pad(local.getUTCHours())}:${pad(local.getUTCMinutes())}:${pad(local.getUTCSeconds())}.${pad(local.getUTCMilliseconds(), 3)}`;
			const fails = random() < 0.12;
			if (fails) failed += 1;
			db.push(`${syslog} db-${int(random, 1, 3)} write req=${rid} table=${pick(random, ["orders", "carts", "ledger"])} ${fails ? `status=FAILED code=${pick(random, [40001, 23505, 57014])}` : "status=ok"}`);
			if (random() < 0.3) db.push(`${syslog} db-${int(random, 1, 3)} read req=${rid} table=users status=ok`);
		}
		const status = denied ? 401 : failed ? 500 : method === "POST" ? 201 : 200;
		gateway.push(`${stamp} INFO req=${rid} ${method} ${pick(random, paths)} -> ${status} ${int(random, 3, 900)}ms`);
		const revokedAt = revoked.get(entry.token);
		if (!denied && revokedAt !== undefined && revokedAt <= authTime) {
			revokedRequests += 1;
			failedWrites += failed;
			if (failed) failuresByUser.set(entry.user, (failuresByUser.get(entry.user) ?? 0) + failed);
			if (first === null) first = rid;
		}
	}
	shuffle(random, auth);
	let topUser = null;
	for (const [user, failures] of failuresByUser)
		if (!topUser || failures > topUser[1] || (failures === topUser[1] && user < topUser[0])) topUser = [user, failures];
	return {
		files: {
			"logs/gateway.log": `${gateway.join("\n")}\n`,
			"logs/auth.jsonl": `${auth.join("\n")}\n`,
			"logs/db.log": `${db.join("\n")}\n`,
			"logs/revocations.csv": `${revocationRows.join("\n")}\n`,
		},
		hidden: answerHidden({
			revoked_requests: { value: revokedRequests },
			failed_writes: { value: failedWrites },
			top_user: { value: topUser[0] },
			first_request: { value: first },
		}),
	};
}

function buildDeployAsOf() {
	const random = rng(606);
	const hosts = Array.from({ length: 16 }, (_, index) => `web-${pad(index + 1)}`);
	const start = Date.UTC(2026, 5, 10) / 1000;
	const span = 3 * 86400;
	const versions = Array.from({ length: 12 }, (_, index) => `v2.${index}.${index % 3}`);
	const badness = new Map(versions.map((version) => [version, 0.01]));
	badness.set("v2.7.1", 0.034);
	badness.set("v2.4.1", 0.017);
	// Deploy events per host: each version rolls out host by host; some are rolled back.
	const events = hosts.map(() => []);
	versions.forEach((version, index) => {
		const at = start + 3600 + index * Math.floor(span / versions.length);
		for (let host = 0; host < hosts.length; host += 1) {
			if (index === 0 && host >= 12) continue; // four hosts join late
			const time = at + host * int(random, 60, 400);
			events[host].push({ time, action: "deploy", version });
			if (random() < (version === "v2.7.1" ? 0.5 : 0.1)) events[host].push({ time: time + int(random, 600, 7200), action: "rollback" });
		}
	});
	const deployLines = [];
	for (const [host, list] of events.entries())
		for (const event of list)
			deployLines.push({
				time: event.time,
				text: `${isoAt(event.time, pick(random, [0, -420, 60, 330]), true)} host=${hosts[host]} action=${event.action}${event.action === "deploy" ? ` version=${event.version}` : ""}`,
			});
	deployLines.sort((a, b) => a.time - b.time);
	const served = new Map();
	const errors = new Map();
	let unversioned = 0;
	let total5xx = 0;
	const files = {};
	for (const [host, list] of events.entries()) {
		const ordered = [...list].sort((a, b) => a.time - b.time);
		const history = [];
		let cursor = 0;
		const lines = [];
		let time = start;
		for (let request = 0; request < 50_000; request += 1) {
			time += random() < 0.001 && cursor < ordered.length ? ordered[cursor].time - time : int(random, 1, 10_300) / 1000;
			if (time < start) time = start;
			time = Math.round(time * 1000) / 1000;
			while (cursor < ordered.length && ordered[cursor].time <= time) {
				const event = ordered[cursor++];
				if (event.action === "deploy") history.push(event.version);
				else history.pop();
			}
			const version = history.at(-1);
			const errorRate = version ? badness.get(version) : 0.01;
			const status = random() < errorRate ? pick(random, [500, 502, 503]) : pick(random, [200, 200, 200, 204, 301, 404]);
			if (status >= 500) total5xx += 1;
			if (version) {
				served.set(version, (served.get(version) ?? 0) + 1);
				if (status >= 500) errors.set(version, (errors.get(version) ?? 0) + 1);
			} else unversioned += 1;
			lines.push(`${time.toFixed(3)} ${pick(random, ["GET", "GET", "POST"])} /${pick(random, ["", "api/items", "api/cart", "static/app.js"])} ${status} ${int(random, 100, 90_000)}`);
		}
		files[`access/${hosts[host]}.log`] = `${lines.join("\n")}\n`;
	}
	files["deploys.log"] = `${deployLines.map((line) => line.text).join("\n")}\n`;
	let worst = null;
	for (const [version, count] of served) {
		const rate = (errors.get(version) ?? 0) / count;
		if (!worst || rate > worst[1]) worst = [version, rate];
	}
	let busiest = null;
	for (const [version, count] of served) if (!busiest || count > busiest[1]) busiest = [version, count];
	return {
		files,
		hidden: answerHidden({
			worst_version: { value: worst[0] },
			worst_version_error_rate: { value: worst[1], tol: 0.000005 },
			most_served_version: { value: busiest[0] },
			unversioned_requests: { value: unversioned },
			total_5xx: { value: total5xx },
		}),
	};
}

function buildBruteForce() {
	const random = rng(707);
	const start = Date.UTC(2026, 6, 1) / 1000;
	const fileCount = 10;
	const perFile = 86400;
	const normalIps = Array.from({ length: 1800 }, () => `${int(random, 11, 220)}.${int(random, 0, 255)}.${int(random, 0, 255)}.${int(random, 1, 254)}`);
	for (let index = 0; index < 200; index += 1) normalIps.push(`2001:db8:${int(random, 0, 0xffff).toString(16)}::${int(random, 1, 0xffff).toString(16)}`);
	const events = [];
	const agents = ["Mozilla/5.0 (X11; Linux x86_64)", "curl/8.9.1", "Mozilla/5.0 (Macintosh)", "python-requests/2.32"];
	const paths = ["/", "/login", "/api/items?page=2", "/static/app.css", "/admin/dashboard", "/administrator/index.php", "/admin"];
	for (let index = 0; index < 330_000; index += 1) {
		const path = pick(random, paths);
		const admin = path.startsWith("/admin");
		const status = admin ? pick(random, [200, 302, 401, 403]) : pick(random, [200, 200, 200, 304, 404]);
		events.push({ time: start + random() * perFile * fileCount, ip: pick(random, normalIps), path, status });
	}
	// Attack patterns with known outcomes.
	const burst = (ip, at, failures, spacing, path, success) => {
		for (let failure = 0; failure < failures; failure += 1)
			events.push({ time: at + failure * spacing, ip, path: failure % 2 ? `${path}?next=%2F` : path, status: 401 });
		if (success !== null) events.push({ time: at + (failures - 1) * spacing + success, ip, path: "/admin/login", status: 200 });
	};
	const attackIp = (index) => (index % 3 === 0 ? `2001:db8:bad::${(index + 10).toString(16)}` : `198.51.100.${index + 10}`);
	for (let index = 0; index < 30; index += 1) {
		const ip = attackIp(index);
		const at = start + int(random, 1, fileCount - 1) * perFile - int(random, 30, 400); // straddle a rotation boundary
		const scenario = index % 6;
		if (scenario === 0) burst(ip, at, 6, 20, "/admin/login", 15); // breach
		else if (scenario === 1) burst(ip, at, 4, 20, "/admin/login", 15); // too few failures
		else if (scenario === 2) burst(ip, at, 5, 140, "/admin/login", 50); // window: first failure falls out (560+50 > 600)
		else if (scenario === 3) burst(ip, at, 5, 100, "/administrator/login", 10); // not an /admin path
		else if (scenario === 4) burst(ip, at, 5, 120, "/admin", 119); // exactly 599 s after the first failure: breach
		else burst(ip, at, 8, 5, "/admin/login", null); // failures only
	}
	events.sort((a, b) => a.time - b.time);
	// Normal traffic may accidentally satisfy the rule; compute the truth from all events.
	const recent = new Map();
	const breached = new Set();
	let breachEvents = 0;
	let firstBreach = null;
	const isAdmin = (path) => {
		const bare = path.split("?")[0];
		return bare === "/admin" || bare.startsWith("/admin/");
	};
	for (const event of events) {
		event.time = Math.floor(event.time);
	}
	events.sort((a, b) => a.time - b.time);
	for (const event of events) {
		if (!isAdmin(event.path)) continue;
		const list = recent.get(event.ip) ?? [];
		while (list.length && list[0] < event.time - 600) list.shift();
		if (event.status === 200 && list.filter((time) => time < event.time).length >= 5) {
			breached.add(event.ip);
			breachEvents += 1;
			if (firstBreach === null) firstBreach = event.time;
		}
		if (event.status === 401) list.push(event.time);
		recent.set(event.ip, list);
	}
	const files = {};
	for (let file = 0; file < fileCount; file += 1) {
		const offset = file < 5 ? -420 : 0; // older files were written before the server moved to UTC
		const from = start + file * perFile;
		const lines = [];
		for (const event of events) {
			if (event.time < from || event.time >= from + perFile) continue;
			const local = new Date((event.time + offset * 60) * 1000);
			const zone = offset ? "-0700" : "+0000";
			const stamp = `${pad(local.getUTCDate())}/${MONTHS[local.getUTCMonth()]}/${local.getUTCFullYear()}:${pad(local.getUTCHours())}:${pad(local.getUTCMinutes())}:${pad(local.getUTCSeconds())} ${zone}`;
			lines.push(`${event.ip} - - [${stamp}] "GET ${event.path} HTTP/1.1" ${event.status} ${int(random, 200, 9000)} "-" "${pick(random, agents)}"`);
		}
		const rotated = fileCount - 1 - file; // newest is access.log
		const body = `${lines.join("\n")}\n`;
		if (rotated === 0) files["logs/access.log"] = body;
		else if (rotated === 1) files["logs/access.log.1"] = body;
		else files[`logs/access.log.${rotated}.gz`] = gzipSync(body);
	}
	return {
		files,
		hidden: answerHidden({
			breached_ips: { value: [...breached].sort() },
			breach_events: { value: breachEvents },
			first_breach_utc: { value: isoUtc(firstBreach) },
		}),
	};
}

// ---------------------------------------------------------------------------------------------
// Huge single files
// ---------------------------------------------------------------------------------------------

function buildCustomerDedupe() {
	const random = rng(808);
	const first = ["Ana", "Bo", "Chen", "Dara", "Eli", "Femi", "Gus", "Hana", "Ivo", "June", "Kai", "Lior"];
	const last = ["Smith", "O'Neil", "Nguyen", "Garcia", "Kowalski", "Okafor", "Silva", "Brown"];
	const cities = ["Lisbon", "Oslo", "Austin, TX", "Lagos", "Kyiv", "Osaka", "Porto", "Quito"];
	const notes = ["", "", "", "VIP", 'said "call later"', "moved, new address", "line one\nline two", "prefers email"];
	const keys = Array.from({ length: 180_000 }, (_, index) => `${pick(random, first).toLowerCase()}.${index.toString(36)}@example.${pick(random, ["com", "org", "net"])}`);
	const rows = [];
	const best = new Map();
	const base = Date.UTC(2025, 0, 1) / 1000;
	let id = 0;
	const emitRow = (key) => {
		const instant = base + int(random, 0, 400 * 86400);
		const style = random();
		const email = style < 0.6 ? key : style < 0.8 ? key.toUpperCase() : style < 0.9 ? `  ${key}` : `${key[0].toUpperCase()}${key.slice(1)} `;
		const fields = [String(++id), email, `${pick(random, first)} ${pick(random, last)}`, pick(random, cities), isoAt(instant, pick(random, [0, 60, -300, 330]), random() < 0.5), pick(random, notes)];
		rows.push(fields);
	};
	for (const key of keys) {
		const copies = random() < 0.45 ? int(random, 2, 4) : 1;
		for (let copy = 0; copy < copies; copy += 1) emitRow(key);
	}
	// Order rows randomly, then find the winners in file order (ties go to the later row).
	shuffle(random, rows);
	for (const fields of rows) {
		const key = fields[1].trim().toLowerCase();
		const instant = Date.parse(fields[4]) / 1000;
		const current = best.get(key);
		if (!current || instant >= current.instant) best.set(key, { instant, fields });
	}
	// Same-instant duplicates written with different offsets: the later row must win.
	const tieKeys = keys.slice(0, 400);
	for (const key of tieKeys) {
		const winner = best.get(key);
		const instant = winner.instant;
		const fields = [String(++id), key.toUpperCase(), "Tie Breaker", "Oslo", isoAt(instant, 60), "later row wins"];
		rows.push(fields);
		best.set(key, { instant, fields });
	}
	const output = [["id", "email", "name", "city", "updated_at", "notes"]];
	for (const key of [...best.keys()].sort()) {
		const fields = [...best.get(key).fields];
		fields[1] = key;
		output.push(fields);
	}
	const digest = createHash("sha256")
		.update(output.map((fields) => fields.join("\x1f")).join("\x1e"))
		.digest("hex");
	const check = code`
import csv, hashlib, sys
try:
    with open("dedup.csv", newline="", encoding="utf-8") as handle:
        rows = list(csv.reader(handle))
except FileNotFoundError:
    sys.exit("dedup.csv missing")
digest = hashlib.sha256("\x1e".join("\x1f".join(row) for row in rows).encode()).hexdigest()
if len(rows) != ${output.length}:
    sys.exit(f"wrong row count {len(rows)}")
if digest != "${digest}":
    sys.exit("content differs")
print("ok")
`;
	return {
		files: { "customers.csv": `id,email,name,city,updated_at,notes\n${rows.map(csvRow).join("\n")}\n` },
		hidden: { "check_dedup_hidden.py": check },
	};
}

function buildCatalogDiff() {
	const random = rng(909);
	const colors = ["red", "green", "blue", "black", "white", "teal"];
	const tagPool = ["sale", "new", "eco", "gift", "bulk", "fragile", "outdoor", "kids"];
	const item = (index) => ({
		id: `it-${pad(index, 6)}`,
		name: `${words(random, 2)} ${index}`,
		price: int(random, 50, 99_999) / 100,
		tags: [...new Set(Array.from({ length: int(random, 0, 4) }, () => pick(random, tagPool)))],
		attrs: { color: pick(random, colors), size: pick(random, ["S", "M", "L"]), dims: { w: int(random, 1, 200), h: int(random, 1, 200) } },
		stock: int(random, 0, 5000),
		description: words(random, int(random, 10, 30)),
	});
	const oldItems = Array.from({ length: 60_000 }, (_, index) => item(index));
	const newItems = [];
	const removed = [];
	const changed = {};
	for (const original of oldItems) {
		const roll = random();
		if (roll < 0.01) {
			removed.push(original.id);
			continue;
		}
		const next = structuredClone(original);
		const fields = new Set();
		if (roll < 0.07) {
			const mutations = int(random, 1, 3);
			for (let step = 0; step < mutations; step += 1) {
				const which = int(random, 0, 5);
				if (which === 0) {
					next.name = `${next.name} `;
					fields.add("name");
				} else if (which === 1) {
					next.price = Math.round((next.price + int(random, 1, 500) / 100) * 100) / 100;
					fields.add("price");
				} else if (which === 2) {
					const tag = tagPool.find((candidate) => !next.tags.includes(candidate));
					if (tag) {
						next.tags.push(tag);
						fields.add("tags");
					}
				} else if (which === 3) {
					next.attrs.dims.h += 1;
					fields.add("attrs");
				} else if (which === 4) {
					next.stock += 1;
					fields.add("stock");
				} else {
					next.description = `${next.description}.`;
					fields.add("description");
				}
			}
		} else if (roll < 0.15) {
			// Representation-only differences that are not changes.
			next.tags = [...next.tags].reverse();
			next.attrs = { dims: { h: next.attrs.dims.h, w: next.attrs.dims.w }, size: next.attrs.size, color: next.attrs.color };
		}
		if (fields.size) changed[original.id] = [...fields].sort();
		newItems.push(next);
	}
	const added = [];
	for (let index = 60_000; index < 60_700; index += 1) {
		added.push(`it-${pad(index, 6)}`);
		newItems.push(item(index));
	}
	shuffle(random, newItems);
	const expected = { added: added.sort(), removed: removed.sort(), changed };
	const check = code`
import json, sys
expected = json.load(open("expected_diff_hidden.json"))
try:
    got = json.load(open("diff.json"))
except Exception as error:
    sys.exit(f"diff.json missing or not JSON: {error}")
problems = [key for key in ("added", "removed", "changed") if got.get(key) != expected[key]]
if problems:
    sys.exit("wrong: " + ", ".join(problems))
print("ok")
`;
	return {
		files: {
			"catalog/old.json": `${JSON.stringify({ generated: "2026-06-01", items: oldItems })}\n`,
			"catalog/new.json": `${JSON.stringify({ generated: "2026-07-01", items: newItems })}\n`,
		},
		hidden: { "check_diff_hidden.py": check, "expected_diff_hidden.json": `${JSON.stringify(expected)}\n` },
	};
}


// ---------------------------------------------------------------------------------------------
// Multi-file bugs behind one symptom (hidden spec-derived test suites)
// ---------------------------------------------------------------------------------------------

const INVOICING_SPEC = `# invoicing specification

\`invoicing.build_invoice(text)\` turns an order (one item per line) into an invoice. All amounts in
the invoice are integer cents (EUR).

## Order lines

- Format: \`<sku> x<qty> @<price> [<unit>]\`, fields separated by one or more spaces; leading and
  trailing whitespace is ignored.
- Blank lines and lines whose first non-space character is \`#\` are ignored.
- \`sku\`: letters, digits and \`-\`. Case-insensitive; normalized to upper case. The category is the
  part before the first \`-\` (\`book-12\` -> sku \`BOOK-12\`, category \`BOOK\`).
- \`qty\`: positive integer; \`_\` may separate digit groups (\`x1_000\`). Zero is invalid.
- \`price\`: EUR per unit, a decimal with 0 to 4 fractional digits (\`@4\`, \`@4.5\`, \`@0.1234\`).
- \`unit\`: optional, case-insensitive, one of \`ea\` (default), \`kg\`, \`g\`, \`lb\`; normalized to lower case.
- Any other line raises \`ValueError\`.

## Amounts

- Line amount: \`qty * price\` computed exactly, then rounded to cents half-up (0.005 -> 0.01).
- Subtotal: sum of line amounts.
- Discount rate by subtotal: 15% when subtotal >= 1000.00, else 10% when >= 500.00, else 5% when
  >= 100.00, else 0. Discount = subtotal * rate, rounded to cents half-up.
- VAT: 20%. Lines of category \`BOOK\` or \`FOOD\` are exempt. Tax = (sum of taxable line amounts) *
  (1 - discount rate) * 0.20, rounded to cents half-up.
- Shipping by total weight in kg: \`kg\` lines weigh qty kg, \`g\` lines qty * 0.001 kg, \`lb\` lines
  qty * 0.45359237 kg, \`ea\` lines nothing. Weight 0 -> 0.00; above 0 up to and including 1 kg ->
  4.90; up to and including 10 kg -> 9.90; above 10 kg -> 9.90 plus 0.50 for every started kg
  above 10 (10.2 kg -> 10.40, 11 kg -> 10.40).
- Total = subtotal - discount + tax + shipping.

## Result

\`build_invoice\` returns a dict: \`lines\` (list of dicts with \`sku\`, \`qty\`, \`unit\`, \`amount\`),
\`subtotal\`, \`discount\`, \`tax\`, \`shipping\`, \`total\`.

## Helpers

- \`invoicing.money.to_cents(amount)\`: Decimal EUR -> integer cents, rounded half-up.
- \`invoicing.money.fmt(cents)\`: \`123456\` -> \`"1,234.56"\`, \`-5\` -> \`"-0.05"\`, \`-123456\` -> \`"-1,234.56"\`.
- \`invoicing.parse.parse_order(text)\`: list of line objects with \`sku\`, \`qty\`, \`price\` (Decimal),
  \`unit\` and \`category\`.
`;

const INVOICING_BUGGY = {
	"invoicing/__init__.py": code`
from .invoice import build_invoice

__all__ = ["build_invoice"]
`,
	"invoicing/money.py": code`
def to_cents(amount):
    """Round a Decimal amount in EUR to integer cents."""
    return int(round(float(amount) * 100))


def fmt(cents):
    """Render integer cents as 1,234.56."""
    whole, frac = divmod(cents, 100)
    return f"{whole:,}.{frac:02d}"
`,
	"invoicing/parse.py": code`
import re
from dataclasses import dataclass
from decimal import Decimal

LINE = re.compile(r"^\s*([A-Za-z0-9-]+)\s+x(\d+)\s+@(\d+\.\d{2})(?:\s+(ea|kg|g|lb))?\s*$")


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
    qty = int(qty)
    if qty <= 0:
        raise ValueError("quantity must be positive")
    return Line(sku, qty, Decimal(price), unit or "ea")


def parse_order(text):
    return [parse_line(line) for line in text.splitlines() if line.strip()]
`,
	"invoicing/units.py": code`
from decimal import Decimal

KG_PER_UNIT = {"ea": Decimal(0), "kg": Decimal(1), "g": Decimal("0.001"), "lb": Decimal("0.4536")}


def weight_kg(line):
    return line.qty * KG_PER_UNIT[line.unit]
`,
	"invoicing/pricing.py": code`
from decimal import Decimal

TIERS = [(100000, Decimal("0.15")), (50000, Decimal("0.10")), (10000, Decimal("0.05"))]


def discount_rate(subtotal_cents):
    for threshold, rate in TIERS:
        if subtotal_cents > threshold:
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
    return to_cents(Decimal(taxable) / 100 * VAT)
`,
	"invoicing/shipping.py": code`
def shipping_cents(weight_kg):
    if weight_kg == 0:
        return 0
    if weight_kg <= 1:
        return 490
    if weight_kg <= 10:
        return 990
    return 990 + 50 * int(weight_kg - 10)
`,
	"invoicing/invoice.py": code`
from decimal import Decimal

from .money import to_cents
from .parse import parse_order
from .pricing import discount_rate
from .shipping import shipping_cents
from .tax import tax_cents
from .units import weight_kg


def build_invoice(text):
    lines = parse_order(text)
    amounts = [to_cents(line.qty * line.price) for line in lines]
    subtotal = sum(amounts)
    rate = discount_rate(subtotal)
    discount = to_cents(Decimal(subtotal) / 100 * rate)
    tax = tax_cents(lines, amounts, rate)
    shipping = shipping_cents(sum((weight_kg(line) for line in lines), Decimal(0)))
    return {
        "lines": [
            {"sku": line.sku, "qty": line.qty, "unit": line.unit, "amount": amount}
            for line, amount in zip(lines, amounts)
        ],
        "subtotal": subtotal,
        "discount": discount,
        "tax": tax,
        "shipping": shipping,
        "total": subtotal - discount + tax + shipping,
    }
`,
	"tests/test_smoke.py": code`
import unittest

from invoicing import build_invoice


class Smoke(unittest.TestCase):
    def test_simple_order(self):
        invoice = build_invoice("TOY-1 x2 @10.00")
        self.assertEqual(invoice["subtotal"], 2000)
        self.assertEqual(invoice["total"], 2400)


if __name__ == "__main__":
    unittest.main()
`,
};

const INVOICING_HIDDEN = code`
import math
import random
import unittest
from decimal import ROUND_HALF_UP, Decimal

from invoicing import build_invoice
from invoicing.money import fmt, to_cents
from invoicing.parse import parse_order


def ref_cents(amount):
    return int((amount * 100).quantize(Decimal(1), rounding=ROUND_HALF_UP))


def reference(order):
    amounts = [ref_cents(qty * Decimal(price)) for _, qty, price, _ in order]
    subtotal = sum(amounts)
    if subtotal >= 100000:
        rate = Decimal("0.15")
    elif subtotal >= 50000:
        rate = Decimal("0.10")
    elif subtotal >= 10000:
        rate = Decimal("0.05")
    else:
        rate = Decimal(0)
    discount = ref_cents(Decimal(subtotal) / 100 * rate)
    taxable = sum(a for (sku, _, _, _), a in zip(order, amounts) if sku.upper().split("-")[0] not in ("BOOK", "FOOD"))
    tax = ref_cents(Decimal(taxable) / 100 * (1 - rate) * Decimal("0.20"))
    factors = {"ea": Decimal(0), "kg": Decimal(1), "g": Decimal("0.001"), "lb": Decimal("0.45359237")}
    weight = sum((qty * factors[(unit or "ea").lower()] for _, qty, _, unit in order), Decimal(0))
    if weight == 0:
        shipping = 0
    elif weight <= 1:
        shipping = 490
    elif weight <= 10:
        shipping = 990
    else:
        shipping = 990 + 50 * math.ceil(weight - 10)
    return {
        "lines": [
            {"sku": sku.upper(), "qty": qty, "unit": (unit or "ea").lower(), "amount": amount}
            for (sku, qty, _, unit), amount in zip(order, amounts)
        ],
        "subtotal": subtotal,
        "discount": discount,
        "tax": tax,
        "shipping": shipping,
        "total": subtotal - discount + tax + shipping,
    }


class Money(unittest.TestCase):
    def test_to_cents_half_up(self):
        self.assertEqual(to_cents(Decimal("2.675")), 268)
        self.assertEqual(to_cents(Decimal("0.125")), 13)
        self.assertEqual(to_cents(Decimal("1.005")), 101)
        self.assertEqual(to_cents(Decimal("0.004")), 0)

    def test_fmt(self):
        self.assertEqual(fmt(123456), "1,234.56")
        self.assertEqual(fmt(0), "0.00")
        self.assertEqual(fmt(-5), "-0.05")
        self.assertEqual(fmt(-123456), "-1,234.56")
        self.assertEqual(fmt(100000000), "1,000,000.00")


class Parsing(unittest.TestCase):
    def test_fields(self):
        (line,) = parse_order("  ab-1   x1_000   @0.1234   KG  ")
        self.assertEqual((line.sku, line.qty, line.price, line.unit, line.category), ("AB-1", 1000, Decimal("0.1234"), "kg", "AB"))
        (line,) = parse_order("book-7 x2 @4")
        self.assertEqual((line.sku, line.price, line.unit, line.category), ("BOOK-7", Decimal(4), "ea", "BOOK"))
        (line,) = parse_order("Z x3 @4.5 Lb")
        self.assertEqual((line.price, line.unit), (Decimal("4.5"), "lb"))

    def test_skips_blank_and_comment_lines(self):
        self.assertEqual(len(parse_order("# header\n\n   \n  # note\nTOY-1 x1 @1\n")), 1)

    def test_invalid_lines(self):
        for bad in ["TOY-1 x0 @1", "TOY-1 x1 @1.23456", "TOY-1 x1 @1 oz", "TOY-1 @1 x1", "TOY_1 x1 @1", "TOY-1 x-1 @1", "TOY-1 x1 @-1"]:
            with self.assertRaises(ValueError, msg=bad):
                parse_order(bad)


class Invoices(unittest.TestCase):
    def check(self, text, subtotal, discount, tax, shipping, total):
        invoice = build_invoice(text)
        self.assertEqual(
            (invoice["subtotal"], invoice["discount"], invoice["tax"], invoice["shipping"], invoice["total"]),
            (subtotal, discount, tax, shipping, total),
            text,
        )

    def test_examples(self):
        self.check("BOOK-1 x2 @10\nTOY-9 x3 @15.5", 6650, 0, 930, 0, 7580)
        self.check("TOY-1 x1 @100", 10000, 500, 1900, 0, 11400)
        self.check("food-7 x4 @125", 50000, 5000, 0, 0, 45000)
        self.check("RICE-1 x10200 @0.001 g", 1020, 0, 204, 1040, 2264)
        self.check("SAND-1 x10 @1 kg", 1000, 0, 200, 990, 2190)
        self.check("SAND-9 x668 @1 lb", 66800, 6680, 12024, 15640, 87784)
        self.check("# order\n\n  TOY-2 x1_000 @0.0125\n", 1250, 0, 250, 0, 1500)
        self.check("TOY-3 x1 @0.005", 1, 0, 0, 0, 1)
        self.check("TOY-4 x3 @333.335", 100001, 15000, 17000, 0, 102001)

    def test_randomized_against_spec(self):
        rng = random.Random(1234)
        for case in range(400):
            order, text = [], []
            for _ in range(rng.randint(1, 8)):
                sku = rng.choice(["BOOK", "FOOD", "TOY", "TOOL", "book", "Food", "tea"]) + "-" + str(rng.randint(1, 99))
                qty = rng.choice([rng.randint(1, 12), rng.randint(1, 3000)])
                qty_text = f"{qty:_}" if qty >= 1000 and rng.random() < 0.5 else str(qty)
                decimals = rng.randint(0, 4)
                price = f"{rng.randint(0, 400)}" + (f".{rng.randint(0, 10 ** decimals - 1):0{decimals}d}" if decimals else "")
                unit = rng.choice([None, "ea", "kg", "g", "lb", "KG", "Lb", "G"])
                order.append((sku, qty, price, unit))
                text.append(f"{sku}{' ' * rng.randint(1, 3)}x{qty_text} @{price}" + (f" {unit}" if unit else ""))
                if rng.random() < 0.2:
                    text.append(rng.choice(["", "# comment", "   "]))
            self.assertEqual(build_invoice("\n".join(text)), reference(order), "\n".join(text))


if __name__ == "__main__":
    unittest.main()
`;

const SCHED_README = `# sched

Tiny weekly scheduler. CommonJS; no dependencies.

## \`nextRun(spec, from, offset = "+00:00")\`

Returns the first run strictly after \`from\` (an ISO string or Date) as an ISO string in UTC
(\`Date#toISOString()\` format).

- \`spec\` is \`"<time> <days>"\`.
- \`time\` is \`H:MM\` or \`HH:MM\`, 24-hour, from \`0:00\` to \`23:59\`; anything else throws \`RangeError\`.
- \`days\` is \`*\` (every day) or a comma-separated list of items. An item is a day name or an
  inclusive range \`a-b\` of day names. Names are \`mon tue wed thu fri sat sun\` or the full English
  names (\`monday\` ...), case-insensitive. Ranges may wrap past Sunday: \`fri-mon\` is Friday,
  Saturday, Sunday and Monday. Unknown names throw \`RangeError\`.
- \`offset\` is \`+HH:MM\` or \`-HH:MM\`: the fixed UTC offset in which \`time\` and \`days\` are
  interpreted. \`-03:30\` means local time is UTC minus 3 hours 30 minutes.

## \`occurrences(spec, from, count, offset = "+00:00")\`

The next \`count\` runs after \`from\`, in order, each strictly after the previous one.

## \`parseDuration(text)\`

Milliseconds for strings like \`1d2h3m4s\`, \`90m\` or \`45s\`: components \`d\`, \`h\`, \`m\`, \`s\` with
non-negative integer amounts, in that order, each at most once, at least one component.
Anything else (\`""\`, \`1x\`, \`30m1h\`) throws \`RangeError\`.
`;

const SCHED_BUGGY = {
	"sched/index.js": code`
const { nextRun } = require("./next");
const { parseDuration } = require("./duration");

function occurrences(spec, from, count, offset = "+00:00") {
	const runs = [];
	let cursor = from;
	for (let index = 0; index < count; index += 1) {
		cursor = nextRun(spec, cursor, offset);
		runs.push(cursor);
	}
	return runs;
}

module.exports = { nextRun, occurrences, parseDuration };
`,
	"sched/days.js": code`
const NAMES = { sun: 0, mon: 1, tue: 2, wed: 3, thu: 4, fri: 5, sat: 6 };

function dayNumber(name) {
	if (!(name in NAMES)) throw new RangeError("unknown day: " + name);
	return NAMES[name];
}

function parseDays(text) {
	if (text === "*") return new Set([0, 1, 2, 3, 4, 5, 6]);
	const days = new Set();
	for (const item of text.split(",")) {
		const [first, last] = item.split("-");
		if (last === undefined) {
			days.add(dayNumber(first));
			continue;
		}
		for (let day = dayNumber(first); day <= dayNumber(last); day += 1) days.add(day);
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
	if (hours > 24 || minutes > 60) throw new RangeError("bad time: " + text);
	return hours * 60 + minutes;
}

module.exports = { parseClock };
`,
	"sched/offset.js": code`
function parseOffset(text) {
	const match = /^([+-])(\d{2}):(\d{2})$/.exec(text);
	if (!match) throw new RangeError("bad offset: " + text);
	const sign = match[1] === "-" ? -1 : 1;
	return sign * Number(match[2]) * 60 + Number(match[3]);
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
	for (let day = 0; day < 7; day += 1) {
		const candidate = midnight + day * DAY + minutes * 60000;
		if (candidate >= local && allowed.has(new Date(candidate).getUTCDay())) return new Date(candidate - shift).toISOString();
	}
	return null;
}

module.exports = { nextRun };
`,
	"sched/duration.js": code`
const UNITS = { d: 8640000, h: 3600000, m: 60000, s: 1000 };

function parseDuration(text) {
	const match = /^(?:(\d+)d)?(?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s)?$/.exec(text);
	if (!match) throw new RangeError("bad duration: " + text);
	const [, d, h, m, s] = match;
	return (Number(d ?? 0) * UNITS.d) + (Number(h ?? 0) * UNITS.h) + (Number(m ?? 0) * UNITS.m) + (Number(s ?? 0) * UNITS.s);
}

module.exports = { parseDuration };
`,
	"test/smoke.test.js": code`
const assert = require("node:assert");
const { nextRun } = require("../sched");

assert.strictEqual(nextRun("09:00 *", "2026-01-01T08:00:00Z"), "2026-01-01T09:00:00.000Z");
console.log("smoke ok");
`,
};

const SCHED_HIDDEN = code`
const assert = require("node:assert");
const { nextRun, occurrences, parseDuration } = require("./sched");

const cases = [
	["09:00 mon-fri", "2026-01-01T08:00:00Z", "+00:00", "2026-01-01T09:00:00.000Z"],
	["09:00 *", "2026-01-01T09:00:00Z", "+00:00", "2026-01-02T09:00:00.000Z"],
	["10:00 fri-mon", "2026-01-01T12:00:00Z", "+00:00", "2026-01-02T10:00:00.000Z"],
	["10:00 fri-mon", "2026-01-02T11:00:00Z", "+00:00", "2026-01-03T10:00:00.000Z"],
	["10:00 fri-mon", "2026-01-05T11:00:00Z", "+00:00", "2026-01-09T10:00:00.000Z"],
	["10:00 sat-sun", "2026-01-01T00:00:00Z", "+00:00", "2026-01-03T10:00:00.000Z"],
	["7:05 SAT,Sun", "2026-01-01T00:00:00Z", "+00:00", "2026-01-03T07:05:00.000Z"],
	["12:00 Monday-Wednesday", "2026-01-01T00:00:00Z", "+00:00", "2026-01-05T12:00:00.000Z"],
	["23:30 thu", "2026-01-01T00:00:00Z", "-03:30", "2026-01-02T03:00:00.000Z"],
	["01:00 fri", "2026-01-01T20:00:00Z", "+05:30", "2026-01-08T19:30:00.000Z"],
	["0:00 *", "2026-02-28T23:59:59Z", "+00:00", "2026-03-01T00:00:00.000Z"],
];
for (const [spec, from, offset, expected] of cases) assert.strictEqual(nextRun(spec, from, offset), expected, spec + " from " + from + " " + offset);
assert.strictEqual(nextRun("09:00 *", new Date("2026-01-01T08:00:00Z")), "2026-01-01T09:00:00.000Z");

for (const bad of ["24:00 *", "12:60 *", "12:5 *", "123:00 *", "09:00 funday", "09:00 mon-xyz"]) assert.throws(() => nextRun(bad, "2026-01-01T00:00:00Z"), RangeError, bad);

assert.deepStrictEqual(occurrences("12:00 sat,sun", "2026-01-01T00:00:00Z", 3), [
	"2026-01-03T12:00:00.000Z",
	"2026-01-04T12:00:00.000Z",
	"2026-01-10T12:00:00.000Z",
]);

assert.strictEqual(parseDuration("1h30m"), 5400000);
assert.strictEqual(parseDuration("2d"), 172800000);
assert.strictEqual(parseDuration("90s"), 90000);
assert.strictEqual(parseDuration("1d2h3m4s"), 93784000);
for (const bad of ["", "1x", "30m1h", "h", "1.5h"]) assert.throws(() => parseDuration(bad), RangeError, JSON.stringify(bad));

// Randomized comparison against a brute-force minute scan of the specification.
let seed = 99;
const random = () => {
	seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
	return seed / 2 ** 32;
};
const names = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
const numbers = { mon: 1, tue: 2, wed: 3, thu: 4, fri: 5, sat: 6, sun: 0 };
for (let round = 0; round < 400; round += 1) {
	const hour = Math.floor(random() * 24);
	const minute = Math.floor(random() * 60);
	const allowed = new Set();
	const items = [];
	for (let item = 0; item < 1 + Math.floor(random() * 3); item += 1) {
		const a = Math.floor(random() * 7);
		if (random() < 0.5) {
			items.push(names[a]);
			allowed.add(numbers[names[a]]);
		} else {
			const b = Math.floor(random() * 7);
			items.push(names[a] + "-" + names[b]);
			for (let k = a; ; k = (k + 1) % 7) {
				allowed.add(numbers[names[k]]);
				if (k === b) break;
			}
		}
	}
	const offsetMinutes = (random() < 0.5 ? -1 : 1) * Math.floor(random() * 14) * 30;
	const abs = Math.abs(offsetMinutes);
	const offset = (offsetMinutes < 0 ? "-" : "+") + String(Math.floor(abs / 60)).padStart(2, "0") + ":" + String(abs % 60).padStart(2, "0");
	const from = Date.UTC(2026, 0, 1) + Math.floor(random() * 400 * 86400) * 1000 + (random() < 0.3 ? 0 : Math.floor(random() * 60000));
	let expected = null;
	for (let t = Math.floor(from / 60000) * 60000; ; t += 60000) {
		if (t <= from) continue;
		const local = new Date(t + offsetMinutes * 60000);
		if (local.getUTCHours() === hour && local.getUTCMinutes() === minute && allowed.has(local.getUTCDay())) {
			expected = new Date(t).toISOString();
			break;
		}
	}
	const spec = hour + ":" + String(minute).padStart(2, "0") + " " + items.join(",");
	assert.strictEqual(nextRun(spec, new Date(from).toISOString(), offset), expected, spec + " from " + new Date(from).toISOString() + " " + offset);
}
console.log("ok");
`;

const CACHE_BUGGY = code`
"""A TTL + LRU cache.

Specification:

- TTLCache(capacity, ttl, clock=time.monotonic). capacity >= 1 (else ValueError).
- An entry is expired when clock() - inserted_at >= ttl, where inserted_at is the clock value of
  the last put() of that key. get() never extends an entry's lifetime.
- get(key, default=None): if the key is stored and not expired, count a hit, mark it most recently
  used and return its value. If it is stored but expired, remove it, count an expiration and a
  miss, and return default. If it is not stored, count a miss and return default.
- put(key, value): if the key is stored (expired or not), replace its value, reset inserted_at and
  mark it most recently used; nothing is counted. Otherwise, if the cache already stores
  capacity entries (expired entries not yet removed count), first remove every expired entry
  (each counts as an expiration); if it still stores capacity entries, remove the least recently
  used entry (counts as an eviction). Then store the new entry as most recently used.
- "Recently used" is updated only by put() and by get() hits.
- delete(key): remove the key if stored; return True only if a non-expired entry was removed.
- len(cache): number of stored non-expired entries. key in cache: stored and not expired.
  Neither changes recency, statistics or contents.
- stats(): dict with integer keys hits, misses, evictions, expirations.
"""

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
        return now - inserted_at > self.ttl

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
        self._stats["hits"] += 1
        return value

    def put(self, key, value):
        now = self.clock()
        if key in self._items:
            inserted_at = self._items[key][1]
            self._items[key] = (value, inserted_at)
            self._items.move_to_end(key)
            return
        if len(self._items) >= self.capacity:
            self._items.popitem(last=False)
            self._stats["evictions"] += 1
        self._items[key] = (value, now)

    def delete(self, key):
        return self._items.pop(key, None) is not None

    def __len__(self):
        return len(self._items)

    def __contains__(self, key):
        if key not in self._items:
            return False
        return not self._expired(self._items[key][1], self.clock())

    def stats(self):
        return dict(self._stats)
`;

const CACHE_HIDDEN = code`
import random
from collections import OrderedDict

from cache import TTLCache


class Model:
    def __init__(self, capacity, ttl, clock):
        self.capacity, self.ttl, self.clock = capacity, ttl, clock
        self.items = OrderedDict()
        self.stats = {"hits": 0, "misses": 0, "evictions": 0, "expirations": 0}

    def expired(self, key, now):
        return now - self.items[key][1] >= self.ttl

    def get(self, key, default=None):
        now = self.clock()
        if key not in self.items:
            self.stats["misses"] += 1
            return default
        if self.expired(key, now):
            del self.items[key]
            self.stats["expirations"] += 1
            self.stats["misses"] += 1
            return default
        self.items.move_to_end(key)
        self.stats["hits"] += 1
        return self.items[key][0]

    def put(self, key, value):
        now = self.clock()
        if key in self.items:
            self.items[key] = (value, now)
            self.items.move_to_end(key)
            return None
        if len(self.items) >= self.capacity:
            for stale in [k for k in self.items if self.expired(k, now)]:
                del self.items[stale]
                self.stats["expirations"] += 1
        if len(self.items) >= self.capacity:
            self.items.popitem(last=False)
            self.stats["evictions"] += 1
        self.items[key] = (value, now)
        return None

    def delete(self, key):
        if key not in self.items:
            return False
        live = not self.expired(key, self.clock())
        del self.items[key]
        return live

    def length(self):
        now = self.clock()
        return sum(1 for key in self.items if not self.expired(key, now))

    def contains(self, key):
        return key in self.items and not self.expired(key, self.clock())


try:
    TTLCache(0, 1)
    raise SystemExit("capacity 0 must raise ValueError")
except ValueError:
    pass

rng = random.Random(7)
for case in range(3000):
    now = [0]
    clock = lambda: now[0]
    capacity, ttl = rng.randint(1, 5), rng.randint(1, 6)
    real, model = TTLCache(capacity, ttl, clock=clock), Model(capacity, ttl, clock)
    log = []
    for step in range(rng.randint(1, 40)):
        now[0] += rng.choice([0, 0, 1, 1, 2, 3])
        key = rng.randint(0, 7)
        op = rng.choice(["get", "get", "put", "put", "put", "delete", "len", "contains"])
        log.append((now[0], op, key))
        if op == "get":
            got, want = real.get(key, "miss"), model.get(key, "miss")
        elif op == "put":
            got, want = real.put(key, step), model.put(key, step)
        elif op == "delete":
            got, want = real.delete(key), model.delete(key)
        elif op == "len":
            got, want = len(real), model.length()
        else:
            got, want = key in real, model.contains(key)
        if got != want or real.stats() != model.stats:
            raise SystemExit(f"mismatch (capacity={capacity}, ttl={ttl}) after {log}: got {got!r}, stats {real.stats()}")
print("ok")
`;

// ---------------------------------------------------------------------------------------------
// Refactors checked by hidden property tests against a frozen copy of the original
// ---------------------------------------------------------------------------------------------

const INTERVALS_ORIGINAL = code`
"""Interval utilities. Endpoints are numbers (ints or floats) and may be given in either order."""


def normalize(intervals):
    """Merge intervals that overlap or lie within 1 of each other; return sorted (start, end) tuples."""
    items = [[min(a, b), max(a, b)] for a, b in intervals]
    merged = True
    while merged:
        merged = False
        for i in range(len(items)):
            for j in range(i + 1, len(items)):
                x, y = items[i], items[j]
                if y[0] <= x[1] + 1 and x[0] <= y[1] + 1:
                    items[i] = [min(x[0], y[0]), max(x[1], y[1])]
                    del items[j]
                    merged = True
                    break
            if merged:
                break
    return sorted((a, b) for a, b in items)


def covered(intervals):
    """Total length of normalize(intervals)."""
    return sum(b - a for a, b in normalize(intervals))


def overlaps(intervals, points):
    """For each point, how many of the given intervals contain it (endpoints inclusive)."""
    spans = [(min(a, b), max(a, b)) for a, b in intervals]
    return [sum(1 for a, b in spans if a <= p <= b) for p in points]
`;

const INTERVALS_HIDDEN = code`
import random
import signal
import time

import _orig_intervals_hidden as original
import intervals

rng = random.Random(5)


def sample(n, floats):
    out = []
    for _ in range(n):
        a = rng.uniform(-50, 50) if floats else rng.randint(-50, 50)
        b = a + (rng.uniform(-6, 6) if floats else rng.randint(-6, 6))
        out.append((a, b))
    return out


for case in range(600):
    floats = case % 3 == 0
    data = sample(rng.randint(0, 40), floats)
    points = [rng.uniform(-60, 60) if floats else rng.randint(-60, 60) for _ in range(rng.randint(0, 20))]
    points += [a for a, _ in data[:3]] + [b for _, b in data[:3]]
    assert intervals.normalize(data) == original.normalize(data), data
    assert intervals.normalize(iter(data)) == original.normalize(data), "must accept any iterable"
    assert intervals.covered(data) == original.covered(data), data
    assert intervals.overlaps(data, points) == original.overlaps(data, points), (data, points)
    assert intervals.overlaps(iter(data), iter(points)) == original.overlaps(data, points), "must accept any iterable"

assert intervals.normalize([]) == [] and intervals.covered([]) == 0 and intervals.overlaps([], [1]) == [0]
assert intervals.normalize([(3, 1), (4, 6)]) == [(1, 6)]

big = [(rng.randint(0, 10 ** 9), rng.randint(0, 10 ** 9)) for _ in range(200_000)]
big = [(a, a + (b % 5000)) for a, b in big]
queries = [rng.randint(0, 10 ** 9) for _ in range(200_000)]
for name, call in [
    ("normalize", lambda: intervals.normalize(big)),
    ("covered", lambda: intervals.covered(big)),
    ("overlaps", lambda: intervals.overlaps(big, queries)),
]:
    signal.alarm(30)  # a quadratic implementation would run for hours
    started = time.perf_counter()
    result = call()
    elapsed = time.perf_counter() - started
    signal.alarm(0)
    assert elapsed < 6, f"{name} took {elapsed:.1f}s on 200k intervals"
small = big[:2000]
assert intervals.normalize(small) == original.normalize(small)
assert intervals.overlaps(small, queries[:300]) == original.overlaps(small, queries[:300])
print("ok")
`;

const SHOP_ORIGINAL = code`
"""Shop domain logic: money helpers, carts, promotions and receipts."""

from dataclasses import dataclass, field
from decimal import ROUND_HALF_EVEN, Decimal

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

CURRENCY = "EUR"
PROMOTIONS = {}


def cents(amount):
    """Convert an amount in currency units (number or string) to integer cents, half-even."""
    return int((Decimal(str(amount)) * 100).quantize(Decimal(1), rounding=ROUND_HALF_EVEN))


def format_cents(value):
    sign = "-" if value < 0 else ""
    whole, frac = divmod(abs(value), 100)
    return f"{sign}{CURRENCY} {whole}.{frac:02d}"


def promotion(name):
    """Register a promotion: a function cart -> discount in cents."""

    def register(function):
        PROMOTIONS[name] = function
        return function

    return register


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


@promotion("TENOFF")
def ten_off(cart):
    return cart.subtotal() // 10


@promotion("BULK3")
def bulk_three(cart):
    return sum(item.price_cents for item in cart.items if item.qty >= 3)


@promotion("FREESHIP")
def free_shipping(cart):
    return 499 if cart.subtotal() >= 5000 else 0


def receipt(cart):
    lines = [f"{item.qty} x {item.sku} @ {format_cents(item.price_cents)}" for item in sorted(cart.items, key=lambda i: i.sku)]
    lines.append(f"subtotal {format_cents(cart.subtotal())}")
    for code in cart.codes:
        lines.append(f"promo {code} -{format_cents(PROMOTIONS[code](cart))}")
    lines.append(f"total {format_cents(cart.total())}")
    return "\n".join(lines)
`;

const SHOP_HIDDEN = code`
import importlib
import os
import pickle
import random
import subprocess
import sys

import _orig_shop_hidden as original

assert not os.path.exists("shop.py"), "shop.py must be replaced by the shop/ package"
assert os.path.isfile(os.path.join("shop", "__init__.py")), "shop/ must be a package"

for module in ["shop.money", "shop.cart", "shop.promotions", "shop.receipts", "shop"]:
    result = subprocess.run([sys.executable, "-c", f"import {module}"], capture_output=True, text=True)
    assert result.returncode == 0, f"import {module} failed on its own: {result.stderr[-400:]}"

import shop
import shop.cart
import shop.money
import shop.promotions
import shop.receipts

homes = {
    "cents": "shop.money",
    "format_cents": "shop.money",
    "promotion": "shop.promotions",
    "Item": "shop.cart",
    "Cart": "shop.cart",
    "ten_off": "shop.promotions",
    "bulk_three": "shop.promotions",
    "free_shipping": "shop.promotions",
    "receipt": "shop.receipts",
}
for name in original.__all__:
    assert hasattr(shop, name), f"shop.{name} missing"
for name, home in homes.items():
    obj = getattr(shop, name)
    assert obj.__module__ == home, f"{name} should be defined in {home}, not {obj.__module__}"
    assert getattr(importlib.import_module(home), name) is obj
assert shop.promotions.PROMOTIONS is shop.PROMOTIONS
assert shop.money.CURRENCY == "EUR"
assert set(shop.PROMOTIONS) == {"TENOFF", "BULK3", "FREESHIP"}

rng = random.Random(3)
for case in range(500):
    new_cart, old_cart = shop.Cart(), original.Cart()
    for _ in range(rng.randint(0, 8)):
        sku = rng.choice(["apple", "pear", "fig", "kiwi"])
        price = rng.choice([rng.randint(1, 3000) / 100, f"{rng.randint(0, 99)}.{rng.randint(0, 999):03d}", rng.randint(1, 60)])
        qty = rng.randint(1, 4)
        new_cart.add(sku, price, qty)
        old_cart.add(sku, price, qty)
    for code in rng.sample(["TENOFF", "BULK3", "FREESHIP"], rng.randint(0, 3)):
        new_cart.apply(code)
        old_cart.apply(code)
    assert (new_cart.subtotal(), new_cart.discount(), new_cart.total()) == (old_cart.subtotal(), old_cart.discount(), old_cart.total())
    assert shop.receipts.receipt(new_cart) == original.receipt(old_cart)
    assert shop.receipt(new_cart) == original.receipt(old_cart)
    assert pickle.loads(pickle.dumps(new_cart)) == new_cart
try:
    shop.Cart().apply("NOPE")
    raise SystemExit("unknown code must raise KeyError")
except KeyError:
    pass


@shop.promotion("FLAT1")
def flat_one(cart):
    return 100


cart = shop.Cart().add("fig", "2.50", 2).apply("FLAT1")
assert cart.total() == 400 and "FLAT1" in shop.promotions.PROMOTIONS


@shop.promotions.promotion("FLAT2")
def flat_two(cart):
    return 200


assert shop.Cart().add("fig", 5).apply("FLAT2").total() == 300
shop.money.CURRENCY = "USD"
assert shop.receipt(cart).splitlines()[-1] == "total USD 4.00", shop.receipt(cart)
assert shop.format_cents(-5) == "-USD 0.05"
print("ok")
`;

const RECORDS_ORIGINAL = code`
"""Reader for the .rec format.

A record starts at a line beginning with "%% record". Each following line is "key: value". A line
starting with a space or tab continues the previous value (joined with a newline, leading
whitespace removed). Lines starting with "#" are comments; blank lines are ignored. A key that
appears more than once in a record collects its values in a list, in order. A line beginning with
"%% end" closes the current record early (lines after it are ignored until the next "%% record").
"""


def parse_file(path):
    with open(path, encoding="utf-8") as handle:
        text = handle.read()
    text = text.replace("\r\n", "\n")
    records = []
    current = None
    last_key = None
    for line in text.split("\n"):
        if line.startswith("%% record"):
            current = {}
            records.append(current)
            last_key = None
            continue
        if line.startswith("%% end"):
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
    return records


def summarize(path):
    """Return (number of records, {key: number of records containing key})."""
    records = parse_file(path)
    counts = {}
    for record in records:
        for key in record:
            counts[key] = counts.get(key, 0) + 1
    return len(records), counts
`;

const RECORDS_HIDDEN = code`
import inspect
import os
import random
import tempfile
import tracemalloc

import _orig_records_hidden as original
import records

assert inspect.isgeneratorfunction(records.iter_records) or hasattr(records.iter_records(__file__), "__next__")
rng = random.Random(11)
pieces = [
    "%% record", "%% record extra", "%% end", "%%record", "", "   ", "\t", "# comment", "  # indented",
    "key: value", "key:value:with:colons", "other : spaced ", "no colon here", " continued", "\tcontinued tab",
    "dup: one", "dup: two", "unicode: café ☃", "empty:", ":novalue", "k: v\r", "  cont\r",
]
workdir = tempfile.mkdtemp()
for case in range(500):
    lines = [rng.choice(pieces) for _ in range(rng.randint(0, 40))]
    newline = rng.choice(["\n", "\r\n", "\n", "\r"])
    text = "".join(line + (newline if rng.random() < 0.9 else "\r\n") for line in lines)
    if rng.random() < 0.3:
        text = text.rstrip("\r\n")
    path = os.path.join(workdir, f"case{case}.rec")
    with open(path, "w", encoding="utf-8", newline="") as handle:
        handle.write(text)
    expected = original.parse_file(path)
    assert list(records.iter_records(path)) == expected, repr(text)
    assert records.parse_file(path) == expected, repr(text)
    assert records.summarize(path) == original.summarize(path), repr(text)

big = os.path.join(workdir, "big.rec")
with open(big, "w", encoding="utf-8", newline="") as handle:
    for index in range(250_000):
        handle.write(f"%% record\r\nid: {index}\nname: item {index} " + "x" * 60 + "\n  more text for the name\ntag: a\ntag: b\n# note\n")
size = os.path.getsize(big)
assert size > 30_000_000, size


def peak(call):
    tracemalloc.start()
    try:
        call()
        return tracemalloc.get_traced_memory()[1]
    finally:
        tracemalloc.stop()


def consume():
    count = 0
    for record in records.iter_records(big):
        count += 1
    assert count == 250_000, count


limit = 8 * 1024 * 1024
used = peak(consume)
assert used < limit, f"iter_records peak {used / 1e6:.1f} MB"
summary = []
used = peak(lambda: summary.append(records.summarize(big)))
assert used < limit, f"summarize peak {used / 1e6:.1f} MB"
assert summary[0] == (250_000, {"id": 250_000, "name": 250_000, "tag": 250_000}), summary[0]
print("ok")
`;

// ---------------------------------------------------------------------------------------------
// The frozen hard set
// ---------------------------------------------------------------------------------------------

const withFiles = (files, hidden) => () => ({ files, hidden });

export function tasks() {
	const list = [
		{
			id: "bugs-invoicing",
			category: "multi-bug",
			build: withFiles({ ...INVOICING_BUGGY, "SPEC.md": INVOICING_SPEC }, { "test_invoicing_hidden.py": INVOICING_HIDDEN }),
			prompts: [
				"Customers report wrong invoice totals from the `invoicing` package, and some valid orders are rejected. SPEC.md is the authoritative specification. Find and fix every place where the package deviates from SPEC.md (there is more than one). Keep the public API unchanged. A hidden test suite derived from SPEC.md will be run against the package.",
			],
			verify: "python3 -m unittest -q test_invoicing_hidden",
		},
		{
			id: "bugs-scheduler",
			category: "multi-bug",
			build: withFiles({ ...SCHED_BUGGY, "README.md": SCHED_README }, { "test_sched_hidden.js": SCHED_HIDDEN }),
			prompts: [
				"Users of the `sched` package (specified in README.md) see wrong next-run times, and some valid specs are rejected or accepted when they should not be. Find and fix every deviation from README.md across the package (there is more than one). A hidden test suite derived from README.md will be run with node.",
			],
			verify: "node test_sched_hidden.js",
		},
		{
			id: "bugs-ttl-cache",
			category: "multi-bug",
			build: withFiles(
				{
					"cache.py": CACHE_BUGGY,
					"test_cache.py": code`
from cache import TTLCache

cache = TTLCache(2, 10, clock=lambda: 0)
cache.put("a", 1)
assert cache.get("a") == 1
assert cache.get("b") is None
print("ok")
`,
				},
				{ "test_cache_hidden.py": CACHE_HIDDEN },
			),
			prompts: [
				"TTLCache in cache.py does not behave as its module docstring specifies. Fix every deviation (there are several) so that it conforms exactly to the docstring. A hidden randomized test will compare it against the specification.",
			],
			verify: "python3 test_cache_hidden.py",
		},
		{
			id: "refactor-intervals-fast",
			category: "refactor",
			build: withFiles({ "intervals.py": INTERVALS_ORIGINAL }, { "_orig_intervals_hidden.py": INTERVALS_ORIGINAL, "test_intervals_hidden.py": INTERVALS_HIDDEN }),
			prompts: [
				"intervals.py is correct but far too slow for production data (hundreds of thousands of intervals and query points). Rewrite normalize, covered and overlaps to run in roughly O(n log n) without changing their results for any input they currently accept: same return values and types, reversed endpoints, ints and floats, any iterable. Hidden tests compare the new code against the current implementation on many inputs and time it on 200,000 intervals.",
			],
			verify: "python3 test_intervals_hidden.py",
		},
		{
			id: "refactor-shop-package",
			category: "refactor",
			build: withFiles(
				{
					"shop.py": SHOP_ORIGINAL,
					"tests/test_shop.py": code`
import shop

cart = shop.Cart().add("apple", "1.20", 3).apply("TENOFF")
assert cart.total() == 324
print(shop.receipt(cart))
`,
				},
				{ "_orig_shop_hidden.py": SHOP_ORIGINAL, "test_shop_hidden.py": SHOP_HIDDEN },
			),
			prompts: [
				"Split shop.py into a package: shop/money.py (CURRENCY, cents, format_cents), shop/cart.py (Item, Cart), shop/promotions.py (PROMOTIONS, promotion and the built-in promotions), shop/receipts.py (receipt). Remove shop.py. `import shop` must keep exposing every name in the old `__all__`, and each submodule must be importable on its own. Behavior must stay identical, including: promotions registered later with shop.promotion or shop.promotions.promotion work in carts, and assigning shop.money.CURRENCY changes formatting everywhere, receipts included. Hidden tests check the structure and compare behavior against the original module.",
			],
			verify: "python3 test_shop_hidden.py",
		},
		{
			id: "refactor-streaming-records",
			category: "refactor",
			build: withFiles({ "records.py": RECORDS_ORIGINAL }, { "_orig_records_hidden.py": RECORDS_ORIGINAL, "test_records_hidden.py": RECORDS_HIDDEN }),
			prompts: [
				"records.py reads whole files into memory, and our .rec exports are now several GB. Add a generator `iter_records(path)` that yields the same records in the same order as `parse_file(path)` returns them while keeping memory bounded (stream the file; never hold it whole). Make `parse_file(path)` return `list(iter_records(path))` and make `summarize(path)` stream too. Results must be identical to the current implementation for every input file, including unusual line endings and other edge cases. Hidden tests compare against the current implementation on many generated files and measure peak memory on a 35 MB file.",
			],
			verify: "python3 test_records_hidden.py",
		},
		{
			id: "data-sessions",
			category: "large-data",
			build: buildSessions,
			prompts: [
				"events.jsonl (about 35 MB, one JSON object per line) holds web analytics events with fields event_id, user, ts (ISO-8601 with a UTC offset), type and ingest. Some events were ingested more than once (same event_id; other fields may differ): count each event_id once. Per user, order events by time; a session is a maximal run of that user's events in which consecutive events are at most 30 minutes apart (a gap of more than 30 minutes starts a new session). Write answer.json with exactly these keys: total_sessions (integer, over all users); top_user (the user with the most sessions; ties go to the lexicographically smallest id); median_session_seconds (median over all sessions of last event time minus first event time, in seconds; a one-event session lasts 0; for an even count use the mean of the two middle values); busiest_hour_utc (integer 0-23: the UTC hour of day containing the most distinct events; ties go to the smallest hour).",
			],
			verify: ANSWER_VERIFY,
		},
		{
			id: "data-ledger-reconcile",
			category: "large-data",
			build: buildLedger,
			prompts: [
				"Reconcile bank.csv (bank statement lines: date, reference, description, amount, currency) against ledger.csv (entry_id, posted_date, ref, amount_eur), using rates.csv (date, currency, eur_per_unit) for currency conversion. EUR amounts need no conversion; rates.csv has no EUR rows and no weekend rows. Convert each bank amount to EUR with the most recent rate for its currency dated on or before the bank line's date. A bank line matches the ledger entry whose ref equals its reference after trimming whitespace and ignoring case, provided the converted amount and amount_eur differ by at most 0.01; a line or entry without such a partner is unmatched. References are unique within each file. Write answer.json with: matched (number of matched pairs); unmatched_bank (number of unmatched bank lines); unmatched_ledger (number of unmatched ledger entries); unmatched_bank_total_eur (sum of the unrounded converted amounts of the unmatched bank lines, rounded to 2 decimals only at the end); largest_mismatch_ref (among references present in both files whose amounts differ by more than 0.01, the bank reference with the largest absolute difference between converted amount and amount_eur).",
			],
			verify: ANSWER_VERIFY,
		},
		{
			id: "data-sensors",
			category: "large-data",
			build: buildSensors,
			prompts: [
				"sensors/ holds CSV exports from temperature sensors (about 35 MB in 300 files). Files come in two layouts: comma-separated with header timestamp,sensor,temp_c,humidity (ISO timestamps) and semicolon-separated with header sensor;epoch;temp_f;humidity (Unix seconds, Fahrenheit). Missing temperatures are NA or empty: ignore those rows for temperature statistics. meta.csv maps sensor_id to site. Rows are not necessarily in time order. Write answer.json with: hottest_site (the site with the highest mean temperature in degrees Celsius over all valid readings of its sensors pooled together); hottest_site_mean_c (that mean, with at least 4 decimals); readings_above_40c (number of valid readings strictly above 40.0 C); max_gap_sensor and max_gap_seconds (the sensor with the longest gap between consecutive readings in time order, and that gap in seconds; every row counts as a reading here, even one with a missing temperature).",
			],
			verify: ANSWER_VERIFY,
		},
		{
			id: "data-dependencies",
			category: "large-data",
			build: buildDependencies,
			prompts: [
				"packages.jsonl (one JSON object per line with name, version, deps, description) lists every published version of every package in a registry. Only the latest version of each package matters, comparing versions numerically by major.minor.patch. The dependency graph has an edge from each package to every name in its latest version's deps. Write answer.json with: closure_size (number of distinct packages reachable from app-root, excluding app-root itself); max_depth (the largest shortest-path distance, in edges, from app-root to any reachable package); cycle_packages (number of packages in the whole graph that lie on at least one dependency cycle; a package that depends on itself counts); most_depended_on (the package with the most distinct direct dependents; ties go to the lexicographically smallest name).",
			],
			verify: ANSWER_VERIFY,
		},
		{
			id: "logs-revoked-tokens",
			category: "log-forensics",
			build: buildRevokedTokens,
			prompts: [
				"logs/ holds about two days of logs from an API: gateway.log (one line per request, UTC timestamps, req=<request id>); auth.jsonl (the authentication decision for each authenticated request: t in epoch milliseconds, request, user, token, result); db.log (database operations tagged req=<request id>; timestamps are local time UTC+02:00 in 2026); revocations.csv (token, revoked_at with a UTC offset, reason; a token may appear more than once and counts as revoked from its earliest revoked_at). Security wants to know how revoked tokens were still used. A request counts if its auth result was ok although its token had been revoked at or before the auth decision time t. Write answer.json with: revoked_requests (number of such requests); failed_writes (number of db.log write operations with status=FAILED belonging to those requests); top_user (the user with the most such failed writes; ties go to the lexicographically smallest user); first_request (the request id of the earliest such request by gateway.log timestamp).",
			],
			verify: ANSWER_VERIFY,
		},
		{
			id: "logs-deploy-asof",
			category: "log-forensics",
			build: buildDeployAsOf,
			prompts: [
				"Find which release hurt reliability. deploys.log lists deploy and rollback events for hosts web-01 to web-16 (ISO timestamps with various UTC offsets). access/<host>.log holds that host's requests: Unix time with milliseconds, method, path, status, bytes. At any instant a host runs the version of its most recent deploy; a rollback returns the host to the version it ran before the deploy being rolled back (possibly no version at all). A request is served by the version its host runs at the request's timestamp; an event at exactly that timestamp already applies. Write answer.json with: worst_version (the version with the highest 5xx error rate: requests with status 500-599 divided by all requests it served); worst_version_error_rate (that rate as a fraction, with at least 6 decimals); most_served_version (the version that served the most requests); unversioned_requests (requests served while their host ran no version); total_5xx (all 5xx responses).",
			],
			verify: ANSWER_VERIFY,
		},
		{
			id: "logs-bruteforce",
			category: "log-forensics",
			build: buildBruteForce,
			prompts: [
				"logs/ holds ten days of rotated web server access logs in combined log format: access.log (newest), access.log.1, then access.log.2.gz to access.log.9.gz (older, gzip-compressed). Timestamps carry the server's UTC offset, which changed during the period. An admin path is /admin or anything under /admin/ (ignore any query string). A breach event is a response with status 200 for an admin path at time T from an IP that received at least 5 responses with status 401 for admin paths at times t with T - 600 s <= t < T. Write answer.json with: breached_ips (sorted list of the distinct IPs with at least one breach event); breach_events (number of breach events); first_breach_utc (time of the earliest breach event as YYYY-MM-DDTHH:MM:SSZ).",
			],
			verify: ANSWER_VERIFY,
		},
		{
			id: "huge-customer-dedupe",
			category: "huge-file",
			build: buildCustomerDedupe,
			prompts: [
				"customers.csv (about 35 MB of RFC 4180 CSV: fields may be quoted and contain commas, quotes or newlines) has duplicate customers. Two rows are the same customer when their emails are equal after trimming whitespace and lower-casing. For each customer keep the row with the most recent updated_at (ISO-8601 timestamps with UTC offsets: compare instants); if several rows share that instant, keep the one appearing last in the file. Write dedup.csv: the same header, one row per customer with the email replaced by its normalized form and every other field exactly as in the kept row, rows sorted by normalized email (plain character order), valid CSV.",
			],
			verify: "python3 check_dedup_hidden.py",
		},
		{
			id: "huge-catalog-diff",
			category: "huge-file",
			build: buildCatalogDiff,
			prompts: [
				'catalog/old.json and catalog/new.json are two snapshots of a product catalog (about 20 MB each), shaped {"generated": ..., "items": [...]} with a unique id per item. Write diff.json as {"added": [...], "removed": [...], "changed": {...}}: added = ids only in new, removed = ids only in old (both sorted ascending); changed maps every id present in both whose item differs to the sorted list of top-level field names whose values differ. Compare values as JSON data (object key order never matters), except that tags is an unordered set: the same tags in a different order are not a change. Ids whose items are equal must not appear in changed.',
			],
			verify: "python3 check_diff_hidden.py",
		},
	];
	// Hidden checks here generate data and time code, so they get more room than the default 60 s.
	return list.map((task) => ({ verifyTimeoutMs: 300_000, ...task }));
}
