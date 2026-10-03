/* This small synchronous script prevents a theme flash. It stores appearance only. */
(() => {
  let choice = 'system';
  try { choice = localStorage.getItem('switchyard.workspace.theme') || 'system'; } catch {}
  if (!['light', 'dark', 'system'].includes(choice)) choice = 'system';
  const system = matchMedia('(prefers-color-scheme: dark)');
  const apply = () => { document.documentElement.dataset.theme = choice === 'system' ? (system.matches ? 'dark' : 'light') : choice; };
  apply();
  system.addEventListener('change', apply);
  window.addEventListener('switchyard-theme', event => {
    if (!['light', 'dark', 'system'].includes(event.detail)) return;
    choice = event.detail;
    try { localStorage.setItem('switchyard.workspace.theme', choice); } catch {}
    apply();
  });
})();
