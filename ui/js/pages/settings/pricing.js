// Settings, Pricing tab: USD per million tokens by model pattern, for the
// cost estimates. GET /pricing loads the list; PUT /pricing replaces it.

import { Component, html, useMemo, useRef, useState } from '../../../vendor/preact-htm.js';
import { Button, EmptyState, ErrorState, Form, Icon, IconButton, Input, Notice, NumberInput, Panel, Skeleton, StatusLamp, confirm, toast } from '../../components/index.js';
import { ApiError } from '../../lib/api.js';
import { formatCurrency, plural, sentence } from '../../lib/format.js';
import { useDebounced, useUid } from '../../lib/hooks.js';
import { ConflictNotice, SaveBar, SaveError, focusMoved, focusSoon, moveItem, rowId, useDiskInvalid, useFieldIssues, useListDraft, useRevealProblem, useSaveHotkey, useUnsavedGuard, wildcardMatch } from './common.js';

const PRICE_FIELDS = [
  { key: 'input', label: 'Input', required: true },
  { key: 'output', label: 'Output', required: true },
  { key: 'cache_read', label: 'Cache read', required: false },
  { key: 'cache_write', label: 'Cache write', required: false },
];

// Up to this many rows, "never used" is worked out as the user types.
const SHADOW_AT_ONCE = 50;

const toRow = (row) => ({
  _id: rowId(),
  model: row.model ?? '',
  input: row.input ?? null,
  output: row.output ?? null,
  cache_read: row.cache_read ?? null,
  cache_write: row.cache_write ?? null,
});

const toDraft = (data) => (data ?? []).map(toRow);

const toBody = (draft) =>
  draft.map((row) => {
    const out = { model: row.model.trim(), input: row.input, output: row.output };
    if (row.cache_read != null) out.cache_read = row.cache_read;
    if (row.cache_write != null) out.cache_write = row.cache_write;
    return out;
  });

/**
 * For each pattern, the index of an earlier one that matches everything it
 * matches (so it can never win), or -1. An earlier pattern covers a later
 * one when it matches the later pattern read as plain text: its own `*`
 * then stands in for every `*` of the later one.
 */
export function shadowedBy(patterns) {
  const lower = patterns.map((pattern) => pattern.toLowerCase());
  return lower.map((pattern, index) => {
    if (!pattern) return -1;
    for (let earlier = 0; earlier < index; earlier += 1) {
      const other = lower[earlier];
      if (!other) continue;
      // Without a wildcard a pattern only covers itself.
      if (other.includes('*') ? wildcardMatch(other, pattern) : other === pattern) return earlier;
    }
    return -1;
  });
}

function TestResult({ name, rows, winner, matches }) {
  if (!name) return html`<p class="faint settings-test-result">Enter a model id to see which row prices it. Patterns are matched against the id sent to the provider, ignoring case.</p>`;
  if (winner === -1) {
    return html`
      <p class="settings-test-result">
        <${StatusLamp} tone="off" label="No row matches" />
        <span class="muted">Requests for <span class="mono settings-break">${name}</span> are recorded without a cost.</span>
      </p>
    `;
  }
  const row = rows[winner];
  const others = matches.filter((index) => index !== winner);
  const price = (value, fallback) => (value == null ? `${formatCurrency(fallback)} (as input)` : formatCurrency(value));
  return html`
    <div class="settings-test-result">
      <${StatusLamp} tone="clear" label=${`Row ${winner + 1} prices it`} detail=${html`<span class="mono settings-break">${row.model}</span>`} />
      <span class="muted">
        Input <span class="num">${formatCurrency(row.input)}</span>, output <span class="num">${formatCurrency(row.output)}</span>, cache read <span class="num">${price(row.cache_read, row.input)}</span>, cache write <span class="num">${price(row.cache_write, row.input)}</span> per million tokens.${others.length > 0 ? ` ${others.length === 1 ? `Row ${others[0] + 1} matches too` : `Rows ${others.map((i) => i + 1).join(', ')} match too`}, but the first match wins.` : ''}
      </span>
    </div>
  `;
}

// Everything a row is drawn from. A row is rendered again only when one of
// these changes: with a few hundred prices, typing in one field must not
// redraw the five inputs of every other row.
const ROW_PROPS = ['row', 'index', 'last', 'note', 'noteTone', 'match', 'errors', 'rowError', 'actions'];

class PriceRow extends Component {
  shouldComponentUpdate(next) {
    return ROW_PROPS.some((key) => !Object.is(this.props[key], next[key]));
  }

  render({ row, index, last, note, noteTone, match, errors, rowError, actions }) {
    const id = row._id;
    const pattern = row.model.trim();
    const hint = note && noteTone ? html`<span class=${`settings-${noteTone}`}>${note}</span>` : note || undefined;
    return html`
      <li class="settings-price" data-row=${id} data-match=${match ? '' : undefined}>
        <div class="settings-price-order">
          <span class="num settings-rule-index">${index + 1}</span>
          <${IconButton} icon="arrow-up" size="sm" data-move="up" label=${`Move row ${index + 1} up`} disabled=${index === 0} onClick=${() => actions.move(id, -1)} />
          <${IconButton} icon="arrow-down" size="sm" data-move="down" label=${`Move row ${index + 1} down`} disabled=${last} onClick=${() => actions.move(id, 1)} />
        </div>
        <${Input}
          id=${`price-model-${id}`}
          class="settings-price-model"
          label="Model pattern"
          mono
          value=${row.model}
          onChange=${(value) => actions.setCell(id, 'model', value)}
          placeholder="gpt-5*"
          hint=${hint}
          error=${errors?.model}
        />
        ${PRICE_FIELDS.map(
          (field) => html`
            <${NumberInput}
              key=${field.key}
              class="settings-price-cell"
              label=${field.label}
              value=${row[field.key]}
              onChange=${(value) => actions.setCell(id, field.key, value)}
              min=${0}
              step=${0.01}
              placeholder=${field.required || row.input == null ? undefined : String(row.input)}
              error=${errors?.[field.key]}
            />
          `,
        )}
        <div class="settings-price-delete">
          <${IconButton} icon="trash" data-delete="" label=${`Delete row ${index + 1}${pattern ? `, ${pattern}` : ''}`} onClick=${() => actions.remove(id)} />
        </div>
        ${rowError && html`<div class="field-error settings-price-problem"><${Icon} name="alert-circle" size=${14} /><span>${rowError}</span></div>`}
      </li>
    `;
  }
}

// The messages a refused save left on one row, as an object that keeps its
// identity while the messages stay the same (so the row is not redrawn).
// The body of PUT /pricing is the list itself, so an issue's path starts at
// the row: "[2].model", "[2].input" for an invalid price, or "[2]" for
// a problem with the row as a whole.
function useRowErrors(issues, rows) {
  const cache = useRef(new Map());
  if (issues.all.length === 0) {
    if (cache.current.size > 0) cache.current = new Map();
    return () => null;
  }
  // Every row asks for its issues, so FormError lists only what no row shows.
  const next = new Map();
  rows.forEach((row, index) => {
    const found = { row: issues.at(`[${index}]`), model: issues.at(`[${index}].model`) };
    for (const field of PRICE_FIELDS) found[field.key] = issues.at(`[${index}].${field.key}`);
    const key = JSON.stringify(found);
    if (key === '{}') return;
    const before = cache.current.get(row._id);
    next.set(row._id, before && before.key === key ? before : { key, found });
  });
  cache.current = next;
  return (id) => next.get(id)?.found ?? null;
}

export function PricingTab({ onDiskInvalid }) {
  const list = useListDraft('/pricing', { toDraft, toBody });
  const formId = useUid('pricing-form');
  const [test, setTest] = useState('');
  const [clientError, setClientError] = useState(null);
  const error = clientError ?? list.saveError;
  const issues = useFieldIssues(error);
  useDiskInvalid(list.saveError, onDiskInvalid);

  useUnsavedGuard(list.dirty, 'prices');
  useSaveHotkey(formId, list.dirty && !list.saving);
  useRevealProblem(formId, error);

  const rows = list.draft ?? [];
  const name = test.trim();
  const matches = useMemo(() => (name ? rows.flatMap((row, index) => (row.model.trim() && wildcardMatch(row.model.trim(), name) ? [index] : [])) : []), [rows, name]);
  const winner = matches.length > 0 ? matches[0] : -1;

  // Which rows can never win compares every pattern with every earlier one.
  // In a long list that is done once typing pauses, not on every keystroke;
  // until then a verdict is only shown for patterns that have not changed
  // since. (A pattern is one line of text, so a line break can separate them.)
  const patternKey = rows.map((row) => row.model.trim()).join('\n');
  const pausedKey = useDebounced(patternKey, 200);
  const judgedKey = rows.length <= SHADOW_AT_ONCE ? patternKey : pausedKey;
  const patterns = useMemo(() => patternKey.split('\n'), [patternKey]);
  const judged = useMemo(() => {
    const list = judgedKey.split('\n');
    return { list, shadows: shadowedBy(list) };
  }, [judgedKey]);
  const shadowOf = (index) => {
    const by = judged.shadows[index] ?? -1;
    if (by === -1 || judged.list.length !== patterns.length) return -1;
    return judged.list[index] === patterns[index] && judged.list[by] === patterns[by] ? by : -1;
  };

  const errorsOf = useRowErrors(issues, rows);

  // Handlers the rows call. The object keeps its identity (rows compare it);
  // its functions are replaced on every render so they see the current list.
  const actions = useRef({}).current;

  if (list.error) {
    return html`<${Panel} flush><${ErrorState} title="Could not load the prices" error=${list.error} onRetry=${list.refresh} /><//>`;
  }
  if (list.loading || !list.draft) {
    return html`<${Panel} title="Prices" aria-busy="true" aria-label="Loading prices"><${Skeleton} lines=${5} /><//>`;
  }

  const change = (next) => {
    if (clientError) setClientError(null);
    list.setDraft(next);
  };
  // Updates are functions of the current draft, so two edits in one tick both land.
  actions.setCell = (id, key, value) => change((current) => current.map((row) => (row._id === id ? { ...row, [key]: value } : row)));
  actions.move = (id, direction) => {
    change((current) => moveItem(current, current.findIndex((row) => row._id === id), direction));
    focusMoved(id, direction);
  };

  const add = () => {
    const row = toRow({});
    change((current) => [...current, row]);
    // The new row's first field takes the focus once it is on the page.
    requestAnimationFrame(() => document.getElementById(`price-model-${row._id}`)?.focus());
  };

  actions.remove = async (id) => {
    const index = rows.findIndex((row) => row._id === id);
    if (index === -1) return;
    const row = rows[index];
    const blank = !row.model.trim() && PRICE_FIELDS.every((field) => row[field.key] == null);
    const ok =
      blank ||
      (await confirm({
        danger: true,
        title: `Delete the price for ${row.model.trim() || `row ${index + 1}`}?`,
        message: 'Models it matched are then priced by the next matching row, or recorded without a cost. The row leaves the list now and stops applying when you save.',
        confirmLabel: 'Delete price',
      }));
    if (!ok) return;
    change((current) => current.filter((other) => other._id !== id));
    // The button that was pressed is gone with its row: the keyboard moves
    // to the row that took its place, else to the one before, else to the
    // Add button.
    const neighbour = rows[index + 1] ?? rows[index - 1];
    focusSoon(() => document.querySelector(neighbour ? `[data-row="${neighbour._id}"] [data-delete]` : `#${formId} [data-add]`));
  };

  const submit = async () => {
    const problems = [];
    rows.forEach((row, index) => {
      if (!row.model.trim()) problems.push({ path: `[${index}].model`, message: 'Enter a model pattern, for example gpt-5* or * for every model.' });
      for (const field of PRICE_FIELDS) {
        if (field.required && row[field.key] == null) problems.push({ path: `[${index}].${field.key}`, message: 'Enter a price. Use 0 for free.' });
      }
    });
    if (problems.length > 0) {
      list.clearSaveError();
      setClientError(new ApiError(0, 'Nothing was saved.', { issues: problems, code: 'invalid' }));
      return;
    }
    setClientError(null);
    const count = rows.length;
    if (await list.save()) {
      toast.success('Prices saved', { description: `${count === 0 ? 'No prices: costs are not estimated.' : `${plural(count, 'row')}.`} They apply to requests from now on; recorded costs do not change.` });
    }
  };

  return html`
    <${Form} id=${formId} class="settings-form" onSubmit=${submit}>
      <${ConflictNotice} list=${list} noun="prices" />
      ${list.stale && html`<${Notice} tone="caution" title="Could not refresh the prices">${sentence(list.stale.message)} The rows below are the last ones loaded.<//>`}

      <${Panel}
        flush
        title="Prices"
        description="USD per million tokens, used for the cost estimates. For each model the first row whose pattern matches wins, so put specific patterns above general ones. An empty cache price means the input price."
        actions=${rows.length > 0 ? html`<${Button} size="sm" icon="plus" data-add="" onClick=${add}>Add price<//>` : null}
      >
        ${rows.length === 0
          ? html`<${EmptyState}
              icon="usage"
              title="No prices yet"
              description="Without prices the dashboard shows token counts but no costs. Add a row for each model pattern you want costed."
              action=${html`<${Button} variant="primary" icon="plus" data-add="" onClick=${add}>Add price<//>`}
            />`
          : html`
              <div class="settings-test">
                <${Input} label="Test a model name" icon="search" mono value=${test} onChange=${setTest} onEnter=${(event) => event.preventDefault()} placeholder="gpt-5-mini" autocomplete="off" />
                <${TestResult} name=${name} rows=${rows} winner=${winner} matches=${matches} />
              </div>
              <div class="settings-prices-head" aria-hidden="true">
                <span>Order</span>
                <span>Model pattern</span>
                ${PRICE_FIELDS.map((field) => html`<span key=${field.key}>${field.label}</span>`)}
                <span></span>
              </div>
              <ol class="settings-prices">
                ${rows.map((row, index) => {
                  const found = errorsOf(row._id);
                  const isWinner = index === winner;
                  let note = '';
                  let noteTone = '';
                  if (isWinner) {
                    note = `Prices ${name}`;
                    noteTone = 'accent';
                  } else if (winner !== -1 && matches.includes(index)) {
                    note = `Matches ${name} too, but row ${winner + 1} wins`;
                  } else {
                    const by = shadowOf(index);
                    if (by !== -1) {
                      note = `Never used: row ${by + 1} (${patterns[by]}) matches every model this pattern does.`;
                      noteTone = 'caution';
                    }
                  }
                  return html`<${PriceRow} key=${row._id} row=${row} index=${index} last=${index === rows.length - 1} note=${note} noteTone=${noteTone} match=${isWinner} errors=${found} rowError=${found?.row} actions=${actions} />`;
                })}
              </ol>
            `}
      <//>

      <${SaveError} error=${error} issues=${issues} title="Could not save the prices" />
      <${SaveBar} dirty=${list.dirty} saving=${list.saving} what="prices" onDiscard=${() => { setClientError(null); list.discard(); }} summary="Unsaved changes to the prices" saveLabel="Save prices" />
    <//>
  `;
}
