import { html, useEffect, useRef, useState } from './vendor/preact-htm.js';
import { Icon } from './icons.js';
import { Button, Notice, Dialog, RawValue, TextContent, CopyButton } from './components.js';
import { stateLabel, stateTone, timeLabel, pretty } from './presentation.js';
import { readAdapterJournal, writeAdapterJournal } from './adapter-journal.js';
import { chosenProfile, canStartProfile, authLabel } from './account-presentation.js';

const enc=encodeURIComponent;
const active=state=>['starting','running','awaiting_approval','interrupting'].includes(state);
const errorText=error=>error?.message||'The existing agent could not complete this request.';
const availabilityLabel=status=>({available:'Available',not_installed:'Not installed',integration_not_implemented:'No workspace integration',version_check_failed:'Version check failed'}[status]||'Status unavailable');
export function mergeAdapterEvents(previous,incoming,runId) {
  const bySeq=new Map();for(const event of [...previous,...incoming])if(event.run_id===runId&&Number.isSafeInteger(event.seq))bySeq.set(event.seq,event);
  return [...bySeq.values()].sort((a,b)=>a.seq-b.seq).slice(-600);
}
export function adapterTranscript(events) {
  const items=[],streams=new Map();
  for(const event of events){
    const p=event.payload||{};
    if(event.kind==='user_task')items.push({...event,type:'user',text:typeof p.text==='string'?p.text:''});
    else if(event.kind==='assistant_delta'){
      const key=p.item_id||'assistant';let item=streams.get(key);
      if(!item){item={...event,type:'assistant',text:''};streams.set(key,item);items.push(item);}
      item.text+=typeof p.text==='string'?p.text:'';
    }else if(event.kind==='item_completed'&&p.item?.type==='agentMessage'){
      const key=p.item.id||'assistant';const stream=streams.get(key);
      if(stream)stream.text=typeof p.item.text==='string'?p.item.text:stream.text;
      else items.push({...event,type:'assistant',text:p.item.text||''});
      streams.delete(key);
    }else items.push({...event,type:'activity'});
  }
  return items;
}

function AdapterReview({run,onDecision,busy,error,connected}) {
  const approvals=run?.pending_approvals||[];
  return html`<header class="review-header"><h2>Agent review</h2><span class="field-hint">${approvals.length?`${approvals.length} pending`:'No pending decisions'}</span></header><div class="review-content">${error&&html`<${Notice} title="Decision could not be confirmed" tone="stop">${error}<//>`}${approvals.map(approval=>html`<section class="approval" key=${approval.id}><p class="eyebrow">CODEX REQUEST</p><h3>${approval.method}</h3><${Notice} title="Agent permission boundary" tone="caution">${approval.permission_boundary}<//><${RawValue} value=${approval.preview} label="Exact Codex approval request"/><details class="operation-details"><summary>Approval identity</summary><dl><dt>Request</dt><dd><code>${approval.id}</code></dd><dt>Expected hash</dt><dd><code>${approval.expected_hash}</code></dd></dl><${CopyButton} text=${pretty(approval.preview)} label="Copy full request"/></details>${!approval.can_allow_once&&html`<p class="field-hint">This request cannot be approved once by the adapter. Deny it to let the agent respond.</p>`}<div class="approval-actions"><${Button} onClick=${()=>onDecision(approval,'deny')} disabled=${busy||!connected}>Deny<//><${Button} variant="primary" onClick=${()=>onDecision(approval,'allow_once')} disabled=${busy||!connected||!approval.can_allow_once}>Allow once<//></div></section>`)}${!approvals.length&&html`<div class="empty-review"><${Icon} name="lock"/><h3>Requests appear here.</h3><p>Codex approval requests include the exact command or change supplied by the CLI.</p></div>`}${run&&html`<details class="operation-details"><summary>Run details and permissions</summary><dl><dt>Run</dt><dd><code>${run.id}</code></dd><dt>Model</dt><dd><code>${run.model||'Reported after the agent starts'}</code></dd><dt>Account</dt><dd>${run.profile_name||'Not reported'}</dd>${run.profile_id&&html`<dt>Account profile ID</dt><dd><code>${run.profile_id}</code></dd>`}<dt>Working folder</dt><dd><code>${run.project_path}</code></dd></dl><p>${run.permission_boundary}</p><p class="field-hint">This run exists only while the local host is running. It is separate from saved native sessions.</p></details>`}</div>`;
}

export function AdapterWorkspace({api,project,hostId,storage,onBusyChange,onOpenNavigation,onNative,onOpenProject,profiles=[],accountDefaults={},accountsLoading=false,accountsError='',onOpenAccounts}) {
  const [adapters,setAdapters]=useState([]),[runs,setRuns]=useState([]),[selectedId,setSelectedId]=useState('');
  const [run,setRun]=useState(null),[events,setEvents]=useState([]),[prompt,setPrompt]=useState(''),[adapterId,setAdapterId]=useState('');
  const [error,setError]=useState(''),[decisionError,setDecisionError]=useState(''),[connected,setConnected]=useState(false),[loading,setLoading]=useState(true);
  const [busy,setBusy]=useState(''),[journal,setJournal]=useState(()=>readAdapterJournal(storage,hostId)),[tick,setTick]=useState(0),[reviewOpen,setReviewOpen]=useState(false),[storageWarning,setStorageWarning]=useState('');
  const eventsRef=useRef([]),lock=useRef(false),scroll=useRef(null),follow=useRef(true),projectRef=useRef(project?.id),selectionRef=useRef(selectedId),journalRef=useRef(journal),mounted=useRef(true);
  const discoveryCache=useRef({value:null,until:0});
  const [profileId,setProfileId]=useState('');
  const runProfile=chosenProfile(profiles,accountDefaults,adapterId,profileId);
  const availableProfiles=profiles.filter(profile=>profile.agent_id===adapterId);
  useEffect(()=>setProfileId(''),[adapterId]);
  projectRef.current=project?.id;journalRef.current=journal;
  selectionRef.current=selectedId;
  const pendingStart=journal[project?.id];
  function changeBusy(value){setBusy(value);onBusyChange?.(Boolean(value));}
  function savePending(request){const next={...readAdapterJournal(storage,hostId),...journalRef.current,[request.project_id]:request};journalRef.current=next;setJournal(next);if(!writeAdapterJournal(storage,hostId,next))setStorageWarning('This browser cannot retain pending starts after reload. Keep this tab open until the run is confirmed.');}
  function clearPending(request){const next={...journalRef.current,...readAdapterJournal(storage,hostId)};if(next[request.project_id]?.command_id===request.command_id)delete next[request.project_id];journalRef.current=next;writeAdapterJournal(storage,hostId,next);if(mounted.current)setJournal(next);}
  useEffect(()=>()=>{mounted.current=false;onBusyChange?.(false);},[]);
  eventsRef.current=events;
  const refresh=()=>setTick(value=>value+1);
  const available=adapters.filter(adapter=>adapter.installed&&adapter.supported);
  const selectedAdapter=adapters.find(adapter=>adapter.id===adapterId);
  useEffect(()=>{setRun(null);setEvents([]);eventsRef.current=[];setSelectedId('');setPrompt('');setDecisionError('');},[project?.id]);
  useEffect(()=>{
    const abort=new AbortController();let stopped=false,timer;
    async function poll(){
      const discovery=discoveryCache.current.value&&Date.now()<discoveryCache.current.until?Promise.resolve(discoveryCache.current.value):api.request('/adapters',{signal:abort.signal}).then(value=>{discoveryCache.current={value,until:Date.now()+60000};return value;});
      const results=await Promise.allSettled([discovery,api.request('/adapter-runs',{signal:abort.signal})]);
      if(stopped||projectRef.current!==project?.id)return;
      if(results.every(result=>result.status==='fulfilled')){
        const next=results[0].value.adapters||[];setAdapters(next);setAdapterId(old=>next.some(item=>item.id===old&&item.installed&&item.supported)?old:next.find(item=>item.installed&&item.supported)?.id||'');
        const current=(results[1].value.runs||[]).filter(item=>!project||item.project_path===project.root);setRuns(current);setConnected(true);setError('');
        if(pendingStart){const confirmed=current.find(item=>item.command_id===pendingStart.command_id);if(confirmed){select(confirmed.id,confirmed);clearPending(pendingStart);setPrompt('');}}
      }else{setConnected(false);setError(errorText(results.find(result=>result.status==='rejected').reason));}
      setLoading(false);timer=setTimeout(poll,3500);
    }
    poll();return()=>{stopped=true;abort.abort();clearTimeout(timer);};
  },[project?.id,tick,pendingStart?.command_id]);
  useEffect(()=>{
    if(!selectedId)return;
    const abort=new AbortController();let stopped=false,timer;
    const runProjectId=project?.id;
    const outdated=()=>stopped||projectRef.current!==runProjectId||selectionRef.current!==selectedId;
    async function poll(){
      try{
        const result=await api.request(`/adapter-runs/${enc(selectedId)}`,{signal:abort.signal});if(outdated())return;
        const next=result.run;let cursor=eventsRef.current.at(-1)?.seq||0;
        cursor=Math.max(cursor,(next.first_retained_seq||1)-1,next.last_seq-600);
        let collected=eventsRef.current;
        for(let page=0;page<3&&cursor<next.last_seq;page++){
          const response=await api.request(`/adapter-runs/${enc(selectedId)}/events?after_seq=${cursor}&limit=200`,{signal:abort.signal});if(outdated())return;
          collected=mergeAdapterEvents(collected,response.events||[],selectedId);if(!response.events?.length)break;cursor=response.events.at(-1).seq;
        }
        setRun(next);setEvents(collected);setConnected(true);setError('');timer=setTimeout(poll,active(next.state)?900:3000);
      }catch(error){if(!outdated()&&error.code!=='aborted'){setError(errorText(error));setConnected(false);timer=setTimeout(poll,3000);}}
    }
    poll();return()=>{stopped=true;abort.abort();clearTimeout(timer);};
  },[selectedId,project?.id,tick]);
  useEffect(()=>{if(follow.current&&scroll.current)scroll.current.scrollTop=scroll.current.scrollHeight;},[events]);
  function select(id,selectedRun=runs.find(item=>item.id===id)||null){selectionRef.current=id;setSelectedId(id);setRun(selectedRun);setEvents([]);eventsRef.current=[];setError('');setDecisionError('');follow.current=true;}
  async function start(event,retry=false){
    event?.preventDefault();if(lock.current||!project||!connected||(!retry&&(!prompt.trim()||!adapterId||pendingStart||!canStartProfile(runProfile))))return;
    lock.current=true;changeBusy('start');setError('');
    const request=retry?pendingStart:{adapter_id:adapterId,project_id:project.id,profile_id:runProfile.id,prompt:prompt.trim(),command_id:crypto.randomUUID()};
    if(!request){lock.current=false;changeBusy('');return;}
    savePending(request);
    try{const result=await api.request('/adapter-runs',{method:'POST',body:request});clearPending(request);if(mounted.current&&projectRef.current===request.project_id){setRuns(old=>[result.run,...old.filter(item=>item.id!==result.run.id)]);select(result.run.id,result.run);setPrompt('');refresh();}}
    catch(error){if(!error.uncertain)clearPending(request);if(mounted.current&&projectRef.current===request.project_id){setError(errorText(error));refresh();}}
    finally{changeBusy('');lock.current=false;}
  }
  async function decide(approval,decision){
    if(lock.current||!run)return;lock.current=true;changeBusy('decision');setDecisionError('');
    try{await api.request(`/adapter-runs/${enc(run.id)}/decisions`,{method:'POST',body:{approval_id:approval.id,expected_hash:approval.expected_hash,decision}});refresh();}
    catch(error){setDecisionError(errorText(error));refresh();}
    finally{changeBusy('');lock.current=false;}
  }
  async function interrupt(){
    if(lock.current||!run)return;lock.current=true;changeBusy('interrupt');
    try{await api.request(`/adapter-runs/${enc(run.id)}/interrupt`,{method:'POST',body:{}});refresh();}
    catch(error){setError(errorText(error));refresh();}
    finally{changeBusy('');lock.current=false;}
  }
  const review=html`<${AdapterReview} run=${run} onDecision=${decide} busy=${Boolean(busy)} error=${decisionError} connected=${connected}/>`;
  return html`<main class="workspace-body"><header class="workspace-header"><${Button} class="rail-toggle" icon="menu" aria-label="Open projects and sessions" onClick=${onOpenNavigation}/><div class="header-project"><span class="eyebrow">${project?.name||'LOCAL WORKSPACE'}</span><h1>Existing agents</h1>${run&&html`<p class="run-account">Account for this run: <strong>${run.profile_name||run.profile_id||'Not reported'}</strong></p>`}</div><div class="header-actions">${run&&html`<span class="status-label" data-tone=${stateTone(run.state)}>${stateLabel(run.state)}</span>`}<${Button} class="review-toggle" icon="sidebar" onClick=${()=>setReviewOpen(true)}>Review${run?.pending_approvals?.length?' request':''}<//></div></header><section class="conversation" aria-label="Existing agent run"><div class="conversation-scroll" ref=${scroll} onScroll=${()=>{const node=scroll.current;follow.current=node.scrollHeight-node.scrollTop-node.clientHeight<110;}}>
    <div class="adapter-intro"><p class="eyebrow">CONNECTED THROUGH YOUR CLI</p><p>Run an installed agent in this project using its existing sign-in and configuration. These are single-turn runs kept for this host session.</p><${Button} onClick=${()=>{if(!lock.current)onNative();}} disabled=${Boolean(busy)} icon="switch">Back to saved sessions<//> <${Button} icon="refresh" disabled=${Boolean(busy)} onClick=${()=>{discoveryCache.current={value:null,until:0};refresh();}}>Check installed agents<//></div>
    ${loading&&html`<p role="status">Checking installed agents…</p>`}${adapters.length>0&&html`<div class="adapter-list">${adapters.map(adapter=>html`<div class="adapter-status" key=${adapter.id}><div><strong>${adapter.name}</strong><span class="field-hint">${adapter.version||''}</span></div><span>${availabilityLabel(adapter.status)}</span>${!adapter.supported&&html`<p class="field-hint">${adapter.id==='claude'?'Use Accounts to sign in and open Claude Code in its own terminal.':'Runs are not available in this version.'}</p>`}</div>`)}</div>`}
    ${runs.length>0&&html`<label class="field"><span>Runs in this host session</span><select value=${selectedId} onChange=${event=>select(event.currentTarget.value)} disabled=${Boolean(busy)}><option value="">Choose a run</option>${runs.map(item=>html`<option key=${item.id} value=${item.id}>${item.adapter_id} · ${item.profile_name||item.profile_id||'Account not reported'} · ${timeLabel(item.started_at_ms)} · ${stateLabel(item.state)}</option>`)}</select></label>`}
    ${run?.first_retained_seq>1&&html`<p class="history-note">Earlier output has left the host's bounded event window.</p>`}
    ${adapterTranscript(events).map(item=>item.type==='assistant'||item.type==='user'?html`<article class="message" data-role=${item.type} key=${item.seq}><div class="message-header"><span class="message-avatar" aria-hidden="true">${item.type==='user'?'Y':html`<${Icon} name="command"/>`}</span><strong>${item.type==='user'?'You':'Codex'}</strong><time>${timeLabel(item.at_ms)}</time></div><${TextContent} text=${item.text}/></article>`:item.kind==='notice'||item.kind==='adapter_error'?html`<${Notice} key=${item.seq} title=${item.kind==='adapter_error'?'Agent error':'Agent notice'} tone=${item.kind==='adapter_error'?'stop':'neutral'}>${item.payload?.message}<//>`:item.kind==='state_changed'?html`<div class="state-event" key=${item.seq}><${Icon} name="info"/><span>${stateLabel(item.payload?.state)}</span></div>`:item.kind==='approval_requested'?html`<div class="tool-event" key=${item.seq}><${Button} icon="lock" onClick=${()=>setReviewOpen(true)}>Review agent request<//></div>`:html`<details class="activity-details" key=${item.seq}><summary>${item.kind.replaceAll('_',' ')}</summary><${RawValue} value=${item.payload}/></details>`)}
    </div></section><section class="composer-area" aria-label="Existing agent task">${error&&html`<${Notice} title="Existing agent status" tone="stop" action=${html`<${Button} icon="refresh" onClick=${refresh}>Refresh state<//>`}>${error}<//>`}${!project&&html`<${Notice} title="Open a project first" action=${html`<${Button} onClick=${onOpenProject}>Open project<//>`}>Choose the local folder this agent should work in.<//>`}${pendingStart&&html`<${Notice} title="Start request awaiting confirmation" tone="caution" action=${html`<${Button} onClick=${event=>start(event,true)} disabled=${Boolean(busy)||!connected}>Retry same request<//>`}>Account for this pending request: <strong>${profiles.find(profile=>profile.id===pendingStart.profile_id)?.name||pendingStart.profile_id||'Not reported'}</strong>. Refresh checks whether the run was created. Retrying retains this account and the original request ID.<//>`}${run?.state==='recovery_required'&&html`<${Notice} title="Review the interrupted operation" tone="stop">The CLI could not confirm the outcome. Check its output and project state before explicitly starting another run.<//>`}${storageWarning&&html`<p class="field-hint" role="status">${storageWarning}</p>`}${accountsError&&html`<p class="field-error" role="alert">Account list: ${accountsError}</p>`}${runProfile&&!canStartProfile(runProfile)&&html`<${Notice} title="Check the selected account" action=${html`<${Button} onClick=${onOpenAccounts}>Open Accounts<//>`}>${runProfile.name}${runProfile.account?.auth_status==='signed_out'?' needs a sign-in before starting a run.':' needs a confirmed sign-in status. Use Refresh account in Accounts to check sign-in status before starting a run.'}<//>`}<form class="composer adapter-composer" onSubmit=${start}><div class="composer-top"><label class="model-select"><span>Agent</span><select value=${adapterId} onChange=${event=>setAdapterId(event.currentTarget.value)} disabled=${!available.length||Boolean(busy)} aria-label="Existing agent"><option value="" disabled>${available.length?'Choose agent':'No supported agent available'}</option>${available.map(adapter=>html`<option key=${adapter.id} value=${adapter.id}>${adapter.name}</option>`)}</select></label><label class="model-select account-select"><span>Account</span><select value=${runProfile?.id||''} onChange=${event=>setProfileId(event.currentTarget.value)} disabled=${Boolean(busy)||Boolean(pendingStart)||!availableProfiles.length} aria-label="Account for new run"><option value="" disabled>${accountsLoading?'Loading accounts…':'Choose account'}</option>${availableProfiles.map(profile=>html`<option key=${profile.id} value=${profile.id}>${profile.name} · ${authLabel(profile.account?.auth_status)}</option>`)}</select></label><${Button} class="manage-accounts" onClick=${onOpenAccounts} disabled=${Boolean(busy)}>Accounts<//></div><div class="composer-text"><textarea id="task-input" rows="1" value=${prompt} aria-label="Task for existing agent" onInput=${event=>setPrompt(event.currentTarget.value)} placeholder="Describe one task for the existing agent…" disabled=${!project||Boolean(busy)} onKeyDown=${event=>{if((event.ctrlKey||event.metaKey)&&event.key==='Enter'){event.preventDefault();start(event);}}}/></div><div class="composer-bottom"><span class="field-hint">Starting may use your provider account.</span><div>${run&&active(run.state)&&html`<${Button} icon="stop" onClick=${interrupt} disabled=${Boolean(busy)||!connected}>${busy==='interrupt'?'Interrupting…':'Interrupt'}<//>`}<${Button} type="submit" icon="play" variant="primary" disabled=${!connected||!project||!adapterId||!prompt.trim()||Boolean(busy)||Boolean(pendingStart)||!canStartProfile(runProfile)} busy=${busy==='start'}>${busy==='start'?'Starting…':'Start new run'}<//></div></div></form>${selectedAdapter&&html`<details class="permission-note"><summary>Existing agent permissions</summary><p>${selectedAdapter.permission_boundary}</p></details>`}</section></main><aside class="review-pane" aria-label="Review existing agent actions">${review}</aside><${Dialog} open=${reviewOpen} onClose=${()=>setReviewOpen(false)} title="Agent review" class="review-dialog">${reviewOpen&&review}<//>`;
}
