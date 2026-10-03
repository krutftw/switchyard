// Models page, "Aliases" tab: the editor for virtual models
// (GET /aliases, PUT /aliases).
//
// The gateway stores aliases as one list and replaces it as a whole, so the
// editor works on a draft of the list: edit any number of aliases, then save
// once. The gateway validates the list; the issues of a refused save (422,
// with paths into the list that was sent: "[2].name", "[2].targets",
// "[2].targets[1]") are put back on the row and field they belong to.
// Deleting a saved alias is the one change that is applied at once, after a
// confirmation that names it.
//
// PUT /aliases has no version check, so both writes read the gateway's list
// once more just before they are sent: a save stops when the list is not the
// one this page showed (the conflict notice then says what changed), and a
// delete removes its one alias from the list as it is at that moment.
//
// The draft lives in a module-level store, so it survives a visit to another
// tab or page: nothing is lost by moving around the dashboard, and nobody is
// asked. Only closing or reloading the window would lose it, and that asks.

import { html, useCallback, useEffect, useMemo, useRef, useState } from '../../../vendor/preact-htm.js';
import {
  Badge,
  Button,
  EmptyState,
  ErrorState,
  Form,
  FormError,
  Icon,
  IconButton,
  Input,
  Notice,
  Panel,
  Skeleton,
  StatusLamp,
  Switch,
  confirm,
  toast,
  useIssues,
} from '../../components/index.js';
import { api } from '../../lib/api.js';
import { plural, sentence } from '../../lib/format.js';
import { useAsync } from '../../lib/hooks.js';
import { registerLeaveGuard } from '../../lib/router.js';
import { createStore, useStore } from '../../lib/store.js';
import Combobox from './combobox.js';
import { aliasWarnings, draftFromServer, draftToBody, lookup, newAlias, newTarget, parseSuffix, sameAliases, suffixChoices } from './logic.js';

// ---------------------------------------------------------------------------
// The draft
// ---------------------------------------------------------------------------

/** rows: the editor's rows, null until the saved list first arrives. base: the saved list they were made from. */
const draftStore = createStore({ rows: null, base: null });

/** Take over a saved list. Rows already in the editor keep their keys, so fields stay mounted and focus stays put. */
const adopt = (list) => draftStore.replace({ rows: draftFromServer(list, draftStore.get().rows), base: list });
const updateRows = (fn) => draftStore.set({ rows: fn(draftStore.get().rows ?? []) });
const patchRow = (key, patch) => updateRows((rows) => rows.map((row) => (row.key === key ? { ...row, ...(typeof patch === 'function' ? patch(row) : patch) } : row)));

const isDirty = ({ rows, base }) => !!rows && !!base && !sameAliases(draftToBody(rows), base);

// Closing or reloading the window with unsaved edits asks first: a leave
// guard of the router that lets every route change pass (the draft survives
// those) and only makes the browser ask on unload. It lives with the draft,
// not with the tab that shows it: the draft outlives the Aliases tab and the
// Models page, and so does the risk of losing it.
let unguard = null;
draftStore.subscribe((state) => {
  const dirty = isDirty(state);
  if (dirty === (unguard !== null) || typeof window === 'undefined') return;
  if (dirty) unguard = registerLeaveGuard(() => true, { unload: true });
  else {
    unguard();
    unguard = null;
  }
});

/**
 * The draft, kept in step with the saved list (`saved`, GET /aliases):
 * a draft without edits follows the gateway; a draft with edits is never
 * overwritten, and `conflict` says the gateway's list changed under it.
 * Called by the page, so the tab strip can show "unsaved" from any tab.
 */
export function useAliasDraft(saved) {
  const { rows, base } = useStore(draftStore);
  useEffect(() => {
    if (!saved) return;
    const state = draftStore.get();
    if (state.rows === null) adopt(saved);
    else if (!sameAliases(saved, state.base) && !isDirty(state)) adopt(saved);
  }, [saved]);
  const dirty = isDirty({ rows, base });
  const conflict = dirty && !!saved && !sameAliases(saved, base);
  return { rows, base, dirty, conflict };
}

/** "a", "a and b", "a, b and c". */
const listOf = (names) => (names.length <= 1 ? names.join('') : `${names.slice(0, -1).join(', ')} and ${names[names.length - 1]}`);

/** "x was added; y and z were removed": how the gateway's list differs from the one the draft started from. */
function describeChange(base, saved) {
  const key = (alias) => String(alias.name ?? '').trim();
  const before = new Map((base ?? []).map((a) => [key(a), a]));
  const after = new Map((saved ?? []).map((a) => [key(a), a]));
  const added = [...after.keys()].filter((name) => !before.has(name));
  const removed = [...before.keys()].filter((name) => !after.has(name));
  const changed = [...after.keys()].filter((name) => before.has(name) && !sameAliases([before.get(name)], [after.get(name)]));
  // "smart was changed", "a, b and 3 more were added"
  const part = (names, what) => {
    if (names.length === 0) return '';
    const shown = names.length > 5 ? [...names.slice(0, 4), `${names.length - 4} more`] : names;
    return `${listOf(shown)} ${names.length === 1 ? 'was' : 'were'} ${what}`;
  };
  return [part(added, 'added'), part(removed, 'removed'), part(changed, 'changed')].filter(Boolean).join('; ') || 'the aliases are in a different order';
}

/**
 * What deleting a saved alias does, as sentences for the confirmation. Only
 * what the loaded data supports is claimed: a target is said to stay
 * reachable only when it matches a model that routes, and nothing is said
 * about models while the model table is not loaded (`ready` false).
 *
 * alias   the saved alias ({ name, targets, hide_targets })
 * saved   the whole saved list, to see what other aliases hide
 */
function deleteConsequences(alias, { saved, rowsByName, realNames, ready }) {
  const name = alias.name.trim();
  if (!ready) return [`Requests for ${name} are no longer sent to its targets.`];
  const lower = name.toLowerCase();
  const targets = (alias.targets ?? []).map((t) => String(t).trim()).filter(Boolean);
  const own = rowsByName.get(name);
  // The gateway's own word for an alias none of whose targets routes (GET /models).
  const ignored = !!own?.isAlias && own.ignored;
  const matched = (target) => {
    const entry = lookup(rowsByName, parseSuffix(target).base) ?? lookup(rowsByName, target);
    return entry && entry.name.toLowerCase() !== lower && entry.routes.length > 0 ? entry.name : null;
  };
  const out = [];
  if (ignored) {
    out.push(`Nothing changes for clients: none of its targets matches a model, so the gateway already ignores ${name}.`);
  } else if (realNames.has(lower)) {
    out.push(
      targets.some((t) => parseSuffix(t).raw !== null && parseSuffix(t).base.trim().toLowerCase() === lower)
        ? `Requests for ${name} go straight to the provider's model of that name again, without the reasoning depth this alias sets.`
        : `Requests for ${name} go to the provider's model of that name again, which this alias has been hiding.`,
    );
  } else {
    out.push(`Requests for ${name} will fail as an unknown model from now on.`);
  }
  const reachable = [...new Set(targets.map(matched).filter(Boolean))];
  if (reachable.length > 0) {
    // Another alias with "hide targets" may keep a target out of the lists.
    const hiddenElsewhere = new Set(
      (saved ?? [])
        .filter((other) => other.name !== alias.name && other.hide_targets)
        .flatMap((other) => (other.targets ?? []).map(matched))
        .filter(Boolean),
    );
    const relisted = alias.hide_targets && !ignored && reachable.every((target) => !hiddenElsewhere.has(target));
    const one = reachable.length === 1;
    out.push(`${listOf(reachable)} ${one ? 'stays' : 'stay'} reachable under ${one ? 'its' : 'their'} own name${one ? '' : 's'}${relisted ? ` and ${one ? 'shows' : 'show'} up in client model lists again` : ''}.`);
  }
  return out;
}

// ---------------------------------------------------------------------------
// Pieces
// ---------------------------------------------------------------------------

const SUFFIX_HINT = { none: 'reasoning off', auto: 'provider picks the depth' };

function TargetRow({ row, target, index, count, options, more, error, warning, drop, dragging, handlers }) {
  const name = target.value.trim() || `target ${index + 1}`;
  return html`
    <li class="models-target" data-target=${target.id} data-drop=${drop?.id === target.id ? drop.pos : undefined} data-dragging=${dragging === target.id ? '' : undefined}>
      <span
        class="models-target-grip"
        draggable="true"
        title="Drag to reorder"
        aria-hidden="true"
        onDragStart=${(event) => handlers.dragStart(event, row.key, target)}
        onDragEnd=${handlers.dragEnd}
      >
        <${Icon} name="grip" />
      </span>
      <span class="models-target-order num" aria-hidden="true">${index + 1}</span>
      <div class="models-target-field">
        <${Combobox}
          value=${target.value}
          onChange=${(value) => handlers.setTarget(row.key, target.id, value)}
          options=${options}
          more=${more}
          error=${error}
          warning=${warning}
          label=${`Target ${index + 1} of ${row.name.trim() || 'the new alias'}`}
          placeholder=${index === 0 ? 'Model name, such as gpt-5 or gpt-5(high)' : 'Next model to try'}
          data-focus=${`target-${target.id}`}
        />
      </div>
      <span class="models-target-actions">
        <${IconButton} icon="arrow-up" size="sm" label=${`Move ${name} up`} disabled=${index === 0} data-focus=${`up-${target.id}`} onClick=${() => handlers.move(row.key, target.id, -1)} />
        <${IconButton} icon="arrow-down" size="sm" label=${`Move ${name} down`} disabled=${index === count - 1} data-focus=${`down-${target.id}`} onClick=${() => handlers.move(row.key, target.id, 1)} />
        <${IconButton} icon="x" size="sm" label=${`Remove ${name}`} disabled=${count === 1 && !target.value} onClick=${() => handlers.removeTarget(row.key, target.id)} />
      </span>
    </li>
  `;
}

function AliasBlock({ row, savedAlias, modelRow, modelsState, warnings, issues, baseOptions, more, drop, dragging, busy, handlers, onOpen }) {
  const lower = row.name.trim().toLowerCase();
  // An alias cannot target itself, so its own name is not offered.
  const options = useMemo(() => (lower ? baseOptions.filter((o) => o.value.toLowerCase() !== lower) : baseOptions), [baseOptions, lower]);
  const edited = savedAlias && !sameAliases(draftToBody([row]), [savedAlias]);
  const title = row.name.trim() || 'New alias';
  // Whether a saved alias routes is read from the model table; until that has loaded nothing is claimed.
  let state;
  if (!savedAlias) state = html`<${Badge} tone="info">Not saved yet<//>`;
  else if (modelsState === 'loading') state = html`<${Skeleton} class="models-alias-pending" /><span class="sr-only">Checking its routes</span>`;
  else if (modelsState === 'failed') state = html`<${StatusLamp} tone="off" label="Routes not known" />`;
  else if (modelRow) state = html`<${StatusLamp} tone=${modelRow.availability.tone} label=${modelRow.availability.label} detail=${modelRow.routes.length ? plural(modelRow.routes.length, 'route') : null} />`;
  else state = html`<${StatusLamp} tone="off" label="Not in the model table" />`;
  return html`
    <section class="models-alias" aria-label=${`Alias ${title}`} data-alias=${row.key} tabindex="-1">
      <div class="models-alias-head">
        <div class="models-alias-state">
          ${state}
          ${edited && html`<${Badge} outline>Edited<//>`}
        </div>
        <div class="models-alias-tools">
          ${savedAlias && modelRow && html`<${Button} variant="ghost" size="sm" onClick=${() => onOpen(modelRow.name)}>Show routes<//>`}
          <${Button} variant="danger-quiet" size="sm" icon="trash" disabled=${busy} onClick=${() => handlers.removeAlias(row)}>Delete alias<//>
        </div>
      </div>
      <div class="models-alias-body">
        <div class="models-alias-main">
          <${Input}
            label="Name"
            mono
            value=${row.name}
            onChange=${(value) => handlers.setName(row.key, value)}
            error=${issues?.name}
            warning=${warnings.name}
            placeholder="smart"
            hint="What clients put in the model field."
            autocomplete="off"
            data-focus=${`name-${row.key}`}
          />
          <${Switch}
            label="Hide targets"
            hint="Leave the targets' own names out of the model lists clients fetch. Requests that name them still work."
            checked=${row.hide_targets}
            onChange=${(checked) => handlers.setHide(row.key, checked)}
          />
        </div>
        <div class="models-alias-targets">
          <div class="field-label"><span>Targets</span><span class="field-optional">tried in this order</span></div>
          <ol
            class="models-target-list"
            onDragEnter=${(event) => handlers.dragOver(event, row.key)}
            onDragOver=${(event) => handlers.dragOver(event, row.key)}
            onDragLeave=${handlers.dragLeave}
            onDrop=${(event) => handlers.drop(event, row.key)}
          >
            ${row.targets.map(
              (target, index) => html`
                <${TargetRow}
                  key=${target.id}
                  row=${row}
                  target=${target}
                  index=${index}
                  count=${row.targets.length}
                  options=${options}
                  more=${more}
                  error=${issues?.targets.get(target.id)}
                  warning=${warnings.targets.get(target.id)}
                  drop=${drop}
                  dragging=${dragging}
                  handlers=${handlers}
                />
              `,
            )}
          </ol>
          ${issues?.targetList && html`<div class="field-error"><${Icon} name="alert-circle" size=${14} /><span>${issues.targetList}</span></div>`}
          <div>
            <${Button} size="sm" icon="plus" onClick=${() => handlers.addTarget(row.key)}>Add target<//>
          </div>
        </div>
      </div>
      ${issues?.other.length > 0 && html`<div class="field-error"><${Icon} name="alert-circle" size=${14} /><span>${issues.other.join(' ')}</span></div>`}
    </section>
  `;
}

// ---------------------------------------------------------------------------
// The tab
// ---------------------------------------------------------------------------

/**
 * saved        GET /aliases data (undefined while loading)
 * loading, error, onRetry   of that request
 * draft        from useAliasDraft(saved)
 * modelRows    table rows (logic.js buildRows) for suggestions and statuses
 * modelsState  "ready" | "loading" | "failed": whether the model table is
 *              there to check names against
 * rowsByName   Map(name -> row)
 * realNames    Set of lower-cased names providers serve (what an alias can shadow)
 * onFresh      (list) => void: the gateway's list was re-read and is this now
 * onSaved      (list) => void: the gateway now runs on this list
 * onOpen       (name) => void: show a model's drawer
 */
export default function AliasesTab({ saved, loading, error, onRetry, draft, modelRows, modelsState = 'ready', rowsByName, realNames, onFresh, onSaved, onOpen }) {
  const { rows, dirty, conflict } = draft;
  const sent = useRef([]); // what the last save sent: [{ key, targetIds }]
  const focusNext = useRef(null);
  const drag = useRef(null);
  const [drop, setDrop] = useState(null);
  const [dragging, setDragging] = useState(null);
  const [announce, setAnnounce] = useState('');
  const savedRef = useRef(saved);
  savedRef.current = saved;
  const ready = modelsState === 'ready';

  // PUT /aliases replaces the whole list and has no version check, so the
  // list is read once more right before writing. If it is not the one this
  // page last showed, somebody else changed it: nothing is written, and the
  // notice above the editor says what changed. Saving again from there is
  // the informed choice to replace it.
  const save = useAsync(async () => {
    const fresh = await api.get('/aliases');
    if (!sameAliases(fresh, savedRef.current)) return { changedElsewhere: fresh };
    const current = draftStore.get().rows ?? [];
    sent.current = current.map((row) => ({ key: row.key, targetIds: row.targets.filter((t) => t.value.trim()).map((t) => t.id) }));
    return { list: await api.put('/aliases', draftToBody(current)) };
  });
  const issues = useIssues(save.error);

  // The gateway addresses an issue by its place in the list that was sent
  // ("[2].name", "[2].targets", "[2].targets[1]"; useIssues reads them as
  // "2.name", "2.targets.1"). Rows may have moved or gone since, so the
  // messages are handed out by row key and target id. Only a refusal of the
  // list itself is mapped: the issues of a 409 are about the configuration
  // file on disk, not about this list, and stay in the form's error notice.
  const rowIssues = new Map();
  if (save.error && save.error.status !== 409) {
    sent.current.forEach((entry, i) => {
      const nameIssue = issues.at(`${i}.name`);
      const out = { name: nameIssue ? sentence(nameIssue) : undefined, targets: new Map(), targetList: undefined, other: [] };
      const list = [];
      for (const issue of issues.under(String(i))) {
        if (issue.path === `${i}.name`) continue;
        const message = sentence(issue.message);
        if (issue.path === `${i}.targets`) {
          // The list as a whole: it is empty.
          list.push(message);
        } else if (issue.path.startsWith(`${i}.targets.`)) {
          // One target, by its place among the targets that were sent (blank rows are not).
          const id = entry.targetIds[Number(issue.path.slice(`${i}.targets.`.length).split('.')[0])];
          if (id) out.targets.set(id, [out.targets.get(id), message].filter(Boolean).join(' '));
          else list.push(message);
        } else out.other.push(message);
      }
      out.targetList = list.join(' ') || undefined;
      rowIssues.set(entry.key, out);
    });
  }

  // Focus follows what the user just did (a new row, a moved row, a row that
  // is gone). Placing it once is enough: a confirm dialog that is closing
  // leaves a focus the page has placed where it is. But it has to be placed
  // by the render that shows the change. An effect still owed from the
  // render before it is run just ahead of the next one, with the old rows
  // on screen: it would take the request and focus something that is about
  // to go (the "Add alias" of a list whose last alias was just deleted).
  useEffect(() => {
    const selectors = focusNext.current;
    if (!selectors || rows !== draftStore.get().rows) return;
    focusNext.current = null;
    for (const selector of selectors) {
      const el = document.querySelector(selector);
      if (el && !el.disabled) {
        if (document.activeElement !== el) el.focus();
        break;
      }
    }
  });

  const baseOptions = useMemo(
    () =>
      (modelRows ?? []).map((row) => ({
        value: row.name,
        hint: row.isAlias ? 'Alias' : (row.info.display_name ?? 'Model'),
        tone: row.availability.tone,
        toneLabel: row.availability.label,
      })),
    [modelRows],
  );

  // For a model that reasons, offer its name with each depth it takes.
  const more = useCallback(
    (text) => {
      const typed = text.trim();
      if (!typed) return [];
      const open = typed.lastIndexOf('(');
      const suffixing = open > 0;
      const base = suffixing ? typed.slice(0, open) : typed;
      const partial = suffixing ? typed.slice(open + 1).replace(/\)$/, '').toLowerCase() : '';
      const entry = lookup(rowsByName, base);
      if (!entry?.info?.thinking) return [];
      return suffixChoices(entry.info.thinking)
        .filter((choice) => choice.startsWith(partial))
        .map((choice) => ({ value: `${entry.name}(${choice})`, hint: SUFFIX_HINT[choice] ?? (/^\d+$/.test(choice) ? 'token budget' : 'reasoning level') }))
        .filter((option) => option.value !== typed);
    },
    [rowsByName],
  );

  const moveTo = (rowKey, id, toIndex) => {
    let said = '';
    patchRow(rowKey, (row) => {
      const from = row.targets.findIndex((t) => t.id === id);
      if (from === -1 || toIndex < 0 || toIndex >= row.targets.length || toIndex === from) return {};
      const targets = [...row.targets];
      const [moved] = targets.splice(from, 1);
      targets.splice(toIndex, 0, moved);
      said = `${moved.value.trim() || 'Target'} is now number ${toIndex + 1} of ${targets.length}.`;
      return { targets };
    });
    if (said) setAnnounce(said);
  };

  const handlers = {
    setName: (key, name) => patchRow(key, { name }),
    setHide: (key, hide_targets) => patchRow(key, { hide_targets }),
    setTarget: (key, id, value) => patchRow(key, (row) => ({ targets: row.targets.map((t) => (t.id === id ? { ...t, value } : t)) })),
    addTarget: (key) => {
      const target = newTarget();
      patchRow(key, (row) => ({ targets: [...row.targets, target] }));
      focusNext.current = [`[data-focus="target-${target.id}"]`];
    },
    removeTarget: (key, id) => {
      patchRow(key, (row) => {
        const at = row.targets.findIndex((t) => t.id === id);
        const targets = row.targets.filter((t) => t.id !== id);
        // An alias always shows one target row to type into.
        if (targets.length === 0) targets.push(newTarget());
        const neighbour = targets[Math.min(at, targets.length - 1)];
        focusNext.current = [`[data-focus="target-${neighbour.id}"]`];
        return { targets };
      });
    },
    move: (key, id, delta) => {
      const row = (draftStore.get().rows ?? []).find((r) => r.key === key);
      const from = row?.targets.findIndex((t) => t.id === id) ?? -1;
      if (from === -1) return;
      moveTo(key, id, from + delta);
      // The pressed button may now be disabled (first or last row): then its opposite.
      focusNext.current = delta < 0 ? [`[data-focus="up-${id}"]`, `[data-focus="down-${id}"]`] : [`[data-focus="down-${id}"]`, `[data-focus="up-${id}"]`];
    },
    dragStart: (event, key, target) => {
      drag.current = { key, id: target.id };
      event.dataTransfer.effectAllowed = 'move';
      event.dataTransfer.setData('text/plain', target.value);
      const item = event.currentTarget.closest('li');
      if (item && event.dataTransfer.setDragImage) event.dataTransfer.setDragImage(item, 16, item.offsetHeight / 2);
      setDragging(target.id);
    },
    // The three below are on the list, not on its rows: the whole list takes
    // the drop, the gaps between rows included (the line that shows where
    // the target will land is drawn in a gap, and that is where a hand lets
    // go). Entering counts as much as moving over: a browser may ask only
    // once before the drop.
    dragOver: (event, key) => {
      // Targets are reordered within their alias, not moved between aliases.
      if (!drag.current || drag.current.key !== key) return;
      event.preventDefault();
      event.dataTransfer.dropEffect = 'move';
      // The row under the pointer; in a gap, the nearer of its two neighbours.
      let place = null;
      let nearest = Infinity;
      for (const item of event.currentTarget.children) {
        const rect = item.getBoundingClientRect();
        const middle = rect.top + rect.height / 2;
        const distance = Math.abs(event.clientY - middle);
        if (distance < nearest) {
          nearest = distance;
          place = { id: item.getAttribute('data-target'), pos: event.clientY < middle ? 'before' : 'after' };
        }
      }
      drag.current.place = place;
      setDrop((prev) => (prev && place && prev.id === place.id && prev.pos === place.pos ? prev : place));
    },
    dragLeave: (event) => {
      // Out of the list altogether (not from one row to the next): letting
      // go there moves nothing, so no line says otherwise.
      const rect = event.currentTarget.getBoundingClientRect();
      if (event.clientX >= rect.left && event.clientX <= rect.right && event.clientY >= rect.top && event.clientY <= rect.bottom) return;
      if (drag.current) drag.current.place = null;
      setDrop(null);
    },
    drop: (event, key) => {
      const from = drag.current;
      if (!from || from.key !== key) return;
      event.preventDefault();
      const place = from.place;
      drag.current = null;
      setDrop(null);
      setDragging(null);
      if (!place || place.id === from.id) return;
      const row = (draftStore.get().rows ?? []).find((r) => r.key === key);
      if (!row) return;
      const without = row.targets.filter((t) => t.id !== from.id);
      const at = without.findIndex((t) => t.id === place.id);
      if (at !== -1) moveTo(key, from.id, place.pos === 'before' ? at : at + 1);
    },
    dragEnd: () => {
      drag.current = null;
      setDrop(null);
      setDragging(null);
    },
    removeAlias: async (row) => {
      const savedAlias = row.origin != null ? (saved ?? []).find((a) => a.name === row.origin) : null;
      // The button that was pressed goes with the row: focus moves to the
      // alias that takes its place, else the one before, else "Add alias".
      const all = draftStore.get().rows ?? [];
      const at = all.findIndex((r) => r.key === row.key);
      const focusAfter = [all[at + 1], all[at - 1]].filter(Boolean).map((r) => `[data-alias="${r.key}"]`).concat('[data-focus="add-alias"]');
      if (!savedAlias) {
        // Never saved: nothing on the gateway to delete.
        const index = (draftStore.get().rows ?? []).findIndex((r) => r.key === row.key);
        focusNext.current = focusAfter;
        updateRows((list) => list.filter((r) => r.key !== row.key));
        if (row.name.trim() || row.targets.some((t) => t.value.trim())) {
          toast.info('Unsaved alias removed', {
            action: {
              label: 'Undo',
              onClick: () =>
                updateRows((list) => {
                  const next = [...list];
                  next.splice(Math.min(index, next.length), 0, row);
                  return next;
                }),
            },
          });
        }
        return;
      }
      const others = dirty && !sameAliases(draftToBody((rows ?? []).filter((r) => r.key !== row.key)), (draft.base ?? []).filter((a) => a.name !== row.origin));
      let result = null;
      const ok = await confirm({
        danger: true,
        title: `Delete alias ${savedAlias.name.trim()}?`,
        message: [...deleteConsequences(savedAlias, { saved, rowsByName, realNames, ready }), others ? 'Your other unsaved changes stay in the editor and are not saved by this.' : ''].filter(Boolean).join(' '),
        confirmLabel: 'Delete alias',
        // Built from the list as it is on the gateway at this moment, so only
        // this alias goes: what someone else saved meanwhile is left alone.
        action: async () => {
          const fresh = (await api.get('/aliases')) ?? [];
          const without = fresh.filter((a) => a.name !== savedAlias.name);
          result = without.length === fresh.length ? fresh : ((await api.put('/aliases', without)) ?? without);
        },
      });
      if (!ok) return;
      // The draft loses the row and its starting point loses the alias; any
      // other difference between that and the gateway's list is someone
      // else's change, which useAliasDraft takes over or reports as usual.
      const state = draftStore.get();
      focusNext.current = focusAfter;
      draftStore.replace({ rows: (state.rows ?? []).filter((r) => r.key !== row.key), base: (state.base ?? []).filter((a) => a.name !== savedAlias.name) });
      onSaved(result);
      toast.success(`Alias ${savedAlias.name.trim()} deleted`);
    },
  };

  const addAlias = () => {
    const row = newAlias();
    updateRows((list) => [...list, row]);
    focusNext.current = [`[data-focus="name-${row.key}"]`];
  };

  const submit = async () => {
    // Enter in a field submits the form; with nothing to save there is nothing to send.
    if (!dirty || save.loading) return;
    const result = await save.run();
    if (!result) return;
    if (result.changedElsewhere) {
      // Not written. The page takes the gateway's list as the saved one, which raises the notice above the editor.
      onFresh?.(result.changedElsewhere);
      toast.warning('Aliases not saved', { description: 'The list changed on the gateway while you were editing. See the notice above the aliases.' });
      return;
    }
    const list = Array.isArray(result.list) ? result.list : draftToBody(draftStore.get().rows ?? []);
    // The Save button is disabled once nothing is unsaved, and a disabled
    // button cannot hold the focus: when it was pressed (not when Enter in a
    // field saved), focus goes to the line that now says "all saved".
    // (After the last alias went there is no such line: then "Add alias".)
    if (document.activeElement?.closest?.('.models-savebar-actions')) focusNext.current = ['[data-focus="savebar"]', '[data-focus="add-alias"]'];
    adopt(list);
    onSaved(list);
    toast.success(list.length === 0 ? 'Aliases cleared' : 'Aliases saved');
  };

  const discard = async () => {
    const ok = await confirm({
      danger: true,
      title: 'Discard unsaved alias changes?',
      message: 'The editor goes back to the list the gateway is using. What you typed is lost.',
      confirmLabel: 'Discard changes',
    });
    if (!ok) return;
    save.reset();
    // The button that was pressed is disabled (or gone) once nothing is unsaved.
    focusNext.current = ['[data-focus="add-alias"]'];
    adopt(savedRef.current ?? draftStore.get().base ?? []);
  };

  // ---- States -------------------------------------------------------------

  if (!rows) {
    if (error && !loading) {
      return html`<${Panel} flush><${ErrorState} title="Could not load aliases" error=${error} onRetry=${onRetry} /><//>`;
    }
    return html`
      <${Panel} title="Aliases">
        <div class="stack" aria-busy="true">
          <${Skeleton} width="30%" height="20px" />
          <${Skeleton} lines=${3} />
          <${Skeleton} width="45%" height="20px" />
          <${Skeleton} lines=${2} />
        </div>
      <//>
    `;
  }

  const busy = save.loading;
  const savedByName = new Map((draft.base ?? []).map((a) => [a.name, a]));
  const changed = rows.filter((row) => row.origin == null || !savedByName.has(row.origin) || !sameAliases(draftToBody([row]), [savedByName.get(row.origin)])).length;
  const removed = (draft.base ?? []).filter((a) => !rows.some((row) => row.origin === a.name)).length;

  return html`
    <${Form} class="models-aliases" data-dirty=${dirty ? '' : undefined} onSubmit=${submit}>
      ${conflict &&
      html`
        <${Notice}
          tone="caution"
          title="The alias list changed on the gateway while you were editing"
          action=${html`<${Button} size="sm" onClick=${discard}>Load the gateway's list<//>`}
        >
          It was saved from somewhere else, or the configuration was reloaded. On the gateway now: ${describeChange(draft.base, saved)}. Saving replaces the gateway's list with the one on this page; loading the gateway's list drops your unsaved changes.
        <//>
      `}
      ${error &&
      html`<${Notice} tone="caution" title="Could not refresh aliases" action=${html`<${Button} size="sm" icon="refresh" onClick=${onRetry}>Try again<//>`}>${error.message} The editor shows the list as it was last loaded.<//>`}
      <${Panel}
        title="Aliases"
        description="A name of your own that routes to other models. Targets are tried in order, so one alias can fail over between providers."
        flush
        actions=${rows.length > 0 ? html`<${Button} icon="plus" disabled=${busy} data-focus="add-alias" onClick=${addAlias}>Add alias<//>` : null}
        footer=${rows.length > 0 || dirty
          ? html`
              <span class="models-savebar-text" role="status" tabindex="-1" data-focus="savebar">
                ${dirty
                  ? [changed > 0 ? `${plural(changed, 'alias', 'aliases')} changed` : '', removed > 0 ? `${removed} removed` : ''].filter(Boolean).join(', ') || 'Unsaved changes'
                  : `${plural(rows.length, 'alias', 'aliases')}, all saved`}
              </span>
              <span class="models-savebar-actions">
                <${Button} disabled=${!dirty || busy} onClick=${discard}>Discard changes<//>
                <${Button} type="submit" variant="primary" loading=${busy} disabled=${!dirty}>Save aliases<//>
              </span>
            `
          : null}
      >
        ${rows.length === 0
          ? html`
              <${EmptyState}
                icon="junction"
                title="No aliases yet"
                description="An alias gives clients a stable name, such as smart or fast, and routes it to the models you choose. Change the targets later and no client has to change."
                action=${html`<${Button} variant="primary" icon="plus" data-focus="add-alias" onClick=${addAlias}>Add alias<//>`}
              />
            `
          : html`
              <div class="models-alias-list">
                ${rows.map((row) => {
                  const savedAlias = row.origin != null ? savedByName.get(row.origin) : null;
                  // The model table lists an alias under its trimmed name.
                  const modelRow = savedAlias ? rowsByName.get(savedAlias.name.trim()) : null;
                  return html`
                    <${AliasBlock}
                      key=${row.key}
                      row=${row}
                      savedAlias=${savedAlias}
                      modelRow=${modelRow?.isAlias ? modelRow : null}
                      modelsState=${modelsState}
                      warnings=${aliasWarnings(row, rows, rowsByName, realNames, { ready })}
                      issues=${rowIssues.get(row.key)}
                      baseOptions=${baseOptions}
                      more=${more}
                      drop=${drop}
                      dragging=${dragging}
                      busy=${busy}
                      handlers=${handlers}
                      onOpen=${onOpen}
                    />
                  `;
                })}
              </div>
            `}
        ${save.error && html`<div class="models-aliases-error"><${FormError} error=${save.error} issues=${issues} title="Could not save aliases" /></div>`}
      <//>
      <span class="sr-only" role="status">${announce}</span>
    <//>
  `;
}
