// Providers page: small pieces shared by the list, the detail drawer and
// the editor. Everything here is built from the shared components and
// tokens; what the kit lacks is listed in the page's hand-over notes.

import { html, useRef } from '../../../vendor/preact-htm.js';
import { Badge, Button, Field, Icon, Segmented } from '../../components/index.js';
import { rovingIndex } from '../../components/nav.js';
import { cx } from '../../lib/dom.js';
import { useNow, useUid } from '../../lib/hooks.js';
import { liveState } from '../../lib/live.js';
import { useStore } from '../../lib/store.js';
import { summarizeCounts } from './model.js';

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

// The gateway's clock minus this device's, measured when the live
// connection said hello. Cooldowns end at a time on the gateway's clock.
let clockSkew = 0;

/**
 * "Now" on the gateway's clock, ticking every `stepMs`. Countdowns use it so
 * a laptop whose clock is a minute off does not show a minute of cooldown
 * that is not there.
 */
export function useServerNow(stepMs = 1000) {
  const live = useStore(liveState);
  if (live.status === 'open' && live.hello?.server_time) {
    const skew = live.hello.server_time - live.since;
    // Under two seconds is the time the hello frame took to arrive, not skew.
    clockSkew = Math.abs(skew) < 2000 ? 0 : skew;
  }
  return useNow(stepMs) + clockSkew;
}

// ---------------------------------------------------------------------------
// Lamps
// ---------------------------------------------------------------------------

/**
 * One lamp per credential, in configuration order, with the count in words
 * next to them. `states` comes from providerHealth().
 */
export function CredentialLamps({ states, counts, total, max = 8, words = true }) {
  const shown = states.slice(0, max);
  const more = states.length - shown.length;
  const summary = summarizeCounts(counts, total);
  return html`
    <span class="prov-lamps" title=${words ? undefined : summary}>
      ${shown.length > 0 &&
      html`<span class="prov-lamps-row" aria-hidden="true">
        ${shown.map((s) => html`<span class="lamp" key=${s.id} data-tone=${s.tone}></span>`)}
        ${more > 0 && html`<span class="prov-lamps-more">+${more}</span>`}
      </span>`}
      ${words ? html`<span class="prov-lamps-text">${summary}</span>` : html`<span class="sr-only">${summary}</span>`}
    </span>
  `;
}

/** The legend above the list: what each lamp means and how many there are. */
export function LampLegend({ totals }) {
  const items = [
    ['clear', totals.ready, 'ready'],
    ['caution', totals.cooling, 'cooling'],
    ['stop', totals.unusable, 'unusable'],
    ['off', totals.disabled + totals.idle + totals.unknown, 'off'],
  ];
  return html`
    <div class="prov-legend" role="group" aria-label="Credentials by state">
      ${items.map(
        ([tone, count, word]) => html`
          <span class="prov-legend-item" key=${word} data-zero=${count === 0 ? '' : undefined}>
            <span class="lamp" data-tone=${tone} aria-hidden="true"></span>
            <span><span class="num">${count}</span> ${word}</span>
          </span>
        `,
      )}
    </div>
  `;
}

// ---------------------------------------------------------------------------
// Form pieces
// ---------------------------------------------------------------------------

/** A titled block of the editor or the detail drawer. */
export function Section({ id, title, description, actions, children, class: className }) {
  return html`
    <section class=${cx('prov-section', className)} id=${id} aria-labelledby=${id ? `${id}-title` : undefined}>
      <header class="prov-section-head">
        <div class="prov-section-text">
          <h3 class="prov-section-title" id=${id ? `${id}-title` : undefined}>${title}</h3>
          ${description && html`<p class="prov-section-desc">${description}</p>`}
        </div>
        ${actions && html`<div class="row">${actions}</div>`}
      </header>
      ${children}
    </section>
  `;
}

/**
 * A setting with three states: the kind's default, on, off. The gateway
 * stores "default" as null, which is why a switch will not do.
 */
export function TriState({ label, hint, error, value, onChange, defaultMeans }) {
  const id = useUid('tri');
  const text = value == null ? 'default' : value ? 'on' : 'off';
  return html`
    <${Field} label=${label} hint=${hint} error=${error} id=${id}>
      <${Segmented}
        label=${label}
        size="sm"
        value=${text}
        onChange=${(next) => onChange(next === 'default' ? null : next === 'on')}
        options=${[
          { value: 'default', label: defaultMeans ? `Default (${defaultMeans})` : 'Default' },
          { value: 'on', label: 'On' },
          { value: 'off', label: 'Off' },
        ]}
      />
    <//>
  `;
}

/** A pressed-or-not chip, for picking several of a few (reasoning levels). */
export function ToggleChips({ label, options, value, onChange, disabled = false }) {
  return html`
    <div class="prov-chips" role="group" aria-label=${label}>
      ${options.map((option) => {
        const on = value.includes(option);
        return html`
          <button
            key=${option}
            type="button"
            class="prov-chip"
            aria-pressed=${on ? 'true' : 'false'}
            disabled=${disabled}
            onClick=${() => onChange(on ? value.filter((v) => v !== option) : [...value, option])}
          >
            ${on && html`<${Icon} name="check" size=${12} />`}<span>${option}</span>
          </button>
        `;
      })}
    </div>
  `;
}

/**
 * A stored secret as the gateway showed it (a mask, or a reference as
 * written), with the button that swaps it for an input. Nothing here can
 * reveal the secret: the dashboard never has it.
 */
export function StoredSecret({ value, reference = false, onReplace, replaceLabel = 'Replace', what = 'key', disabled = false }) {
  return html`
    <div class="prov-stored">
      <span class="prov-stored-value mono" title=${reference ? 'Read from the environment when the gateway starts' : `The stored ${what}, masked`}>${value}</span>
      ${reference && html`<${Badge} outline>reference<//>`}
      <${Button} size="sm" variant="ghost" disabled=${disabled} aria-label=${`${replaceLabel} ${what} ${value}`} onClick=${onReplace}>${replaceLabel}<//>
    </div>
  `;
}

/** The kinds as a radio group of cards, each with its one-line explanation. */
export function KindPicker({ kinds, value, onChange, error }) {
  const group = useRef(null);
  const onKeyDown = (event, index) => {
    const next = rovingIndex(event.key, index, kinds.length);
    if (next === -1) return;
    event.preventDefault();
    group.current.querySelectorAll('[role="radio"]')[next]?.focus();
    if (kinds[next].value !== value) onChange(kinds[next].value);
  };
  const stop = Math.max(0, kinds.findIndex((k) => k.value === value));
  return html`
    <${Field} label="Kind" error=${error}>
      <div ref=${group} class="prov-kinds" role="radiogroup" aria-label="Kind">
        ${kinds.map(
          (kind, index) => html`
            <button
              key=${kind.value}
              type="button"
              class="prov-kind"
              role="radio"
              aria-checked=${kind.value === value ? 'true' : 'false'}
              tabindex=${index === stop ? 0 : -1}
              onClick=${() => onChange(kind.value)}
              onKeyDown=${(event) => onKeyDown(event, index)}
            >
              <span class="prov-kind-top">
                <span class="prov-kind-mark" aria-hidden="true"></span>
                <span class="prov-kind-label">${kind.label}</span>
                <span class="prov-kind-id mono">${kind.value}</span>
              </span>
              <span class="prov-kind-blurb">${kind.blurb}</span>
            </button>
          `,
        )}
      </div>
    <//>
  `;
}

/**
 * Move the keyboard focus once the next render is on screen: to the first
 * element `find()` returns that can take it. List editors use it so that a
 * row that was added gets the caret, and a row that was removed hands the
 * focus to its neighbour instead of dropping it on the page.
 *
 * A timer, not an animation frame: frames stop while the tab is in the
 * background.
 */
export function focusSoon(...finders) {
  setTimeout(() => {
    for (const find of finders) {
      const el = typeof find === 'string' ? document.getElementById(find) : find();
      if (el && !el.disabled && typeof el.focus === 'function') {
        el.focus();
        return;
      }
    }
  }, 40);
}

/** The first field of a list row, by the row's element id. */
export const firstFieldOf = (rowId) => () => document.getElementById(rowId)?.querySelector('input:not([disabled]), select:not([disabled]), textarea:not([disabled])');

/**
 * Where the focus goes when the row at `index` of `rows` is removed: the
 * Remove button of the row that takes its place, else of the one before it,
 * else the list's Add button.
 */
export function focusAfterRemoval(rows, index, addButtonId) {
  const next = rows[index + 1] ?? rows[index - 1];
  focusSoon(next ? `${next.uid}-remove` : addButtonId, addButtonId);
}

/** "Advanced" and other show-more buttons: a button that owns a region. */
export function Disclosure({ open, onToggle, controls, children, count }) {
  return html`
    <button type="button" class="prov-disclosure" aria-expanded=${open ? 'true' : 'false'} aria-controls=${controls} onClick=${() => onToggle(!open)}>
      <${Icon} name=${open ? 'chevron-down' : 'chevron-right'} size=${14} />
      <span>${children}</span>
      ${count > 0 && html`<span class="prov-disclosure-count">${count}</span>`}
    </button>
  `;
}

export default CredentialLamps;
