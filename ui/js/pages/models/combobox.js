// Combobox: a text field that suggests known values as you type. Free text
// stays allowed (an alias may target a model that is not listed yet).
//
// The shared kit has no autocomplete, so this one lives with the Models page
// (reported as a gap). It follows the WAI-ARIA combobox pattern with a
// listbox popup: focus stays in the field, arrows move the active option,
// Enter takes it (or closes the list when none is active), Escape closes
// the list.
//
//   html`<${Combobox} value=${target} onChange=${setTarget}
//         options=${[{ value: 'gpt-4o', hint: 'Model', tone: 'clear' }]}
//         more=${(text) => extraOptions} label="Target 1 of smart" />`

import { html, useEffect, useLayoutEffect, useMemo, useRef, useState } from '../../../vendor/preact-htm.js';
import { Input, Portal, StatusLamp } from '../../components/index.js';
import { placeFloating } from '../../lib/dom.js';
import { useUid } from '../../lib/hooks.js';

/** Options offered at once. More than this is noise; typing narrows it. */
const LIMIT = 40;

/**
 * Rank options against what was typed: names that start with it, then names
 * that contain it. An option equal to the text is left out (there is nothing
 * to complete). Exported for tests.
 */
export function rankOptions(text, options) {
  const needle = String(text ?? '').trim().toLowerCase();
  const starts = [];
  const contains = [];
  for (const option of options) {
    const value = option.value.toLowerCase();
    if (value === needle) continue;
    if (!needle || value.startsWith(needle)) starts.push(option);
    else if (value.includes(needle)) contains.push(option);
  }
  return [...starts, ...contains];
}

/**
 * value, onChange(value)
 * options      [{ value, hint?, tone? }]: tone draws a lamp before the value
 * more         (text) => options to put first for this exact text (the page
 *              uses it to offer reasoning suffixes for the typed model)
 * label        accessible name (the field has no visible label of its own)
 * error        message under the field; marks it invalid
 * warning      message under the field in the caution colour: the value is
 *              allowed, with a caveat (an error takes its place)
 * placeholder, disabled, and any other prop go to the <input>
 */
export default function Combobox({ value, onChange, options = [], more, label, error, warning, placeholder, disabled = false, ...rest }) {
  const anchor = useRef(null);
  const list = useRef(null);
  const listId = useUid('combo');
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(-1);
  const [pos, setPos] = useState(null);

  const items = useMemo(() => {
    const extra = more ? more(value ?? '') : [];
    const seen = new Set(extra.map((o) => o.value));
    return [...extra, ...rankOptions(value, options).filter((o) => !seen.has(o.value))].slice(0, LIMIT);
  }, [value, options, more]);

  const shown = open && items.length > 0 && !disabled;

  const place = () => {
    if (!anchor.current || !list.current) return;
    // The field itself, not the error or warning line that may sit under it.
    const rect = (anchor.current.querySelector('.input') ?? anchor.current).getBoundingClientRect();
    const width = Math.max(rect.width, 260);
    const next = placeFloating(rect, { width, height: list.current.offsetHeight }, { side: 'bottom', align: 'start', gap: 4 });
    setPos((prev) => (prev && prev.top === next.top && prev.left === next.left && prev.width === width ? prev : { top: next.top, left: next.left, width }));
  };

  // Place on opening and whenever the list changes height.
  useLayoutEffect(() => {
    if (shown) place();
    else setPos(null);
  }, [shown, items.length]);

  // Follow the field while the page scrolls or resizes under an open list
  // (a phone's keyboard does both when it appears).
  useEffect(() => {
    if (!shown) return undefined;
    let frame = 0;
    const follow = (event) => {
      if (list.current && event?.target instanceof Node && list.current.contains(event.target)) return;
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(place);
    };
    window.addEventListener('scroll', follow, true);
    window.addEventListener('resize', follow);
    return () => {
      cancelAnimationFrame(frame);
      window.removeEventListener('scroll', follow, true);
      window.removeEventListener('resize', follow);
    };
  }, [shown]);

  // Keep the active option in view.
  useEffect(() => {
    if (shown && active >= 0) list.current?.querySelector(`[data-index="${active}"]`)?.scrollIntoView({ block: 'nearest' });
  }, [shown, active]);

  const close = () => {
    setOpen(false);
    setActive(-1);
  };

  const pick = (item) => {
    onChange?.(item.value);
    close();
  };

  const onKeyDown = (event) => {
    if (event.isComposing) return;
    switch (event.key) {
      case 'ArrowDown':
      case 'ArrowUp': {
        if (items.length === 0) return;
        event.preventDefault();
        const step = event.key === 'ArrowDown' ? 1 : -1;
        if (!shown) {
          setOpen(true);
          setActive(step > 0 ? 0 : items.length - 1);
        } else {
          setActive((at) => (at === -1 ? (step > 0 ? 0 : items.length - 1) : (at + step + items.length) % items.length));
        }
        break;
      }
      case 'Enter':
        // While the list is open Enter belongs to it and never submits the
        // form around the field: it takes the active option, or with none
        // active closes the list and leaves the text as typed (free text is
        // allowed, so the first suggestion is not assumed).
        if (shown) {
          event.preventDefault();
          if (active >= 0 && items[active]) pick(items[active]);
          else close();
        }
        break;
      case 'Escape':
        // Closes the list only; whatever is behind it stays open.
        if (shown) {
          event.preventDefault();
          event.stopPropagation();
          close();
        }
        break;
      case 'Tab':
        close();
        break;
      default:
    }
  };

  // The empty `hint` keeps the field in one place in the tree. An Input
  // without a label is a bare box until it has a message and is put inside a
  // Field when it gets one, which mounts the <input> anew: a warning that
  // comes or goes with a keystroke (it does here) would take the focus and
  // the caret with it. With a hint, even an empty one, the Field is always
  // there; models.css hides the empty line.
  return html`
    <span ref=${anchor} class="models-combo">
      <${Input}
        mono
        value=${value}
        placeholder=${placeholder}
        disabled=${disabled}
        error=${error}
        warning=${warning}
        hint=""
        role="combobox"
        aria-label=${label}
        aria-autocomplete="list"
        aria-expanded=${shown ? 'true' : 'false'}
        aria-controls=${shown ? listId : undefined}
        aria-activedescendant=${shown && active >= 0 ? `${listId}-${active}` : undefined}
        autocomplete="off"
        onChange=${(next) => {
          onChange?.(next);
          setOpen(true);
          setActive(-1);
        }}
        onKeyDown=${onKeyDown}
        onPointerDown=${() => setOpen(true)}
        onFocus=${() => {
          // Tabbing through filled fields should not flash a list at each one.
          if (!value) setOpen(true);
        }}
        onBlur=${close}
        ...${rest}
      />
    </span>
    ${shown &&
    html`
      <${Portal}>
        <div
          ref=${list}
          id=${listId}
          class="models-combo-list"
          role="listbox"
          aria-label=${label}
          style=${pos ? `top:${pos.top}px;left:${pos.left}px;width:${pos.width}px` : 'top:0;left:0;visibility:hidden'}
          onMouseDown=${(event) => event.preventDefault()}
        >
          ${items.map(
            (item, index) => html`
              <div
                key=${item.value}
                id=${`${listId}-${index}`}
                class="models-combo-item"
                role="option"
                aria-selected=${index === active ? 'true' : 'false'}
                data-index=${index}
                data-active=${index === active ? '' : undefined}
                onPointerMove=${() => active !== index && setActive(index)}
                onClick=${() => pick(item)}
              >
                ${item.tone && html`<${StatusLamp} tone=${item.tone} title=${item.toneLabel ?? item.tone} />`}
                <span class="mono models-combo-value">${item.value}</span>
                ${item.hint && html`<span class="models-combo-hint">${item.hint}</span>`}
              </div>
            `,
          )}
        </div>
      <//>
    `}
  `;
}
