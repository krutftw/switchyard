import test from 'node:test';
import assert from 'node:assert/strict';
import { transcript, mergeEvents, reviewOperations, stateTone, canSubmit } from '../presentation.js';
import { acknowledgedTurn, readJournal, writeJournal } from '../turn-journal.js';
const event=(seq,kind,payload={},run_id='r1',session_id='s1')=>({seq,kind,payload,run_id,session_id,at_ms:1000+seq});
test('streamed assistant completion replaces deltas and does not duplicate output',()=>{
  const result=transcript([event(1,'turn.started',{text:'Task'}),event(2,'model.delta',{text:'hel'}),event(3,'model.delta',{text:'lo'}),event(4,'model.completed',{text:'hello'})]);
  assert.equal(result.length,2);assert.equal(result[1].text,'hello');assert.equal(result[1].streaming,false);
});

test('tool-only model turns do not create empty assistant messages',()=>{
  const result=transcript([event(1,'model.delta',{text:''}),event(2,'model.completed',{text:'',finish:'tool_calls'}),event(3,'tool.proposed',{id:'read',name:'read_file'}),event(4,'model.completed',{text:'   ',finish:'tool_calls'})]);
  assert.equal(result.length,1);assert.equal(result[0].type,'tool');assert.equal(result[0].operation.id,'read');
});
test('tool steps separate adjacent assistant messages and track exact results',()=>{
  const operation={id:'o1',name:'run_command',arguments_hash:'abc'};
  const result=transcript([event(1,'model.completed',{text:'Checking'}),event(2,'tool.proposed',operation),event(3,'approval.required',operation),event(4,'approval.decided',{operation_id:'o1',decision:'allow_once'}),event(5,'tool.started',{operation_id:'o1'}),event(6,'tool.completed',{operation_id:'o1',result:{status:'error',exit_code:1}}),event(7,'model.completed',{text:'Check failed'})]);
  assert.equal(result.length,3);assert.equal(result[1].status,'error');assert.equal(result[1].result.exit_code,1);assert.equal(result[2].text,'Check failed');
});
test('unknown events remain visible as untrusted data',()=>{
  const result=transcript([event(1,'future.event',{html:'<script>bad</script>'})]);
  assert.equal(result[0].type,'activity');assert.equal(result[0].payload.html,'<script>bad</script>');
});
test('bounded event merge deduplicates sequences and excludes another session',()=>{
  const old=Array.from({length:610},(_,i)=>event(i+1,'notice'));
  const result=mergeEvents(old,[event(610,'changed'),event(611,'notice',{},'r','other')],'s1');
  assert.equal(result.length,600);assert.equal(result[0].seq,11);assert.equal(result.at(-1).kind,'changed');
});
test('completed is neutral and uncertain recovery cannot submit a turn',()=>{
  assert.equal(stateTone('completed'),'neutral');assert.equal(canSubmit('recovery_required'),false);assert.equal(canSubmit('interrupted'),true);assert.equal(canSubmit('running'),false);
});
test('review retains proposal when a tool result arrives',()=>{
  const result=reviewOperations([event(1,'tool.proposed',{id:'o',name:'apply_patch',preview:{edits:[]}}),event(2,'tool.completed',{operation_id:'o',name:'apply_patch',result:{status:'ok'}})]);
  assert.deepEqual(result[0].preview,{edits:[]});assert.equal(result[0].result.status,'ok');
});
test('journal accepts only complete original turns and acknowledges the exact session and command',()=>{
  let saved='';const storage={getItem:()=>saved,setItem:(_,value)=>{saved=value;}};
  const turn={session_id:'s1',command_id:'4bd6cc30-30ef-4872-b8d7-6e5ef50cfd61',text:'Run focused checks'};
  assert.equal(writeJournal(storage,{s1:turn}),true);assert.deepEqual(readJournal(storage),{s1:turn});
  assert.equal(acknowledgedTurn([event(1,'turn.started',{command_id:turn.command_id})],turn),true);
  assert.equal(acknowledgedTurn([event(1,'turn.started',{command_id:turn.command_id},'r','other')],turn),false);
  saved=JSON.stringify({s1:{...turn,command_id:'invalid'},s2:turn});assert.deepEqual(readJournal(storage),{});
});
test('blocked storage is reported without losing the in-memory request',()=>{
  assert.deepEqual(readJournal({getItem(){throw Error('blocked');}}),{});
  assert.equal(writeJournal({setItem(){throw Error('blocked');}},{s1:{}}),false);
});
