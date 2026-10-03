import { html, useEffect, useRef, useState } from './vendor/preact-htm.js';
import { Button, CopyButton, Dialog, Notice } from './components.js';
import { fullTime } from './presentation.js';
import { activeLogin, agentName, authLabel, foldProfileName, mergeLoginState, percentLabel, usageWindows, validProfileName } from './account-presentation.js';

const enc=encodeURIComponent;
const errorText=error=>error?.message||'Account status could not be read.';
export function useAccountProfiles(api,enabled){
  const [data,setData]=useState({profiles:[],defaults:{}}),[loading,setLoading]=useState(false),[error,setError]=useState(''),[tick,setTick]=useState(0);
  useEffect(()=>{
    if(!enabled)return;
    const abort=new AbortController();let stopped=false,timer;setLoading(true);
    async function poll(){
      let next;
      try{next=await api.request('/account-profiles',{signal:abort.signal});if(stopped)return;setData(next);setError('');}
      catch(error){if(!stopped&&error.code!=='aborted')setError(errorText(error));}
      finally{if(!stopped){setLoading(false);timer=setTimeout(poll,next?.profiles?.some(profile=>profile.refreshing)?1000:5000);}}
    }
    poll();return()=>{stopped=true;abort.abort();clearTimeout(timer);};
  },[enabled,tick]);
  return {...data,loading,error,refresh:()=>setTick(value=>value+1)};
}

function Usage({account}){
  const windows=usageWindows(account),usage=account?.usage;
  return html`<section class="account-usage" aria-label="Reported usage limits">${windows.length?windows.map(window=>html`<div class="usage-window" key=${window.id}><div class="usage-heading"><span>${window.name||window.id||'Usage window'}</span><strong>${window.remaining_percent!==null?`${percentLabel(window.remaining_percent)} remaining`:window.used_percent!==null?`${percentLabel(window.used_percent)} used`:'Percentage unavailable'}</strong></div>${window.remaining_percent!==null&&html`<meter min="0" max="100" value=${window.remaining_percent} aria-label=${`${window.name||window.id||'Usage window'} remaining`}>${percentLabel(window.remaining_percent)}</meter>`}<p class="field-hint">${window.resets_at?`Resets ${fullTime(window.resets_at*1000)}`:'Reset time unavailable'}</p></div>`):html`<p class="field-hint">${usage?.message||(usage?.status==='error'?'Usage limits could not be read.':'Usage limits are unavailable for this account.')}</p>`}${usage?.ordinary_usage_allowed===false&&html`<p class="field-hint">The provider reports that ordinary usage is currently unavailable.</p>`}<p class="account-checked field-hint">${account?.checked_at_ms>0?`Last checked ${fullTime(account.checked_at_ms)}`:'Not checked yet'}</p></section>`;
}

export function AccountsDialog({api,open,onClose,profiles=[],defaults={},loading,error:loadError,onRefresh,project}){
  const [agentId,setAgentId]=useState('codex'),[name,setName]=useState(''),[busy,setBusy]=useState(''),[error,setError]=useState(''),[notice,setNotice]=useState(''),[logins,setLogins]=useState({});
  const lock=useRef(false),openRef=useRef(open),loginsRef=useRef(logins);openRef.current=open;loginsRef.current=logins;
  const profileIds=profiles.filter(profile=>!profile.read_only).map(profile=>profile.id).join(',');
  useEffect(()=>{if(!open){setError('');setNotice('');setLogins({});}},[open]);
  useEffect(()=>{
    if(!open)return;
    const abort=new AbortController();let stopped=false;
    const ids=profileIds.split(',').filter(Boolean);
    Promise.allSettled(ids.map(async id=>({id,result:await api.request(`/account-profiles/${enc(id)}/login`,{signal:abort.signal})}))).then(results=>{
      if(stopped)return;
      setLogins(old=>mergeLoginState(old,results.filter(result=>result.status==='fulfilled').map(result=>result.value.result.login)));
    });
    return()=>{stopped=true;abort.abort();};
  },[open,profileIds]);
  const activeIds=Object.entries(logins).filter(([,login])=>activeLogin(login)).map(([id])=>id).join(',');
  useEffect(()=>{
    if(!open||!activeIds)return;
    const abort=new AbortController();let stopped=false,timer;
    async function poll(){
      const results=await Promise.allSettled(activeIds.split(',').map(async id=>({id,result:await api.request(`/account-profiles/${enc(id)}/login`,{signal:abort.signal})})));
      if(stopped)return;
      const incoming=results.filter(result=>result.status==='fulfilled').map(result=>result.value.result.login).filter(Boolean);
      const completed=incoming.some(login=>login.status==='completed'&&loginsRef.current[login.profile_id]?.status!=='completed');
      setLogins(old=>mergeLoginState(old,incoming));
      const failure=results.find(result=>result.status==='rejected');if(failure)setError(errorText(failure.reason));
      if(completed)onRefresh();
      timer=setTimeout(poll,1000);
    }
    timer=setTimeout(poll,1000);return()=>{stopped=true;abort.abort();clearTimeout(timer);};
  },[open,activeIds]);
  async function action(profile,kind){
    if(lock.current)return;lock.current=true;setBusy(`${profile.id}:${kind}`);setError('');setNotice('');
    try{
      const result=await api.request(`/account-profiles/${enc(profile.id)}/${kind}`,{method:'POST',body:kind==='terminal'?{project_id:project.id}:{}});
      if(!openRef.current)return;
      if(result.login)setLogins(old=>mergeLoginState(old,[result.login]));
      if(kind==='select')setNotice(`${profile.name} is the default for future ${agentName(profile.agent_id)} runs. Running sessions keep their current account.`);
      if(kind==='terminal')setNotice(result.message||'Claude Code was opened in a separate terminal. This session is not tracked in Switchya.');
      if(kind==='login/open')setNotice('The official sign-in page was opened. Finish signing in there, then return to Switchya.');
      onRefresh();
    }catch(error){if(openRef.current){setError(error.uncertain?'The action outcome could not be confirmed. Refresh the account status before trying again.':errorText(error));onRefresh();}}
    finally{setBusy('');lock.current=false;}
  }
  async function create(event){
    event.preventDefault();if(lock.current||!validProfileName(name))return;lock.current=true;setBusy('create');setError('');setNotice('');
    try{const result=await api.request('/account-profiles',{method:'POST',body:{agent_id:agentId,name:name.trim()}});if(openRef.current){setName('');setNotice(`${result.profile.name} was created. Use Refresh account to check sign-in status.`);onRefresh();}}
    catch(error){if(openRef.current){setError(error.uncertain?'Profile creation could not be confirmed. Refresh the list before trying again.':errorText(error));onRefresh();}}
    finally{setBusy('');lock.current=false;}
  }
  const duplicate=profiles.some(profile=>profile.agent_id===agentId&&foldProfileName(profile.name)===foldProfileName(name));
  return html`<${Dialog} open=${open} onClose=${()=>{if(!busy)onClose();}} title="Accounts" class="accounts-dialog"><div class="dialog-body accounts-body"><p class="field-hint">Keep separate Codex and Claude Code sign-ins, check reported limits, and choose an account for future work. Sign-in and usage status come from the installed CLI; no generation request is sent to check them.</p>${(error||loadError)&&html`<${Notice} title="Account status" tone="stop">${error||loadError}<//>`}${notice&&html`<${Notice} title="Account update">${notice}<//>`}<div class="accounts-toolbar"><span class="field-hint">${profiles.length?`${profiles.length} account ${profiles.length===1?'profile':'profiles'}`:loading?'Loading account profiles…':'No account profiles yet'}</span><${Button} icon="refresh" onClick=${onRefresh} disabled=${Boolean(busy)}>Refresh list<//></div>
    <div class="account-list">${profiles.map(profile=>{const account=profile.account||{},login=logins[profile.id],authBusy=activeLogin(login),selected=defaults[profile.agent_id]===profile.id||profile.selected;return html`<article key=${profile.id} class="account-card"><header class="account-heading"><div><p class="eyebrow">${agentName(profile.agent_id)}${profile.read_only?' · EXISTING CLI':''}</p><h3>${profile.name}</h3></div>${selected&&html`<span class="account-default">Default</span>`}</header><div class="account-identity"><strong>${account.email||authLabel(account.auth_status)}</strong><span class="field-hint">${account.email?authLabel(account.auth_status):''}${account.plan?`${account.email?' · ':''}${account.plan}`:''}</span></div>${account.message&&html`<p class="field-hint">${account.message}</p>`}<${Usage} account=${account}/>${profile.refreshing&&html`<p class="field-hint" role="status">Checking this account…</p>`}
      ${login&&html`<section class="account-login" aria-label=${`Sign-in for ${profile.name}`}><p><strong>${({starting:'Starting sign-in',awaiting_user:'Finish signing in',completed:'Sign-in completed',failed:'Sign-in failed',cancelled:'Sign-in cancelled'})[login.status]||'Sign-in status'}</strong></p>${login.message&&html`<p class="field-hint">${login.message}</p>`}${authBusy&&login.user_code&&html`<div class="login-code"><code>${login.user_code}</code><${CopyButton} text=${login.user_code} label="Copy sign-in code"/></div>`}${authBusy&&html`<div class="account-actions">${login.auth_url&&html`<${Button} icon="external" variant="primary" onClick=${()=>action(profile,'login/open')} disabled=${Boolean(busy)}>Continue in browser<//>`}<${Button} onClick=${()=>action(profile,'login/cancel')} disabled=${Boolean(busy)}>Cancel sign-in<//></div>`}</section>`}
      <div class="account-actions"><${Button} icon="refresh" onClick=${()=>action(profile,'refresh')} disabled=${Boolean(busy)||profile.refreshing||authBusy} busy=${busy===`${profile.id}:refresh`}>Refresh account<//><${Button} onClick=${()=>action(profile,'select')} disabled=${Boolean(busy)||selected}>Use as default<//>${!profile.read_only&&html`<${Button} icon="key" onClick=${()=>action(profile,'login')} disabled=${Boolean(busy)||profile.refreshing||authBusy||account.auth_status!=='signed_out'}>Sign in<//>`}${profile.agent_id==='claude'&&profile.managed&&html`<${Button} icon="external" onClick=${()=>action(profile,'terminal')} disabled=${Boolean(busy)||!project||authBusy||account.auth_status!=='signed_in'}>Open Claude Code<//>`}</div><p class="field-hint account-boundary">${profile.read_only?'Uses the existing Codex CLI sign-in. This app does not replace it.':account.auth_status==='unknown'?'Use Refresh account to check sign-in status before signing in.':profile.agent_id==='claude'?`Open Claude Code starts an external terminal in ${project?.name||'an opened project'}. Its session is not tracked here.`:account.auth_status==='signed_in'?'To add another sign-in, create another profile.':'Sign in through the provider’s official page.'}</p></article>`;})}</div>
    <form class="account-form" onSubmit=${create}><h3>Add an account profile</h3><div class="provider-fields"><label class="field" for="account-agent"><span>Agent</span><select id="account-agent" value=${agentId} onChange=${event=>setAgentId(event.currentTarget.value)} disabled=${Boolean(busy)}><option value="codex">Codex</option><option value="claude">Claude Code</option></select></label><label class="field" for="account-name"><span>Profile name</span><input id="account-name" value=${name} onInput=${event=>setName(event.currentTarget.value)} placeholder="Personal, work, or another name" autocomplete="off" required disabled=${Boolean(busy)}/></label></div>${duplicate&&html`<p class="field-hint">That name is already used for this agent.</p>`}<${Button} type="submit" icon="plus" variant="primary" disabled=${Boolean(busy)||duplicate||!validProfileName(name)} busy=${busy==='create'}>Create profile<//><p class="field-hint">New profiles keep their own CLI sign-in. Changing the default does not move a running session.</p></form></div><//>`;
}
