import test from 'node:test';
import assert from 'node:assert/strict';
import { CommandOutput, Result, reviewSummary, commandResultPresentation } from '../review.js';

const event=(kind,payload)=>({kind,payload});
const patch={id:'pending-patch',name:'apply_patch',preview:{edits:[{path:'one.js'},{path:'two.js'}]}};

test('a pending patch counts once as a proposal before it appears in retained events',()=>{
  const summary=reviewSummary([],patch);
  assert.equal(summary.changeCount,1);
  assert.equal(summary.pendingChanges,true);
  assert.equal(summary.changes.length,0);
  assert.equal(summary.checks.length,0);
});

test('pending approval events do not double-count a proposal or imply a completed result',()=>{
  const summary=reviewSummary([
    event('tool.proposed',patch),event('approval.required',patch),
    event('tool.proposed',{id:'earlier-patch',name:'apply_patch'}),
    event('tool.completed',{operation_id:'earlier-patch',name:'apply_patch',result:{status:'completed'}}),
  ],patch);
  assert.equal(summary.changeCount,2);
  assert.deepEqual(summary.changes.map(operation=>operation.id),['earlier-patch']);
  assert.equal(summary.checks.length,1);
});

test('pending commands do not inflate changes or results and empty state remains honest',()=>{
  const summary=reviewSummary([],{id:'command',name:'run_command'});
  assert.equal(summary.changeCount,0);
  assert.equal(summary.pendingChanges,false);
  assert.equal(summary.checks.length,0);
  assert.equal(reviewSummary().changeCount,0);
});

test('stdout and stderr retain exact text and a zero exit code remains actual evidence',()=>{
  const result={status:'completed',exit_code:0,output:{stdout:'line one\nline two\n',stderr:'<script>untrusted output</script>\n'}};
  const presentation=commandResultPresentation(result);
  assert.equal(presentation.stdout,result.output.stdout);
  assert.equal(presentation.stderr,result.output.stderr);
  assert.equal(presentation.exitCode,0);
  assert.equal(presentation.tone,'neutral');
  assert.equal(presentation.outputReported,true);
});

test('missing output or exit code is distinct from explicitly empty streams and exit zero',()=>{
  const missing=commandResultPresentation({status:'failed',output:null,exit_code:null});
  assert.equal(missing.stdout,null);
  assert.equal(missing.stderr,null);
  assert.equal(missing.outputReported,false);
  assert.equal(missing.exitCode,null);
  assert.equal(missing.tone,'stop');
  const empty=commandResultPresentation({status:'completed',exit_code:0,output:{stdout:'',stderr:''}});
  assert.equal(empty.outputReported,true);
  assert.equal(empty.stdout,'');
  assert.equal(empty.stderr,'');
});

test('backend failure, timeout, cancellation and capture errors retain their meaning',()=>{
  for(const status of ['failed','error','timed_out'])assert.equal(commandResultPresentation({status}).tone,'stop');
  assert.equal(commandResultPresentation({status:'cancelled'}).tone,'caution');
  assert.equal(commandResultPresentation({status:'completed',exit_code:7}).tone,'stop');
  const partial=commandResultPresentation({output:{stdout:'partial',stdout_read_error:'capture interrupted',stderr_read_error:'pipe failed'}});
  assert.equal(partial.stdoutError,'capture interrupted');
  assert.equal(partial.stderrError,'pipe failed');
});

function visit(node,predicate){
  if(Array.isArray(node))return node.flatMap(child=>visit(child,predicate));
  if(!node||typeof node!=='object')return [];
  return [...(predicate(node)?[node]:[]),...visit(node.props?.children,predicate)];
}

test('readable command streams are text values and full JSON stays inside collapsed details',()=>{
  const result={status:'completed',exit_code:0,output:{stdout:'<img onerror="untrusted">\n',stderr:'warning\n',cwd:'project',sandboxed:false}};
  const tree=CommandOutput({result});
  const streams=visit(tree,node=>node.props?.class==='command-output');
  assert.equal(streams.length,2);
  assert.deepEqual(streams.map(stream=>visit(stream,node=>node.type?.name==='RawValue')[0].props.value),[result.output.stdout,result.output.stderr]);
  const details=visit(tree,node=>node.type==='details')[0];
  assert.equal(Boolean(details.props.open),false);
  assert.equal(visit(details,node=>node.type?.name==='RawValue')[0].props.value,result);
  assert.equal(visit(tree,node=>node.type==='img'||'dangerouslySetInnerHTML' in (node.props??{})).length,0);
});

function visibleText(node){
  if(Array.isArray(node))return node.map(visibleText).join(' ');
  if(typeof node==='string'||typeof node==='number')return String(node);
  if(!node||typeof node!=='object'||node.type==='details')return '';
  return visibleText(node.props?.children);
}

test('denial uses human decision copy and keeps provider instructions only inside collapsed Details',()=>{
  const result={status:'denied',error:'The user denied this operation. Do not disguise it or attempt to bypass this decision.',output:{message:'Internal model instruction'}};
  for(const [name,message] of [['apply_patch','You denied this edit.'],['run_command','You denied this command.'],['future_tool','You denied this operation.']]){
    const tree=Result({operation:{name,result}});
    const text=visibleText(tree);
    assert.ok(text.includes(message));
    assert.ok(text.includes('Denied'));
    assert.equal(text.includes(result.error),false);
    assert.equal(text.includes('Internal model instruction'),false);
    assert.equal(text.includes('Exit code'),false);
    assert.equal(visit(tree,node=>node.type?.name==='CommandOutput').length,0);
    const details=visit(tree,node=>node.type==='details')[0];
    assert.equal(Boolean(details.props.open),false);
    assert.equal(visit(details,node=>node.type?.name==='RawValue')[0].props.value,result);
  }
});

test('ordinary failed tool errors remain visible and are not mistaken for a denial',()=>{
  const result={status:'failed',error:'File changed since the proposal was prepared.'};
  const tree=Result({operation:{name:'apply_patch',result}});
  assert.ok(visibleText(tree).includes(result.error));
  assert.equal(visibleText(tree).includes('You denied'),false);
  assert.equal(tree.props['data-tone'],'stop');
});
