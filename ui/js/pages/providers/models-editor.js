// Providers page: the "Models" section of the editor. Discovery switch,
// the explicit model list, exclude patterns, and the panel that fetches
// what the upstream offers.

import { html, useState } from '../../../vendor/preact-htm.js';
import {
  Badge,
  Button,
  Field,
  FormRow,
  IconButton,
  Input,
  Notice,
  NumberInput,
  Segmented,
  Switch,
  TagInput,
} from '../../components/index.js';
import { api } from '../../lib/api.js';
import { formatCompact, formatNumber, plural, sentence } from '../../lib/format.js';
import { useAsync, useIsPhone } from '../../lib/hooks.js';
import { EFFORT_LEVELS, blankModel, hasCatalog, servesNothing, wildcardMatch } from './model.js';
import { Disclosure, Section, ToggleChips, firstFieldOf, focusAfterRemoval, focusSoon } from './parts.js';

const THINKING_MODES = [
  { value: 'none', label: 'Not set' },
  { value: 'levels', label: 'Levels' },
  { value: 'budget', label: 'Budget' },
  { value: 'both', label: 'Both' },
];

/** How many of a model row's optional details are filled in. */
function detailCount(row) {
  return (row.display_name.trim() ? 1 : 0) + (row.context_window != null ? 1 : 0) + (row.max_output_tokens != null ? 1 : 0) + (row.thinkingMode !== 'none' ? 1 : 0);
}

function ThinkingEditor({ row, onChange, error }) {
  const mode = row.thinkingMode;
  const levels = mode === 'levels' || mode === 'both';
  const budget = mode === 'budget' || mode === 'both';
  return html`
    <div class="prov-thinking">
      <${Field}
        label="Reasoning support"
        error=${error}
        hint=${mode === 'none'
          ? 'Not set: the built-in catalog decides what this model accepts.'
          : levels && row.levels.length === 0 && !budget
            ? 'No level picked: the model is treated as taking no reasoning setting.'
            : 'Overrides the built-in catalog for this model.'}
      >
        <${Segmented} label="Reasoning support" size="sm" value=${mode} onChange=${(next) => onChange({ thinkingMode: next })} options=${THINKING_MODES} />
      <//>
      ${levels &&
      html`
        <${Field} label="Effort levels the model accepts">
          <${ToggleChips} label="Effort levels" options=${EFFORT_LEVELS} value=${row.levels} onChange=${(next) => onChange({ levels: next })} />
        <//>
      `}
      ${budget &&
      html`
        <${FormRow}>
          <${NumberInput} label="Smallest budget" unit="tokens" min=${0} step=${1024} value=${row.min} onChange=${(v) => onChange({ min: v })} placeholder="No lower bound" optional />
          <${NumberInput} label="Largest budget" unit="tokens" min=${0} step=${1024} value=${row.max} onChange=${(v) => onChange({ max: v })} placeholder="No upper bound" optional />
        <//>
      `}
      ${mode !== 'none' &&
      html`
        <div class="prov-flags">
          <${Switch} label="Reasoning can be switched off" checked=${row.zero_allowed} onChange=${(v) => onChange({ zero_allowed: v })} />
          <${Switch} label="Accepts a budget chosen by the provider" checked=${row.dynamic_allowed} onChange=${(v) => onChange({ dynamic_allowed: v })} />
        </div>
      `}
    </div>
  `;
}

function ModelRow({ row, index, phone, issues, path, forceOpen, onChange, onRemove }) {
  const at = (field) => (path ? issues.at(field ? `${path}.${field}` : path) : undefined);
  const rowError = at('');
  const thinkingError = path
    ? issues
        .under(`${path}.thinking`)
        .map((i) => i.message)
        .join(' ') || undefined
    : undefined;
  const open = row.open || forceOpen;
  const regionId = `${row.uid}-details`;
  const named = row.id.trim() || `model ${index + 1}`;
  const details = detailCount(row);

  return html`
    <li class="prov-item" id=${row.uid} data-open=${open ? '' : undefined}>
      <div class="prov-item-main prov-model-grid">
        <${Input}
          mono
          label=${phone ? 'Upstream model id' : undefined}
          aria-label=${phone ? undefined : `Upstream model id, row ${index + 1}`}
          placeholder="gpt-4o"
          value=${row.id}
          onChange=${(v) => onChange({ id: v })}
          error=${at('id') || rowError}
        />
        <${Input}
          mono
          label=${phone ? 'Alias' : undefined}
          aria-label=${phone ? undefined : `Alias of ${named}`}
          placeholder=${row.id.trim() ? `Same as the id` : 'Same as the id'}
          value=${row.alias}
          onChange=${(v) => onChange({ alias: v })}
          error=${at('alias')}
        />
        <div class="prov-item-tools">
          <${Disclosure} open=${open} onToggle=${(next) => onChange({ open: next })} controls=${regionId} count=${open ? 0 : details}>Details<//>
          <${IconButton} id=${`${row.uid}-remove`} icon="trash" size="sm" label=${`Remove ${named}`} onClick=${onRemove} />
        </div>
      </div>
      ${open &&
      html`
        <div class="prov-item-more" id=${regionId}>
          <${FormRow}>
            <${Input} label="Display name" optional value=${row.display_name} onChange=${(v) => onChange({ display_name: v })} error=${at('display_name')} placeholder="From the catalog" />
            <${NumberInput}
              label="Context window"
              optional
              unit="tokens"
              min=${1}
              step=${1000}
              value=${row.context_window}
              onChange=${(v) => onChange({ context_window: v })}
              error=${at('context_window')}
              placeholder="From the catalog"
            />
            <${NumberInput}
              label="Max output"
              optional
              unit="tokens"
              min=${1}
              step=${1000}
              value=${row.max_output_tokens}
              onChange=${(v) => onChange({ max_output_tokens: v })}
              error=${at('max_output_tokens')}
              placeholder="From the catalog"
            />
          <//>
          <${ThinkingEditor} row=${row} onChange=${onChange} error=${thinkingError} />
        </div>
      `}
    </li>
  `;
}

// ---------------------------------------------------------------------------
// Fetch model list
// ---------------------------------------------------------------------------

const MAX_SHOWN = 150;

/**
 * What to do about a model list that could not be fetched. The gateway's
 * statuses for this request: 503 no usable credential to ask with, 504 the
 * upstream did not answer in time, 502 the upstream failed or refused (its
 * own words, secrets masked, are in the message).
 */
function fetchAdvice(error) {
  if (error.status === 503) return 'Add a usable credential and save, then try again.';
  if (error.status === 504) return 'Try again. If it keeps happening, check the base URL.';
  if (error.status === 502) return 'If the base URL or a key is wrong, correct it and save before trying again.';
  return 'Try again.';
}

function DiscoverPanel({ providerName, canFetch, dirty, draft, onAdd, onExclude, onFetched }) {
  const [filter, setFilter] = useState('');
  const fetchList = useAsync(() => api.post(`/providers/${encodeURIComponent(providerName)}/discover`, {}, { timeout: 60_000 }));
  const found = Array.isArray(fetchList.data?.models) ? fetchList.data.models : null;
  // Either way the outcome is the provider's new model-list state: the page shows it.
  const fetchNow = async () => {
    await fetchList.run();
    onFetched?.();
  };

  const listed = new Set(draft.models.map((m) => m.id.trim()).filter(Boolean));
  const excluded = (id) => draft.exclude.some((pattern) => wildcardMatch(pattern, id));

  const needle = filter.trim().toLowerCase();
  const matching = found ? found.filter((m) => !needle || m.id.toLowerCase().includes(needle) || (m.display_name ?? '').toLowerCase().includes(needle)) : [];
  const shown = matching.slice(0, MAX_SHOWN);
  // What "Add all" adds: not what is listed already, and not what an exclude
  // pattern hides (it would be added only to be hidden again).
  const addable = matching.filter((m) => !listed.has(m.id) && !excluded(m.id));
  const skipped = matching.filter((m) => !listed.has(m.id) && excluded(m.id)).length;

  return html`
    <div class="prov-discover">
      <div class="prov-discover-head">
        <div class="prov-section-text">
          <h4 class="prov-subtitle">What the provider offers</h4>
          <p class="prov-section-desc">
            ${canFetch
              ? 'Asks the upstream for its model list now, with the saved settings and credentials.'
              : 'Create the provider first. The list is fetched with its saved settings and credentials.'}
          </p>
        </div>
        <${Button} icon="download" loading=${fetchList.loading} disabled=${!canFetch} onClick=${fetchNow}>Fetch model list<//>
      </div>

      ${canFetch && dirty && !found && !fetchList.error && html`<p class="prov-note">Changes in this form are not used for the fetch until they are saved.</p>`}

      ${fetchList.error &&
      !fetchList.loading &&
      html`
        <${Notice} tone="stop" title="Could not fetch the model list">
          <span class="prov-break">${sentence(fetchList.error.message)}</span>
          <span> ${fetchAdvice(fetchList.error)}</span>
        <//>
      `}

      ${found &&
      found.length === 0 &&
      html`<${Notice} tone="info" title="The provider lists no models">Add the models you need to the explicit list by hand.<//>`}

      ${found &&
      found.length > 0 &&
      html`
        <div class="prov-discover-tools">
          <${Input} class="grow" size="sm" icon="search" type="search" aria-label="Filter the fetched models" placeholder=${`Filter ${plural(found.length, 'model')}`} value=${filter} onChange=${setFilter} />
          <${Button} size="sm" disabled=${addable.length === 0} onClick=${() => onAdd(addable)}>
            ${addable.length === 0 ? 'Nothing left to add' : needle ? `Add the ${formatNumber(addable.length)} shown` : `Add all ${formatNumber(addable.length)}`}
          <//>
        </div>
        ${shown.length === 0
          ? html`<p class="prov-note">No fetched model contains "${filter.trim()}".</p>`
          : html`
              <ul class="prov-found" aria-label="Models the provider offers">
                ${shown.map((model) => {
                  const inList = listed.has(model.id);
                  const isExcluded = excluded(model.id);
                  return html`
                    <li key=${model.id} class="prov-found-item">
                      <div class="prov-found-text">
                        <span class="mono prov-break">${model.id}</span>
                        <span class="prov-found-meta">
                          ${model.display_name && html`<span>${model.display_name}</span>`}
                          ${model.context_window != null && html`<span class="num">${formatCompact(model.context_window)} context</span>`}
                          ${inList && html`<${Badge} tone="info">in the list<//>`}
                          ${isExcluded && html`<${Badge} tone="caution">excluded<//>`}
                        </span>
                      </div>
                      <div class="prov-found-actions">
                        <${Button} size="sm" variant="ghost" disabled=${inList} aria-label=${`Add ${model.id} to the explicit list`} onClick=${() => onAdd([model])}>Add<//>
                        <${Button} size="sm" variant="ghost" disabled=${isExcluded} aria-label=${`Exclude ${model.id}`} onClick=${() => onExclude(model.id)}>Exclude<//>
                      </div>
                    </li>
                  `;
                })}
              </ul>
            `}
        ${matching.length > shown.length && html`<p class="prov-note">Showing the first ${formatNumber(shown.length)} of ${formatNumber(matching.length)}. Filter to narrow the list.</p>`}
        ${skipped > 0 && html`<p class="prov-note">${needle ? 'Add the shown' : 'Add all'} leaves out ${plural(skipped, 'excluded model')}.</p>`}
      `}
    </div>
  `;
}

// ---------------------------------------------------------------------------
// Section
// ---------------------------------------------------------------------------

/**
 * draft, update(patch)     the editor's draft and its shallow setter
 * issues                   useIssues() of the last failed save
 * paths                    row uid -> "models[2]" as last sent
 * hasIssueUnder(path)      whether any issue sits at or below a path
 * providerName             the saved name (null for a provider being created)
 * onModelsFetched()        the upstream was asked for its model list (it answered or failed)
 */
export default function ModelsSection({ draft, update, issues, paths, hasIssueUnder, providerName, dirty, onModelsFetched }) {
  const phone = useIsPhone();
  const setModels = (fn) => update((d) => ({ models: fn(d.models) }));
  const changeRow = (uid, patch) => setModels((rows) => rows.map((r) => (r.uid === uid ? { ...r, ...patch } : r)));
  const excludeIssues = issues.under('exclude');
  const listIssue = issues.at('models');

  const addFound = (models) =>
    setModels((rows) => {
      const have = new Set(rows.map((r) => r.id.trim()));
      return [...rows, ...models.filter((m) => !have.has(m.id)).map((m) => blankModel({ id: m.id }))];
    });

  const ADD_ID = 'prov-add-model';
  const addRow = () => {
    const row = blankModel();
    setModels((rows) => [...rows, row]);
    focusSoon(firstFieldOf(row.uid));
  };
  const removeRow = (uid) => {
    focusAfterRemoval(draft.models, draft.models.findIndex((r) => r.uid === uid), ADD_ID);
    setModels((rows) => rows.filter((r) => r.uid !== uid));
  };

  const explicit = draft.models.filter((m) => m.id.trim()).length;
  const mock = draft.kind === 'mock';
  const catalog = hasCatalog(draft.kind);
  const asks = 'Asks the provider for its model list when the gateway starts, when these settings change and on reload.';
  const discoverHint =
    explicit > 0
      ? 'Not used while the explicit list below has entries: only those models are served.'
      : mock
        ? 'Not used by the mock provider: its models are built into the gateway.'
        : catalog
          ? `${asks} Off: the built-in catalog for the kind is used.`
          : `${asks} This kind has no built-in catalog: off, only the explicit list below is served.`;

  return html`
    <${Section} id="prov-sec-models" title="Models" description="Which models this provider serves, and under which names clients ask for them.">
      <${Switch}
        label="Discover from upstream"
        checked=${draft.discover}
        onChange=${(v) => update({ discover: v })}
        error=${issues.at('discover')}
        warning=${servesNothing({ kind: draft.kind, discover: draft.discover, explicit })
          ? 'With discovery off and no explicit models, this provider serves no models: this kind has no built-in catalog. Add the models below, or switch discovery on.'
          : undefined}
        hint=${discoverHint}
      />

      <div class="prov-list-block">
        <div class="prov-list-head">
          <div class="prov-section-text">
            <h4 class="prov-subtitle">Explicit models</h4>
            <p class="prov-section-desc">${mock ? 'Leave empty to serve the built-in mock models.' : catalog ? 'Leave empty to serve what discovery or the catalog gives.' : 'Leave empty to serve what discovery finds.'} An alias is the name clients use instead of the upstream id.</p>
          </div>
        </div>
        ${draft.models.length > 0 &&
        html`
          ${!phone &&
          html`<div class="prov-model-grid prov-list-cols" aria-hidden="true">
            <span class="plate-label">Upstream model id</span><span class="plate-label">Alias</span><span></span>
          </div>`}
          <ul class="prov-items">
            ${draft.models.map(
              (row, index) => html`
                <${ModelRow}
                  key=${row.uid}
                  row=${row}
                  index=${index}
                  phone=${phone}
                  issues=${issues}
                  path=${paths[row.uid]}
                  forceOpen=${!!paths[row.uid] && ['display_name', 'context_window', 'max_output_tokens', 'thinking'].some((f) => hasIssueUnder(`${paths[row.uid]}.${f}`))}
                  onChange=${(patch) => changeRow(row.uid, patch)}
                  onRemove=${() => removeRow(row.uid)}
                />
              `,
            )}
          </ul>
        `}
        ${listIssue && html`<${Notice} tone="stop">${listIssue}<//>`}
        <div class="row row-wrap">
          <${Button} id=${ADD_ID} size="sm" icon="plus" onClick=${addRow}>Add model<//>
          ${draft.models.length > 0 && html`<span class="faint prov-count">${plural(draft.models.length, 'model')} listed</span>`}
        </div>
      </div>

      <${TagInput}
        label="Exclude"
        optional
        value=${draft.exclude}
        onChange=${(v) => update({ exclude: v })}
        placeholder="*-preview, gpt-3.5*"
        hint="Model names to hide from this provider. * matches any run of characters."
        error=${excludeIssues.length > 0 ? excludeIssues.map((i) => i.message).join(' ') : undefined}
      />

      <${DiscoverPanel}
        providerName=${providerName}
        canFetch=${!!providerName}
        dirty=${dirty}
        draft=${draft}
        onAdd=${addFound}
        onExclude=${(id) => update((d) => ({ exclude: d.exclude.includes(id) ? d.exclude : [...d.exclude, id] }))}
        onFetched=${onModelsFetched}
      />
    <//>
  `;
}
