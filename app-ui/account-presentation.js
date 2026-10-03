export const agentName=id=>({codex:'Codex',claude:'Claude Code'}[id]||id);
export const authLabel=status=>({signed_in:'Signed in',signed_out:'Signed out',unknown:'Sign-in status unknown'}[status]||'Sign-in status unknown');
export const activeLogin=login=>['starting','awaiting_user'].includes(login?.status);
export function mergeLoginState(previous,incoming){
  const next={...previous};for(const login of incoming){if(!login?.profile_id)continue;const current=next[login.profile_id];if(!current||login.updated_at_ms>=current.updated_at_ms)next[login.profile_id]=login;}return next;
}
const percent=value=>typeof value==='number'&&Number.isFinite(value)&&value>=0&&value<=100?value:null;
export function usageWindows(account){
  if(account?.usage?.status!=='available'||!Number.isFinite(account.checked_at_ms)||account.checked_at_ms<=0)return [];
  return (Array.isArray(account.usage.windows)?account.usage.windows:[]).map(window=>({
    ...window,used_percent:percent(window.used_percent),remaining_percent:percent(window.remaining_percent),
    resets_at:typeof window.resets_at==='number'&&Number.isFinite(window.resets_at)&&window.resets_at>0?window.resets_at:null,
  }));
}
export const percentLabel=value=>`${new Intl.NumberFormat(undefined,{maximumFractionDigits:1}).format(value)}%`;
export function chosenProfile(profiles,defaults,agentId,explicitId){
  const id=explicitId||defaults?.[agentId];
  return profiles.find(profile=>profile.id===id&&profile.agent_id===agentId)||null;
}
export const canStartProfile=profile=>Boolean(profile)&&(profile.managed?profile.account?.auth_status==='signed_in':profile.account?.auth_status!=='signed_out');
export const foldProfileName=value=>String(value||'').trim().replace(/[A-Z]/g,letter=>letter.toLowerCase());
export function validProfileName(value){const name=String(value||'').trim();return name.length>0&&[...name].length<=64&&!/\p{Cc}/u.test(name);}
