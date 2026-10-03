import { html, useEffect, useRef, useState } from './vendor/preact-htm.js';
import { Button, Dialog, Notice } from './components.js';

const providerNames={openai:'OpenAI',anthropic:'Anthropic',gemini:'Gemini',ollama:'Ollama','openai-compat':'Local / compatible'};
const statusNames={disabled:'Disabled',applying:'Applying configuration',env_missing:'Environment variable missing',credential_missing:'Credential missing',discovering:'Discovering models',catalog_ready:'Model list available',discovery_failed:'Model discovery failed',discovery_off:'Discovery is off'};
export function ProviderDialog({api,open,onClose,onModelsChanged}) {
  const [providers,setProviders]=useState([]),[loading,setLoading]=useState(false),[error,setError]=useState(''),[notice,setNotice]=useState(''),[loadError,setLoadError]=useState('');
  const [kind,setKind]=useState('openai'),[name,setName]=useState('openai'),[credentialMode,setCredentialMode]=useState('api_key');
  const [key,setKey]=useState(''),[env,setEnv]=useState('OPENAI_API_KEY'),[baseUrl,setBaseUrl]=useState('http://127.0.0.1:11434/v1');
  const [busy,setBusy]=useState(''),[tick,setTick]=useState(0);const lock=useRef(false);
  useEffect(()=>{if(!open){setKey('');setError('');setNotice('');}},[open]);
  useEffect(()=>{
    if(!open)return;
    const abort=new AbortController();let stopped=false,timer;
    setLoading(true);
    async function poll(){
      try{const result=await api.request('/providers',{signal:abort.signal});if(stopped)return;setProviders(result.providers||[]);setLoadError('');}
      catch(error){if(!stopped&&error.code!=='aborted')setLoadError(error.message||'Provider status could not be read.');}
      finally{if(!stopped){setLoading(false);timer=setTimeout(poll,2500);}}
    }
    poll();return()=>{stopped=true;abort.abort();clearTimeout(timer);};
  },[open,tick]);
  function selectKind(next){setKind(next);setName(next);setKey('');setEnv(({openai:'OPENAI_API_KEY',anthropic:'ANTHROPIC_API_KEY',gemini:'GEMINI_API_KEY'})[next]||'');setError('');}
  async function save(event){
    event.preventDefault();if(lock.current)return;lock.current=true;setBusy('save');setError('');setNotice('');
    const credential=kind==='ollama'?{mode:'none'}:credentialMode==='api_key'?{mode:'api_key',value:key}:{mode:'env',name:env.trim()};
    const body={name:name.trim(),kind,credential,...(kind==='ollama'?{base_url:baseUrl.trim()}: {})};
    try{
      const result=await api.request('/providers',{method:'POST',body});
      setProviders(old=>[...old.filter(item=>item.name!==result.provider.name),result.provider]);
      setNotice(`${result.provider.name} was saved. ${result.provider.message||'Model discovery and credential status are shown above.'}`);
      setTick(value=>value+1);onModelsChanged();
    }catch(error){setError(error.uncertain?'The save outcome is unknown. Refresh provider status before saving this name again.':error.message||'Provider could not be saved.');}
    finally{setKey('');setBusy('');lock.current=false;}
  }
  async function discover(provider){
    if(lock.current)return;lock.current=true;setBusy(provider.name);setError('');setNotice('');
    try{const result=await api.request(`/providers/${encodeURIComponent(provider.name)}/refresh`,{method:'POST',body:{}});setProviders(old=>old.map(item=>item.name===provider.name?result.provider:item));setNotice(result.provider.message||'Model discovery requested.');setTick(value=>value+1);onModelsChanged();}
    catch(error){setError(error.message||'Model discovery could not be requested.');}
    finally{setBusy('');lock.current=false;}
  }
  const hasDuplicate=providers.some(provider=>provider.name===name.trim());
  const canSave=/^[a-z][a-z0-9_-]{0,47}$/.test(name.trim())&&!hasDuplicate&&(kind==='ollama'?Boolean(baseUrl.trim()):credentialMode==='api_key'?Boolean(key.trim()):Boolean(env.trim()));
  return html`<${Dialog} open=${open} onClose=${()=>{if(!busy){setKey('');onClose();onModelsChanged();}}} title="Connect a provider" class="provider-dialog"><div class="dialog-body provider-body"><p class="field-hint">Connect a cloud account or local Ollama server to Switchyard Gateway. Saving and discovering a model list do not send a generation request.</p>${loading&&providers.length===0&&html`<p role="status">Reading configured providers…</p>`}${(error||loadError)&&html`<${Notice} title="Provider setup" tone="stop">${error||loadError}<//>`}${notice&&html`<${Notice} title="Configuration update">${notice}<//>`}
    ${providers.length>0&&html`<section class="provider-list" aria-label="Configured providers">${providers.map(provider=>html`<article class="provider-status" key=${provider.name}><div class="check-heading"><h3>${provider.name}</h3><span class="field-hint">${providerNames[provider.kind]||provider.kind}</span></div><p class="provider-state" data-tone=${['env_missing','credential_missing','discovery_failed'].includes(provider.status)?'caution':'neutral'}>${statusNames[provider.status]||provider.status}</p><p class="field-hint">${provider.message}</p><dl><dt>Credential source</dt><dd>${provider.credential_source==='env'?`Environment: ${provider.environment_variable||'not specified'}`:provider.credential_source==='api_key'?'Saved API key':provider.credential_source==='none'?'No credential':'Configured sources'}</dd><dt>Models reported</dt><dd>${provider.model_count}</dd></dl><${Button} icon="refresh" onClick=${()=>discover(provider)} disabled=${Boolean(busy)||!provider.enabled||!provider.config_applied} busy=${busy===provider.name}>${busy===provider.name?'Discovering…':'Discover models'}<//></article>`)}</section>`}
    <form onSubmit=${save} class="provider-form"><h3>Add a provider</h3><div class="provider-fields"><label class="field" for="provider-kind"><span>Provider</span><select id="provider-kind" value=${kind} onChange=${event=>selectKind(event.currentTarget.value)} disabled=${Boolean(busy)}><option value="openai">OpenAI</option><option value="anthropic">Anthropic</option><option value="gemini">Gemini</option><option value="ollama">Ollama — local</option></select></label><label class="field" for="provider-name"><span>Configuration name</span><input id="provider-name" value=${name} onInput=${event=>setName(event.currentTarget.value)} pattern="[a-z][a-z0-9_-]{0,47}" maxlength="48" required autocomplete="off" spellcheck="false" disabled=${Boolean(busy)}/></label></div>${hasDuplicate&&html`<p class="field-hint">This name is already configured. Choose another name to add a separate provider.</p>`}
    ${kind==='ollama'?html`<label class="field" for="provider-local-url"><span>Local Ollama URL</span><input id="provider-local-url" type="url" value=${baseUrl} onInput=${event=>setBaseUrl(event.currentTarget.value)} required autocomplete="off" spellcheck="false" disabled=${Boolean(busy)}/><span class="field-hint">Use a loopback address with /v1. The Ollama server must already be running.</span></label>`:html`<label class="field" for="provider-credential-mode"><span>Credential source</span><select id="provider-credential-mode" value=${credentialMode} onChange=${event=>{setCredentialMode(event.currentTarget.value);setKey('');}} disabled=${Boolean(busy)}><option value="api_key">Save an API key</option><option value="env">Use an environment variable</option></select></label>${credentialMode==='api_key'?html`<label class="field" for="provider-api-key"><span>API key</span><input id="provider-api-key" type="password" value=${key} onInput=${event=>setKey(event.currentTarget.value)} autocomplete="off" spellcheck="false" required disabled=${Boolean(busy)}/><span class="field-hint">Sent only to your local app for its gateway configuration. The saved key is never displayed here.</span></label>`:html`<label class="field" for="provider-env"><span>Environment variable name</span><input id="provider-env" value=${env} onInput=${event=>setEnv(event.currentTarget.value)} autocomplete="off" spellcheck="false" required disabled=${Boolean(busy)}/><span class="field-hint">The app process must receive this variable. If it is missing, configure it and restart the app.</span></label>`}`}
    <div class="provider-form-actions"><${Button} type="submit" variant="primary" icon="plus" disabled=${!canSave||Boolean(busy)} busy=${busy==='save'}>${busy==='save'?'Saving…':'Save provider'}<//><${Button} icon="refresh" onClick=${()=>{setTick(value=>value+1);onModelsChanged();}} disabled=${Boolean(busy)}>Refresh status<//></div><p class="field-hint">A fetched model list confirms catalog discovery. Generation and billing have not been tested.</p></form></div><//>`;
}
