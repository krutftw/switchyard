import { html, useState } from './vendor/preact-htm.js';
import { Icon } from './icons.js';
import { Button, Notice, RawValue, CopyButton, downloadText } from './components.js';
import { DiffView } from './diff.js';
import { reviewOperations, toolName, toolStateLabel } from './presentation.js';

export function reviewSummary(events=[],pendingOperation=null) {
  const history=reviewOperations(events).filter(operation=>operation.id!==pendingOperation?.id);
  const changes=history.filter(operation=>operation.name==='apply_patch');
  const checks=history.filter(operation=>operation.result);
  const pendingChanges=pendingOperation?.name==='apply_patch';
  return {history,changes,checks,pendingChanges,changeCount:changes.length+Number(pendingChanges)};
}

export function commandResultPresentation(result={}) {
  const output=result.output;
  const stdout=typeof output?.stdout==='string'?output.stdout:null;
  const stderr=typeof output?.stderr==='string'?output.stderr:null;
  const exitCode=Number.isInteger(result.exit_code)?result.exit_code:null;
  const tone=['failed','error','timed_out'].includes(result.status)||(exitCode!==null&&exitCode!==0)
    ?'stop':result.status==='cancelled'?'caution':'neutral';
  return {stdout,stderr,exitCode,tone,
    outputReported:stdout!==null||stderr!==null,
    stdoutError:typeof output?.stdout_read_error==='string'?output.stdout_read_error:'',
    stderrError:typeof output?.stderr_read_error==='string'?output.stderr_read_error:'',
  };
}

function Identity({operation}) {
  return html`<details class="operation-details"><summary>Exact request details</summary><dl><dt>Tool</dt><dd><code>${operation.name}</code></dd><dt>Operation</dt><dd><code>${operation.id}</code></dd><dt>Argument hash</dt><dd><code>${operation.arguments_hash||'Not present in retained events'}</code></dd></dl>${operation.arguments&&html`<${RawValue} value=${operation.arguments} label="Exact tool arguments"/>`}</details>`;
}

function Proposal({operation}) {
  const preview=operation.preview||{};
  if(operation.name==='run_command')return html`<div class="command-review"><label class="section-label">COMMAND</label><pre class="command-preview" tabindex="0" aria-label="Exact proposed command"><code>${preview.command??operation.arguments?.command??'Command unavailable'}</code></pre><dl class="command-location"><dt>Working directory</dt><dd><code>${preview.absolute_cwd||preview.cwd||operation.arguments?.cwd||'Not reported'}</code></dd>${preview.shell&&html`<dt>Shell</dt><dd><code>${preview.shell}</code></dd>`}${preview.timeout_ms!=null&&html`<dt>Time limit</dt><dd>${preview.timeout_ms} ms</dd>`}</dl><${Notice} title="Runs with your machine permissions" tone="caution">${preview.warning||'This command runs with your account’s machine and network access. Its working directory is not a sandbox.'}<//><${CopyButton} text=${preview.command??operation.arguments?.command??''} label="Copy command"/></div>`;
  if(operation.name==='apply_patch'){
    const edits=Array.isArray(preview.edits)?preview.edits:[];
    const fullDiff=edits.map(edit=>edit.diff||'').join('\n');
    return html`<div class="patch-review">${preview.note&&html`<p class="field-hint">${preview.note}</p>`}${edits.length===0&&html`<${Notice} title="Diff preview unavailable" tone="caution">Review the exact request details below before deciding.<//>`}${edits.map((edit,index)=>html`<section class="file-review" key=${`${edit.path}:${index}`}><h4><${Icon} name="edit"/><code>${edit.path}</code><span class="field-hint">${edit.before_sha256==null?'New file':'Edit'}</span></h4><${DiffView} text=${edit.diff||''} label=${`Proposed changes to ${edit.path}`}/><details class="file-hashes"><summary>File hashes</summary><dl><dt>Expected before</dt><dd><code>${edit.before_sha256??'File must not exist'}</code></dd><dt>Proposed after</dt><dd><code>${edit.after_sha256||'Not reported'}</code></dd></dl></details></section>`)}${fullDiff&&html`<div class="review-tools"><${Button} icon="download" onClick=${()=>downloadText(fullDiff,`${operation.id}-proposed.diff`)}>Download full diff<//><${CopyButton} text=${fullDiff} label="Copy full diff"/></div>`}</div>`;
  }
  return html`<${RawValue} value=${operation.preview||operation.arguments||{}} label="Tool proposal"/>`;
}

export function CommandOutput({result}) {
  const {stdout,stderr,outputReported,stdoutError,stderrError}=commandResultPresentation(result);
  return html`${stdout&&html`<section class="command-output" aria-label="Standard output"><h4>Standard output</h4><${RawValue} value=${stdout} label="Command standard output"/></section>`}${stderr&&html`<section class="command-output" aria-label="Standard error"><h4>Standard error</h4><${RawValue} value=${stderr} label="Command standard error"/></section>`}${!stdout&&!stderr&&html`<p class="field-hint">${outputReported?'No stdout or stderr output was recorded.':'Command output was not reported.'}</p>`}${stdoutError&&html`<p class="field-error">Standard output could not be fully read: ${stdoutError}</p>`}${stderrError&&html`<p class="field-error">Standard error could not be fully read: ${stderrError}</p>`}<details class="operation-details result-details"><summary>Details (raw result)</summary><${RawValue} value=${result} label="Full recorded command result JSON"/></details>`;
}

export function Result({operation}) {
  const result=operation.result;
  if(!result)return null;
  if(result.status==='denied'){
    const message=operation.name==='apply_patch'?'You denied this edit.':operation.name==='run_command'?'You denied this command.':'You denied this operation.';
    return html`<div class="check-result" data-tone="neutral"><div class="check-heading"><strong>Decision recorded</strong><span class="status-label" data-tone="neutral">Denied</span></div><p class="field-hint">${message}</p><details class="operation-details result-details"><summary>Details (raw result)</summary><${RawValue} value=${result} label="Full recorded denial result JSON"/></details></div>`;
  }
  const command=operation.name==='run_command';
  const {tone,exitCode}=commandResultPresentation(result);
  const status=({completed:'Completed',failed:'Failed',error:'Error',cancelled:'Cancelled',timed_out:'Timed out'})[result.status]||result.status||'Status not reported';
  return html`<div class="check-result" data-tone=${tone}><div class="check-heading"><strong>${command?'Observed command result':'Tool result'}</strong><span class="status-label" data-tone=${tone}>${status}</span></div>${exitCode!==null?html`<p class="exit-status">Exit code <code>${exitCode}</code></p>`:command&&html`<p class="exit-status field-hint">Exit code not reported.</p>`}${result.error&&html`<p class="field-error">${result.error}</p>`}${command?html`<${CommandOutput} result=${result}/>`:result.output!==undefined&&html`<${RawValue} value=${result.output} label="Recorded tool output"/>`}${result.truncated&&html`<p class="field-hint">The tool reported that this output was truncated.</p>`}${result.changes?.length>0&&html`<div class="recorded-changes"><h4>Recorded file changes</h4>${result.changes.map(change=>html`<details key=${change.path}><summary><code>${change.path}</code></summary><dl><dt>Before</dt><dd><code>${change.before_sha256??'New file'}</code></dd><dt>After</dt><dd><code>${change.after_sha256||'Not reported'}</code></dd></dl></details>`)}</div>`}<p class="field-hint">This is the recorded tool result. A completed model turn does not verify the project.</p></div>`;
}

export function ReviewPane({pendingOperation,events=[],selectedOperationId='',onSelectOperation,onDecision,decisionBusy=false,decisionError='',disabled=false}) {
  const [tab,setTab]=useState('changes');
  const {history,changes,checks,pendingChanges,changeCount}=reviewSummary(events,pendingOperation);
  const visible=tab==='changes'?changes:checks;
  const selected=history.find(operation=>operation.id===selectedOperationId);
  return html`<header class="review-header"><div><p class="eyebrow">WORKSPACE REVIEW</p><h2>Actions & results</h2></div><span class="review-signal" data-tone=${pendingOperation?'caution':'neutral'} aria-label=${pendingOperation?'Approval required':'No approval pending'}></span></header><div class="review-content">
    ${pendingOperation&&html`<section class="approval" aria-label="Pending approval"><p class="eyebrow">YOUR APPROVAL IS REQUIRED</p><h3>${pendingOperation.name==='apply_patch'?'Review proposed edits':pendingOperation.name==='run_command'?'Review command':pendingOperation.name}</h3><p class="field-hint">Allow once applies only to this exact recorded request.</p><${Proposal} operation=${pendingOperation}/><${Identity} operation=${pendingOperation}/>${decisionError&&html`<p class="field-error" role="alert">${decisionError}</p>`}<div class="approval-actions"><${Button} onClick=${()=>onDecision(pendingOperation,'deny')} disabled=${disabled||decisionBusy}>Deny<//><${Button} variant="primary" icon="check" onClick=${()=>onDecision(pendingOperation,'allow_once')} disabled=${disabled||decisionBusy} busy=${decisionBusy}>${decisionBusy?'Confirming…':'Allow once'}<//></div></section>`}
    ${!pendingOperation&&decisionError&&html`<${Notice} title="Approval status" tone="stop">${decisionError}<//>`}
    ${selected&&html`<section class="selected-operation"><div class="check-heading"><h3>${toolName(selected.name)}</h3><${Button} onClick=${()=>onSelectOperation?.('')}>Close details<//></div><${Proposal} operation=${selected}/><${Result} operation=${selected}/><${Identity} operation=${selected}/></section>`}
    <div class="review-tabs" aria-label="Review category"><button type="button" aria-pressed=${tab==='changes'} data-active=${tab==='changes'} aria-label=${`Changes: ${changeCount} proposal${changeCount===1?'':'s'}${pendingChanges?', including one awaiting approval':''}`} onClick=${()=>setTab('changes')}>Changes <span>${changeCount}</span></button><button type="button" aria-pressed=${tab==='checks'} data-active=${tab==='checks'} onClick=${()=>setTab('checks')}>Results <span>${checks.length}</span></button></div>
    ${tab==='changes'&&pendingChanges&&html`<p class="field-hint review-pending-note">The change proposal above is awaiting your decision. It has not been applied.</p>`}
    ${visible.length===0&&!(tab==='changes'&&pendingChanges)&&html`<div class="empty-review"><${Icon} name=${tab==='changes'?'edit':'command'}/><h3>${tab==='changes'?'No change proposals in this activity.':'No tool results in this activity.'}</h3><p>${tab==='changes'?'File edits are shown as diffs before you approve them.':'Commands include their actual output and exit status when available.'}</p></div>`}
    ${visible.map(operation=>html`<details class="operation-history" key=${operation.id}><summary><${Icon} name=${operation.name==='apply_patch'?'edit':'command'}/><span>${toolName(operation.name)}</span><span class="field-hint">${toolStateLabel(operation.result?.status||operation.state)}</span></summary>${tab==='changes'&&html`<${Proposal} operation=${operation}/>`}<${Result} operation=${operation}/><${Identity} operation=${operation}/></details>`)}
  </div>`;
}
