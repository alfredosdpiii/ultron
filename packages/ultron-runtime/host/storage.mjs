import { openSync, closeSync, writeFileSync, fsyncSync, renameSync, mkdirSync, readFileSync, unlinkSync } from 'node:fs';
import { dirname } from 'node:path';
import { randomUUID } from 'node:crypto';

export function atomicJSON(path, value) {
  mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
  const temp = `${path}.${randomUUID()}.tmp`;
  let fd;
  try {
    fd = openSync(temp, 'wx', 0o600);
    writeFileSync(fd, JSON.stringify(value) + '\n'); fsyncSync(fd); closeSync(fd); fd = undefined;
    renameSync(temp, path);
    const directory = openSync(dirname(path), 'r'); try { fsyncSync(directory); } finally { closeSync(directory); }
  } finally { if (fd !== undefined) closeSync(fd); try { unlinkSync(temp); } catch {} }
}
export function readJSON(path, fallback) {
  try { return JSON.parse(readFileSync(path, 'utf8')); }
  catch (error) { if (error.code === 'ENOENT') return structuredClone(fallback); throw error; }
}

/** OS process lock. Linux boot/start identity prevents PID reuse stealing an owner. */
function identity(pid) {
  try {
    const stat = readFileSync(`/proc/${pid}/stat`, 'utf8');
    return readFileSync('/proc/sys/kernel/random/boot_id', 'utf8').trim() + ':' + stat.slice(stat.lastIndexOf(')') + 2).split(' ')[19];
  } catch (error) { if (error.code === 'ENOENT') return null; throw error; }
}
export function acquireOwner(path) {
  mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
  const token = randomUUID();
  for (let attempt = 0; attempt < 3; attempt++) {
    try {
      const fd = openSync(path, 'wx', 0o600);
      try { writeFileSync(fd, JSON.stringify({ pid: process.pid, identity: identity(process.pid), token })); fsyncSync(fd); }
      finally { closeSync(fd); }
      return () => { const value = readJSON(path, null); if (value?.token === token) unlinkSync(path); };
    } catch (error) {
      if (error.code !== 'EEXIST') throw error;
      const old = readJSON(path, null);
      if (!old?.pid || old.identity === identity(old.pid)) throw new Error('Session already has a live owner');
      // Rename the stale lock rather than deleting an arbitrary replacement.
      const stale = `${path}.stale-${token}`;
      renameSync(path, stale);
      const captured = readJSON(stale, null);
      if (captured?.token !== old.token) {
        try { renameSync(stale, path); } catch {}
        throw new Error('Concurrent owner recovery; retry after inspection');
      }
      unlinkSync(stale);
    }
  }
  throw new Error('Cannot acquire session owner');
}
