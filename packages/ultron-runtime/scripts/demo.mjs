import { JournalStore } from '../host/journal.mjs';
import { TaskService } from '../host/tasks.mjs';
import { WorkflowService } from '../host/workflows.mjs';
import { definitions } from '../host/definitions.mjs';
const tasks = new TaskService({store:new JournalStore(),adapters:{deterministic:async({request})=>({value:request.input,cost:0})}});
tasks.register(definitions[0]);
console.log(JSON.stringify({direct:await tasks.invoke('identity@1',{answer:42}),workflow:await new WorkflowService(tasks).run([{id:'first',definition:'identity@1',input:{answer:42}},{id:'next',definition:'identity@1',dependsOn:['first'],inputFrom:'first'}]),controls:tasks.controls},null,2));
