import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, chmodSync, rmSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { createJiti } from 'jiti';

const jiti = createJiti(import.meta.url);
const { RlmChildRegistry } = await jiti.import('../../compat/agent/extensions/prime-rlm/registry.ts');

function fakePi(dir) {
  const script = join(dir, 'fake-pi.mjs');
  writeFileSync(script, `#!/usr/bin/env node
let seq = 0;
process.stdout.write(JSON.stringify({type:'session',id:'session-fake'})+'\\n');
let buffer='';
process.stdin.on('data', chunk => {
  buffer += chunk;
  let i;
  while ((i=buffer.indexOf('\\n')) >= 0) {
    const line=buffer.slice(0,i); buffer=buffer.slice(i+1);
    const command=JSON.parse(line);
    if(command.type !== 'prompt') continue;
    const id=command.id;
    if(id) process.stdout.write(JSON.stringify({type:'response',command:'prompt',id,success:true})+'\\n');
    if(process.env.FAKE_HANG === '1') continue;
    const text=command.message.includes('second') ? JSON.stringify({answer:2}) : JSON.stringify({answer:1});
    const message={role:'assistant',id:'assistant-one',content:[{type:'text',text}],stopReason:'stop',usage:{input:10,output:5,totalTokens:15,cost:{total:0.25}}};
    process.stdout.write(JSON.stringify({type:'message_end',message})+'\\n');
    process.stdout.write(JSON.stringify({type:'agent_end',messages:[message]})+'\\n');
    process.stdout.write(JSON.stringify({type:'agent_settled'})+'\\n');
    seq++;
  }
});
`);
  chmodSync(script, 0o700);
  return script;
}

async function registryWait(registry, id) { for (let i=0;i<100;i++){ const value=registry.list().find(x=>x.rlm_child_id===id); if(value?.status!=='running') return value; await new Promise(r=>setTimeout(r,20)); } throw new Error('registry wait timeout'); }

function makeRegistry(dir, executable, extra={}) {
  return new RlmChildRegistry({ sessionId:'parent', sessionDir:dir, cwd:dir, piExecutable:executable, maxDepth:3, depth:0, approvalBypass:()=>true, ...extra });
}

test('retained child accepts a second invocation and keeps cumulative usage without double counting', { timeout: 10000 }, async () => {
  const dir=mkdtempSync(join(tmpdir(),'rlm-registry-')); const exe=fakePi(dir); const registry=makeRegistry(dir,exe);
  try {
    const first=await registry.spawn('first', {name:'reviewer', retain:true});
    let result=(await registry.collect([first.rlm_child_id],3000))[0];
    assert.equal(result.status,'completed'); assert.equal(result.result_status,'complete');
    assert.equal(result.usage_records,1); assert.equal(result.usage.cost,0.25);
    const second=await registry.continue(first.rlm_child_id,'second');
    result=(await registry.collect([second.rlm_child_id],3000))[0];
    assert.equal(result.status,'completed'); assert.equal(result.result_status,'complete');
    assert.equal(result.result.includes('answer":2'),true);
    assert.equal(result.usage_records,1); assert.equal(result.usage.cost,0.25);
    assert.equal(result.continuation_mode,'live');
  } finally { await registry.shutdown(); rmSync(dir,{recursive:true,force:true}); }
});

test('a non-retained completed child cannot be continued', { timeout: 10000 }, async () => {
  const dir=mkdtempSync(join(tmpdir(),'rlm-registry-')); const exe=fakePi(dir); const registry=makeRegistry(dir,exe);
  try { const child=await registry.spawn('first',{name:'one'}); await registry.collect([child.rlm_child_id],3000); await assert.rejects(registry.continue(child.rlm_child_id,'second'),/retained/); }
  finally { await registry.shutdown(); rmSync(dir,{recursive:true,force:true}); }
});

test('aborting a running child terminates its process group and leaves cancellation terminal', { timeout: 10000 }, async () => {
  const dir=mkdtempSync(join(tmpdir(),'rlm-registry-')); const exe=fakePi(dir); const old=process.env.FAKE_HANG; process.env.FAKE_HANG='1'; const registry=makeRegistry(dir,exe);
  try { const controller=new AbortController(); const child=await registry.spawn('hang',{name:'hang'},controller.signal); await new Promise(r=>setTimeout(r,100)); controller.abort(); const result=(await registry.collect([child.rlm_child_id],3000))[0]; assert.equal(result.status,'cancelled'); }
  finally { if(old===undefined) delete process.env.FAKE_HANG; else process.env.FAKE_HANG=old; await registry.shutdown(); rmSync(dir,{recursive:true,force:true}); }
});

test('persisted running children become interrupted on registry reload', { timeout: 10000 }, async () => {
  const dir=mkdtempSync(join(tmpdir(),'rlm-registry-')); const exe=fakePi(dir); const first=makeRegistry(dir,exe);
  const child=await first.spawn('first',{name:'persisted',retain:true});
  await registryWait(first, child.rlm_child_id);
  await first.shutdown();
  const path=join(dir,'registry.json'); const saved=JSON.parse(readFileSync(path,'utf8')); saved.children[0].status='running'; saved.children[0].error=undefined; writeFileSync(path,JSON.stringify(saved)+'\n');
  // New registry marks stale running metadata terminal without replaying the prompt.
  const second=makeRegistry(dir,exe); const record=second.list().find(x=>x.rlm_child_id===child.rlm_child_id); assert.equal(record.status,'error'); assert.match(record.error,/interrupted/);
  await second.shutdown(); rmSync(dir,{recursive:true,force:true});
});
