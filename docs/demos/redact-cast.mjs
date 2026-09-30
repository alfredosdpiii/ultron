#!/usr/bin/env node
// Redacts personal details from an asciinema v2 cast before it is rendered, and checks that none are left.
//   node redact-cast.mjs <file.cast>          rewrite the file in place
//   node redact-cast.mjs --check <file.cast>  exit 1 if anything personal or key-like remains
// Rewriting also drops the exit: every event after DEMO_CUT seconds (record.sh sets it when it starts quitting), or
// else from the first alternate-screen exit on, so the GIF ends on the finished screen rather than the shell.
// Replacements keep the same length so terminal layouts stay aligned.
import { readFileSync, writeFileSync } from "node:fs";
import { hostname, userInfo } from "node:os";

const args = process.argv.slice(2);
const check = args[0] === "--check";
const file = check ? args[1] : args[0];
const { username, homedir } = userInfo();
const extra = (process.env.DEMO_REDACT ?? "").split(",").filter(Boolean);

const escape = (text) => text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
const same = (text, fill) => fill.repeat(Math.ceil(text.length / fill.length)).slice(0, text.length);
const literals = [homedir, username, hostname(), ...extra].filter((s) => s && s.length >= 3);
const patterns = [
	/[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}/g, // e-mail addresses
	/\b(sk|rk|pk)-[A-Za-z0-9_-]{12,}/g, // API keys
	/\b(ghp|gho|github_pat|xox[abp])_[A-Za-z0-9_]{12,}/g,
	/\bAKIA[0-9A-Z]{16}\b/g,
];

function redact(text) {
	let out = text;
	for (const literal of literals)
		out = out.replace(new RegExp(escape(literal), "gi"), (m) => same(m, literal.startsWith("/") ? "/x" : "x"));
	for (const pattern of patterns) out = out.replace(pattern, (m) => same(m, "*"));
	return out;
}

function problems(text) {
	const found = [];
	for (const literal of literals)
		if (new RegExp(escape(literal), "i").test(text)) found.push(`literal ${literal.length} chars`);
	for (const pattern of patterns) for (const m of text.matchAll(pattern)) found.push(`${m[0].slice(0, 3)}…`);
	if (text.includes("/home/")) found.push("/home/ path");
	return found;
}

const lines = readFileSync(file, "utf8").split("\n");
if (check) {
	const found = new Set(lines.flatMap(problems));
	if (found.size) {
		console.error(`${file}: personal or key-like text remains: ${[...found].join(", ")}`);
		process.exit(1);
	}
	console.log(`${file}: clean`);
} else {
	const out = lines.map((line, index) => {
		if (index === 0 || !line.trim()) return line; // header: dimensions only (env is not recorded with -q? it is: strip it)
		const event = JSON.parse(line);
		event[2] = redact(event[2]);
		return JSON.stringify(event);
	});
	const events = out.slice(1).filter((line) => line.trim());
	const until = Number(process.env.DEMO_CUT || Number.POSITIVE_INFINITY);
	let cut = events.findIndex((line) => {
		const [time, , data] = JSON.parse(line);
		return time > until || data.includes("\u001b[?1049l");
	});
	if (cut < 0) cut = events.length;
	out.splice(1, out.length - 1, ...events.slice(0, cut));
	const header = JSON.parse(out[0]);
	delete header.env;
	delete header.title;
	out[0] = JSON.stringify(header);
	writeFileSync(file, out.join("\n"));
}
