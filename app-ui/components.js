import { html, useEffect, useRef, useState } from './vendor/preact-htm.js';
import { Icon } from './icons.js';
import { pretty } from './presentation.js';

let nextId=0;
export function Button({children,icon,onClick,type='button',variant='',class:className='',busy=false,...props}) {
  return html`<button type=${type} class=${`button ${className}`} data-variant=${variant||undefined} onClick=${onClick} aria-busy=${busy||undefined} ...${props}>${icon&&html`<${Icon} name=${icon}/>`}${children}</button>`;
}
export function IconButton({label,icon,class:className='',...props}) {
  return html`<${Button} class=${`icon-button ${className}`} aria-label=${label} title=${label} icon=${icon} ...${props}/>`;
}
export function Notice({title,children,tone='neutral',action}) {
  return html`<div class="notice" data-tone=${tone} role=${tone==='stop'?'alert':undefined}><${Icon} name=${tone==='stop'||tone==='caution'?'alert':'info'}/><div><strong>${title}</strong>${children&&html`<div class="notice-body">${children}</div>`}${action}</div></div>`;
}
export function Dialog({open,onClose,title,children,class:className=''}) {
  const ref=useRef(null), opener=useRef(null), id=useRef(`dialog-title-${++nextId}`);
  const closeRef=useRef(onClose); closeRef.current=onClose;
  useEffect(()=>{
    const dialog=ref.current;
    if(open&&!dialog.open){opener.current=document.activeElement;dialog.showModal();}
    if(!open&&dialog.open){dialog.close();if(opener.current?.isConnected)opener.current.focus();}
  },[open]);
  useEffect(()=>()=>{const dialog=ref.current;if(dialog?.open)dialog.close();if(opener.current?.isConnected)opener.current.focus();},[]);
  const close=()=>closeRef.current?.();
  return html`<dialog ref=${ref} class=${`dialog ${className}`} aria-labelledby=${id.current} onCancel=${event=>{event.preventDefault();close();}} onClick=${event=>{if(event.target===ref.current){const r=ref.current.getBoundingClientRect();if(event.clientX<r.left||event.clientX>r.right||event.clientY<r.top||event.clientY>r.bottom)close();}}}><header class="dialog-header"><h2 id=${id.current}>${title}</h2><${IconButton} label="Close" icon="x" onClick=${close}/></header>${children}</dialog>`;
}
export function CopyButton({text,label='Copy',class:className=''}) {
  const [state,setState]=useState('');
  const timeout=useRef(null);
  useEffect(()=>()=>clearTimeout(timeout.current),[]);
  async function copy(){try{await navigator.clipboard.writeText(text);setState('Copied');}catch{setState('Copy unavailable');}clearTimeout(timeout.current);timeout.current=setTimeout(()=>setState(''),2200);}
  return html`<${Button} class=${className} icon="copy" onClick=${copy} aria-label=${label}>${state||label}<span class="sr-only" role="status">${state}</span><//>`;
}
export function RawValue({value,label='Details',max=60000}) {
  const text=pretty(value);
  return html`<div class="raw-value"><pre tabindex="0" aria-label=${label}>${text.slice(0,max)}</pre>${text.length>max&&html`<p class="field-hint">Preview limited to ${max.toLocaleString()} characters. <${CopyButton} text=${text} label="Copy full output"/></p>`}</div>`;
}
export function TextContent({text=''}) {
  const parts=String(text).split(/(```[^\n]*\n[\s\S]*?```)/g);
  return html`<div class="message-text">${parts.map((part,index)=>part.startsWith('```')&&part.endsWith('```')?html`<pre key=${index} tabindex="0" aria-label="Code"><code>${part.slice(part.indexOf('\n')+1,-3)}</code></pre>`:html`<div key=${index} class="prose">${part}</div>`)}</div>`;
}
export function downloadText(text,name='proposed-changes.diff') {
  const href=URL.createObjectURL(new Blob([text],{type:'text/plain;charset=utf-8'}));
  const link=document.createElement('a');link.href=href;link.download=name;link.click();
  setTimeout(()=>URL.revokeObjectURL(href),1000);
}
