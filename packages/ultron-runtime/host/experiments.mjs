import { join } from 'node:path';
import { randomUUID } from 'node:crypto';
import { atomicJSON, readJSON } from './storage.mjs';
export class Experiments {
  constructor(directory){this.path=join(directory,'experiments.json');this.runs=readJSON(this.path,[]);}
  record(run){
    if(!run||typeof run.variant!=='string'||typeof run.fixtureHash!=='string'||!['passed','failed','incomplete'].includes(run.outcome))throw new Error('Experiment requires variant, fixtureHash, outcome');
    const value={...structuredClone(run),id:randomUUID(),recordedAt:new Date().toISOString()};this.runs.push(value);atomicJSON(this.path,this.runs);return value;
  }
  list(){return structuredClone(this.runs);}
  compare(a,b){
    const left=this.runs.find(r=>r.id===a),right=this.runs.find(r=>r.id===b);
    if(!left||!right)throw new Error('Unknown experiment');
    if(left.fixtureHash!==right.fixtureHash)throw new Error('Different fixtures cannot form a matched comparison');
    return {baseline:structuredClone(left),candidate:structuredClone(right),claim:'Observed runs only; no statistical superiority established'};
  }
}
