import test from 'node:test';
import assert from 'node:assert/strict';
import { adapterTranscript, mergeAdapterEvents, groupConversations, continuationTarget, continuationBlocker } from '../adapters.js';
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

const run=(id,started_at_ms,extra={})=>({id,conversation_id:'c1',started_at_ms,state:'completed',thread_id:'t1',ephemeral:false,title:'',...extra});
test('conversations group continued runs oldest first and list the newest conversation first',()=>{
  const groups=groupConversations([run('b',20),run('x',30,{conversation_id:'c2',title:' Other '}),run('a',10,{title:'First task'}),run('legacy',5,{conversation_id:undefined})]);
  assert.deepEqual(groups.map(group=>group.id),['c2','c1','legacy']);
  assert.deepEqual(groups[1].runs.map(item=>item.id),['a','b']);
  assert.equal(groups[1].title,'First task');assert.equal(groups[1].latest.id,'b');assert.equal(groups[0].title,'Other');
});
test('a finished saved conversation continues from its latest run that attached a thread',()=>{
  const [conversation]=groupConversations([run('a',10),run('b',20),run('c',30,{state:'failed',thread_id:null})]);
  assert.equal(continuationTarget(conversation).id,'b');assert.equal(continuationBlocker(conversation),'');
});
test('active, interrupted-unreviewed and unsaved conversations cannot be continued',()=>{
  const busy=groupConversations([run('a',10),run('b',20,{state:'awaiting_approval'})])[0];
  assert.equal(continuationTarget(busy),null);assert.equal(continuationBlocker(busy),'');
  const review=groupConversations([run('a',10,{state:'recovery_required'})])[0];
  assert.equal(continuationTarget(review),null);assert.equal(continuationBlocker(review),'review');
  const unsaved=groupConversations([run('a',10,{ephemeral:true})])[0];
  assert.equal(continuationTarget(unsaved),null);assert.match(continuationBlocker(unsaved),/not saved/);
  const threadless=groupConversations([run('a',10,{state:'failed',thread_id:null})])[0];
  assert.equal(continuationTarget(threadless),null);assert.match(continuationBlocker(threadless),/did not create a conversation/);
  assert.equal(continuationTarget(null),null);assert.equal(continuationBlocker(null),'');
});
test('pending continuation starts keep their continued run and reject an empty one',()=>{
  const request={project_id:'p',adapter_id:'codex',prompt:'Next step',continue_run_id:'run-1',command_id:'4bd6cc30-30ef-4872-b8d7-6e5ef50cfd61'};
  assert.deepEqual(readAdapterJournal({getItem:()=>JSON.stringify({host_id:'continue-host',requests:{p:request}})},'continue-host').p,request);
  assert.deepEqual(readAdapterJournal({getItem:()=>JSON.stringify({host_id:'continue-empty',requests:{p:{...request,continue_run_id:''}}})},'continue-empty'),{});
});
