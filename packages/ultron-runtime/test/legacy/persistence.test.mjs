import test from 'node:test';
import assert from 'node:assert/strict';
import {mkdtempSync,rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {createPiService} from '../../host/pi-service.mjs';
import {TaskService} from '../../host/tasks.mjs';
import {JournalStore} from '../../host/journal.mjs';
const setup=directory=>createPiService({directory,children:{},route:()=>{},predict:()=>{throw Error('No model');}});

test('session task results and custom definitions survive owner restart; concurrent owner refused',async()=>{
 const dir=mkdtempSync(join(tmpdir(),'pi-persist-'));let service=setup(dir);
 try{
  assert.throws(()=>setup(dir),/owner/);
  const h=await service.dispatch('agents.spawn',{definition:'identity@1',input:{v:42}});await service.dispatch('agents.result',{id:h.id});
  service.tasks.register({id:'custom',version:'1',strategy:'predict',inputSchema:{},outputSchema:{}});
  await service.shutdown();service=setup(dir);
  assert.equal((await service.dispatch('agents.result',{id:h.id})).value.v,42);
  assert.ok(service.tasks.listDefinitions().some(d=>d.key==='custom@1'));
 }finally{await service.shutdown();rmSync(dir,{recursive:true,force:true});}
});

test('retained logical instance serializes invocations and keeps old results immutable',async()=>{
 const dir=mkdtempSync(join(tmpdir(),'pi-instance-'));const service=setup(dir);
 try{
 const i=await service.dispatch('agents.create',{definition:'identity@1'});
 const [a,b]=await Promise.all([service.dispatch('instances.invoke',{id:i.id,input:1}),service.dispatch('instances.invoke',{id:i.id,input:2})]);
 assert.equal(a.value,1);assert.equal(b.value,2);
 const record=service.instances.get(i.id);assert.equal(record.invocations.length,2);
 assert.equal(service.store.get(record.invocations[0]).result.value,1);
 await service.dispatch('instances.close',{id:i.id});
 await assert.rejects(service.dispatch('instances.invoke',{id:i.id,input:3}),/closed/);
 }finally{await service.shutdown();rmSync(dir,{recursive:true,force:true});}
});

test('branch filtering hides abandoned tasks; artifacts preserve full data; experiments are matched',async()=>{
 const dir=mkdtempSync(join(tmpdir(),'pi-branch-'));const service=setup(dir);
 try{
 const h=await service.dispatch('agents.spawn',{definition:'identity@1',input:1},{branch:'abandoned'});await service.tasks.handle(h.id).result();
 service.selectBranch(['root','other']);await assert.rejects(service.dispatch('agents.inspect',{id:h.id}),/another branch/);
 const artifact=service.artifacts.put('x'.repeat(100000));assert.equal(artifact.bytes,100000);assert.equal(service.artifacts.read(artifact.id,{offset:99990,length:10}).text,'xxxxxxxxxx');
 const a=service.experiments.record({variant:'old',fixtureHash:'same',outcome:'failed'}),b=service.experiments.record({variant:'new',fixtureHash:'same',outcome:'passed'});
 assert.equal(service.experiments.compare(a.id,b.id).baseline.outcome,'failed');
 }finally{await service.shutdown();rmSync(dir,{recursive:true,force:true});}
});

test('typed repair retries only within configured allowance and records every attempt',async()=>{
 const store=new JournalStore();let calls=0;
 const tasks=new TaskService({store,adapters:{predict:async({repair})=>{calls++;return {value:repair?42:'bad',cost:1};}}});
 tasks.register({id:'repair',version:'1',strategy:'predict',maxRepairs:1,inputSchema:{},outputSchema:{type:'integer'}});
 assert.equal((await tasks.invoke('repair@1',null)).value,42);assert.equal(calls,2);assert.equal(store.total('root'),2);
});
