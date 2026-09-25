#!/usr/bin/env node
import { createBackgroundRunner, runBackgroundWorker } from '../host/background.mjs';
const [command, ...args] = process.argv.slice(2);
if (command === '--worker') {
  try { await runBackgroundWorker(args[0], args[1]); }
  catch (error) {
    try { const module = await import('../host/background.mjs'); module.markBackgroundWorkerFailure(args[0], args[1], error); }
    catch {}
    console.error(error?.stack ?? error);
    process.exitCode = 1;
  }
  // Keep the worker event loop alive while runBackgroundWorker owns the child.
  // Only exit after its lifecycle has settled.
}
const jobs = createBackgroundRunner();
if (command === 'start') console.log(JSON.stringify(await jobs.start(args.join(' ')), null, 2));
else if (command === 'list') console.log(JSON.stringify(jobs.list(), null, 2));
else if (command === 'inspect') console.log(JSON.stringify(jobs.inspect(args[0]), null, 2));
else if (command === 'stop') console.log(JSON.stringify(await jobs.stop(args[0]), null, 2));
else { console.error('Usage: background.mjs start <prompt> | list | inspect <id> | stop <id>'); process.exitCode = 2; }
