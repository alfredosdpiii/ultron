#!/usr/bin/env node
/**
 * Generator for the frozen research corpus behind evals/quality/tasks-research.mjs (`incident-root-causes`).
 * Committed for reproducibility; the eval never runs it and never calls a model (build() only unpacks the fixture).
 *
 *   node scripts/generate-research-corpus.mjs [--concurrency 4] [--limit N] [--cache path] [--out-dir evals/quality/fixtures]
 *
 * 1. Seeds are deterministic (seeded RNG): {id, label, true_cause, decoy_type, service, style, no_keyword, facts}.
 *    About 25 true positives (root cause: a TLS/SSL certificate that really expired; at least a third never use
 *    the substring "cert"), about 40 decoys (certificates prominent, root cause different) and the rest other causes.
 * 2. Each report is written by GENERATOR_MODEL through the local OpenAI-compatible proxy (cliproxyapi). The API key is
 *    read from ~/.ultron/agent/models.json at runtime and never printed or written.
 * 3. Every report is validated: (a) programmatic checks (own id and service present, no other incident id, length in
 *    range, keyword rules per subgroup) and (b) an independent label check by a second model (VALIDATOR_MODELS in
 *    order, first that answers; a different prompt from the generator's) that must agree with the seed label.
 *    A failing report is regenerated up to 3 times; then the seed is dropped and a spare seed of the same class is used.
 * 4. Output: <out-dir>/incident-root-causes.jsonl.gz ({id, path, text} per line, sorted by id) and
 *    <out-dir>/incident-root-causes.meta.json (labels per id plus generation stats). Print their sha256 to pin in the
 *    task file. Accepted reports are cached (--cache) so an interrupted run resumes without new model calls.
 */
import { createHash } from "node:crypto";
import { appendFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { gzipSync } from "node:zlib";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const PROXY = "http://127.0.0.1:8317/v1/chat/completions";
const GENERATOR_MODEL = "glm-5.3-flash";
const VALIDATOR_MODELS = ["gpt-5.6-sol", "glm-5.3-flash"];
const TOTALS = { tp: 25, decoy: 40, other: 335 };
const NO_KEYWORD_TP = 10;
const MIN_CHARS = 3000;
const MAX_CHARS = 8500;
const MAX_ATTEMPTS = 4; // one generation plus up to three regenerations

function arg(name, fallback) {
	const index = process.argv.indexOf(`--${name}`);
	return index === -1 ? fallback : process.argv[index + 1];
}

function rng(seed) {
	let state = seed >>> 0;
	return () => {
		state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
		return state / 2 ** 32;
	};
}
const pick = (random, values) => values[Math.floor(random() * values.length)];
const int = (random, low, high) => low + Math.floor(random() * (high - low + 1));
function shuffle(random, values) {
	for (let index = values.length - 1; index > 0; index -= 1) {
		const other = Math.floor(random() * (index + 1));
		[values[index], values[other]] = [values[other], values[index]];
	}
	return values;
}

const SERVICES = [
	"ledger-api", "checkout-gateway", "auth-broker", "payments-core", "search-indexer", "notif-dispatch", "orders-svc",
	"inventory-sync", "media-transcoder", "billing-exporter", "edge-proxy", "partner-webhooks", "kyc-verifier",
	"geo-router", "session-store", "reporting-etl", "mobile-bff", "feature-flags", "email-relay", "sso-bridge",
	"metrics-ingest", "vault-sidecar", "catalog-api", "pricing-engine", "fraud-scorer", "shipping-quotes",
	"tenant-admin", "audit-log", "chat-relay", "cdn-origin", "mq-bridge", "invoice-render", "ledger-reconciler",
	"warehouse-scanner", "loyalty-points", "status-page", "sms-gateway", "sftp-drop", "grpc-mesh", "cluster-ingress",
	"payout-scheduler", "doc-signer", "tax-calculator", "recommendations", "image-resizer", "webhook-fanout",
	"identity-graph", "carrier-sync", "support-desk-api", "export-worker",
];
const PEOPLE = [
	"Priya", "Marcus", "Ines", "Tomasz", "Aiko", "Deshawn", "Lena", "Ravi", "Olu", "Freya", "Mateo", "Svetlana", "Kwame",
	"Hana", "Diego", "Noor", "Bjorn", "Chen", "Amara", "Luca", "Yusuf", "Elif", "Jonah", "Mei", "Rafael", "Ana", "Kofi",
	"Greta", "Sanjay", "Wren",
];
const STYLES = {
	formal: "a formal postmortem with headed sections (Summary, Impact, Timeline, Root Cause, Contributing Factors, Remediation, Action Items)",
	terse: "terse on-call engineer notes: bullet points, abbreviations, lowercase, few full sentences",
	narrative: "a first-person narrative blog-style retrospective written by the incident commander, flowing paragraphs, few headings",
	chatlog: "a report dominated by pasted chat transcript excerpts (timestamped lines like `[14:02] name: ...`) with short commentary between them",
	ticket: "an issue-tracker ticket export: fields like Status/Severity/Owner/Components at the top, then description and a long comment thread",
	exec_email: "an email to leadership: plain-language summary first, then a technical appendix with timeline and log snippets",
	table: "a structured SRE template with a markdown timeline table (time | event | actor) and short sections",
	nonnative: "written by a non-native English speaker: slightly unusual grammar and phrasing but technically precise",
	five_whys: "a blameless review organized as a 'five whys' analysis followed by a timeline and remediation list",
	handoff: "a shift handoff document that grew into an incident writeup: status updates appended over time, some sections marked UPDATE",
};
const STYLE_NAMES = Object.keys(STYLES);

/** How a true positive's expiry is described. `noKeyword` phrasings must never contain the substring "cert". */
const TP_PHRASINGS = {
	keyword: [
		"State plainly that the TLS certificate served by the service expired (give the exact expiry timestamp) because renewal was a manual yearly task nobody owned.",
		"The narrative describes the cause only obliquely (a 'renewal that never happened'); the decisive evidence is client log lines containing `x509: certificate has expired or is not yet valid`, quoted in the timeline. The Root Cause statement must still make clear the serving certificate had expired.",
		"The ACME/Let's Encrypt automated renewal job had been silently failing for weeks (a changed DNS-01 credential), so the 90-day SSL certificate expired.",
		"An intermediate CA certificate in the served chain expired; the leaf was still valid, but clients rejected the chain.",
		"The mTLS client certificate the service presents to an upstream partner expired; the partner's gateway started rejecting every call.",
		"A Java keystore entry (the server certificate) passed its expiry date; the JVM-based service kept serving it after a restart.",
		"The wildcard SSL certificate on the load balancer expired; the renewal reminder emails went to a departed employee's mailbox.",
		"The certificate used by the internal service mesh sidecars expired because the rotation controller had been paused during a migration and never resumed.",
	],
	noKeyword: [
		"Describe the cause as: the leaf credential's validity window lapsed at 00:00 UTC and nobody had renewed it.",
		"Show `openssl s_client`-style output where notAfter is in the past, and state that the notAfter date had passed on the serving X.509 identity.",
		"Describe the cause as: the TLS chain went stale, meaning the intermediate issuer in the chain reached its end-of-validity date.",
		"Describe the cause as: the 90-day ACME-issued identity for the endpoint was never renewed because the renewal cron was deleted in a cleanup, and its validity ran out.",
		"Clients logged TLS handshake alert 45 ('expired' alert); explain that the peer's X.509 identity had aged past its validity period.",
		"Describe the cause as: the keypair+X.509 bundle in the JKS keystore reached its end date; renewal was a yearly calendar reminder that was lost.",
		"Describe the cause as: the mTLS client identity (a short-lived X.509 document issued by the internal CA) lapsed because the renewal agent crashed weeks earlier.",
		"Describe the cause as: the HTTPS endpoint's signed identity from the public CA expired on a Sunday; browsers told users the connection was not private because the site's identity had expired.",
	],
};

/** Decoys: certificates are prominent, the root cause is different. */
const DECOYS = {
	rotation_ruled_out:
		"A scheduled TLS certificate rotation happened the same morning and was the first suspect for hours; it was verified (new certificate valid, expiry a year out) and ruled out. The real root cause was {other}.",
	revoked_not_expired:
		"The service's TLS certificate was REVOKED by the CA (mis-issuance / key compromise report) while still well within its validity period; clients checking OCSP rejected it. Stress that it had not expired. Root cause: revocation and lack of a replacement procedure.",
	staging_expired_unrelated:
		"During the outage, someone noticed that the SSL certificate on an unrelated staging host (staging-{service}.internal) had expired days earlier; it was discussed at length and fixed later, but it had nothing to do with production. The real root cause was {other}.",
	suspected_was_dns:
		"The team suspected an expired certificate because clients showed TLS errors; checking showed the certificate was valid. The errors came from DNS pointing at an old decommissioned host (a stale DNS record after a migration). Root cause: DNS.",
	renewal_missing_intermediate:
		"The certificate was renewed in time before expiry, but the new certificate was deployed without the intermediate chain file, so some clients failed chain validation. Root cause: incomplete chain in the renewal deployment, not expiry.",
	hostname_mismatch:
		"A newly issued certificate (valid dates) lacked a SAN entry for one hostname, so clients calling that hostname got hostname-mismatch errors. Mention that expiry was checked and was fine. Root cause: missing SAN.",
	clock_skew:
		"A fleet of hosts had their clocks jump years into the future after a broken NTP change, so perfectly valid certificates were reported as `certificate has expired`. Root cause: clock skew / NTP misconfiguration; the certificates themselves were valid.",
	oauth_secret_expired:
		"Engineers first assumed a certificate expiry (the certificate dashboard was checked repeatedly and everything was green), but the actual cause was an expired OAuth client secret for the identity provider integration. Root cause: expired client secret, not a certificate.",
	ca_bundle_update:
		"An OS base-image update removed an old root CA from the trust store, so outbound calls to a vendor whose certificate chained to that root failed validation. The vendor certificate was not expired. Root cause: trust-store change.",
	expiry_alert_false_alarm:
		"A certificate-expiry monitor fired a (false) critical alert at the start of the incident and consumed the responders' attention; the certificate had months left. The real root cause was {other}.",
	past_incident_action_items:
		"The incident's root cause was {other}. The report repeatedly references last year's expired-certificate outage as a comparison, and its action items include certificate expiry monitoring improvements.",
	tls_version_disabled:
		"A security hardening change disabled TLS 1.0/1.1 on the edge; older partner clients could no longer connect. The team first checked certificate expiry dates (all valid). Root cause: protocol version change.",
};
const DECOY_NAMES = Object.keys(DECOYS);

/** Other root causes; some carry an unrelated 'expired' thing or an incidental TLS mention as noise. */
const OTHER_CAUSES = [
	"a database connection pool exhaustion after a traffic spike",
	"a disk filling up with unrotated debug logs",
	"a memory leak in a new release causing repeated OOM kills",
	"a bad configuration push that set a timeout to 0",
	"a primary database failover that left replicas lagging",
	"an upstream payment provider outage",
	"a DNS record deleted by a Terraform change",
	"a cache stampede after a cache cluster restart",
	"a runaway cron job saturating the database with full table scans",
	"a Kubernetes node pool upgrade that evicted pods without disruption budgets",
	"a message queue backlog after a consumer deadlock",
	"a rate limit on a third-party API being exceeded after a retry storm",
	"a schema migration that locked a large table",
	"a BGP route leak at the cloud provider",
	"a feature flag rolled out to 100% by mistake",
	"thread pool starvation from a blocking call in an async handler",
	"an expired software license key for the search engine cluster",
	"an expired domain registration for a marketing domain that also served an API redirect",
	"a service account password that expired under a 90-day rotation policy",
	"a Kerberos ticket-granting ticket that expired on a batch host",
	"cache entries with a TTL misconfigured to expire every second",
	"a leap-second-like timestamp bug in a scheduler",
	"a corrupted container image layer in the registry",
	"a load balancer health check pointed at the wrong port",
	"a noisy neighbour exhausting IOPS on shared storage",
	"a regex with catastrophic backtracking in request validation",
	"an integer overflow in an ID sequence",
	"a misrouted network ACL change blocking the database subnet",
	"a cloud provider zonal outage",
	"a dependency upgrade that changed JSON serialization of decimals",
	"an expired cloud billing payment card that suspended a storage bucket",
	"a split-brain in the Redis sentinel cluster",
	"a GC pause storm after heap size was halved",
	"a clock drift that broke token signature timestamps",
	"an exhausted IP address pool in the VPC subnet",
];
const INCIDENTAL = [
	"Mention in passing that TLS termination happens at the load balancer (irrelevant to the cause).",
	"Mention in passing that session tokens expire after 30 minutes (irrelevant to the cause).",
	"Mention in passing that a TLS handshake latency graph was checked and looked normal.",
	"Mention in passing that some cache keys expired during recovery (irrelevant to the cause).",
];

function makeSeeds() {
	const random = rng(20260926);
	const numbers = shuffle(random, Array.from({ length: 9000 }, (_, index) => index + 1000));
	let cursor = 0;
	const nextId = () => `INC-${String(numbers[cursor++]).padStart(4, "0")}`;
	const date = () => {
		const day = new Date(Date.UTC(2024, 0, 1) + int(random, 0, 900) * 86400000);
		return day.toISOString().slice(0, 10);
	};
	const base = () => ({
		service: pick(random, SERVICES),
		secondary: shuffle(random, [...SERVICES]).slice(0, 2),
		style: pick(random, STYLE_NAMES),
		date: date(),
		start: `${String(int(random, 0, 23)).padStart(2, "0")}:${String(int(random, 0, 59)).padStart(2, "0")}`,
		durationMin: int(random, 12, 400),
		people: shuffle(random, [...PEOPLE]).slice(0, int(random, 2, 4)),
		severity: pick(random, ["SEV1", "SEV2", "SEV2", "SEV3"]),
	});
	const make = (label, index) => {
		const facts = base();
		const seed = { id: nextId(), label, ...facts };
		if (label === "tp") {
			seed.no_keyword = index % 5 < 2; // 40% of true positives use no "cert" substring
			seed.true_cause = "expired_certificate";
			seed.phrasing = pick(random, seed.no_keyword ? TP_PHRASINGS.noKeyword : TP_PHRASINGS.keyword);
		} else if (label === "decoy") {
			seed.decoy_type = DECOY_NAMES[index % DECOY_NAMES.length];
			seed.other = pick(random, OTHER_CAUSES.slice(0, 16));
			seed.true_cause = `decoy:${seed.decoy_type}`;
		} else {
			seed.true_cause = pick(random, OTHER_CAUSES);
			if (random() < 0.3) seed.incidental = pick(random, INCIDENTAL);
		}
		return seed;
	};
	const pools = {};
	for (const [label, total] of Object.entries(TOTALS)) {
		// Spares follow the primaries so a dropped seed is replaced deterministically by the next one of its class.
		pools[label] = Array.from({ length: total + Math.ceil(total / 4) + 3 }, (_, index) => make(label, index));
	}
	return pools;
}

function generationPrompt(seed) {
	const cause =
		seed.label === "tp"
			? `ROOT CAUSE (must be unambiguous in the report): an expired TLS certificate. ${seed.phrasing}`
			: seed.label === "decoy"
				? `ROOT CAUSE SCENARIO: ${DECOYS[seed.decoy_type].replaceAll("{other}", seed.other).replaceAll("{service}", seed.service)} Certificates must be prominent in the report (discussed several times), but a careful reader must be able to tell the real root cause.`
				: `ROOT CAUSE: ${seed.true_cause}. ${seed.incidental ?? ""}`;
	const forbidden = seed.no_keyword
		? '\n\nHARD RULE: the text must never contain the letter sequence "cert" anywhere, in any case: do not write certificate, cert, certs, certbot, certain, certainly, uncertain, ascertain, concert, cert-manager. Refer to the thing only indirectly (X.509 identity, TLS credential, keypair, validity window, notAfter, chain, issuer). Avoid the word "certain" completely.'
		: "";
	return `Write a fictional internal incident report for a software company.

Incident id: ${seed.id} (use this exact id in the title/header; do not mention any other incident id)
Primary affected service: ${seed.service} (use this exact name); also involved: ${seed.secondary.join(", ")}
Date: ${seed.date}, first alert at ${seed.start} UTC, duration about ${seed.durationMin} minutes, severity ${seed.severity}
People involved: ${seed.people.join(", ")}
Style: ${STYLES[seed.style]}

${cause}

Include: a timeline with timestamps, at least one excerpt of chat messages between the people, some log or command output, customer impact, and remediation/follow-up items. Invent realistic but fictional detail (hostnames, metrics, error rates). Do not use a heading or sentence that literally announces the answer category like "Category: certificate expiry"; let the report read naturally.
Length: about 5000 characters (roughly 750-850 words); never under 3500 or over 7500 characters.
Output only the report text, no preamble.${forbidden}`;
}

function validationPrompt(text) {
	return `You are auditing an incident postmortem written by another team. Read it and determine the single primary root cause.

Question: was the primary ROOT CAUSE that a TLS/SSL (X.509) certificate genuinely reached or passed its expiry (notAfter) date and was still in use? This includes an expired leaf, intermediate or client certificate, however it is worded.
Answer false when certificates were only suspected and ruled out, were revoked but not expired, had a missing chain, a hostname mismatch or trust-store problem, only LOOKED expired because of a clock problem, when an expired certificate was on an unrelated host that did not cause the incident, or when some other credential (password, token, secret, license, domain) expired.

Reply with only a JSON object: {"root_cause": "<one sentence>", "expired_certificate_root_cause": true or false}

REPORT:
<<<
${text}
>>>`;
}

const API_KEY = (() => {
	const models = JSON.parse(readFileSync(join(homedir(), ".ultron", "agent", "models.json"), "utf8"));
	const key = models.providers?.cliproxyapi?.apiKey;
	if (!key) throw new Error("providers.cliproxyapi.apiKey missing in ~/.ultron/agent/models.json");
	return key;
})();

const stats = {
	httpCalls: 0,
	httpFailures: 0,
	generationCalls: 0,
	validationCalls: 0,
	promptTokens: 0,
	completionTokens: 0,
	regenerations: 0,
	regenerationReasons: {},
	dropped: [],
	cachedReused: 0,
};

async function chat(model, content, { maxTokens = 12000, temperature = 0.9 } = {}) {
	let lastError;
	for (let attempt = 0; attempt < 5; attempt += 1) {
		stats.httpCalls += 1;
		try {
			const response = await fetch(PROXY, {
				method: "POST",
				headers: { authorization: `Bearer ${API_KEY}`, "content-type": "application/json" },
				body: JSON.stringify({ model, messages: [{ role: "user", content }], max_tokens: maxTokens, temperature }),
				signal: AbortSignal.timeout(300_000),
			});
			const body = await response.json().catch(() => ({}));
			if (!response.ok) {
				lastError = new Error(`${model} HTTP ${response.status}: ${JSON.stringify(body.error ?? body).slice(0, 200)}`);
				stats.httpFailures += 1;
				if (response.status === 429 && /usage_limit|cooldown/.test(JSON.stringify(body))) throw Object.assign(lastError, { fatal: true });
				await new Promise((wait) => setTimeout(wait, 2000 * 2 ** attempt));
				continue;
			}
			stats.promptTokens += body.usage?.prompt_tokens ?? 0;
			stats.completionTokens += body.usage?.completion_tokens ?? 0;
			const text = body.choices?.[0]?.message?.content ?? "";
			if (!text.trim()) {
				lastError = new Error(`${model} returned empty content (finish ${body.choices?.[0]?.finish_reason})`);
				stats.httpFailures += 1;
				continue;
			}
			return text;
		} catch (error) {
			if (error.fatal) throw error;
			lastError = error;
			stats.httpFailures += 1;
			await new Promise((wait) => setTimeout(wait, 2000 * 2 ** attempt));
		}
	}
	throw lastError;
}

let validatorModel;
async function pickValidator() {
	for (const model of VALIDATOR_MODELS) {
		try {
			await chat(model, 'Reply with only the JSON {"ok": true}.', { maxTokens: 2000, temperature: 0 });
			return model;
		} catch (error) {
			console.error(`validator ${model} unavailable: ${String(error.message).slice(0, 160)}`);
		}
	}
	throw new Error("no validator model answered");
}

const INCIDENT_ID = /INC-\d{4}/g;
function programmaticProblems(seed, text) {
	const problems = [];
	if (!text.includes(seed.id)) problems.push("id missing");
	if ((text.match(INCIDENT_ID) ?? []).some((id) => id !== seed.id)) problems.push("other incident id");
	if (!text.includes(seed.service)) problems.push("service missing");
	if (text.length < MIN_CHARS || text.length > MAX_CHARS) problems.push(`length ${text.length}`);
	const certs = (text.match(/cert/gi) ?? []).length;
	if (seed.no_keyword && certs > 0) problems.push("cert substring in no-keyword report");
	if (seed.label === "tp" && !seed.no_keyword && certs === 0) problems.push("keyword report without cert");
	if (seed.label === "decoy" && certs < 3) problems.push("decoy mentions certificates fewer than 3 times");
	return problems;
}

function parseVerdict(reply) {
	const match = reply.match(/\{[\s\S]*\}/);
	if (!match) return null;
	try {
		const parsed = JSON.parse(match[0]);
		return typeof parsed.expired_certificate_root_cause === "boolean" ? parsed : null;
	} catch {
		return null;
	}
}

function noteRegeneration(reason) {
	stats.regenerations += 1;
	stats.regenerationReasons[reason] = (stats.regenerationReasons[reason] ?? 0) + 1;
}

/** Generate and validate one seed; null when every attempt failed. */
async function produce(seed) {
	for (let attempt = 1; attempt <= MAX_ATTEMPTS; attempt += 1) {
		if (attempt > 1) console.log(`  regenerate ${seed.id} (${seed.label}) attempt ${attempt}`);
		stats.generationCalls += 1;
		let text;
		try {
			text = (await chat(GENERATOR_MODEL, generationPrompt(seed))).trim();
		} catch (error) {
			if (attempt < MAX_ATTEMPTS) noteRegeneration("generator error");
			console.log(`  ${seed.id}: generator error ${String(error.message).slice(0, 120)}`);
			continue;
		}
		const problems = programmaticProblems(seed, text);
		if (problems.length) {
			if (attempt < MAX_ATTEMPTS) noteRegeneration(`programmatic: ${problems[0].replace(/\d+/g, "N")}`);
			console.log(`  ${seed.id}: ${problems.join("; ")}`);
			continue;
		}
		stats.validationCalls += 1;
		let verdict = null;
		try {
			verdict = parseVerdict(await chat(validatorModel, validationPrompt(text), { temperature: 0 }));
		} catch (error) {
			console.log(`  ${seed.id}: validator error ${String(error.message).slice(0, 120)}`);
		}
		if (!verdict) {
			if (attempt < MAX_ATTEMPTS) noteRegeneration("validator unparseable");
			continue;
		}
		const expected = seed.label === "tp";
		if (verdict.expired_certificate_root_cause !== expected) {
			if (attempt < MAX_ATTEMPTS) noteRegeneration(`label disagreement (${seed.label})`);
			console.log(`  ${seed.id}: validator disagrees (${seed.label}): ${String(verdict.root_cause).slice(0, 140)}`);
			continue;
		}
		return { text, validatorRootCause: String(verdict.root_cause ?? "").slice(0, 300), attempts: attempt };
	}
	return null;
}

async function main() {
	const concurrency = Number(arg("concurrency", "4"));
	const limit = Number(arg("limit", "0"));
	const outDir = resolve(root, arg("out-dir", "evals/quality/fixtures"));
	const cachePath = resolve(arg("cache", join(outDir, ".incident-root-causes.cache.jsonl")));
	const statsPath = `${cachePath}.stats.json`;
	// Stats accumulate across resumed runs, so the recorded call count covers every call ever made for this corpus.
	if (existsSync(statsPath)) Object.assign(stats, JSON.parse(readFileSync(statsPath, "utf8")), { cachedReused: 0 });
	const saveStats = () => writeFileSync(statsPath, `${JSON.stringify(stats)}\n`);
	const cache = new Map();
	if (existsSync(cachePath))
		for (const line of readFileSync(cachePath, "utf8").split("\n").filter(Boolean)) {
			const entry = JSON.parse(line);
			cache.set(entry.id, entry);
		}
	mkdirSync(dirname(cachePath), { recursive: true });
	validatorModel = await pickValidator();
	console.log(`generator ${GENERATOR_MODEL}, validator ${validatorModel}, cached ${cache.size}`);

	const pools = makeSeeds();
	const queues = Object.fromEntries(Object.entries(pools).map(([label, pool]) => [label, { pool, next: TOTALS[label] }]));
	let jobs = Object.entries(TOTALS).flatMap(([label, total]) => pools[label].slice(0, total));
	if (limit) jobs = jobs.filter((_, index) => index % Math.ceil(jobs.length / limit) === 0).slice(0, limit);
	const accepted = [];
	let cursor = 0;
	let done = 0;
	const started = Date.now();
	await Promise.all(
		Array.from({ length: concurrency }, async () => {
			while (cursor < jobs.length) {
				let seed = jobs[cursor++];
				for (;;) {
					const cached = cache.get(seed.id);
					const result = cached ?? (await produce(seed));
					if (cached) stats.cachedReused += 1;
					if (result) {
						if (!cached) {
							const entry = { id: seed.id, text: result.text, validatorRootCause: result.validatorRootCause, attempts: result.attempts };
							mkdirSync(dirname(cachePath), { recursive: true });
							appendFileSync(cachePath, `${JSON.stringify(entry)}\n`);
						}
						accepted.push({ seed, ...result });
						break;
					}
					stats.dropped.push({ id: seed.id, label: seed.label, decoy_type: seed.decoy_type ?? null, no_keyword: !!seed.no_keyword });
					const queue = queues[seed.label];
					// Keep the no-keyword share of true positives: replace like with like.
					let replacement;
					while (queue.next < queue.pool.length) {
						const candidate = queue.pool[queue.next++];
						if (seed.label !== "tp" || !!candidate.no_keyword === !!seed.no_keyword) {
							replacement = candidate;
							break;
						}
					}
					if (!replacement) throw new Error(`ran out of spare ${seed.label} seeds`);
					if (seed.label === "decoy") replacement.decoy_type = seed.decoy_type;
					if (seed.label === "decoy") replacement.true_cause = `decoy:${seed.decoy_type}`;
					console.log(`  dropped ${seed.id}; replacing with ${replacement.id}`);
					seed = replacement;
				}
				done += 1;
				saveStats();
				if (done % 10 === 0 || done === jobs.length)
					console.log(`${done}/${jobs.length} accepted, ${stats.httpCalls} calls, ${((Date.now() - started) / 60000).toFixed(1)} min`);
			}
		}),
	);

	accepted.sort((a, b) => a.seed.id.localeCompare(b.seed.id));
	const fixture = gzipSync(
		`${accepted.map(({ seed, text }) => JSON.stringify({ id: seed.id, path: `incidents/${seed.id}.md`, text })).join("\n")}\n`,
		{ level: 9 },
	);
	const labels = Object.fromEntries(
		accepted.map(({ seed, attempts, validatorRootCause }) => [
			seed.id,
			{
				label: seed.label,
				true_cause: seed.true_cause,
				decoy_type: seed.decoy_type ?? null,
				no_keyword: !!seed.no_keyword,
				service: seed.service,
				style: seed.style,
				attempts,
				validator_root_cause: validatorRootCause,
			},
		]),
	);
	const count = (predicate) => Object.values(labels).filter(predicate).length;
	const bytes = accepted.reduce((sum, { text }) => sum + Buffer.byteLength(text), 0);
	const meta = {
		task: "incident-root-causes",
		generatorModel: GENERATOR_MODEL,
		validatorModel,
		corpus: {
			reports: accepted.length,
			bytes,
			truePositives: count((entry) => entry.label === "tp"),
			noKeywordTruePositives: count((entry) => entry.label === "tp" && entry.no_keyword),
			decoys: count((entry) => entry.label === "decoy"),
			other: count((entry) => entry.label === "other"),
		},
		stats: { ...stats, dropped: stats.dropped, modelCalls: stats.httpCalls },
		labels,
	};
	mkdirSync(outDir, { recursive: true });
	const fixturePath = join(outDir, "incident-root-causes.jsonl.gz");
	const metaPath = join(outDir, "incident-root-causes.meta.json");
	writeFileSync(fixturePath, fixture);
	writeFileSync(metaPath, `${JSON.stringify(meta, null, 2)}\n`);
	const sha = (buffer) => createHash("sha256").update(buffer).digest("hex");
	console.log(JSON.stringify({ ...meta, labels: undefined }, null, 2));
	console.log(`fixture ${fixturePath} sha256 ${sha(fixture)}`);
	console.log(`meta ${metaPath} sha256 ${sha(readFileSync(metaPath))}`);
}

await main();
