import { join } from 'node:path';
import { randomUUID } from 'node:crypto';
import { atomicJSON, readJSON } from './storage.mjs';

export class InstanceService {
  constructor(directory,tasks){this.tasks=tasks;this.path=join(directory,'instances.json');this.records=readJSON(this.path,{});this.queues=new Map();}
  save(){atomicJSON(this.path,this.records);}
  create(definition,{branch='root',model=null}={}){
    if(!this.tasks.definitions.has(definition))throw new Error('Unknown definition');
    const id=randomUUID();this.records[id]={id,definition,branch,model,state:'idle',invocations:[],messages:[],childId:null};this.save();return this.get(id);
  }
  get(id){if(!this.records[id])throw new Error('Unknown instance');return structuredClone(this.records[id]);}
  list(){return Object.keys(this.records).map(id=>this.get(id));}
  bindChild(id,childId){const r=this.records[id];if(!r)throw new Error('Unknown instance');r.childId=childId;this.save();}
  async invoke(id,input,options={}){
    const previous=this.queues.get(id)??Promise.resolve();
    const run=previous.catch(()=>{}).then(async()=>{
      const r=this.records[id];if(!r||r.state==='closed')throw new Error('Instance closed');
      r.state='running';this.save();
      try{
        const job=await this.tasks.spawn(r.definition,input,{...options,branch:r.branch,model:options.model??r.model,instanceId:id});
        r.invocations.push(job.id);this.save();return await job.result();
      }finally{if(r.state!=='closed')r.state='idle';this.save();}
    });
    this.queues.set(id,run);run.finally(()=>{if(this.queues.get(id)===run)this.queues.delete(id);}).catch(()=>{});return run;
  }
  message(id,text,{key=randomUUID(),sender='user'}={}){
    const r=this.records[id];if(!r||r.state==='closed')throw new Error('Instance closed');
    if(typeof text!=='string'||!text.trim()||Buffer.byteLength(text)>16384)throw new Error('Message must be 1..16384 bytes');
    const old=r.messages.find(m=>m.key===key);if(old){if(old.text!==text||old.sender!==sender)throw new Error('Message key conflict');return structuredClone(old);}
    if(r.messages.filter(m=>m.state==='queued').length>=32)throw new Error('Message queue full');
    const message={id:randomUUID(),key,text,sender,state:'queued',createdAt:Date.now()};r.messages.push(message);this.save();return structuredClone(message);
  }
  consume(id){
    const r=this.records[id];if(!r)throw new Error('Unknown instance');
    const messages=[];for(const m of r.messages)if(m.state==='queued'){m.state=Date.now()-m.createdAt>1800000?'expired':'consumed';if(m.state==='consumed')messages.push(m);}
    this.save();return structuredClone(messages);
  }
  close(id){const r=this.records[id];if(!r)throw new Error('Unknown instance');r.state='closed';for(const task of r.invocations)this.tasks.cancel(task);this.save();return this.get(id);}
  recover(){for(const r of Object.values(this.records))if(r.state==='running')r.state='idle';this.save();}
}
