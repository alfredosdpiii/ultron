import test from 'node:test';
import assert from 'node:assert/strict';
import {mkdtempSync,rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {MemoryService} from '../../host/memory.mjs';

function fake(directory, gate=async request=>request.action==='recall'?{retrieve:true}:{action:'keep'}) {
  const docs=new Map(), ops=new Map(); let calls={recall:0,retain:0,get:0,delete:0};
  const backend={namespace:'fake://bank',scopeTags:{session:['pi:session:s1'],project:['pi:project:p1']},
    async recall(request){calls.recall++;assert.deepEqual(request.tags,['pi:session:s1']);return {results:[{id:'m1',text:'old fact',tags:['pi:session:s1']}]};},
    async retain(request){calls.retain++;const item=request.items[0];docs.set(item.document_id,{id:item.document_id,original_text:item.content,tags:item.tags});ops.set(request.operation_id,'completed');return {success:true,async:true,operation_id:request.operation_id};},
    async operation(id){return {operation_id:id,status:ops.get(id)??'not_found'};},
    async get(id){calls.get++;return docs.get(id);},
    async delete(id){calls.delete++;docs.delete(id);return {success:true,document_id:id};},
  };
  return {service:new MemoryService({directory,backend,gate}),backend,calls,docs};
}

test('memory skip performs no backend call and why does not search',async()=>{const dir=mkdtempSync(join(tmpdir(),'mem-'));try{const x=fake(dir,async()=>({retrieve:false}));const out=await x.service.prepare({query:'math',scope:'session',taskId:'t'});assert.equal(out.results.length,0);assert.equal(x.calls.recall,0);assert.equal(x.service.why('t').length,1);}finally{rmSync(dir,{recursive:true,force:true});}});

test('memory recall sends exact scope tags and rejects out-of-scope results',async()=>{const dir=mkdtempSync(join(tmpdir(),'mem-'));try{const x=fake(dir);const out=await x.service.prepare({query:'prior',scope:'session',taskId:'t'});assert.equal(out.results[0].id,'m1');assert.equal(x.calls.recall,1);const bad=fake(dir,async()=>({retrieve:true}));bad.backend.recall=async()=>({results:[{id:'bad',text:'x',tags:['pi:project:p1']}]});await assert.rejects(bad.service.prepare({query:'x',scope:'session',taskId:'t'}),error=>error.code==='INVALID_RESPONSE');}finally{rmSync(dir,{recursive:true,force:true});}});

test('async retain is accepted until operation status confirms storage; forget deletes document',async()=>{const dir=mkdtempSync(join(tmpdir(),'mem-'));try{const x=fake(dir);const accepted=await x.service.propose({text:'fact',evidence:[{ref:'task:t'}],scope:'session'});assert.equal(accepted.state,'accepted');const read=await x.service.get(accepted.memoryId);assert.equal(read.state,'stored');assert.equal(read.content,'fact');const forgotten=await x.service.forget(accepted.memoryId);assert.equal(forgotten.state,'forgotten');assert.equal(x.calls.delete,1);await assert.rejects(x.service.get(accepted.memoryId),error=>error.code==='FORGOTTEN' || error.code==='UNKNOWN_MEMORY');}finally{rmSync(dir,{recursive:true,force:true});}});

test('missing evidence and unsupported scopes fail before backend mutation',async()=>{const dir=mkdtempSync(join(tmpdir(),'mem-'));try{const x=fake(dir);await assert.rejects(x.service.propose({text:'fact',evidence:[],scope:'session'}),/evidence/);await assert.rejects(x.service.propose({text:'fact',evidence:[{ref:'x'}],scope:'global'}),error=>error.code==='UNSUPPORTED_SCOPE');assert.equal(x.calls.retain,0);}finally{rmSync(dir,{recursive:true,force:true});}});
