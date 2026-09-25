import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { JournalStore } from '../../host/journal.mjs';
import { TaskService } from '../../host/tasks.mjs';
import { WorkflowService } from '../../host/workflows.mjs';
import { resolveControls, authorize } from '../../host/policy.mjs';

const def = { id: 'sum', version: '1', strategy: 'deterministic', capabilities: ['write'], inputSchema: { type: 'array', items: { type: 'number' } }, outputSchema: { type: 'number' } };
function service(adapter, extra = {}) {
  const tasks = new TaskService({ store: new JournalStore(), adapters: { deterministic: adapter }, ...extra });
  tasks.register(def); return tasks;
}

test('default controls permit execution without consulting a permission or risk service', async () => {
  const controls = resolveControls();
  assert.ok(Object.values(controls).every(v => v === false));
  await authorize(controls, { capabilities: ['write'], grants: [] }, { confirm() { throw Error('unexpected permission'); }, screen() { throw Error('unexpected screening'); } });
  const tasks = service(async ({ request }) => ({ value: request.input.reduce((a,b) => a+b, 0), cost: 0 }));
  assert.deepEqual(await tasks.invoke('sum@1', [2,3,7]), { status: 'succeeded', value: 12, verification: 'unverified' });
});

test('opt-in permission and capability controls refuse unapproved execution', async () => {
  await assert.rejects(authorize(resolveControls({ permissionPrompts: true }), {}), /Permission/);
  await assert.rejects(authorize(resolveControls({ capabilityEnforcement: true }), { capabilities: ['write'] }), /Capability/);
  assert.throws(() => resolveControls({ invented: true }), /Invalid control/);
  await assert.rejects(authorize(resolveControls({ sandboxRequired: true }), { executionProfile: 'isolated' }), /Sandbox/);
});

test('malformed input never executes; malformed output cannot become success', async () => {
  let executed = 0;
  const tasks = service(async () => { executed++; return { value: 'wrong' }; });
  await assert.rejects(tasks.invoke('sum@1', ['bad']), /Invalid input/);
  assert.equal(executed, 0);
  const result = await tasks.invoke('sum@1', [1]);
  assert.equal(result.status, 'failed'); assert.match(result.error, /Invalid output/);
});

test('admission returns before result, cancellation rejects a late success', async () => {
  let release;
  const tasks = service(async () => { await new Promise(r => { release = r; }); return { value: 3 }; });
  const handle = await tasks.spawn('sum@1', [1,2]);
  assert.equal(handle.inspect().state, 'running');
  assert.equal(handle.cancel(), true);
  assert.deepEqual(await handle.result(), { status: 'cancelled' });
  release(); await tasks.shutdown();
  assert.equal(handle.inspect().state, 'cancelled');
});

test('idempotent admission runs once and detects conflicting reuse', async () => {
  let calls=0; const tasks=service(async () => { calls++; return { value: 5 }; });
  const a=await tasks.spawn('sum@1',[5],{key:'same'}); await a.result();
  const b=await tasks.spawn('sum@1',[5],{key:'same'});
  assert.equal(a.id,b.id); assert.equal(calls,1);
  await assert.rejects(tasks.spawn('sum@1',[6],{key:'same'}),/Idempotency/);
});

test('explicit models reach the adapter unchanged', async () => {
  let selected; const tasks=service(async ({request}) => { selected=request.model; return {value:1}; });
  await tasks.invoke('sum@1',[1],{model:'provider/explicit'});
  assert.equal(selected,'provider/explicit');
});

test('graph invokes the same validated tasks and waits for actual dependency results', async () => {
  const tasks=service(async ({request}) => ({value:request.input.reduce((a,b)=>a+b,0),cost:0}));
  const graph=new WorkflowService(tasks);
  const direct=await tasks.invoke('sum@1',[4,8]);
  const results=await graph.run([{id:'a',definition:'sum@1',input:[4,8]},{id:'b',definition:'sum@1',input:[9],dependsOn:['a']}]);
  assert.deepEqual(results.a,direct); assert.equal(results.b.value,9);
  assert.equal(tasks.list().length,3);
});

test('cycles and invalid bindings are rejected before any graph node executes', async () => {
  let calls=0; const tasks=service(async ()=>{ calls++; return {value:1}; }); const graph=new WorkflowService(tasks);
  await assert.rejects(graph.run([{id:'free',definition:'sum@1',input:[1]},{id:'cycle',definition:'sum@1',input:[1],dependsOn:['cycle']}]),/cycle/);
  await assert.rejects(graph.run([{id:'a',definition:'sum@1',input:[1]},{id:'b',definition:'sum@1',inputFrom:'a'}]),/inputFrom/);
  assert.equal(calls,0);
});

test('failed output skips dependents rather than treating admission as completion', async () => {
  let calls=0; const tasks=service(async()=>{calls++;return {value:'bad'};});
  const out=await new WorkflowService(tasks).run([{id:'a',definition:'sum@1',input:[1]},{id:'b',definition:'sum@1',input:[2],dependsOn:['a']}]);
  assert.equal(out.a.status,'failed'); assert.equal(out.b.status,'skipped'); assert.equal(calls,1);
});

test('journal restart preserves terminal evidence and marks unfinished work interrupted', () => {
  const dir=mkdtempSync(join(tmpdir(),'pi-task-test-')); const path=join(dir,'tasks.jsonl');
  try {
    const first=new JournalStore(path); const a=first.admit('a','one',{input:1});
    first.transition(a.id,'admitted','running');
    const second=new JournalStore(path); second.recover();
    assert.equal(second.get(a.id).state,'interrupted');
    assert.equal(second.admit('a','one',{input:1}).created,false);
    assert.equal(new JournalStore(path).get(a.id).result.status,'interrupted');
  } finally {rmSync(dir,{recursive:true,force:true});}
});

test('usage reservations enforce only an explicit cap; missing usage stays unresolved', () => {
  const store=new JournalStore(); const id=store.reserve('root',4,5);
  assert.throws(()=>store.reserve('root',2,5),/Budget/);
  store.settle(id,null); assert.equal(store.total('root'),4); assert.equal(store.accounting('root').unresolved,1);
  store.reserve('root',100); assert.equal(store.total('root'),104);
  assert.equal(store.settle(id,0),false);
});

test('adapter claims do not satisfy an enabled host verification gate', async () => {
  const tasks=service(async()=>({value:1,verification:'passed'}),{controls:{completionGates:true}});
  assert.equal((await tasks.invoke('sum@1',[1])).status,'failed');
});
