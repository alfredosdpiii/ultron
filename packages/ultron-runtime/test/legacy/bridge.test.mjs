import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { createJiti } from 'jiti';
import { createPiService } from '../../host/pi-service.mjs';
const jiti=createJiti(import.meta.url);
const { RlmKernel }=await jiti.import('../../compat/agent/extensions/prime-rlm/kernel.ts');

test('real persistent Python invokes typed tasks and optional workflow via host bridge', {timeout:15000}, async () => {
  const dir=mkdtempSync(join(tmpdir(),'pi-python-bridge-'));
  const service=createPiService({directory:dir,children:{},route:()=>{},predict:()=>{throw Error('No model should run');}});
  const kernel=new RlmKernel({cwd:dir,runtimePath:resolve('compat/agent/extensions/prime-rlm/runtime.py')},(type,payload)=>service.dispatch(type,payload));
  try {
    let result=await kernel.execute("x = list(range(1000))\nr = await agents.invoke('identity@1', {'total': sum(x)})\nprint(r['status'], r['value']['total'], r['verification'])");
    assert.equal(result.status,'ok'); assert.match(result.stdout,/succeeded 499500 unverified/);
    result=await kernel.execute("job = await agents.spawn('identity@1', 42)\nr = await job.result()\nprint(r['value'], len(x))");
    assert.equal(result.status,'ok'); assert.match(result.stdout,/42 1000/);
    result=await kernel.execute("r = await workflows.run([{'id':'first', 'definition':'identity@1', 'input':7}, {'id':'second', 'definition':'identity@1', 'dependsOn':['first'], 'inputFrom':'first'}])\nprint(r['second']['value'])");
    assert.equal(result.status,'ok'); assert.match(result.stdout,/7/);
    result=await kernel.execute("s = await agents.status()\nprint(any(s['controls'].values()))");
    assert.match(result.stdout,/False/);
  } finally {await service.shutdown(); await kernel.shutdown(); rmSync(dir,{recursive:true,force:true});}
});
