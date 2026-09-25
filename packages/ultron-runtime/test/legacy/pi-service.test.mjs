import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { createPiService } from '../../host/pi-service.mjs';

test('typed RLM validates real child completion contract and preserves explicit model', async () => {
  const dir=mkdtempSync(join(tmpdir(),'pi-service-')); let entry, selection;
  const children={ async spawn(prompt, kwargs) {selection=kwargs;entry={rlm_child_id:'child',status:'running'};return entry;}, list(){return [entry];} };
  const service=createPiService({directory:dir,children,route(){throw Error('explicit model must skip route');},predict(){throw Error('wrong strategy');}});
  try {
    const handle=await service.dispatch('agents.spawn',{definition:'security-reviewer@1',input:{request:'Review sample'},model:'provider/selected'});
    assert.equal(service.store.get(handle.id).state,'running');
    assert.equal(selection.model,'provider/selected');
    entry={...entry,status:'completed',result_status:'complete',result:JSON.stringify({outcome:'no_findings',findings:[]})};
    const result=await service.dispatch('agents.result',{id:handle.id});
    assert.deepEqual(result,{status:'succeeded',value:{outcome:'no_findings',findings:[]},verification:'unverified'});
  } finally {await service.shutdown();rmSync(dir,{recursive:true,force:true});}
});

test('partial or failed child never produces a typed success', async () => {
  const dir=mkdtempSync(join(tmpdir(),'pi-service-'));
  const children={async spawn(){return {rlm_child_id:'c'};},list(){return [{rlm_child_id:'c',status:'completed',result_status:'partial',result:'{}'}];}};
  const service=createPiService({directory:dir,children,route:async()=>({model:'provider/model'}),predict:async()=>({value:{category:'research'},cost:0})});
  try {
    const result=await service.dispatch('agents.invoke',{definition:'security-reviewer@1',input:{request:'x'}});
    assert.equal(result.status,'failed');
    assert.equal((await service.dispatch('agents.invoke',{definition:'classifier@1',input:'investigate'})).value.category,'research');
    const state=await service.dispatch('agents.status',{});
    assert.ok(Object.values(state.controls).every(x=>x===false));
  } finally {await service.shutdown();rmSync(dir,{recursive:true,force:true});}
});
