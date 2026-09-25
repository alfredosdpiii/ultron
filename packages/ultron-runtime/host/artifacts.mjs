import { createHash } from 'node:crypto';
import { mkdirSync, writeFileSync, readFileSync, existsSync } from 'node:fs';
import { join } from 'node:path';
import { atomicJSON, readJSON } from './storage.mjs';

export class ArtifactStore {
  constructor(directory) { this.directory=directory; this.index=join(directory,'index.json'); this.records=readJSON(this.index,{}); mkdirSync(directory,{recursive:true,mode:0o700}); }
  put(text, {mediaType='text/plain',label=''}={}) {
    if(typeof text!=='string') throw new TypeError('artifact content must be a string');
    const bytes=Buffer.from(text); const id=createHash('sha256').update(bytes).digest('hex');
    const path=join(this.directory,id);
    if(!existsSync(path)) writeFileSync(path,bytes,{mode:0o600,flag:'wx'});
    this.records[id]={id,bytes:bytes.length,mediaType,label}; atomicJSON(this.index,this.records);
    return {...this.records[id],preview:bytes.subarray(0,2048).toString('utf8')};
  }
  read(id,{offset=0,length=8192}={}) {
    if(!/^[a-f0-9]{64}$/.test(id)||!this.records[id]) throw new Error('Unknown artifact');
    if(!Number.isSafeInteger(offset)||offset<0||!Number.isSafeInteger(length)||length<0||length>1048576) throw new Error('Invalid artifact range');
    const bytes=readFileSync(join(this.directory,id));
    if(createHash('sha256').update(bytes).digest('hex')!==id) throw new Error('Artifact integrity failure');
    return {id,text:bytes.subarray(offset,offset+length).toString('utf8'),offset,total:bytes.length};
  }
  list(){return Object.values(this.records);}
}
