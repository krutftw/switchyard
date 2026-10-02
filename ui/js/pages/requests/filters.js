// Requests page: the filter bar. Every filter is a query parameter of the
// page (and of GET /requests), so a link reproduces the view.

import { html, useEffect, useMemo, useRef, useState } from '../../../vendor/preact-htm.js';
import { Badge, Button, IconButton, Input, Select } from '../../components/index.js';
import { useDebounced, useHotkey, useIsPhone } from '../../lib/hooks.js';

const STATUS_OPTIONS = [
  { value: 'ok', label: 'Succeeded' },
  { value: 'error', label: 'Failed' },
  { value: '4xx', label: '4xx, refused by the gateway' },
  { value: '5xx', label: '5xx, gateway or upstream failed' },
  { value: '400', label: '400 Bad request' },
  { value: '401', label: '401 Unauthorized' },
  { value: '403', label: '403 Forbidden' },
  { value: '404', label: '404 Not found' },
  { value: '413', label: '413 Body too large' },
  { value: '422', label: '422 Not valid' },
  { value: '429', label: '429 Rate limited' },
  { value: '499', label: '499 Client went away' },
  { value: '500', label: '500 Gateway error' },
  { value: '502', label: '502 Upstream failed' },
  { value: '503', label: '503 Unavailable' },
  { value: '504', label: '504 Upstream timed out' },
];

/**
 * The option a value from the URL stands for: the one with that value, or,
 * for client keys, the one with that id (GET /requests takes a key's name or
 * its id, and the API keys page links here by id). Case is ignored, as the
 * gateway ignores it.
 */
function optionFor(options, value) {
  const want = value.toLowerCase();
  return options.find((o) => String(o.value).toLowerCase() === want) ?? options.find((o) => o.ids?.some((id) => String(id).toLowerCase() === want));
}

/**
 * One option per client key name. Names are not unique (two keys may share
 * one, and a key may be called "dashboard"); the filter matches by name, so
 * one entry stands for all of them and remembers their ids.
 */
function keyOptionsOf(keys) {
  const byName = new Map();
  const add = (name, label, id) => {
    const known = byName.get(name.toLowerCase());
    if (known) {
      if (id != null) known.ids.push(id);
    } else {
      byName.set(name.toLowerCase(), { value: name, label, ids: id != null ? [id] : [] });
    }
  };
  for (const k of keys ?? []) if (k?.name) add(k.name, k.name, k.id);
  add('dashboard', 'Playground (dashboard)');
  add('anonymous', 'No client key');
  return [...byName.values()];
}

/** Options for a select, with the value from the URL added when the list lacks it. */
function withCurrent(options, value) {
  if (!value || optionFor(options, value)) return options;
  return [{ value, label: value }, ...options];
}

/** The value of the option that `value` stands for, so the select shows it. */
function selected(options, value) {
  if (!value) return '';
  return optionFor(options, value)?.value ?? value;
}

/**
 * filters   { status, model, provider, key, q } from the URL
 * onChange  (patch) => void: writes the changed filters to the URL
 * onClear   () => void: removes every filter
 * models, providers  names to offer; undefined while they load
 * keys      [{ id, name }] client keys to offer; undefined while they load
 */
export default function FilterBar({ filters, onChange, onClear, models, providers, keys }) {
  const phone = useIsPhone();
  const input = useRef(null);

  // The search box keeps its own text and writes it to the URL once typing
  // pauses. `written` is the last value this box put there: a different
  // value in the URL came from outside (Back, a link, "Clear filters") and
  // replaces the text. Decided while rendering, not in an effect, so it can
  // never overwrite a keystroke.
  const [draft, setDraft] = useState(filters.q);
  const written = useRef(filters.q);
  let text = draft;
  if (filters.q !== written.current) {
    written.current = filters.q;
    if (draft.trim() !== filters.q) {
      text = filters.q;
      setDraft(filters.q);
    }
  }

  const write = (value) => {
    const q = value.trim();
    if (q === written.current) return;
    written.current = q;
    onChange({ q });
  };

  const settled = useDebounced(text, 300);
  useEffect(() => {
    write(settled);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [settled]);

  useHotkey('/', () => input.current?.focus());

  const statusOptions = useMemo(() => withCurrent(STATUS_OPTIONS, filters.status), [filters.status]);
  const modelOptions = useMemo(() => withCurrent((models ?? []).map((name) => ({ value: name, label: name })), filters.model), [models, filters.model]);
  const providerOptions = useMemo(() => withCurrent((providers ?? []).map((name) => ({ value: name, label: name })), filters.provider), [providers, filters.provider]);
  const keyOptions = useMemo(() => withCurrent(keyOptionsOf(keys), filters.key), [keys, filters.key]);

  const active = [filters.status, filters.model, filters.provider, filters.key].filter(Boolean).length;
  const [expanded, setExpanded] = useState(active > 0);
  const showSelects = !phone || expanded;

  return html`
    <div class="req-filters" role="search" aria-label="Filter requests">
      <div class="req-filters-search">
        <${Input}
          class="req-search"
          label="Search"
          size="sm"
          type="search"
          icon="search"
          value=${text}
          onChange=${setDraft}
          onEnter=${(event) => write(event.target.value)}
          inputRef=${input}
          placeholder="Request id, model, provider, key, endpoint or error text"
          actions=${text ? html`<${IconButton} icon="x" label="Clear search" size="sm" onClick=${() => { setDraft(''); write(''); input.current?.focus(); }} />` : null}
        />
        ${phone &&
        html`
          <${Button} size="sm" icon="filter" aria-expanded=${expanded ? 'true' : 'false'} aria-controls="req-filter-selects" onClick=${() => setExpanded(!expanded)}>
            <span>Filters</span>
            ${active > 0 && html`<${Badge} tone="info">${active}<//>`}
          <//>
        `}
      </div>
      ${showSelects &&
      html`
        <div class="req-filters-selects" id="req-filter-selects">
          <${Select}
            label="Status"
            size="sm"
            placeholder="All statuses"
            value=${selected(statusOptions, filters.status)}
            options=${statusOptions}
            onChange=${(status) => onChange({ status })}
          />
          <${Select}
            label="Model"
            size="sm"
            placeholder="All models"
            value=${selected(modelOptions, filters.model)}
            options=${modelOptions}
            onChange=${(model) => onChange({ model })}
          />
          <${Select}
            label="Provider"
            size="sm"
            placeholder="All providers"
            value=${selected(providerOptions, filters.provider)}
            options=${providerOptions}
            onChange=${(provider) => onChange({ provider })}
          />
          <${Select}
            label="Client key"
            size="sm"
            placeholder="All client keys"
            value=${selected(keyOptions, filters.key)}
            options=${keyOptions}
            onChange=${(key) => onChange({ key })}
          />
          ${(active > 0 || filters.q) &&
          html`<${Button} class="req-filters-clear" variant="ghost" size="sm" icon="x" onClick=${() => { setDraft(''); written.current = ''; onClear(); }}>Clear filters<//>`}
        </div>
      `}
    </div>
  `;
}
