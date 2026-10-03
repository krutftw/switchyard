// Combobox: a text field with a list of suggestions. Anything can be typed;
// the list only helps. Local to the playground until the shared kit has one.
//
//   html`<${Combobox} label="Model" value=${model} onChange=${setModel}
//          options=${[{ value: 'gpt-4o', hint: 'openai-main' }]}
//          loading=${models.loading} loadError=${models.error} onRetry=${models.refresh} />`
//
// Keyboard: typing filters; ArrowDown / ArrowUp open the list and move;
// Enter takes the highlighted suggestion; Escape or Tab closes. Focus never
// leaves the field (the WAI-ARIA combobox pattern with a listbox popup).
// The list is only in the document while it is open, so the field names it
// in aria-controls only then: no reference to an element that is not there.

import { html, useEffect, useLayoutEffect, useMemo, useRef, useState } from '../../../vendor/preact-htm.js';
import { Button, Field, Icon, Portal, Skeleton, StatusLamp } from '../../components/index.js';
import { cx, placeFloating, scrollMoves } from '../../lib/dom.js';
import { useOutsidePointer, useUid } from '../../lib/hooks.js';

/** Suggestions drawn at once; a longer list asks for a narrower search. */
const LIST_LIMIT = 80;

/**
 * value, onChange(value)
 * options     [{ value, hint?, tone?, toneLabel? }]: `hint` is quiet text on the
 *             right (a provider), `tone` + `toneLabel` a lamp with its words
 * loading     the options are still being fetched (the field already works)
 * loadError   ApiError: shown in the list with a retry
 * onRetry     () => void
 * noun        what the options are, plural ("models")
 * emptyText   shown when there are no options at all
 * label, hint, error, optional, disabled, placeholder, inputRef
 */
export default function Combobox({
  label,
  hint,
  error,
  optional,
  value,
  onChange,
  options = [],
  loading = false,
  loadError = null,
  onRetry,
  noun = 'options',
  emptyText,
  placeholder,
  disabled = false,
  inputRef,
  class: className,
}) {
  const id = useUid('combo');
  const anchor = useRef(null);
  const list = useRef(null);
  const ownInput = useRef(null);
  const input = inputRef ?? ownInput;
  const [open, setOpen] = useState(false);
  // What the list is filtered by. null while the field still shows the value
  // it was opened with: then every option is listed.
  const [query, setQuery] = useState(null);
  const [active, setActive] = useState(-1);
  const [pos, setPos] = useState(null);

  const matches = useMemo(() => {
    const q = (query ?? '').trim().toLowerCase();
    if (!q) return options;
    return options.filter((o) => o.value.toLowerCase().includes(q) || (o.hint ?? '').toLowerCase().includes(q));
  }, [options, query]);
  const shown = matches.length > LIST_LIMIT ? matches.slice(0, LIST_LIMIT) : matches;

  const close = () => {
    setOpen(false);
    setQuery(null);
    setActive(-1);
  };
  const show = (startAt) => {
    setOpen(true);
    if (startAt != null) setActive(startAt);
  };

  useOutsidePointer([anchor, list], close, open);

  useLayoutEffect(() => {
    if (!open || !anchor.current || !list.current) return;
    const rect = anchor.current.getBoundingClientRect();
    const next = placeFloating(rect, { width: rect.width, height: list.current.offsetHeight }, { side: 'bottom', align: 'start', gap: 4 });
    setPos({ ...next, width: Math.round(rect.width) });
  }, [open, shown.length, loading, loadError]);

  useEffect(() => {
    if (!open) {
      setPos(null);
      return undefined;
    }
    // The list is anchored to a field that scrolls away: close rather than
    // chase it. Only scrolling that moves the field counts: an answer that
    // streams into the transcript next to it scrolls by itself.
    const onScroll = (event) => {
      if (scrollMoves(event.target, anchor.current)) close();
    };
    window.addEventListener('scroll', onScroll, true);
    window.addEventListener('resize', close);
    return () => {
      window.removeEventListener('scroll', onScroll, true);
      window.removeEventListener('resize', close);
    };
  }, [open]);

  useEffect(() => {
    if (!open || active < 0) return;
    list.current?.querySelector(`[data-index="${active}"]`)?.scrollIntoView({ block: 'nearest' });
  }, [open, active]);

  const choose = (option) => {
    onChange?.(option.value);
    close();
    input.current?.focus();
  };

  const move = (delta) => {
    if (shown.length === 0) return;
    setActive((at) => (at === -1 ? (delta > 0 ? 0 : shown.length - 1) : (at + delta + shown.length) % shown.length));
  };

  const onKeyDown = (event) => {
    if (event.isComposing) return;
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      if (!open) {
        const current = options.findIndex((o) => o.value === value);
        show(current !== -1 && current < LIST_LIMIT ? current : event.key === 'ArrowDown' ? 0 : Math.min(options.length, LIST_LIMIT) - 1);
      } else {
        move(event.key === 'ArrowDown' ? 1 : -1);
      }
    } else if (event.key === 'Enter') {
      if (open && active >= 0 && shown[active]) {
        event.preventDefault();
        choose(shown[active]);
      }
    } else if (event.key === 'Escape') {
      if (open) {
        // Only the list closes; a drawer or dialog around the field stays.
        event.preventDefault();
        event.stopPropagation();
        close();
      }
    } else if (event.key === 'Tab') {
      close();
    }
  };

  const describedBy = error ? `${id}-error` : hint != null ? `${id}-hint` : undefined;

  let body;
  if (loadError && options.length === 0) {
    body = html`
      <div class="play-combo-note" role="status">
        <span>Could not load the ${noun}: ${loadError.message}</span>
        ${onRetry && html`<${Button} size="sm" icon="refresh" onClick=${() => onRetry()}>Try again<//>`}
      </div>
    `;
  } else if (loading && options.length === 0) {
    body = html`<div class="play-combo-note" aria-label=${`Loading ${noun}`}><${Skeleton} lines=${3} /></div>`;
  } else if (options.length === 0) {
    body = html`<div class="play-combo-note">${emptyText ?? `No ${noun} to suggest. Type a name.`}</div>`;
  } else if (shown.length === 0) {
    body = html`<div class="play-combo-note">No listed ${noun} match. What you typed is sent as it is.</div>`;
  } else {
    body = html`
      ${shown.map(
        (option, index) => html`
          <div
            key=${option.value}
            id=${`${id}-opt-${index}`}
            class="menu-item play-combo-item"
            role="option"
            data-index=${index}
            data-active=${index === active ? '' : undefined}
            aria-selected=${option.value === value ? 'true' : 'false'}
            onPointerMove=${() => active !== index && setActive(index)}
            onClick=${() => choose(option)}
          >
            <${Icon} name="check" size=${14} class=${option.value === value ? undefined : 'invisible'} />
            <span class="menu-label mono play-combo-value">${option.value}</span>
            ${option.tone && html`<${StatusLamp} tone=${option.tone} label=${option.toneLabel} class="play-combo-state" />`}
            ${option.hint && html`<span class="menu-hint play-combo-hint">${option.hint}</span>`}
          </div>
        `,
      )}
      ${matches.length > shown.length && html`<div class="play-combo-note">${matches.length - shown.length} more. Type to narrow the list.</div>`}
    `;
  }

  return html`
    <${Field} label=${label} hint=${hint} error=${error} optional=${optional} htmlFor=${id} class=${className}>
      <div ref=${anchor} class="input play-combo" data-mono="" data-invalid=${error ? '' : undefined} data-disabled=${disabled ? '' : undefined}>
        <input
          ref=${input}
          id=${id}
          class="input-el"
          type="text"
          role="combobox"
          aria-autocomplete="list"
          aria-expanded=${open ? 'true' : 'false'}
          aria-controls=${open ? `${id}-list` : undefined}
          aria-activedescendant=${open && active >= 0 ? `${id}-opt-${active}` : undefined}
          aria-invalid=${error ? 'true' : undefined}
          aria-describedby=${describedBy}
          value=${value ?? ''}
          placeholder=${placeholder}
          disabled=${disabled}
          spellcheck=${false}
          autocapitalize="off"
          autocorrect="off"
          autocomplete="off"
          onInput=${(event) => {
            const text = event.target.value;
            onChange?.(text);
            setQuery(text);
            setActive(-1);
            setOpen(true);
          }}
          onKeyDown=${onKeyDown}
          onBlur=${(event) => {
            // Focus moving into the list (a scrollbar drag) keeps it open.
            if (list.current && list.current.contains(event.relatedTarget)) return;
            close();
          }}
        />
        <span class="input-actions">
          <button
            type="button"
            class="play-combo-toggle"
            tabindex="-1"
            aria-label=${open ? `Hide ${noun}` : `Show ${noun}`}
            disabled=${disabled}
            onMouseDown=${(event) => event.preventDefault()}
            onClick=${() => {
              if (open) close();
              else {
                show(options.findIndex((o) => o.value === value));
                input.current?.focus();
              }
            }}
          >
            <${Icon} name="chevron-down" size=${14} />
          </button>
        </span>
      </div>
      ${open &&
      html`
        <${Portal}>
          <div
            ref=${list}
            id=${`${id}-list`}
            class=${cx('menu', 'play-combo-list')}
            role="listbox"
            aria-label=${typeof label === 'string' ? label : noun}
            data-state=${pos ? 'open' : 'closed'}
            style=${pos
              ? `top:${pos.top}px;left:${pos.left}px;width:${pos.width}px;transition-duration:0ms`
              : 'top:0;left:0;visibility:hidden'}
            onMouseDown=${(event) => event.preventDefault()}
          >
            ${body}
          </div>
        <//>
      `}
    <//>
  `;
}
