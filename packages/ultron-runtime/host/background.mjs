/**
 * Explicit background jobs, not transparent detachment of the current Pi TUI.
 *
 * Integration, with no registration or settings changes:
 *   import { createBackgroundRunner } from '/home/user/.pi/host/background.mjs';
 *   const jobs = createBackgroundRunner();
 *   const job = await jobs.start('Review this repository', { cwd: '/path/to/repo' });
 *   const records = jobs.list();
 *   const record = jobs.inspect(job.id);
 *   const stopped = await jobs.stop(job.id);
 *
 * createBackgroundRunner({ directory, piExecutable, limits }) is also supported.
 * Defaults: ~/.pi/background-jobs and the executable `pi` resolved on PATH.
 * start(prompt, { cwd, env }) inherits the environment, with optional overrides.
 * piExecutable is a trusted executable path, not a shell command. Tests inject
 * an executable fake RPC peer. Credentials in env are inherited, not persisted.
 *
 * Each call to start creates a new job and session. Only call it for an explicit
 * user request. There is no idempotent resubmission, resume, retry, permission UI,
 * spend gate, or task-duration limit. The runner sends exactly one prompt over
 * stdin to `pi --mode rpc --offline --session-dir <job>/session`. Offline disables
 * startup network checks, NOT model calls. Pi's own configured behavior still
 * applies. We do not send settings-mutating RPC commands such as set_auto_retry.
 * An observed error stops the job, even if Pi intended to retry it.
 *
 * start resolves after a durable prompt acceptance acknowledgement or a terminal
 * startup result. It can return failed/interrupted/stopped, not just running.
 * delivery is not_sent, uncertain, accepted, or rejected. Uncertain means effects
 * might have happened. Inspect that job; NEVER automatically submit it again.
 * completed requires both prompt acceptance and agent_settled with no observed
 * RPC, assistant, tool-message, extension, framing, or storage error. agent_end
 * alone and exit code zero are not completion. Dialog requests fail unattended
 * jobs rather than hanging or approving permissions. Notifications are logged.
 *
 * Records: status.json, request.json, events.jsonl, stdout.log, stderr.log,
 * worker.log, session/. Status includes paths, process identities, delivery,
 * timestamps, bounded error text, output byte counts, and truncation flags.
 * JSON status uses atomic rename and fsync. Output files keep bounded prefixes;
 * events.jsonl keeps whole LF-delimited records. Parsing continues after storage
 * caps so late errors are not lost. Session files are Pi-managed and NOT capped,
 * nor are arbitrary files written by model tools. Delete terminal job directories
 * manually when no longer needed; there is no automatic retention policy.
 *
 * Linux only: /proc start times, boot ID, UID, and process-group checks prevent
 * stale PID signalling. The root and job directories must be owned by this UID,
 * non-symlinks, mode 0700; files are 0600. Existing unsafe paths are rejected,
 * not chmodded. This is OS-user authentication, not isolation from the same UID
 * or root. Keep parent directories trusted. No network control endpoint exists.
 *
 * A detached Node worker is the process-group/session leader. Pi inherits that
 * group and uses pipes owned by the worker, never the launching terminal. Stop
 * writes a private request; the worker sends group SIGTERM, drains output for
 * shutdownGraceMs, persists the final record, then SIGKILLs its own group. This
 * also kills descendants that ignore SIGTERM. Descendants that deliberately
 * escape using setsid are outside this process-group contract. A killed owner
 * becomes interrupted on inspect/list; stop can clean up surviving authenticated
 * group members. No work is replayed after owner death or reboot. This is not a
 * boot service and cannot survive a system shutdown or a login manager killing
 * all user processes. Storage failure stops work; if status cannot be written,
 * inspect reports the dead owner as interrupted instead of claiming success.
 */
import {
  constants as F, openSync, closeSync, fstatSync, fsyncSync, writeSync,
  readFileSync, lstatSync, mkdirSync, readdirSync, renameSync, unlinkSync,
} from 'node:fs';
import { join, resolve, dirname } from 'node:path';
import { homedir } from 'node:os';
import { randomUUID } from 'node:crypto';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';

export const BACKGROUND_LIMITS = Object.freeze({
  maxFrameBytes: 8 * 1024 * 1024,
  eventsBytes: 32 * 1024 * 1024,
  stdoutBytes: 8 * 1024 * 1024,
  stderrBytes: 2 * 1024 * 1024,
  workerBytes: 64 * 1024,
  startupTimeoutMs: 30_000,
  shutdownGraceMs: 1_000,
  heartbeatMs: 1_000,
});
const active = new Set(['starting', 'running', 'finishing', 'stopping']);
const idPattern = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const workerScript = fileURLToPath(new URL('../scripts/background.mjs', import.meta.url));
const now = () => new Date().toISOString();
const brief = value => String(value?.message ?? value).slice(0, 4096);

function platform() {
  if (process.platform !== 'linux' || typeof process.getuid !== 'function') {
    throw new Error('Background jobs require Linux /proc and Unix process groups');
  }
}
function secureDirectory(path, create = false) {
  if (create) {
    try { mkdirSync(path, { mode: 0o700 }); }
    catch (error) { if (error.code !== 'EEXIST') throw error; }
  }
  const stat = lstatSync(path);
  if (!stat.isDirectory() || stat.uid !== process.getuid() || (stat.mode & 0o777) !== 0o700) {
    throw new Error(`Unsafe background directory, expected owned mode 0700: ${path}`);
  }
  return path;
}
function privateFile(path, flags) {
  const fd = openSync(path, flags | F.O_NOFOLLOW | F.O_NONBLOCK, 0o600);
  const stat = fstatSync(fd);
  if (!stat.isFile() || stat.uid !== process.getuid() || (stat.mode & 0o777) !== 0o600 || stat.nlink !== 1) {
    closeSync(fd);
    throw new Error(`Unsafe background file, expected owned mode 0600: ${path}`);
  }
  return fd;
}
function readJSON(path) {
  const fd = privateFile(path, F.O_RDONLY);
  try { return JSON.parse(readFileSync(fd, 'utf8')); }
  finally { closeSync(fd); }
}
function writeAll(fd, data) {
  const bytes = Buffer.isBuffer(data) ? data : Buffer.from(data);
  let offset = 0;
  while (offset < bytes.length) {
    const written = writeSync(fd, bytes, offset, bytes.length - offset);
    if (!written) throw new Error('Storage made no write progress');
    offset += written;
  }
}
function atomicJSON(path, data) {
  const temp = `${path}.${randomUUID()}.tmp`;
  const fd = privateFile(temp, F.O_WRONLY | F.O_CREAT | F.O_EXCL);
  try { writeAll(fd, `${JSON.stringify(data)}\n`); fsyncSync(fd); }
  catch (error) { try { unlinkSync(temp); } catch {} throw error; }
  finally { closeSync(fd); }
  try {
    renameSync(temp, path);
    const dir = openSync(dirname(path), F.O_RDONLY | F.O_DIRECTORY | F.O_NOFOLLOW);
    try { fsyncSync(dir); } finally { closeSync(dir); }
  } finally { try { unlinkSync(temp); } catch {} }
}
function identity(pid) {
  if (!Number.isSafeInteger(pid) || pid <= 1) return null;
  try {
    if (lstatSync(`/proc/${pid}`).uid !== process.getuid()) return null;
    const raw = readFileSync(`/proc/${pid}/stat`, 'utf8');
    const fields = raw.slice(raw.lastIndexOf(')') + 2).split(' ');
    return {
      pid, start: fields[19], group: Number(fields[2]), session: Number(fields[3]),
      boot: readFileSync('/proc/sys/kernel/random/boot_id', 'utf8').trim(),
      state: fields[0],
    };
  } catch (error) {
    if (['ENOENT', 'ESRCH'].includes(error.code)) return null;
    throw error;
  }
}
function matches(saved, includeZombie = false) {
  const current = saved && identity(saved.pid);
  return Boolean(current && current.start === saved.start && current.boot === saved.boot &&
    current.group === saved.group && current.session === saved.session &&
    (includeZombie || !['Z', 'X'].includes(current.state)));
}
function jobPath(root, id) {
  if (!idPattern.test(id)) throw new Error('Invalid background job id');
  return secureDirectory(join(root, id));
}
function load(root, id) {
  const path = jobPath(root, id);
  const record = readJSON(join(path, 'status.json'));
  if (record.id !== id || record.uid !== process.getuid() || record.version !== 1) {
    throw new Error(`Invalid background record: ${id}`);
  }
  return record;
}
function save(root, record) {
  record.updatedAt = now();
  atomicJSON(join(jobPath(root, record.id), 'status.json'), record);
}
function claimIdentity(path) {
  try { return readJSON(join(path, 'owner.json')); }
  catch (error) { if (error.code === 'ENOENT') return null; throw error; }
}
function reconcile(root, id) {
  const record = load(root, id);
  if (!active.has(record.status)) return record;
  const owner = record.owner ?? claimIdentity(jobPath(root, id));
  if (owner && matches(owner)) return record;
  if (!owner && Date.now() < Date.parse(record.startupDeadline)) return record;
  record.status = 'interrupted';
  record.error = 'Worker disappeared before a terminal result. Effects may have occurred; do not automatically retry.';
  record.endedAt = now();
  save(root, record);
  return record;
}
function groupWitnesses(record) {
  const owner = record.owner;
  if (!owner || owner.group !== owner.pid || owner.session !== owner.pid) return [];
  const anchors = [owner, record.pi].filter(value => value && matches(value, true));
  if (!anchors.some(value => value.group === owner.pid && value.session === owner.pid)) return [];
  return readdirSync('/proc').filter(name => /^\d+$/.test(name)).map(name => identity(Number(name)))
    .filter(value => value && value.group === owner.pid && value.session === owner.pid);
}
function signalWitnesses(witnesses, signal) {
  const witness = witnesses.find(value => matches(value, true));
  if (!witness) return false;
  try { process.kill(-witness.group, signal); return true; }
  catch (error) { if (error.code === 'ESRCH') return false; throw error; }
}
function limitsFrom(options = {}) {
  for (const [key, value] of Object.entries(options)) {
    if (!(key in BACKGROUND_LIMITS) || !Number.isSafeInteger(value) || value <= 0 || value > 2 ** 30) {
      throw new Error(`Invalid background health limit: ${key}`);
    }
  }
  return { ...BACKGROUND_LIMITS, ...options };
}

export function createBackgroundRunner({
  directory = join(homedir(), '.pi', 'background-jobs'),
  piExecutable = 'pi', limits = {},
} = {}) {
  platform();
  const root = resolve(directory);
  // Only create our own root. Do not chmod or modify existing settings directories.
  if (root === join(homedir(), '.pi', 'background-jobs')) {
    try { mkdirSync(dirname(root), { mode: 0o700 }); }
    catch (error) { if (error.code !== 'EEXIST') throw error; }
  }
  secureDirectory(root, true);
  const health = limitsFrom(limits);
  if (typeof piExecutable !== 'string' || !piExecutable || piExecutable.includes('\0')) {
    throw new Error('piExecutable must be an executable name or path');
  }
  const executable = piExecutable.includes('/') ? resolve(piExecutable) : piExecutable;
  const runner = {
    directory: root,
    inspect(id) { secureDirectory(root); return reconcile(root, id); },
    list() {
      secureDirectory(root);
      return readdirSync(root).filter(id => idPattern.test(id)).map(id => runner.inspect(id))
        .sort((a, b) => b.createdAt.localeCompare(a.createdAt) || a.id.localeCompare(b.id));
    },
    async start(prompt, { cwd = process.cwd(), env = {} } = {}) {
      if (typeof prompt !== 'string' || !prompt.trim()) throw new Error('A nonempty explicit prompt is required');
      secureDirectory(root);
      const id = randomUUID();
      const path = secureDirectory(join(root, id), true);
      const session = secureDirectory(join(path, 'session'), true);
      const record = {
        version: 1, uid: process.getuid(), id, status: 'starting', delivery: 'not_sent',
        createdAt: now(), updatedAt: now(),
        startupDeadline: new Date(Date.now() + health.startupTimeoutMs).toISOString(),
        cwd: resolve(cwd), owner: null, pi: null, acceptedAt: null, endedAt: null, error: null,
        paths: Object.fromEntries(['status.json', 'request.json', 'events.jsonl', 'stdout.log', 'stderr.log', 'worker.log']
          .map(name => [name, join(path, name)])), sessionDir: session, limits: health, output: {},
      };
      atomicJSON(join(path, 'request.json'), { prompt, piExecutable: executable });
      save(root, record);
      let worker;
      try {
        worker = spawn(process.execPath, [workerScript, '--worker', root, id], {
          detached: true, stdio: 'ignore', cwd: record.cwd, env: { ...process.env, ...env },
        });
        worker.on('error', error => {
          try { markBackgroundWorkerFailure(root, id, error); } catch {}
        });
        worker.on('exit', (code, signal) => {
          if (code !== 0 || signal) {
            try { markBackgroundWorkerFailure(root, id, new Error(`worker exited ${signal ?? code}`)); } catch {}
          }
        });
        await new Promise((resolveSpawn, reject) => {
          worker.once('spawn', resolveSpawn);
          worker.once('error', reject);
        });
        worker.unref();
      } catch (error) {
        record.status = 'failed'; record.error = `Worker startup: ${brief(error)}`; record.endedAt = now();
        save(root, record);
        return record;
      }
      const deadline = Date.now() + health.startupTimeoutMs + health.shutdownGraceMs + 2000;
      while (Date.now() < deadline) {
        const current = runner.inspect(id);
        if (current.acceptedAt || !active.has(current.status) && current.status !== 'finishing') return current;
        if (current.status === 'finishing' && current.endedAt) return current;
        await delay(25);
      }
      // No acknowledgement. Stop the one attempt; never create a replacement.
      const current = await runner.stop(id);
      if (current.status === 'stopped') {
        current.status = current.delivery === 'not_sent' ? 'failed' : 'interrupted';
        current.error = 'Startup acknowledgement timed out. Inspect delivery and artifacts; do not automatically retry.';
        save(root, current);
      }
      return current;
    },
    async stop(id) {
      let record = runner.inspect(id);
      if (!active.has(record.status) && record.status !== 'interrupted') return record;
      const path = jobPath(root, id);
      atomicJSON(join(path, 'stop.json'), { id, uid: process.getuid(), requestedAt: now() });
      const deadline = Date.now() + record.limits.shutdownGraceMs + 2000;
      while (matches(record.owner) && Date.now() < deadline) {
        record = runner.inspect(id);
        if (!active.has(record.status)) break;
        await delay(25);
      }
      if (!active.has(record.status) && record.status !== 'interrupted') return record;
      // Also works after owner death, but only with an authenticated group anchor.
      record = load(root, id);
      const witnesses = groupWitnesses(record);
      signalWitnesses(witnesses, 'SIGTERM');
      if (witnesses.some(value => matches(value))) await delay(record.limits.shutdownGraceMs);
      signalWitnesses(witnesses, 'SIGKILL');
      for (let i = 0; i < 100 && witnesses.some(value => matches(value)); i++) await delay(10);
      if (witnesses.some(value => matches(value))) throw new Error(`Could not stop background group: ${id}`);
      const latest = load(root, id);
      if (!active.has(latest.status) && latest.status !== 'interrupted') return latest;
      latest.status = 'stopped'; latest.endedAt = now();
      latest.error ??= 'Stopped by explicit request; any prior effects are not undone.';
      save(root, latest);
      return latest;
    },
  };
  return runner;
}

class BoundedFile {
  constructor(path, limit, wholeRecords = false) {
    this.fd = privateFile(path, F.O_WRONLY | F.O_CREAT | F.O_EXCL);
    this.limit = limit;
    this.wholeRecords = wholeRecords;
    this.bytes = 0;
    this.droppedBytes = 0;
  }
  append(data) {
    const bytes = Buffer.isBuffer(data) ? data : Buffer.from(data);
    const room = this.limit - this.bytes;
    const count = this.wholeRecords && (this.droppedBytes || bytes.length > room) ? 0 : Math.min(room, bytes.length);
    if (count) writeAll(this.fd, bytes.subarray(0, count));
    this.bytes += count;
    this.droppedBytes += bytes.length - count;
  }
  info() { return { bytes: this.bytes, droppedBytes: this.droppedBytes, truncated: this.droppedBytes > 0 }; }
  sync() { fsyncSync(this.fd); }
}
function eventError(event) {
  if (event.type === 'response' && event.success === false) return event.error || `RPC ${event.command} rejected`;
  if (['error', 'extension_error'].includes(event.type)) return event.error || event.message || event.type;
  if (event.errorMessage || event.finalError || event.isError === true) return event.errorMessage || event.finalError || `${event.type} reported an error`;
  const messages = [event.message, ...(Array.isArray(event.messages) ? event.messages : []),
    ...(Array.isArray(event.toolResults) ? event.toolResults : []), event.assistantMessageEvent?.error];
  for (const message of messages) {
    if (message && typeof message === 'object' &&
        (message.isError || message.errorMessage || ['error', 'aborted'].includes(message.stopReason))) {
      return message.errorMessage || `Message ended with ${message.stopReason || 'isError'}`;
    }
  }
  if (event.assistantMessageEvent?.type === 'error') return 'Assistant stream error';
  if (event.type === 'auto_retry_end' && event.success === false) return event.finalError || 'Automatic retry failed';
  if (event.type === 'extension_ui_request' && ['select', 'confirm', 'input', 'editor'].includes(event.method)) {
    return `Background job requires interactive extension UI: ${event.method}`;
  }
  return null;
}

/** Internal CLI entry point. Never call this inside a TUI or a host process. */
export function markBackgroundWorkerFailure(directory, id, error) {
  platform();
  const root = secureDirectory(resolve(directory));
  const record = load(root, id);
  if (active.has(record.status)) {
    record.status = 'failed';
    record.error = `Background worker failed before terminal result: ${brief(error)}`;
    record.endedAt = now();
    try { save(root, record); } catch {}
  }
  return record;
}

export async function runBackgroundWorker(directory, id) {
  platform();
  process.umask(0o077);
  const root = secureDirectory(resolve(directory));
  const path = jobPath(root, id);
  let record;
  try { record = load(root, id); }
  catch (error) { markBackgroundWorkerFailure(root, id, error); return; }
  const owner = identity(process.pid);
  if (!owner || owner.group !== process.pid || owner.session !== process.pid) {
    throw new Error('Background worker must be a detached process-group/session leader');
  }
  // Permanent exclusive claim. A second worker must never replay this request.
  const claim = privateFile(join(path, 'owner.json'), F.O_WRONLY | F.O_CREAT | F.O_EXCL);
  try { writeAll(claim, JSON.stringify(owner)); fsyncSync(claim); } finally { closeSync(claim); }
  record = load(root, id);
  if (record.status !== 'starting' || record.delivery !== 'not_sent') return;
  record.owner = owner;
  const request = readJSON(join(path, 'request.json'));
  const limits = limitsFrom(record.limits);
  const files = {};
  let child, startupTimer, heartbeat, finishTimer;
  let finishing = false, finalStatus = null, storageBroken = false;
  let accepted = false, settled = false, pending = Buffer.alloc(0), sawEOF = false;
  const readyId = `ready-${id}`, promptId = `prompt-${id}`;

  function persist() {
    record.heartbeatAt = now();
    for (const [name, file] of Object.entries(files)) { file.sync(); record.output[name] = file.info(); }
    save(root, record);
  }
  function log(text) { files.worker?.append(`${now()} ${brief(text)}\n`); }
  function signalChild(signal) {
    const target = child?.pid;
    if (!target) return;
    try { process.kill(target, signal); }
    catch (error) { if (error.code !== 'ESRCH') throw error; }
  }
  function killGroup(signal) {
    const target = child?.pid;
    if (!target) return;
    try { process.kill(-target, signal); }
    catch (error) { if (error.code !== 'ESRCH') throw error; }
  }
  function fail(error, status = 'failed') {
    record.error ??= brief(error);
    if (finalStatus !== 'stopped') finalStatus = status;
    finish(status);
  }
  function guarded(fn) {
    try { fn(); }
    catch (error) {
      storageBroken = true;
      record.error = `Storage/worker failure: ${brief(error)}`;
      finalStatus = 'failed';
      finish('failed');
    }
  }
  function finish(status) {
    if (!finishing) {
      finishing = true;
      finalStatus ??= status;
      clearTimeout(startupTimer);
      record.status = status === 'stopped' ? 'stopping' : 'finishing';
      try { log(record.error || status); persist(); } catch { storageBroken = true; finalStatus = 'failed'; }
      signalChild('SIGTERM');
      finishTimer = setTimeout(() => {
        clearInterval(heartbeat);
        if (pending.length) { record.error ??= 'RPC stdout ended with an incomplete frame'; finalStatus = 'failed'; }
        record.status = storageBroken ? 'failed' : finalStatus;
        record.endedAt = now();
        try { persist(); } catch { /* Dead owner is reconciled as interrupted if storage is unavailable. */ }
        // Persist before killing the leader too. No descendant may keep this job alive.
        killGroup('SIGKILL');
      }, limits.shutdownGraceMs);
    }
  }
  function send(command) {
    child.stdin.write(`${JSON.stringify(command)}\n`, error => {
      if (error && !finishing) fail(`RPC stdin: ${brief(error)}`, record.delivery === 'uncertain' ? 'interrupted' : 'failed');
    });
  }
  function handle(event) {
    const error = eventError(event);
    if (error) {
      if (event.type === 'response' && event.id === promptId) record.delivery = 'rejected';
      fail(error);
      return;
    }
    if (finishing) return;
    if (event.type === 'response' && event.id === readyId && event.command === 'get_state' && event.success === true) {
      if (record.delivery !== 'not_sent') return;
      record.delivery = 'uncertain';
      persist(); // Write intent BEFORE stdin; a crash cannot be mistaken for safe replay.
      send({ id: promptId, type: 'prompt', message: request.prompt });
    }
    if (event.type === 'response' && event.id === promptId && event.command === 'prompt' && event.success === true && record.delivery === 'uncertain') {
      accepted = true;
      record.delivery = 'accepted'; record.acceptedAt = now(); record.status = 'running';
      clearTimeout(startupTimer);
      persist(); // This durable record is the startup acknowledgement to the launcher.
    }
    if (event.type === 'agent_settled' && record.delivery !== 'not_sent') settled = true;
    if (accepted && settled) finish('completed');
  }
  function frame(bytes) {
    if (bytes.length > limits.maxFrameBytes) { fail('RPC frame exceeds maxFrameBytes'); return; }
    const text = bytes.toString('utf8').replace(/\r$/, '');
    if (!text) return;
    let event;
    try { event = JSON.parse(text); }
    catch { fail('Malformed RPC JSON frame'); return; }
    if (!event || typeof event !== 'object' || Array.isArray(event) || typeof event.type !== 'string') {
      fail('Malformed RPC event'); return;
    }
    files.events.append(`${text}\n`);
    handle(event);
  }
  function stdout(chunk) {
    files.stdout.append(chunk);
    // Split bytes on LF only. UTF-8 and U+2028/U+2029 survive chunk boundaries.
    let offset = 0;
    while (offset < chunk.length) {
      const newline = chunk.indexOf(10, offset);
      const end = newline < 0 ? chunk.length : newline;
      const part = chunk.subarray(offset, end);
      if (pending.length + part.length > limits.maxFrameBytes) {
        pending = Buffer.alloc(0); fail('RPC frame exceeds maxFrameBytes'); return;
      }
      pending = pending.length ? Buffer.concat([pending, part]) : Buffer.from(part);
      if (newline < 0) break;
      const complete = pending; pending = Buffer.alloc(0);
      frame(complete);
      offset = newline + 1;
    }
  }
  process.on('SIGTERM', () => { if (!finishing) finish('stopped'); });
  process.on('SIGINT', () => { if (!finishing) finish('stopped'); });
  process.on('SIGHUP', () => {}); // Terminal loss does not cancel explicit background work.
  process.on('uncaughtException', error => guarded(() => { throw error; }));
  process.on('unhandledRejection', error => guarded(() => { throw error; }));
  guarded(() => {
    for (const name of ['events', 'stdout', 'stderr', 'worker']) {
      files[name] = new BoundedFile(join(path, name === 'events' ? 'events.jsonl' : `${name}.log`), limits[`${name}Bytes`], name === 'events');
    }
    persist();
    const env = { ...process.env };
    for (const key of ['PI_SESSION_ID', 'PI_SESSION_FILE', 'PI_PROVIDER', 'PI_MODEL', 'PI_REASONING_LEVEL', 'PI_CODING_AGENT_SESSION_DIR']) delete env[key];
    child = spawn(request.piExecutable, ['--mode', 'rpc', '--offline', '--session-dir', record.sessionDir], {
      cwd: record.cwd, env, detached: false, stdio: ['pipe', 'pipe', 'pipe'],
    });
    child.on('error', error => fail(`Pi startup: ${brief(error)}`));
    child.stdin.on('error', error => { if (!finishing) fail(`RPC stdin: ${brief(error)}`); });
    child.stdout.on('data', chunk => guarded(() => stdout(chunk)));
    child.stderr.on('data', chunk => guarded(() => files.stderr.append(chunk)));
    child.stdout.on('end', () => guarded(() => {
      sawEOF = true;
      if (pending.length) { const last = pending; pending = Buffer.alloc(0); frame(last); }
      if (!finishing) fail('RPC stdout closed before accepted agent_settled', 'interrupted');
    }));
    child.on('exit', (code, signal) => {
      record.exit = { code, signal };
      // Stream EOF drains all queued events. An unsolicited nonzero exit is a failure.
      if (!finishing && (code !== 0 || signal)) fail(`Pi exited before settlement: ${code ?? signal}`);
      else if (!finishing && sawEOF) fail('Pi exited without accepted agent_settled', 'interrupted');
    });
    child.once('spawn', () => guarded(() => {
      record.pi = identity(child.pid);
      persist();
      send({ id: readyId, type: 'get_state' });
    }));
    startupTimer = setTimeout(() => fail('Startup acknowledgement timed out; prompt is never resent', record.delivery === 'uncertain' ? 'interrupted' : 'failed'),
      Math.max(1, Date.parse(record.startupDeadline) - Date.now()));
    heartbeat = setInterval(() => guarded(() => {
      try {
        const stop = readJSON(join(path, 'stop.json'));
        if (stop.id !== id || stop.uid !== process.getuid()) throw new Error('Invalid stop request');
        if (!finishing) finish('stopped');
      } catch (error) { if (error.code !== 'ENOENT') throw error; }
      persist();
    }), Math.min(limits.heartbeatMs, 250));
  });
  // The owner exits by group SIGKILL, after its durable terminal record is written.
  void finishTimer;
}
