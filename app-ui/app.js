import { html, render, useEffect, useRef, useState } from './vendor/preact-htm.js';
import { createApi, bootstrapToken, TOKEN_STORAGE_KEY } from './api.js';
import { Icon } from './icons.js';
import { Button, IconButton, Notice, Dialog, RawValue, TextContent } from './components.js';
import { ReviewPane } from './review.js';
import { AdapterWorkspace } from './adapters.js';
import { ProviderDialog } from './providers.js';
import { AccountsDialog, useAccountProfiles } from './accounts.js';
import { BRAND, stateLabel, stateTone, canSubmit, timeLabel, fullTime, modelId, isMockModel, mergeEvents, transcript, toolName, toolStateLabel } from './presentation.js';
import { readJournal, writeJournal, acknowledgedTurn } from './turn-journal.js';

const enc = encodeURIComponent;
const sessionStore = (() => { try { return sessionStorage; } catch { return null; } })();
let previousToken='';try{previousToken=sessionStore?.getItem(TOKEN_STORAGE_KEY)||'';}catch{}
let token = '', launchError = null;
try { token = bootstrapToken(); } catch (error) { launchError = error; }
const api = createApi(token);
let adapterHostId='';
try{if(previousToken===token)adapterHostId=sessionStore?.getItem('switchya.adapterHost')||'';}catch{}
if(!adapterHostId){adapterHostId=crypto.randomUUID();try{sessionStore?.setItem('switchya.adapterHost',adapterHostId);}catch{}}
const readSelection = () => { try { return JSON.parse(sessionStore?.getItem('switchya.selection') || '{}'); } catch { return {}; } };
const savedSelection = readSelection();
const initialJournal = readJournal(sessionStore);
const messageOf = error => error?.message || 'The local app could not complete this request.';
const sortSessions = sessions => [...sessions].sort((a,b) => b.updated_at_ms - a.updated_at_ms);

function Status({state,children}) {
  return html`<span class="status-label" data-tone=${stateTone(state)}><span class="status-dot" aria-hidden="true"></span>${children || stateLabel(state)}</span>`;
}

function Conversation({events,session,project,loading,error,onReview,onOpenProject,hasModels}) {
  const scroll = useRef(null), follow = useRef(true);
  const items = transcript(events);
  useEffect(() => { if (follow.current && scroll.current) scroll.current.scrollTop = scroll.current.scrollHeight; }, [events]);
  useEffect(() => { follow.current = true; if (scroll.current) scroll.current.scrollTop = scroll.current.scrollHeight; }, [session?.id]);
  return html`<section class="conversation" aria-label="Session conversation"><div class="conversation-scroll" ref=${scroll} onScroll=${() => { const node=scroll.current; follow.current=node.scrollHeight-node.scrollTop-node.clientHeight<110; }}>
    ${loading && html`<p class="loading-state" role="status">Loading session…</p>`}
    ${error && html`<${Notice} title="Session could not be refreshed" tone="stop">${error}<//>`}
    ${!loading && !session && html`<div class="empty-state"><div class="empty-mark" aria-hidden="true"><${Icon} name="switch" size=${32}/></div><p class="eyebrow">YOUR CODE. YOUR WORKSPACE.</p><h1>${project ? 'Start with the task.' : 'Give your work a place.'}</h1><p>${project ? 'Describe what you want to build, change or understand. Proposed edits and commands will appear for your review.' : 'Open a local project to keep its conversations, proposed changes and results together.'}</p>${!project && html`<div class="empty-actions"><${Button} variant="primary" icon="plus" onClick=${onOpenProject}>Open a project<//></div>`}${project && !hasModels && html`<p class="field-hint">Connect a coding model below before starting.</p>`}</div>`}
    ${session && !loading && items.length===0 && html`<div class="empty-state"><div class="empty-mark" aria-hidden="true"><${Icon} name="junction" size=${32}/></div><h1>Your session is ready.</h1><p>Send a task below. Model replies and actual tool activity will appear here.</p></div>`}
    ${events[0]?.seq > 1 && html`<p class="history-note">Showing the most recent activity. Earlier events remain in local session storage.</p>`}
    ${items.map(item => item.type==='user' || item.type==='assistant' ? html`<article key=${item.seq} class="message" data-role=${item.type}><div class="message-header"><span class="message-avatar" aria-hidden="true">${item.type==='user'?'Y':html`<${Icon} name="switch"/>`}</span><strong>${item.type==='user'?'You':BRAND}</strong><time title=${fullTime(item.at_ms)}>${timeLabel(item.at_ms)}</time>${item.streaming && session?.state==='running' && html`<span class="stream-label">Writing</span>`}</div><${TextContent} text=${item.text || (item.streaming?'…':'(No text response)')}/></article>` : item.type==='tool' ? html`<article key=${item.seq} class="tool-event"><button class="tool-event-trigger" onClick=${() => onReview(item.operation.id)}><${Icon} name=${item.operation.name==='apply_patch'?'edit':'command'}/><span><strong>${toolName(item.operation.name)}</strong><span class="field-hint">${toolStateLabel(item.status)}</span></span><${Icon} name="external"/></button>${item.result?.error && html`<p class="field-error">${item.result.status==='denied'?(item.operation.name==='apply_patch'?'You denied this edit.':item.operation.name==='run_command'?'You denied this command.':'You denied this operation.'):item.result.error}</p>`}</article>` : item.type==='state' ? html`<div key=${item.seq} class="state-event" data-tone=${item.kind==='run.failed'||item.kind==='recovery.required'?'stop':item.kind==='run.interrupted'?'caution':'neutral'}><${Icon} name=${item.kind==='run.failed'||item.kind==='recovery.required'?'alert':'info'}/><div><strong>${({'run.completed':'Run completed','run.failed':'Run failed','run.interrupted':'Run interrupted','recovery.required':'Recovery required'})[item.kind]}</strong><p>${item.text}</p></div></div>` : html`<details key=${item.seq} class="activity-details"><summary>${item.label}</summary><${RawValue} value=${item.payload}/></details>`)}
  </div></section>`;
}

function ProjectDialog({open,onClose,onOpen}) {
  const [path,setPath]=useState(''), [busy,setBusy]=useState(false), [error,setError]=useState('');
  useEffect(() => { if(open){setError('');setPath('');} },[open]);
  async function submit(event) {
    event.preventDefault(); if(!path.trim() || busy)return;
    setBusy(true);setError('');
    try { await onOpen(path.trim()); onClose(); } catch(error) { setError(messageOf(error)); } finally { setBusy(false); }
  }
  return html`<${Dialog} open=${open} onClose=${() => !busy && onClose()} title="Open a project" class="project-dialog"><form onSubmit=${submit}><div class="dialog-body"><label class="field" for="project-path"><span>Local folder path</span><input id="project-path" value=${path} onInput=${event=>setPath(event.currentTarget.value)} placeholder="C:\\projects\\my-app" autocomplete="off" spellcheck="false" required disabled=${busy} autofocus/></label><p class="field-hint">Choose an existing folder on this computer. The agent can read and search inside this project. Edits and commands require your approval.</p>${error && html`<p class="field-error" role="alert">${error}</p>`}</div><footer class="form-actions"><${Button} onClick=${onClose} disabled=${busy}>Cancel<//><${Button} type="submit" variant="primary" icon="plus" disabled=${busy||!path.trim()} busy=${busy}>${busy?'Opening…':'Open project'}<//></footer></form><//>`;
}

function App() {
  const [status,setStatus]=useState(null), [models,setModels]=useState([]), [projects,setProjects]=useState([]);
  const [booting,setBooting]=useState(Boolean(token&&!launchError)), [connection,setConnection]=useState('connecting'), [globalError,setGlobalError]=useState('');
  const [projectId,setProjectId]=useState(savedSelection.project_id||''), [sessionId,setSessionId]=useState(savedSelection.session_id||'');
  const [sessions,setSessions]=useState([]), [sessionsLoading,setSessionsLoading]=useState(false), [sessionsError,setSessionsError]=useState('');
  const [view,setView]=useState(null), [events,setEvents]=useState([]), [viewLoading,setViewLoading]=useState(false), [viewError,setViewError]=useState('');
  const [model,setModel]=useState(''), [draft,setDraft]=useState(''), [mutationBusy,setMutationBusy]=useState(''), [mutationError,setMutationError]=useState('');
  const [recoveryNote,setRecoveryNote]=useState('');
  const [journal,setJournal]=useState(initialJournal), [storageWarning,setStorageWarning]=useState('');
  const [projectOpen,setProjectOpen]=useState(false), [railOpen,setRailOpen]=useState(false), [reviewOpen,setReviewOpen]=useState(false), [selectedOperationId,setSelectedOperationId]=useState('');
  const [mode,setMode]=useState('native');
  const [providerOpen,setProviderOpen]=useState(false);
  const [accountsOpen,setAccountsOpen]=useState(false);
  const [adapterBusy,setAdapterBusy]=useState(false);
  const [decisionBusy,setDecisionBusy]=useState(false), [decisionError,setDecisionError]=useState(''), [refreshTick,setRefreshTick]=useState(0);
  const [theme,setTheme]=useState(() => { try { return localStorage.getItem('switchyard.workspace.theme')||'system'; } catch { return 'system'; } });
  const currentRef=useRef({projectId,sessionId}), journalRef=useRef(journal), eventsRef=useRef(events), selectedSessionRef=useRef(view?.session);
  const requestLock=useRef(false), composer=useRef(null), draftMap=useRef(new Map());
  currentRef.current={projectId,sessionId}; journalRef.current=journal; eventsRef.current=events; selectedSessionRef.current=view?.session;
  const project=projects.find(item=>item.id===projectId)||view?.project||null;
  const session=view?.session?.id===sessionId?view.session:null;
  const realModels=models.filter(item=>modelId(item)&&!isMockModel(item));
  const pendingTurn=journal[sessionId];
  const connected=connection==='connected';
  const accounts=useAccountProfiles(api,connected&&(accountsOpen||mode==='external'));
  const navigationBusy=Boolean(mutationBusy)||adapterBusy||decisionBusy;

  function saveJournal(next) {
    journalRef.current=next;setJournal(next);
    if(!writeJournal(sessionStore,next))setStorageWarning('This browser cannot retain pending requests across reloads. Keep this tab open until a submitted task is confirmed.');
  }
  function forgetTurn(id) { const next={...journalRef.current};delete next[id];saveJournal(next); }
  function refresh() { setRefreshTick(value=>value+1); }
  function switchSession(id) {
    currentRef.current={projectId,sessionId:id};
    setMode('native');
    draftMap.current.set(sessionId||`new:${projectId}`,draft);
    setSessionId(id);setView(null);setEvents([]);setMutationError('');setDecisionError('');setSelectedOperationId('');setRecoveryNote('');
    setDraft(draftMap.current.get(id||`new:${projectId}`)||'');setRailOpen(false);
  }
  function switchProject(id) {
    currentRef.current={projectId:id,sessionId:''};
    draftMap.current.set(sessionId||`new:${projectId}`,draft);
    setProjectId(id);setSessionId('');setView(null);setEvents([]);setSessions([]);setDraft(draftMap.current.get(`new:${id}`)||'');setMutationError('');setRailOpen(false);
  }
  useEffect(()=>{try{sessionStore?.setItem('switchya.selection',JSON.stringify({project_id:projectId,session_id:sessionId}));}catch{}},[projectId,sessionId]);

  useEffect(() => {
    if(!token||launchError){setBooting(false);setConnection('offline');return;}
    const abort=new AbortController();let stopped=false;
    async function load() {
      const results=await Promise.allSettled([api.request('/status',{signal:abort.signal}),api.request('/models',{signal:abort.signal}),api.request('/projects',{signal:abort.signal})]);
      if(stopped)return;
      let error='';
      if(results[0].status==='fulfilled'){setStatus(results[0].value);setConnection('connected');}else{setConnection('offline');error=messageOf(results[0].reason);}
      if(results[1].status==='fulfilled'){const next=results[1].value.models||[];setModels(next);setModel(old=>next.some(item=>modelId(item)===old&&!isMockModel(item))?old:modelId(next.find(item=>!isMockModel(item)))||'');}else error=error||messageOf(results[1].reason);
      if(results[2].status==='fulfilled'){const next=results[2].value.projects||[];setProjects(next);setProjectId(old=>next.some(item=>item.id===old)?old:next[0]?.id||'');}else error=error||messageOf(results[2].reason);
      setGlobalError(error);setBooting(false);
    }
    load();return()=>{stopped=true;abort.abort();};
  },[refreshTick]);

  useEffect(() => {
    if(!token||!projectId)return;
    const abort=new AbortController();let stopped=false,timer;
    setSessionsLoading(true);setSessionsError('');
    async function poll() {
      try { const result=await api.request(`/sessions?project_id=${enc(projectId)}`,{signal:abort.signal});if(stopped||currentRef.current.projectId!==projectId)return;setSessions(sortSessions(result.sessions||[]));setSessionsError(''); }
      catch(error){if(!stopped&&currentRef.current.projectId===projectId&&error.code!=='aborted')setSessionsError(messageOf(error));}
      finally{if(!stopped&&currentRef.current.projectId===projectId){setSessionsLoading(false);timer=setTimeout(poll,5000);}}
    }
    poll();return()=>{stopped=true;abort.abort();clearTimeout(timer);};
  },[projectId,refreshTick]);

  useEffect(() => {
    if(!token||!sessionId){setViewLoading(false);return;}
    const abort=new AbortController();let stopped=false,timer,first=true;
    setViewLoading(true);setViewError('');
    async function poll() {
      try {
        const next=await api.request(`/sessions/${enc(sessionId)}`,{signal:abort.signal});
        if(stopped||currentRef.current.sessionId!==sessionId)return;
        let collected=mergeEvents(eventsRef.current,next.events||[],sessionId);
        // Catch up in bounded pages if the current view omitted an event interval.
        let cursor=Math.max(eventsRef.current.at(-1)?.seq||0,next.session.last_seq-600);
        if(!first && (next.events?.[0]?.seq||0)>cursor+1) {
          for(let pageNumber=0;pageNumber<3&&cursor<next.session.last_seq;pageNumber++){
            const page=await api.request(`/sessions/${enc(sessionId)}/events?after_seq=${cursor}&limit=200`,{signal:abort.signal});
            if(stopped||currentRef.current.sessionId!==sessionId)return;collected=mergeEvents(collected,page.events||[],sessionId);
            if(!page.events?.length)break;
            cursor=page.events.at(-1).seq;
          }
        }
        first=false;setView(next);eventsRef.current=collected;setEvents(collected);setViewError('');setConnection('connected');
        const pending=journalRef.current[sessionId];
        if(acknowledgedTurn(collected,pending)){forgetTurn(sessionId);setDraft(old=>old===pending.text?'':old);}
        setSessions(old=>sortSessions([next.session,...old.filter(item=>item.id!==sessionId)]));
        timer=setTimeout(poll,['running','awaiting_approval'].includes(next.session.state)?900:2500);
      } catch(error) { if(!stopped&&currentRef.current.sessionId===sessionId&&error.code!=='aborted'){setViewError(messageOf(error));setConnection('offline');timer=setTimeout(poll,3000);} }
      finally { if(!stopped&&currentRef.current.sessionId===sessionId)setViewLoading(false); }
    }
    poll();return()=>{stopped=true;abort.abort();clearTimeout(timer);};
  },[sessionId,refreshTick]);

  async function openProject(path) {
    const result=await api.request('/projects',{method:'POST',body:{path}});
    setProjects(old=>[result.project,...old.filter(item=>item.id!==result.project.id)]);switchProject(result.project.id);refresh();
  }
  async function submit(event,retry=false) {
    event?.preventDefault();
    if(requestLock.current||!connected||!projectId)return;
    const text=retry?pendingTurn?.text:draft.trim();
    if(!text || (!retry&&(pendingTurn||!canSubmit(session?.state||'idle')||(!session&&!model))))return;
    requestLock.current=true;setMutationBusy('submit');setMutationError('');
    let target=sessionId;
    try {
      if(!target){
        const created=await api.request('/sessions',{method:'POST',body:{project_id:projectId,model}});
        target=created.session.id;setSessions(old=>sortSessions([created.session,...old]));setSessionId(target);setView({session:created.session,project,pending_operation:null,events:[]});setEvents([]);
      }
      const turn=retry?journalRef.current[target]:{session_id:target,command_id:crypto.randomUUID(),text};
      if(!turn)throw new Error('The pending request is no longer available. Refresh this session.');
      saveJournal({...journalRef.current,[target]:turn});
      await api.request(`/sessions/${enc(target)}/turns`,{method:'POST',body:{command_id:turn.command_id,text:turn.text}});
      forgetTurn(target);setDraft('');draftMap.current.delete(target);refresh();
    } catch(error) {
      if(!error.uncertain&&target)forgetTurn(target);
      setMutationError(error.uncertain?(target?'The request may have reached the app. Refresh is safe; sending a new task is paused until this request is reconciled.':'Session creation could not be confirmed. Refresh the session list before creating another session.'):messageOf(error));
      refresh();
    } finally {requestLock.current=false;setMutationBusy('');}
  }
  async function interrupt() {
    const active=session?.active_run_id;if(!active||requestLock.current)return;
    requestLock.current=true;setMutationBusy('interrupt');setMutationError('');
    try{await api.request(`/sessions/${enc(session.id)}/interrupt`,{method:'POST',body:{run_id:active}});refresh();}
    catch(error){setMutationError(messageOf(error));refresh();}
    finally{requestLock.current=false;setMutationBusy('');}
  }
  async function decide(operation,decision) {
    if(decisionBusy||!connected||operation.id!==view?.pending_operation?.id)return;
    setDecisionBusy(true);setDecisionError('');
    try{await api.request(`/sessions/${enc(operation.session_id)}/operations/${enc(operation.id)}/decision`,{method:'POST',body:{expected_hash:operation.arguments_hash,decision}});setView(old=>old?.session?.id===operation.session_id&&old?.pending_operation?.id===operation.id?{...old,pending_operation:null}:old);refresh();}
    catch(error){if(currentRef.current.sessionId===operation.session_id)setDecisionError(messageOf(error));refresh();}
    finally{setDecisionBusy(false);}
  }
  async function acknowledgeRecovery(event) {
    event.preventDefault();if(requestLock.current||!connected||session?.state!=='recovery_required'||!recoveryNote.trim())return;
    if(new TextEncoder().encode(recoveryNote.trim()).length>2000){setMutationError('Keep the recovery note within 2000 bytes. Shorten the note and try again.');return;}
    requestLock.current=true;setMutationBusy('recovery');setMutationError('');
    try{
      const result=await api.request(`/sessions/${enc(session.id)}/recovery`,{method:'POST',body:{expected_revision:session.revision,note:recoveryNote.trim()}});
      setView(old=>({...old,session:result.session}));setRecoveryNote('');refresh();
    }catch(error){setMutationError(messageOf(error));refresh();}
    finally{requestLock.current=false;setMutationBusy('');}
  }
  function openReview(id='') {
    setSelectedOperationId(id);
    if(matchMedia('(max-width: 1179px)').matches)setReviewOpen(true);
    else document.querySelector('.review-pane')?.focus();
  }
  const reviewProps={pendingOperation:view?.pending_operation,events,selectedOperationId,onSelectOperation:setSelectedOperationId,onDecision:decide,decisionBusy,decisionError,disabled:!connected||Boolean(mutationBusy)};
  const rail=html`<div class="rail-content"><div class="rail-top"><div class="brand" aria-label="Switchya workspace"><img src="./assets/switchya-mark.svg" width="28" height="28" alt=""/><span>${BRAND}</span><span class="brand-label">workspace</span></div><label class="project-picker"><span class="eyebrow">PROJECT</span><select value=${projectId} onChange=${event=>switchProject(event.currentTarget.value)} disabled=${navigationBusy} aria-label="Current project"><option value="" disabled>${projects.length?'Choose project':'No project open'}</option>${projects.map(item=>html`<option key=${item.id} value=${item.id}>${item.name}</option>`)}</select></label><${Button} icon="plus" onClick=${()=>setProjectOpen(true)} disabled=${!connected||navigationBusy}>Open project<//><${Button} icon="edit" variant="primary" onClick=${()=>switchSession('')} disabled=${!projectId||navigationBusy}>New session<//><div class="agent-mode" aria-label="Agent workspace"><button type="button" disabled=${navigationBusy} data-active=${mode==='native'} onClick=${()=>{setMode('native');setRailOpen(false);}}>Built-in agent</button><button type="button" disabled=${navigationBusy} data-active=${mode==='external'} onClick=${()=>{setMode('external');setRailOpen(false);}}>Existing agents</button></div></div><nav class="sessions" aria-label="Project sessions"><div class="section-label"><span>SESSIONS</span><${IconButton} label="Refresh sessions" icon="refresh" onClick=${refresh} disabled=${booting}/></div>${sessionsLoading&&sessions.length===0&&html`<p class="rail-note">Loading sessions…</p>`}${sessionsError&&html`<p class="rail-note" role="alert">${sessionsError}</p>`}${!sessionsLoading&&sessions.length===0&&html`<p class="rail-note">${projectId?'Your sessions will appear here.':'Open a project to begin.'}</p>`}${sessions.map(item=>html`<button key=${item.id} class="session-button" data-active=${item.id===sessionId} aria-current=${item.id===sessionId?'page':undefined} onClick=${()=>switchSession(item.id)} disabled=${navigationBusy}><span class="session-title" title=${item.title}>${item.title}</span><span class="session-meta"><${Status} state=${item.state}/><time title=${fullTime(item.updated_at_ms)}>${timeLabel(item.updated_at_ms)}</time></span>${journal[item.id]&&html`<span class="field-hint">Request confirmation pending</span>`}</button>`)}</nav><footer class="rail-footer"><${Button} class="account-open" icon="key" onClick=${()=>setAccountsOpen(true)} disabled=${!connected}>Accounts<//><${Button} class="provider-open" icon="plug" onClick=${()=>setProviderOpen(true)} disabled=${!connected}>Providers<//><label class="theme-picker"><span><${Icon} name=${theme==='dark'?'moon':theme==='light'?'sun':'monitor'}/> Appearance</span><select value=${theme} onChange=${event=>{setTheme(event.currentTarget.value);window.dispatchEvent(new CustomEvent('switchyard-theme',{detail:event.currentTarget.value}));}} aria-label="Appearance"><option value="system">System</option><option value="light">Light</option><option value="dark">Dark</option></select></label><div class="connection-state" data-state=${connection}><span class="status-dot" aria-hidden="true"></span>${connected?'Local app connected':booting?'Connecting':'Disconnected'}${status?.version&&html`<span class="version">v${status.version}</span>`}</div><p class="powered-by">Powered by Switchyard Gateway</p></footer></div>`;

  if(!token||launchError)return html`<main class="boot-state"><img src="./assets/switchya-mark.svg" width="44" height="44" alt=""/><h1>Open ${BRAND} from the local app.</h1><p>${launchError?messageOf(launchError):'This tab needs the secure launch link generated by the app on your computer.'}</p><p class="field-hint">Start the app again and use its launch link. Provider keys are configured in Switchyard Gateway.</p></main>`;
  const submitDisabled=!connected||!projectId||(!session&&!model)||Boolean(mutationBusy)||Boolean(pendingTurn)||!draft.trim()||!canSubmit(session?.state||'idle')||viewLoading;
  return html`<div class="workspace"><a class="skip-link" href="#task-input">Skip to task input</a><aside class="project-rail" aria-label="Projects and sessions">${rail}</aside>${mode==='external'?html`<${AdapterWorkspace} api=${api} project=${project} profiles=${accounts.profiles} accountDefaults=${accounts.defaults} accountsLoading=${accounts.loading} accountsError=${accounts.error} onOpenAccounts=${()=>setAccountsOpen(true)} hostId=${adapterHostId} storage=${sessionStore} onBusyChange=${setAdapterBusy} onOpenNavigation=${()=>setRailOpen(true)} onNative=${()=>setMode('native')} onOpenProject=${()=>setProjectOpen(true)}/>`:html`<main class="workspace-body"><header class="workspace-header"><${IconButton} label="Open projects and sessions" icon="menu" class="rail-toggle" onClick=${()=>setRailOpen(true)}/><div class="header-project"><span class="eyebrow" title=${project?.root}>${project?.name||'LOCAL WORKSPACE'}</span><h1>${session?.title||'New session'}</h1></div><div class="header-actions">${session&&html`<${Status} state=${session.state}/>`}<${Button} class="review-toggle" icon="sidebar" onClick=${()=>openReview()}>${view?.pending_operation?'Review approval':'Review'}<//></div></header>
    ${booting?html`<div class="loading-state" role="status">Connecting to your workspace…</div>`:html`<${Conversation} events=${events} session=${session} project=${project} loading=${viewLoading} error=${viewError} onReview=${openReview} onOpenProject=${()=>setProjectOpen(true)} hasModels=${realModels.length>0}/>`}
    <section class="composer-area" aria-label="Task input">
      ${globalError&&html`<${Notice} title="Local app connection" tone="stop" action=${html`<${Button} icon="refresh" onClick=${refresh}>Reconnect<//>`}>${globalError}<//>`}
      ${!booting&&realModels.length===0&&html`<${Notice} title="Connect a coding model" tone="caution" action=${html`<div class="empty-actions"><${Button} variant="primary" icon="plug" onClick=${()=>setProviderOpen(true)}>Connect a provider<//><${Button} icon="refresh" onClick=${refresh}>Refresh models<//></div>`}><p>${models.length?'Only a mock model is available. Connect a cloud provider or local Ollama server to begin.':'Connect a provider to make its coding models available in this workspace.'}</p><//>`}
      ${session?.state==='recovery_required'&&html`<${Notice} title="Review the last operation before continuing" tone="stop"><p>The previous operation may have changed files or run a command. Review its recorded output and the affected files. Your acknowledgement does not confirm success or undo any effects.</p><form class="recovery-form" onSubmit=${acknowledgeRecovery}><label class="field" for="recovery-note"><span>What did you check?</span><textarea id="recovery-note" rows="2" maxlength="2000" required value=${recoveryNote} onInput=${event=>setRecoveryNote(event.currentTarget.value)} disabled=${Boolean(mutationBusy)} placeholder="Record the files or command outcome you reviewed…"/></label><${Button} type="submit" disabled=${!connected||!recoveryNote.trim()||Boolean(mutationBusy)} busy=${mutationBusy==='recovery'}>I reviewed the affected files and want to continue<//><p class="field-hint">This unlocks a new follow-up task. Nothing is replayed automatically.</p></form><//>`}
      ${session?.state==='interrupted'&&html`<${Notice} title="Session interrupted" tone="caution">Your conversation is retained. Enter a follow-up task, then choose Resume with task to start an explicit new run.<//>`}
      ${pendingTurn&&html`<${Notice} title="Confirming a submitted task" tone="caution" action=${html`<div class="empty-actions"><${Button} icon="refresh" onClick=${refresh}>Refresh state<//><${Button} onClick=${event=>submit(event,true)} disabled=${!connected||Boolean(mutationBusy)}>Retry same request<//></div>`}>${mutationBusy==='submit'?'The app is processing your request.':'The task was saved before it was sent. Refresh checks for acknowledgement. Retry uses the same request ID to prevent a duplicate run.'}<//>`}
      ${mutationError&&html`<p class="field-error" role="alert">${mutationError}</p>`}${storageWarning&&html`<p class="field-hint" role="status">${storageWarning}</p>`}
      <form class="composer" onSubmit=${submit}><div class="composer-top"><label class="model-select"><span>Model</span>${session?html`<code title=${session.model}>${session.model}</code>`:html`<select value=${model} onChange=${event=>setModel(event.currentTarget.value)} disabled=${!realModels.length||Boolean(mutationBusy)} aria-label="Model for new session"><option value="" disabled>Choose model</option>${realModels.map(item=>html`<option key=${modelId(item)} value=${modelId(item)}>${modelId(item)}</option>`)}</select>`}</label><span class="composer-boundary">Writes and commands need approval</span></div><div class="composer-text"><textarea id="task-input" ref=${composer} aria-label="Task" rows="1" value=${draft} onInput=${event=>setDraft(event.currentTarget.value)} onKeyDown=${event=>{if((event.ctrlKey||event.metaKey)&&event.key==='Enter'){event.preventDefault();if(!submitDisabled)submit(event);}}} placeholder=${project?'Describe the task for this project…':'Open a project to start a task…'} disabled=${!projectId||session?.state==='recovery_required'||Boolean(mutationBusy)}/></div><div class="composer-bottom"><span class="field-hint">Ctrl / ⌘ + Enter to send</span><div>${session?.active_run_id&&html`<${Button} icon="stop" onClick=${interrupt} disabled=${!connected||Boolean(mutationBusy)} busy=${mutationBusy==='interrupt'}>${mutationBusy==='interrupt'?'Interrupting…':'Interrupt'}<//>`}<${Button} type="submit" variant="primary" icon="send" disabled=${submitDisabled} busy=${mutationBusy==='submit'}>${mutationBusy==='submit'?'Sending…':session?.state==='interrupted'?'Resume with task':'Send task'}<//></div></div></form>
    </section></main><aside class="review-pane" tabindex="-1" aria-label="Review proposed actions"><${ReviewPane} ...${reviewProps}/></aside>`}<${Dialog} open=${railOpen} onClose=${()=>setRailOpen(false)} title="Projects and sessions" class="navigation-dialog">${railOpen&&rail}<//><${Dialog} open=${mode==='native'&&reviewOpen} onClose=${()=>setReviewOpen(false)} title="Review" class="review-dialog">${reviewOpen&&html`<${ReviewPane} ...${reviewProps} onClose=${()=>setReviewOpen(false)}/>`}<//><${AccountsDialog} api=${api} open=${accountsOpen} onClose=${()=>setAccountsOpen(false)} profiles=${accounts.profiles} defaults=${accounts.defaults} loading=${accounts.loading} error=${accounts.error} onRefresh=${accounts.refresh} project=${project}/><${ProviderDialog} api=${api} open=${providerOpen} onClose=${()=>setProviderOpen(false)} onModelsChanged=${refresh}/><${ProjectDialog} open=${projectOpen} onClose=${()=>setProjectOpen(false)} onOpen=${openProject}/><span class="sr-only" role="status" aria-live="polite">${session?stateLabel(session.state):connected?'Workspace ready':'Connecting'}</span></div>`;
}

const appRoot=document.getElementById('app');
appRoot.replaceChildren();
render(html`<${App}/>`,appRoot);
