import test from 'node:test';
import assert from 'node:assert/strict';
import { html, render } from '../vendor/preact-htm.js';

// A strict DOM fixture runs the real Preact renderer. It deliberately rejects
// invalid element names like a browser, catching HTM errors that JS parsing misses.
class Element {
  constructor(name,type=1){
    if(type===1&&!/^[A-Za-z][A-Za-z0-9:-]*$/.test(name))throw new DOMException(`Invalid element name: ${name}`,'InvalidCharacterError');
    this.localName=name;this.nodeType=type;this.childNodes=[];this.parentNode=null;this.attributes=[];this.style={setProperty(){}};
  }
  get firstChild(){return this.childNodes[0]||null;}
  get nextSibling(){const siblings=this.parentNode?.childNodes||[];return siblings[siblings.indexOf(this)+1]||null;}
  appendChild(node){if(node.parentNode)node.parentNode.removeChild(node);node.parentNode=this;this.childNodes.push(node);return node;}
  insertBefore(node,before){if(node.parentNode)node.parentNode.removeChild(node);node.parentNode=this;const i=this.childNodes.indexOf(before);this.childNodes.splice(i<0?this.childNodes.length:i,0,node);return node;}
  removeChild(node){this.childNodes.splice(this.childNodes.indexOf(node),1);node.parentNode=null;return node;}
  replaceChildren(...nodes){for(const child of [...this.childNodes])this.removeChild(child);for(const node of nodes)this.appendChild(node);}
  setAttribute(name,value){this.removeAttribute(name);this.attributes.push({name,value:String(value)});}
  removeAttribute(name){this.attributes=this.attributes.filter(item=>item.name!==name);}
  addEventListener(){}
  removeEventListener(){}
}
const flatten=node=>[node,...node.childNodes.flatMap(flatten)];
const memoryStorage=()=>{const map=new Map();return{getItem:key=>map.get(key)||null,setItem:(key,value)=>map.set(key,String(value)),removeItem:key=>map.delete(key)};};

test('workspace, review and adapter render with the shipped HTM/Preact runtime',async()=>{
  const root=new Element('div');
  const loadingFallback=new Element('main');root.appendChild(loadingFallback);
  globalThis.document={createElement:name=>new Element(name),createElementNS:(_,name)=>new Element(name),createTextNode:text=>Object.assign(new Element('#text',3),{data:text}),getElementById:()=>root,activeElement:null};
  globalThis.location={hash:'#token=render-fixture-token',pathname:'/',search:''};
  globalThis.history={state:null,replaceState(){}};
  globalThis.sessionStorage=memoryStorage();globalThis.localStorage=memoryStorage();
  globalThis.window={addEventListener(){},dispatchEvent(){}};
  globalThis.matchMedia=()=>({matches:false,addEventListener(){}});
  // First render is synchronous; no backend is simulated or called by this test.
  await import('../app.js');
  assert.equal(loadingFallback.parentNode,null,'the static loading fallback must be removed at mount');
  let tags=flatten(root).map(node=>node.localName);
  assert.ok(tags.includes('main'));assert.equal(tags.filter(tag=>tag==='aside').length,2);
  assert.ok(tags.includes('textarea'));assert.ok(tags.includes('dialog'));
  render(null,root);
  const { AdapterWorkspace }=await import('../adapters.js');
  render(html`<${AdapterWorkspace} api=${{request(){throw Error('Network should not run during synchronous render');}}} project=${null} hostId="render-fixture" storage=${memoryStorage()} onNative=${()=>{}} onOpenProject=${()=>{}} onOpenNavigation=${()=>{}}/>`,root);
  tags=flatten(root).map(node=>node.localName);
  assert.ok(tags.includes('main'));assert.ok(tags.includes('aside'));assert.ok(tags.includes('textarea'));
  render(null,root);
  const { AccountsDialog }=await import('../accounts.js');
  render(html`<${AccountsDialog} api=${{}} open=${false} onRefresh=${()=>{}} profiles=${[
    {id:'c',name:'Codex profile',agent_id:'codex',managed:true,account:{auth_status:'signed_in',email:'fixture@example.test',plan:'Fixture',checked_at_ms:1234,usage:{status:'available',windows:[{id:'daily',name:'Daily',remaining_percent:0,used_percent:100,resets_at:2345}]}}},
    {id:'a',name:'Claude profile',agent_id:'claude',managed:true,account:{auth_status:'signed_out',checked_at_ms:1234,usage:{status:'unavailable'}}},
  ]}/>`,root);
  tags=flatten(root).map(node=>node.localName);assert.equal(tags.filter(tag=>tag==='meter').length,1);assert.ok(tags.includes('dialog'));assert.ok(tags.includes('input'));
  render(null,root);
  assert.throws(()=>render(html`<><span>Unsupported fragment</span></>`,root),{name:'InvalidCharacterError'});
  render(null,root);
});
