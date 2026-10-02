// Tabs and Segmented: two ways to pick one of a few.
//
// Tabs switch between views of the same thing (Overview / Attempts / Bodies).
// Segmented picks a value that changes what the view shows (1h / 24h / 7d).
//
//   const [tab, setTab] = useQueryParam('tab', 'summary');
//   html`<${Tabs} label="Request detail" value=${tab} onChange=${setTab}
//          tabs=${[{ id: 'summary', label: 'Summary' }, { id: 'attempts', label: 'Attempts', count: 3 }]} />
//        ${tab === 'summary' && html`...`}`
//
//   html`<${Segmented} label="Range" value=${range} onChange=${setRange}
//          options=${['1h', '24h', '7d', '30d']} />`

import { html, useEffect, useLayoutEffect, useRef } from '../../vendor/preact-htm.js';
import { cx } from '../lib/dom.js';
import { Icon } from './icons.js';

/** Space kept between a revealed tab and the edge of its strip, in px (the width of the fade). */
const TAB_REVEAL_MARGIN = 28;

/**
 * Arrow-key movement shared by both. Returns the index to move to (which is
 * `index` itself when it is the only enabled item), or -1 when the key is
 * not a movement key or every item is disabled.
 *
 * Disabled items are skipped (WAI-ARIA tabs pattern): arrows step over them
 * and wrap around, Home and End land on the first and last enabled item.
 * Stopping on a disabled item would strand the keyboard there, because a
 * disabled button cannot take focus. Exported for tests.
 *
 * @param {string} key  KeyboardEvent.key
 * @param {number} index  where focus is now
 * @param {number} count  number of items
 * @param {(i: number) => boolean} [isDisabled]
 */
export function rovingIndex(key, index, count, isDisabled = () => false) {
  let step;
  let from;
  switch (key) {
    case 'ArrowRight':
    case 'ArrowDown':
      step = 1;
      from = index;
      break;
    case 'ArrowLeft':
    case 'ArrowUp':
      step = -1;
      from = index;
      break;
    case 'Home':
      step = 1;
      from = -1;
      break;
    case 'End':
      step = -1;
      from = count;
      break;
    default:
      return -1;
  }
  for (let moved = 1; moved <= count; moved += 1) {
    const next = (((from + step * moved) % count) + count) % count;
    if (!isDisabled(next)) return next;
  }
  return -1;
}

/**
 * tabs      [{ id, label, count?, icon?, disabled? }]
 * value     id of the selected tab
 * onChange  (id) => void
 * label     accessible name of the tab list
 *
 * Tabs only render the tab strip. Render the selected view yourself below
 * it; keep the selected id in the URL (useQueryParam) so links reproduce it.
 *
 * Keyboard: Tab enters the strip at the selected tab; arrows, Home and End
 * move and select, skipping disabled tabs.
 *
 * A strip wider than its box scrolls sideways (its scrollbar is hidden).
 * The selected tab is brought into view inside the strip whenever the
 * selection changes, by the URL or the palette as much as by a click; the
 * page itself is never scrolled. An edge of the strip fades out while more
 * tabs are hidden beyond it.
 */
export function Tabs({ tabs, value, onChange, label, class: className }) {
  const list = useRef(null);

  // Which edges have tabs beyond them: data-more-start / data-more-end, read
  // by the CSS fade. Written straight to the element: scrolling the strip
  // should not render it again.
  const markEdges = () => {
    const strip = list.current;
    if (!strip || typeof strip.scrollWidth !== 'number') return;
    const overflow = strip.scrollWidth - strip.clientWidth;
    strip.toggleAttribute('data-more-start', overflow > 1 && strip.scrollLeft > 1);
    strip.toggleAttribute('data-more-end', overflow > 1 && strip.scrollLeft < overflow - 1);
  };

  // Bring the selected tab into view by moving the strip's own scrollLeft.
  // Not scrollIntoView: that scrolls every scrollable ancestor too, the page
  // included.
  useLayoutEffect(() => {
    const strip = list.current;
    if (!strip || typeof strip.scrollWidth !== 'number') return;
    const tab = strip.querySelector('[aria-selected="true"]');
    if (tab && strip.scrollWidth > strip.clientWidth + 1) {
      const box = strip.getBoundingClientRect();
      const at = tab.getBoundingClientRect();
      // Room for the fade, so the tab is clear of it.
      const margin = Math.min(TAB_REVEAL_MARGIN, Math.max(0, (strip.clientWidth - at.width) / 2));
      if (at.left < box.left + margin) strip.scrollLeft += at.left - box.left - margin;
      else if (at.right > box.right - margin) strip.scrollLeft += at.right - box.right + margin;
    }
    markEdges();
  }, [value, tabs.length]);

  // The strip's width follows the window; its content follows the labels and counts.
  useEffect(() => {
    const strip = list.current;
    if (!strip) return undefined;
    markEdges();
    if (typeof ResizeObserver === 'undefined') return undefined;
    const observer = new ResizeObserver(markEdges);
    observer.observe(strip);
    return () => observer.disconnect();
  }, []);

  const onKeyDown = (event, index) => {
    const next = rovingIndex(event.key, index, tabs.length, (i) => !!tabs[i].disabled);
    if (next === -1) return;
    event.preventDefault();
    list.current.querySelectorAll('[role="tab"]')[next]?.focus();
    if (tabs[next].id !== value) onChange(tabs[next].id);
  };
  // The strip needs one tab stop. That is the selected tab; when `value`
  // names no enabled tab (a stale id from the URL), the first enabled one.
  const stop = tabs.some((tab) => tab.id === value && !tab.disabled) ? value : tabs.find((tab) => !tab.disabled)?.id;
  return html`
    <div ref=${list} class=${cx('tabs', className)} role="tablist" aria-label=${label} onScroll=${markEdges}>
      ${tabs.map(
        (tab, index) => html`
          <button
            key=${tab.id}
            type="button"
            class="tab"
            role="tab"
            aria-selected=${tab.id === value ? 'true' : 'false'}
            tabindex=${tab.id === stop ? 0 : -1}
            disabled=${tab.disabled}
            onClick=${() => onChange(tab.id)}
            onKeyDown=${(event) => onKeyDown(event, index)}
          >
            ${tab.icon && html`<${Icon} name=${tab.icon} size=${14} />`}
            <span>${tab.label}</span>
            ${tab.count != null && html`<span class="tab-count">${tab.count}</span>`}
          </button>
        `,
      )}
    </div>
  `;
}

/**
 * options   strings, or [{ value, label, icon?, title?, disabled? }]
 * value     selected value
 * onChange  (value) => void
 * label     accessible name of the group (required: say what is being chosen)
 * size      "md" | "sm"
 *
 * Keyboard: Tab enters the group at the checked option; arrows, Home and End
 * move and select, skipping disabled options.
 */
export function Segmented({ options, value, onChange, label, size = 'md', class: className }) {
  const group = useRef(null);
  const items = options.map((o) => (typeof o === 'object' ? o : { value: o, label: String(o) }));
  const onKeyDown = (event, index) => {
    const next = rovingIndex(event.key, index, items.length, (i) => !!items[i].disabled);
    if (next === -1) return;
    event.preventDefault();
    group.current.querySelectorAll('[role="radio"]')[next]?.focus();
    if (items[next].value !== value) onChange(items[next].value);
  };
  // One tab stop for the group: the checked option, else the first enabled one.
  const stopAt = items.findIndex((o) => o.value === value && !o.disabled);
  const stop = stopAt !== -1 ? stopAt : items.findIndex((o) => !o.disabled);
  return html`
    <div ref=${group} class=${cx('seg', className)} role="radiogroup" aria-label=${label} data-size=${size === 'sm' ? 'sm' : undefined}>
      ${items.map(
        (option, index) => html`
          <button
            key=${String(option.value)}
            type="button"
            class="seg-opt"
            role="radio"
            title=${option.title}
            aria-checked=${option.value === value ? 'true' : 'false'}
            tabindex=${index === stop ? 0 : -1}
            disabled=${option.disabled}
            onClick=${() => onChange(option.value)}
            onKeyDown=${(event) => onKeyDown(event, index)}
          >
            ${option.icon && html`<${Icon} name=${option.icon} size=${14} />`}
            ${option.label != null && html`<span>${option.label}</span>`}
          </button>
        `,
      )}
    </div>
  `;
}
