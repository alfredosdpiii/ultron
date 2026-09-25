import test from 'node:test';
import assert from 'node:assert/strict';
import {mkdtempSync,writeFileSync,chmodSync,rmSync,readFileSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {createBackgroundRunner} from '../../host/background.mjs';

function fake(dir, mode='ok') { const path=join(dir,'fake.mjs'); writeFileSync(path,`#!/usr/bin/env node
let b='';process.stdin.on('data',c=>{b+=c;let i;while((i=b.indexOf('\\n'))>=0){let x=JSON.parse(b.slice(0,i));b=b.slice(i+1);if(x.type==='get_state'){process.stdout.write(JSON.stringify({type:'response',id:x.id,command:'get_state',success:true})+'\\n');}if(x.type==='prompt'){process.stdout.write(JSON.stringify({type:'response',id:x.id,command:'prompt',success:true})+'\\n');${mode==='fail'?"process.stdout.write(JSON.stringify({type:'error',message:'fail'})+'\\n')":"process.stdout.write(JSON.stringify({type:'agent_settled'})+'\\n');"}}}});`);chmodSync(path,0o700);return path; }

async function waitTerminal(runner,id){for(let i=0;i<100;i++){const value=runner.inspect(id);if(!['starting','running','finishing','stopping'].includes(value.status))return value;await new Promise(r=>setTimeout(r,20));}throw Error('background terminal timeout');}

test('background runner requires explicit start and completes only after settled',async()=>{const dir=mkdtempSync(join(tmpdir(),'bg-'));try{const runner=createBackgroundRunner({directory:join(dir,'jobs'),piExecutable:fake(dir)});const started=await runner.start('do work');assert.equal(started.delivery,'accepted');const result=await waitTerminal(runner,started.id);assert.equal(result.status,'completed');assert.ok(runner.list().length===1);}finally{rmSync(dir,{recursive:true,force:true});}});

test('background observed errors never become completion',async()=>{const dir=mkdtempSync(join(tmpdir(),'bg-'));try{const runner=createBackgroundRunner({directory:join(dir,'jobs'),piExecutable:fake(dir,'fail')});const started=await runner.start('fail');const result=await waitTerminal(runner,started.id);assert.notEqual(result.status,'completed');assert.match(result.error,/fail/i);}finally{rmSync(dir,{recursive:true,force:true});}});

test('background stop is explicit and private records are created',async()=>{const dir=mkdtempSync(join(tmpdir(),'bg-'));const path=fake(dir);const runner=createBackgroundRunner({directory:join(dir,'jobs'),piExecutable:path,limits:{startupTimeoutMs:1000}});try{const result=await runner.start('stop');assert.ok(result.id);const record=runner.inspect(result.id);assert.equal(record.uid,process.getuid());assert.equal(readFileSync(join(dir,'jobs',result.id,'request.json'),'utf8').includes('stop'),true);}finally{rmSync(dir,{recursive:true,force:true});}});
