export const BRAND = 'Switchya';
export const MAX_EVENTS = 600;
export const stateLabel = state => ({idle:'Ready',starting:'Starting',running:'Working',awaiting_approval:'Needs approval',interrupting:'Interrupting',interrupted:'Interrupted',recovery_required:'Recovery required',failed:'Failed',completed:'Completed'}[state] || 'Unknown state');
export const stateTone = state => ({starting:'info',running:'info',awaiting_approval:'caution',interrupting:'caution',interrupted:'caution',recovery_required:'stop',failed:'stop',completed:'neutral'}[state] || 'neutral');
export const canSubmit = state => ['idle','interrupted','failed','completed'].includes(state);
export const timeLabel = ms => Number.isFinite(ms) ? new Intl.DateTimeFormat(undefined,{hour:'numeric',minute:'2-digit'}).format(new Date(ms)) : '';
export const fullTime = ms => Number.isFinite(ms) ? new Date(ms).toLocaleString() : '';
export const pretty = value => typeof value === 'string' ? value : JSON.stringify(value ?? null,null,2);
export const shortPath = path => String(path || '').split(/[\\/]/).filter(Boolean).pop() || path;
export const modelId = model => typeof model?.id === 'string' ? model.id : '';
export const isMockModel = model => modelId(model).startsWith('mock-') || model?.owned_by === 'mock' || model?.provider === 'mock';
export const toolName = name => ({read_file:'Read file',search:'Search project',search_files:'Search project',list_files:'List files',apply_patch:'Edit files',run_command:'Run command'}[name] || String(name||'Tool operation').replaceAll('_',' '));
export const toolStateLabel = state => ({proposed:'Proposed',pending:'Proposed',awaiting_approval:'Needs approval',approved:'Approved once',denied:'Denied',running:'Running',completed:'Completed',ok:'Completed',error:'Failed',failed:'Failed',interrupted:'Interrupted'}[state] || String(state||'Proposed').replaceAll('_',' '));

export function mergeEvents(previous, incoming, sessionId) {
  const bySequence = new Map();
  for (const event of [...previous, ...incoming]) {
    if (event?.session_id === sessionId && Number.isSafeInteger(event.seq) && event.seq > 0) bySequence.set(event.seq,event);
  }
  return [...bySequence.values()].sort((a,b)=>a.seq-b.seq).slice(-MAX_EVENTS);
}

/** Build a transcript only from persisted events. No speculative completion. */
export function transcript(events) {
  const items = [];
  const tools = new Map();
  let stream = null;
  for (const event of events) {
    const payload = event.payload || {};
    const base = {seq:event.seq,at_ms:event.at_ms,run_id:event.run_id};
    if (event.kind === 'turn.started') {
      stream = null;
      items.push({...base,type:'user',text:payload.text || ''});
    } else if (event.kind === 'model.delta') {
      if (!stream || stream.run_id !== event.run_id) {
        stream = {...base,type:'assistant',text:'',streaming:true};
        items.push(stream);
      }
      stream.text += typeof payload.text === 'string' ? payload.text : '';
    } else if (event.kind === 'model.completed') {
      if (!stream || stream.run_id !== event.run_id) {
        stream = {...base,type:'assistant',text:'',streaming:false};
        items.push(stream);
      }
      stream.text = typeof payload.text === 'string' ? payload.text : stream.text;
      stream.streaming = false;
      stream = null;
    } else if (event.kind === 'tool.proposed' || event.kind === 'approval.required') {
      stream = null;
      if (!tools.has(payload.id)) {
        const item = {...base,type:'tool',operation:payload,status:event.kind === 'approval.required' ? 'awaiting_approval' : payload.state};
        tools.set(payload.id,item);
        items.push(item);
      } else if (event.kind === 'approval.required') tools.get(payload.id).status = 'awaiting_approval';
    } else if (event.kind === 'tool.started') {
      if (tools.has(payload.operation_id)) tools.get(payload.operation_id).status = 'running';
      else items.push({...base,type:'activity',label:'Tool started',payload});
    } else if (event.kind === 'tool.completed') {
      if (tools.has(payload.operation_id)) Object.assign(tools.get(payload.operation_id),{status:payload.result?.status || 'completed',result:payload.result});
      else items.push({...base,type:'tool',operation:{id:payload.operation_id,name:payload.name},status:payload.result?.status || 'completed',result:payload.result});
    } else if (event.kind === 'approval.decided') {
      if (tools.has(payload.operation_id)) tools.get(payload.operation_id).status = payload.decision === 'deny' ? 'denied' : 'approved';
    } else if (['run.completed','run.failed','run.interrupted','recovery.required'].includes(event.kind)) {
      stream = null;
      items.push({...base,type:'state',kind:event.kind,text:payload.message || '',payload});
    } else items.push({...base,type:'activity',label:event.kind || 'Activity',payload});
  }
  return items.filter(item=>item.type!=='assistant'||item.streaming||item.text.trim());
}

export function reviewOperations(events) {
  const operations = new Map();
  for (const event of events) {
    const p = event.payload || {};
    if (event.kind === 'tool.proposed' || event.kind === 'approval.required') operations.set(p.id,{...(operations.get(p.id)||{}),...p});
    if (event.kind === 'approval.decided' && operations.has(p.operation_id)) operations.get(p.operation_id).state=p.decision==='deny'?'denied':'approved';
    if (event.kind === 'tool.started' && operations.has(p.operation_id)) operations.get(p.operation_id).state='running';
    if (event.kind === 'tool.completed') operations.set(p.operation_id,{...(operations.get(p.operation_id)||{}),id:p.operation_id,name:p.name,result:p.result});
  }
  return [...operations.values()].reverse();
}
