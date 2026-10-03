export const ADAPTER_JOURNAL_KEY='switchya.adapterStarts.v1';
const inMemory=new Map();
export function readAdapterJournal(storage,hostId){
  if(inMemory.has(hostId))return {...inMemory.get(hostId)};
  try{
    const value=JSON.parse(storage?.getItem(ADAPTER_JOURNAL_KEY)||'null');
    if(!value||value.host_id!==hostId||!value.requests||typeof value.requests!=='object'||Array.isArray(value.requests))return {};
    const requests=Object.fromEntries(Object.entries(value.requests).filter(([projectId,request])=>request?.project_id===projectId
      &&typeof request.adapter_id==='string'&&typeof request.prompt==='string'&&request.prompt.trim()
      &&(request.profile_id===undefined||typeof request.profile_id==='string'&&request.profile_id.length>0)
      &&typeof request.command_id==='string'&&/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(request.command_id)));
    inMemory.set(hostId,requests);return {...requests};
  }catch{return {};}
}
export function writeAdapterJournal(storage,hostId,requests){
  inMemory.set(hostId,{...requests});
  try{storage?.setItem(ADAPTER_JOURNAL_KEY,JSON.stringify({host_id:hostId,requests}));return Boolean(storage);}catch{return false;}
}
