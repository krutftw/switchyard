const preview = document.querySelector('#product-preview');

if (preview) {
  const tabs = [...preview.querySelectorAll('[role="tab"]')];
  const panels = [...preview.querySelectorAll('[role="tabpanel"]')];
  const selectTab = (selected, moveFocus = false) => {
    for (const tab of tabs) {
      const active = tab === selected;
      tab.setAttribute('aria-selected', String(active));
      tab.tabIndex = active ? 0 : -1;
    }
    for (const panel of panels) panel.hidden = panel.id !== selected.getAttribute('aria-controls');
    if (moveFocus) selected.focus();
  };

  for (const [index, tab] of tabs.entries()) {
    tab.addEventListener('click', () => selectTab(tab));
    tab.addEventListener('keydown', event => {
      let next;
      if (event.key === 'ArrowRight') next = (index + 1) % tabs.length;
      if (event.key === 'ArrowLeft') next = (index - 1 + tabs.length) % tabs.length;
      if (event.key === 'Home') next = 0;
      if (event.key === 'End') next = tabs.length - 1;
      if (next === undefined) return;
      event.preventDefault();
      selectTab(tabs[next], true);
    });
  }

  selectTab(tabs[0]);
  preview.dataset.enhanced = 'true';
}

if (window.isSecureContext && navigator.clipboard?.writeText) {
  for (const button of document.querySelectorAll('[data-copy]')) {
    button.hidden = false;
    button.addEventListener('click', async () => {
      const feedback = button.closest('.gateway-command').querySelector('.copy-feedback');
      try {
        await navigator.clipboard.writeText(button.dataset.copy);
        feedback.textContent = 'Gateway command copied.';
      } catch {
        feedback.textContent = 'Select the command below to copy it.';
      }
    });
  }
}
