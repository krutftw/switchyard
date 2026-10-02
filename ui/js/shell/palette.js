// Command palette: Ctrl/Cmd+K, type, Enter.
//
// It lists every page, the global actions, and whatever the current page
// registered with useCommands (lib/commands.js). It opens and closes without
// animation: it is used from the keyboard, many times a day.

import { html, useEffect, useMemo, useRef, useState } from '../../vendor/preact-htm.js';
import { Icon } from '../components/icons.js';
import { Portal } from '../components/portal.js';
import { Kbd } from '../components/status.js';
import { pageCommands } from '../lib/commands.js';
import { useModalLayer, useUid } from '../lib/hooks.js';
import { useStore } from '../lib/store.js';

/** Rank a command against the query: lower is better, -1 is no match. */
function score(command, words) {
  if (words.length === 0) return 0;
  const label = command.label.toLowerCase();
  const haystack = `${label} ${command.group ?? ''} ${command.hint ?? ''} ${command.keywords ?? ''}`.toLowerCase();
  let total = 0;
  for (const word of words) {
    if (label.startsWith(word)) total += 0;
    else if (label.includes(word)) total += 1;
    else if (haystack.includes(word)) total += 2;
    else return -1;
  }
  return total;
}

/**
 * open      whether the palette is shown
 * onClose   () => void
 * commands  the shell's own commands: [{ id, label, group, icon?, hint?, keywords?, run }]
 */
export function CommandPalette({ open, onClose, commands }) {
  const extra = useStore(pageCommands);
  const box = useRef(null);
  const list = useRef(null);
  const listId = useUid('palette');
  const [query, setQuery] = useState('');
  const [active, setActive] = useState(0);

  const pending = useRef(null);
  // Closing without choosing runs nothing.
  const dismiss = () => {
    pending.current = null;
    onClose();
  };

  useModalLayer(box, open, { onClose: dismiss });

  // The chosen command runs once the palette has closed. This effect is
  // declared after useModalLayer on purpose: effect clean-ups run before
  // effects, so by now the layer is off the stack and focus is back where it
  // was. A command that moves focus (opens a dialog, jumps to a section)
  // keeps it; run any earlier and the focus restore would take it away.
  useEffect(() => {
    if (open) {
      setQuery('');
      setActive(0);
      return;
    }
    const command = pending.current;
    pending.current = null;
    command?.run();
  }, [open]);

  const results = useMemo(() => {
    const words = query.toLowerCase().split(/\s+/).filter(Boolean);
    const all = [...commands, ...extra];
    const scored = [];
    all.forEach((command, index) => {
      const s = score(command, words);
      if (s >= 0) scored.push({ command, s, index });
    });
    // With a query, best matches first; without one, the given order.
    if (words.length > 0) scored.sort((a, b) => a.s - b.s || a.index - b.index);
    return scored.slice(0, 50).map((entry) => entry.command);
  }, [query, commands, extra]);

  useEffect(() => {
    list.current?.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: 'nearest' });
  }, [active, results]);

  if (!open) return null;

  const run = (command) => {
    // Picked up by the effect above when the palette has closed.
    pending.current = command;
    onClose();
  };

  const onKeyDown = (event) => {
    if (event.key === 'ArrowDown') {
      event.preventDefault();
      setActive((i) => (results.length ? (i + 1) % results.length : 0));
    } else if (event.key === 'ArrowUp') {
      event.preventDefault();
      setActive((i) => (results.length ? (i - 1 + results.length) % results.length : 0));
    } else if (event.key === 'Home' && !query) {
      event.preventDefault();
      setActive(0);
    } else if (event.key === 'End' && !query) {
      event.preventDefault();
      setActive(Math.max(0, results.length - 1));
    } else if (event.key === 'Enter' && !event.isComposing) {
      event.preventDefault();
      if (results[active]) run(results[active]);
    }
  };

  // Group headings appear when browsing; a search shows one ranked list.
  const grouped = query.trim() === '';
  let lastGroup = null;

  return html`
    <${Portal}>
      <div
        class="palette-pos"
        onPointerDown=${(event) => {
          if (event.target === event.currentTarget) dismiss();
        }}
      >
        <div ref=${box} class="palette" role="dialog" aria-modal="true" aria-label="Command palette" onKeyDown=${onKeyDown}>
          <div class="palette-input">
            <${Icon} name="search" size=${18} />
            <input
              type="text"
              placeholder="Jump to a page or run an action"
              value=${query}
              data-autofocus=""
              role="combobox"
              aria-expanded="true"
              aria-controls=${listId}
              aria-activedescendant=${results[active] ? `${listId}-${active}` : undefined}
              aria-autocomplete="list"
              autocomplete="off"
              spellcheck=${false}
              onInput=${(event) => {
                setQuery(event.target.value);
                setActive(0);
              }}
            />
            <${Kbd}>Esc<//>
          </div>
          <div ref=${list} id=${listId} class="palette-list" role="listbox" aria-label="Commands">
            ${results.length === 0 && html`<div class="palette-empty">Nothing matches “${query}”. Try a page name, or “theme”.</div>`}
            ${results.map((command, index) => {
              const heading = grouped && command.group !== lastGroup ? command.group : null;
              lastGroup = command.group;
              return html`
                ${heading && html`<div class="palette-group" key=${`group-${heading}`} role="presentation">${heading}</div>`}
                <div
                  key=${command.id}
                  id=${`${listId}-${index}`}
                  class="palette-item"
                  role="option"
                  aria-selected=${index === active ? 'true' : 'false'}
                  onPointerMove=${() => index !== active && setActive(index)}
                  onClick=${() => run(command)}
                >
                  <${Icon} name=${command.icon || 'arrow-right'} />
                  <span class="palette-item-label">${command.label}</span>
                  ${command.hint && html`<span class="palette-item-hint">${command.hint}</span>`}
                  ${!grouped && command.group && html`<span class="palette-item-hint">${command.group}</span>`}
                </div>
              `;
            })}
          </div>
          <div class="palette-foot">
            <span><${Kbd}><${Icon} name="arrow-up" size=${10} /><//><${Kbd}><${Icon} name="arrow-down" size=${10} /><//> move</span>
            <span><${Kbd}>Enter<//> open</span>
            <span><${Kbd}>Esc<//> close</span>
          </div>
        </div>
      </div>
    <//>
  `;
}
