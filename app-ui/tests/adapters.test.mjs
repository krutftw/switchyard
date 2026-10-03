import test from 'node:test';
import assert from 'node:assert/strict';
import { adapterTranscript, mergeAdapterEvents } from '../adapters.js';
import { readAdapterJournal, writeAdapterJournal } from '../adapter-journal.js';
const event=(seq,kind,payload={},run_id='r')=>({seq,kind,payload,run_id,at_ms:seq});
test('external run keeps the actual human task and final assistant text once',()=>{
  const items=adapterTranscript([event(1,'user_task',{text:'Inspect the project'}),event(2,'assistant_delta',{item_id:'i',text:'Hel'}),event(3,'assistant_delta',{item_id:'i',text:'lo'}),event(4,'item_completed',{item:{id:'i',type:'agentMessage',text:'Hello'}})]);
  assert.equal(items.length,2);assert.equal(items[0].type,'user');assert.equal(items[0].text,'Inspect the project');assert.equal(items[1].text,'Hello');
});
test('external tool and unfamiliar events retain exact untrusted payloads',()=>{
  const payload={item:{type:'future',output:'<img onerror="bad">'}};
  const items=adapterTranscript([event(1,'item_completed',payload)]);
  assert.equal(items[0].type,'activity');assert.deepEqual(items[0].payload,payload);
});
test('external event history is isolated to a run and bounded',()=>{
  const items=mergeAdapterEvents(Array.from({length:605},(_,i)=>event(i+1,'notice')),[event(605,'updated'),event(606,'foreign',{},'another')],'r');
  assert.equal(items.length,600);assert.equal(items[0].seq,6);assert.equal(items.at(-1).kind,'updated');
});
test('uncertain external starts retain their original request across reload within one host',()=>{
  let stored='';const storage={getItem:()=>stored,setItem:(_,value)=>{stored=value;}};
  const request={project_id:'p1',adapter_id:'codex',prompt:'Inspect local files',command_id:'4bd6cc30-30ef-4872-b8d7-6e5ef50cfd61'};
  assert.equal(writeAdapterJournal(storage,'host1',{p1:request}),true);
  assert.deepEqual(readAdapterJournal(storage,'host1'),{p1:request});
  assert.deepEqual(readAdapterJournal(storage,'new-host'),{});
});
test('external pending journal rejects malformed requests and reports blocked persistence',()=>{
  const storage={getItem:()=>JSON.stringify({host_id:'h',requests:{p:{project_id:'p',adapter_id:'codex',prompt:'hello',command_id:'bad'}}})};
  assert.deepEqual(readAdapterJournal(storage,'h'),{});
  assert.equal(writeAdapterJournal({setItem(){throw Error('blocked');}},'h',{}),false);
});

test('persisted uncertain starts retain their exact account profile',()=>{
  const request={project_id:'p',adapter_id:'codex',profile_id:'original-account',prompt:'Inspect local files',command_id:'4bd6cc30-30ef-4872-b8d7-6e5ef50cfd61'};
  const storage={getItem:()=>JSON.stringify({host_id:'profile-reload',requests:{p:request}})};
  assert.deepEqual(readAdapterJournal(storage,'profile-reload').p,request);
});
test('blocked storage retains uncertain starts in memory across workspace mode changes',()=>{
  const storage={getItem(){throw Error('blocked');},setItem(){throw Error('blocked');}};
  const request={project_id:'p',adapter_id:'codex',prompt:'Inspect local files',command_id:'4bd6cc30-30ef-4872-b8d7-6e5ef50cfd61'};
  assert.equal(writeAdapterJournal(storage,'blocked-host',{p:request}),false);
  assert.deepEqual(readAdapterJournal(storage,'blocked-host'),{p:request});
  writeAdapterJournal(storage,'blocked-host',{});
  assert.deepEqual(readAdapterJournal(storage,'blocked-host'),{});
});
