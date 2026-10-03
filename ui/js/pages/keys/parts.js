// API keys page: pieces shared by the create modal, the edit drawer and the
// table.
//
//   ModelPatternsField  TagInput for the allow-list with a live preview of
//                       which current models the patterns admit
//   ConnectExamples     ready-to-paste snippets for one key
//   PatternSummary      "All models" / "3 patterns" for a table cell
//   SecretText          a key shown in full, wrapping, selectable in one click

import { html, useMemo, useState } from '../../../vendor/preact-htm.js';
import { Button, CodeBlock, Field, Icon, Segmented, Skeleton, Tabs, TagInput, Tooltip } from '../../components/index.js';
import { cx } from '../../lib/dom.js';
import { plural } from '../../lib/format.js';
import { useLocalStorage } from '../../lib/hooks.js';
import { SHELLS, buildExamples, defaultShell, exampleModel, gatewayAddresses, matchModels, servedNames } from './util.js';

// How many matching model names the preview lists before "Show all".
const PREVIEW_CHIPS = 8;

// ---------------------------------------------------------------------------
// Allow-list field
// ---------------------------------------------------------------------------

function ModelPreview({ patterns, models }) {
  const [expanded, setExpanded] = useState(false);
  const names = useMemo(() => servedNames(models.data), [models.data]);
  const result = useMemo(() => matchModels(patterns, names), [patterns, names]);

  if (models.loading && !models.data) {
    return html`<div class="keys-preview" aria-hidden="true"><${Skeleton} width="58%" /></div>`;
  }
  if (!models.data) {
    return html`
      <div class="keys-preview" data-tone="caution">
        <div class="keys-preview-line">
          <${Icon} name="alert" size=${14} />
          <span>Could not load the model list, so matches cannot be previewed. The patterns are saved as typed.</span>
        </div>
        <div><${Button} size="sm" variant="ghost" icon="refresh" onClick=${models.refresh}>Try again<//></div>
      </div>
    `;
  }
  if (result.total === 0) {
    return html`
      <div class="keys-preview">
        <div class="keys-preview-line" role="status"><${Icon} name="info" size=${14} /><span>The gateway serves no models yet, so there is nothing to match against.</span></div>
      </div>
    `;
  }
  if (result.all) {
    return html`
      <div class="keys-preview">
        <div class="keys-preview-line" role="status">
          <${Icon} name="check" size=${14} />
          <span>Every model is allowed: the ${plural(result.total, 'model')} served now, and any added later.</span>
        </div>
      </div>
    `;
  }

  const none = result.matched.length === 0;
  const shown = expanded ? result.matched : result.matched.slice(0, PREVIEW_CHIPS);
  const hidden = result.matched.length - shown.length;
  return html`
    <div class="keys-preview" data-tone=${none ? 'caution' : undefined}>
      <div class="keys-preview-line" role="status">
        <${Icon} name=${none ? 'alert' : 'check'} size=${14} />
        <span>
          ${none
            ? `Matches none of the ${plural(result.total, 'model')} served now. Clients with this key could not use any model until one matches.`
            : html`Matches <strong class="num">${result.matched.length}</strong> of ${plural(result.total, 'model')} served now.`}
        </span>
      </div>
      ${shown.length > 0 &&
      html`
        <ul class="keys-chips" aria-label="Matching models">
          ${shown.map((name) => html`<li class="keys-chip" key=${name} title=${name}>${name}</li>`)}
          ${(hidden > 0 || expanded) &&
          result.matched.length > PREVIEW_CHIPS &&
          html`
            <li>
              <button type="button" class="keys-chip-more" aria-expanded=${expanded ? 'true' : 'false'} onClick=${() => setExpanded(!expanded)}>
                ${expanded ? 'Show fewer' : `Show all ${result.matched.length}`}
              </button>
            </li>
          `}
        </ul>
      `}
      ${!none &&
      result.unmatched.length > 0 &&
      html`
        <div class="keys-preview-line" data-tone="caution">
          <${Icon} name="alert" size=${14} />
          <span>No current model matches <span class="mono">${result.unmatched.join(', ')}</span>. It takes effect when such a model is added.</span>
        </div>
      `}
    </div>
  `;
}

/**
 * The allow-list of a key.
 *
 * value, onChange  string[] of wildcard patterns; [] allows every model
 * models           the useResource('/models') of the page
 * error            field error (API issues)
 */
export function ModelPatternsField({ value, onChange, models, error, disabled = false }) {
  return html`
    <div class="stack" style="--gap:var(--space-2)">
      <${TagInput}
        label="Allowed models"
        optional
        value=${value}
        onChange=${onChange}
        disabled=${disabled}
        placeholder="All models"
        hint=${html`Leave empty to allow every model. Enter or comma adds a pattern; <span class="mono nowrap">*</span> matches any run of characters, as in <span class="mono nowrap">gpt-*</span> or <span class="mono nowrap">*-preview</span>. Case is ignored.`}
        error=${error}
      />
      <${ModelPreview} patterns=${value} models=${models} />
    </div>
  `;
}

// ---------------------------------------------------------------------------
// Table cell: allow-list summary
// ---------------------------------------------------------------------------

/**
 * "All models", the pattern itself when there is one, or "N patterns" with
 * the list in a tooltip. The drawer shows the full list, so the tooltip is
 * never the only copy. The summary is a tab stop: the tooltip opens on
 * keyboard focus as it does on hover.
 */
export function PatternSummary({ patterns, names }) {
  if (patterns.length === 0) return html`<span class="muted">All models</span>`;
  const result = names ? matchModels(patterns, names) : null;
  const tip = html`
    <div class="keys-tip">
      <ul class="keys-tip-list">
        ${patterns.map((pattern) => html`<li key=${pattern}>${pattern}</li>`)}
      </ul>
      ${result && html`<div class="keys-tip-foot">Matches ${result.matched.length} of ${plural(result.total, 'model')} served now</div>`}
    </div>
  `;
  return html`
    <${Tooltip} content=${tip} side="top" align="start">
      ${patterns.length === 1
        ? html`<span class="mono keys-pattern" tabindex="0">${patterns[0]}</span>`
        : html`<span class="keys-pattern-count" tabindex="0">${plural(patterns.length, 'pattern')}</span>`}
    <//>
  `;
}

// ---------------------------------------------------------------------------
// A key in full
// ---------------------------------------------------------------------------

/** The whole key on screen at any width; one click selects all of it. */
export function SecretText({ value, label = 'Key', class: className }) {
  return html`
    <code
      class=${cx('keys-secret', className)}
      tabindex="0"
      aria-label=${label}
      onClick=${(event) => {
        const selection = window.getSelection?.();
        if (!selection || String(selection).length > 0) return;
        const range = document.createRange();
        range.selectNodeContents(event.currentTarget);
        selection.removeAllRanges();
        selection.addRange(range);
      }}
    >${value}</code>
  `;
}

// ---------------------------------------------------------------------------
// Ready-to-paste examples
// ---------------------------------------------------------------------------

/**
 * keyText   what to put where the key goes: the full key, or a placeholder
 * patterns  the key's allow-list, to pick a model the key may use
 * models    current models (array) or undefined while unknown
 * listen    status.listen
 * tls       status.tls
 */
export function ConnectExamples({ keyText, patterns, models, listen, tls }) {
  const [tab, setTab] = useState('request');
  const [shell, setShell] = useLocalStorage('keys.shell', defaultShell());
  const [origin, setOrigin] = useState('page');
  const addresses = useMemo(() => gatewayAddresses(listen, tls), [listen, tls]);
  const base = origin === 'listen' && addresses.listen ? addresses.listen : addresses.page;
  const model = useMemo(() => exampleModel(patterns, models), [patterns, models]);
  const examples = useMemo(() => buildExamples({ base, key: keyText, model, shell }), [base, keyText, model, shell]);
  const current = examples.find((example) => example.id === tab) ?? examples[0];
  // The code block gets a title so its wrap and copy buttons sit in a bar
  // above the code: floating, they would cover the end of the first line.
  const shellLabel = (SHELLS.find((option) => option.value === shell) ?? SHELLS[0]).label;

  return html`
    <div class="keys-examples">
      <div class="keys-examples-bar">
        <${Tabs} class="grow" label="Example" value=${current.id} onChange=${setTab} tabs=${examples.map((example) => ({ id: example.id, label: example.label }))} />
        <${Segmented} size="sm" label="Shell" value=${shell} onChange=${setShell} options=${SHELLS} />
      </div>
      ${addresses.listen &&
      html`
        <${Field} label="Gateway address" hint="This page was opened on a different address than the one the gateway listens on. Use the one your clients can reach.">
          <${Segmented}
            size="sm"
            label="Gateway address"
            value=${origin === 'listen' ? 'listen' : 'page'}
            onChange=${setOrigin}
            options=${[
              { value: 'page', label: addresses.page.replace(/^https?:\/\//, ''), title: 'The address this page was opened on' },
              { value: 'listen', label: addresses.listen.replace(/^https?:\/\//, ''), title: 'The address the gateway listens on' },
            ]}
          />
        <//>
      `}
      <${CodeBlock} language="text" title=${`${current.id === 'request' ? 'Command' : 'Environment variables'}, ${shellLabel}`} value=${current.code} note=${current.note} maxHeight="240px" />
      ${addresses.wildcard &&
      html`<p class="field-hint">The gateway accepts connections on every interface (<span class="mono">${addresses.wildcard}</span>). From another machine, replace the host in these examples with this machine's name or address.</p>`}
    </div>
  `;
}
