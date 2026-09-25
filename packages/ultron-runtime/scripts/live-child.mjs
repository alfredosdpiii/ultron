// Explicit live smoke: one child model call; no repository edits requested.
import { createJiti } from 'jiti';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { createPiService } from '../host/pi-service.mjs';
const jiti = createJiti(import.meta.url);
const { RlmChildRegistry } = await jiti.import('../agent/extensions/prime-rlm/registry.ts');
const directory=mkdtempSync(join(tmpdir(),'pi-typed-live-'));
const controller=new AbortController();
const timer=setTimeout(()=>controller.abort(),120000);
const children=new RlmChildRegistry({sessionId:'typed-live',sessionDir:directory,cwd:directory,maxDepth:1,depth:0,approvalBypass:()=>true,extensionPath:resolve('agent/extensions/prime-rlm/index.ts')});
const service=createPiService({directory,children,route:async()=>({model:'cliproxyapi/gpt-6-astra',thinking:'high'}),predict:()=>{throw Error('Wrong strategy');}});
service.tasks.register({id:'smoke',version:'1',strategy:'rlm',instructions:'Do not call any tools or modify files. Return JSON with answer equal to the arithmetic answer requested.',inputSchema:{type:'string'},outputSchema:{type:'object',required:['answer'],additionalProperties:false,properties:{answer:{type:'integer',const:42}}}});
try {
  const result=await service.dispatch('agents.invoke',{definition:'smoke@1',input:'What is 19 + 23?',model:'cliproxyapi/gpt-6-astra'},{signal:controller.signal});
  console.log(JSON.stringify({directory,result},null,2));
  if(result.status!=='succeeded') process.exitCode=1;
} finally {clearTimeout(timer); await service.shutdown(); await children.shutdown();}
