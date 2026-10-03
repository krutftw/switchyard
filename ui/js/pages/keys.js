// API keys (#/keys): the keys client applications present to the gateway.
//
// The list (GET /keys) with search, a status filter, sorting and paging; an
// enabled switch per row; a modal that creates a key and shows it in full
// once; a drawer per key (?open=<id>) to edit, reveal and delete it.
// Sub-modules live in pages/keys/.
//
// URL state: ?open=<key id> the drawer, ?q= the search, ?show=enabled|disabled
// the status filter, ?sort=<column>:<asc|desc>, ?page=. #/keys?new=1 opens
// the create dialog.

import { html, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from '../../vendor/preact-htm.js';
import {
  Badge,
  Button,
  EmptyState,
  ErrorState,
  IconButton,
  Input,
  Menu,
  Notice,
  Page,
  Pagination,
  Panel,
  Segmented,
  Switch,
  Table,
  sortRows,
  toast,
} from '../components/index.js';
import { api } from '../lib/api.js';
import { useCommands } from '../lib/commands.js';
import { copyText, loadStyles } from '../lib/dom.js';
import { DASH, formatCurrency, formatDateTime, formatNumber, formatPercent, formatRelativeTime, formatTime, formatTokens, plural, sentence } from '../lib/format.js';
import { useIsPhone, useNow, useResource, useSize } from '../lib/hooks.js';
import { liveState, useLive, useLiveGap } from '../lib/live.js';
import { href, mayLeave, routeStore, setQuery, useQueryParam } from '../lib/router.js';
import { useStore } from '../lib/store.js';
import { CreateKeyModal } from './keys/create.js';
import { KeyDrawer, deleteKey, refusalText } from './keys/edit.js';
import { PatternSummary } from './keys/parts.js';
import { servedNames } from './keys/util.js';

await loadStyles('pages/keys.css');

const PAGE_SIZE = 50;
const DEFAULT_SORT = 'name:asc';
const SHOW = [
  { value: 'all', label: 'All' },
  { value: 'enabled', label: 'Enabled' },
  { value: 'disabled', label: 'Disabled' },
];

// The table never scrolls sideways: when its columns do not fit the panel it
// moves to the next, narrower set. What a set leaves out is under another
// column (failed requests under Requests, tokens under Cost, the rate limit
// under Models) or, in the last one, in the key's drawer.
const TIERS = ['full', 'compact', 'narrow', 'tight'];
const TIER_COLUMNS = {
  full: ['name', 'enabled', 'models', 'rate_limit_rpm', 'requests', 'errors', 'tokens', 'cost', 'last_used_at', 'actions'],
  compact: ['name', 'enabled', 'models', 'rate_limit_rpm', 'requests', 'cost', 'last_used_at', 'actions'],
  narrow: ['name', 'enabled', 'limits', 'requests', 'cost', 'last_used_at', 'actions'],
  tight: ['name', 'enabled', 'requests', 'last_used_at', 'actions'],
};

/** The column set (index into TIERS) the table last settled on. */
let lastTier = 0;

/** One empty list for "not loaded yet", so memos that depend on the list hold still. */
const NONE = [];

/** Set on the history entry a drawer was opened into, so closing can step back. */
const PUSHED = 'syKeysDrawer';

/** Does the search text occur in the key's name, masked key, id or patterns? */
function matchesSearch(entry, needle) {
  if (!needle) return true;
  const hay = [entry.name, entry.masked, entry.id, ...(entry.models ?? [])];
  return hay.some((text) => String(text ?? '').toLowerCase().includes(needle));
}

function NameCell({ entry }) {
  const { name } = entry;
  return html`
    <div class="keys-name">
      <span class="keys-name-text" data-off=${entry.enabled ? undefined : ''} title=${name}>${name || 'Unnamed key'}</span>
      <span class="keys-name-sub">
        <span class="keys-masked" title=${entry.is_reference ? 'A reference to an environment variable of the gateway' : 'The key, masked'}>${entry.masked}</span>
        ${entry.is_reference && entry.resolved === false && html`<${Badge} tone="caution">Variable not set<//>`}
      </span>
    </div>
  `;
}

function RateLimit({ rpm }) {
  if (rpm == null) return html`<span class="faint keys-plain">None</span>`;
  return html`<span>${formatNumber(rpm)}<span class="keys-unit">rpm</span></span>`;
}

const failedShare = (requests, errors) => (requests > 0 && errors > 0 ? `${formatPercent(errors / requests)} of this key's requests failed` : undefined);

/**
 * Which column set fits. Starts from the widest and steps down while the
 * table is wider than its box. It steps back up when the box has grown to
 * what the wider set was seen to need, and tries the wider set once whenever
 * the rows change (other data, other widths). Measured, not guessed from the
 * window: the room depends on the sidebar, and the need on the data. All of
 * it happens in layout effects, before the browser paints.
 *
 * rows   the rows on screen; the wider set is tried again when they change
 * off    stop measuring (phones: cards, nothing to fit)
 * ready  false while the rows are placeholders: then it only steps down
 *
 * Returns [callback ref for the box around the table, that box as a ref
 * object, tier name].
 */
function useColumnFit(rows, { off, ready }) {
  const box = useRef(null);
  const [sizeRef, size] = useSize();
  // Coming back to the page starts from the set that fitted last time, so
  // the loading rows already have the columns the loaded ones will have.
  const [tier, setTier] = useState(lastTier);
  lastTier = tier;
  // Widths change once the fonts are in: measure again then.
  const [fonts, setFonts] = useState(() => typeof document === 'undefined' || document.fonts?.status !== 'loading');
  const need = useRef([]);
  const tried = useRef(null);
  const ref = useCallback(
    (el) => {
      box.current = el;
      sizeRef(el);
    },
    [sizeRef],
  );

  useEffect(() => {
    if (fonts) return undefined;
    let alive = true;
    document.fonts.ready.then(() => alive && setFonts(true));
    return () => {
      alive = false;
    };
  }, [fonts]);

  // What the rows and the fonts were when the wider set was last tried.
  // `rows` must keep its identity between renders that change nothing.
  const contents = useMemo(() => ({}), [rows, fonts]);

  // A fitting that does not settle must never hold up the page: after a
  // dozen steps within a second it stays where it is until the next second.
  const churn = useRef({ since: 0, steps: 0 });
  const step = (next) => {
    const at = Date.now();
    if (at - churn.current.since > 1000) churn.current = { since: at, steps: 0 };
    churn.current.steps += 1;
    if (churn.current.steps <= 12) setTier(next);
  };

  useLayoutEffect(() => {
    if (off) return;
    const wrap = box.current?.querySelector('.table-wrap');
    if (!wrap || wrap.clientWidth === 0) return;
    const have = wrap.clientWidth;
    const want = wrap.scrollWidth;
    if (want > have + 1) {
      need.current[tier] = want;
      if (tier < TIERS.length - 1) step(tier + 1);
      return;
    }
    // Placeholder rows say nothing about what the real ones need.
    if (!ready) return;
    const fresh = tried.current !== contents;
    tried.current = contents;
    if (tier > 0 && (fresh || (need.current[tier - 1] != null && have >= need.current[tier - 1]))) step(tier - 1);
  }, [off, ready, tier, size.width, contents]);

  return [ref, box, TIERS[tier]];
}

export default function Keys() {
  const liveOpen = useStore(liveState, (state) => state.status === 'open');
  // The live connection says when something changed; without it, poll.
  const keys = useResource('/keys', { pollMs: liveOpen ? 60_000 : 15_000 });
  const status = useResource('/status', { pollMs: liveOpen ? 60_000 : 20_000 });
  const models = useResource('/models', { pollMs: 120_000 });
  const now = useNow(5000);
  const isPhone = useIsPhone();

  const [open] = useQueryParam('open', '');
  const [q] = useQueryParam('q', '');
  const [showParam] = useQueryParam('show', 'all');
  const [sortParam] = useQueryParam('sort', DEFAULT_SORT);
  const [pageParam] = useQueryParam('page', '1');
  const [newParam] = useQueryParam('new', '');
  // Anything but the two filters means all, in the control and in the list alike.
  const show = SHOW.some((option) => option.value === showParam) ? showParam : 'all';

  const [creating, setCreating] = useState(false);
  const [toggling, setToggling] = useState(() => new Set());
  const [fresh, setFresh] = useState(() => new Set());
  const [reloading, setReloading] = useState(false);

  const authRequired = status.data ? status.data.auth_required !== false : true;
  const listen = status.data?.listen ?? null;
  const tls = status.data?.tls === true;

  // ---- keeping fresh ------------------------------------------------------

  const refreshAll = () => Promise.all([keys.refresh(), status.refresh(), models.refresh()]);

  useLive('config.reloaded', () => {
    refreshAll();
  });

  // Usage moves with every finished request: one refetch per burst.
  const usageTimer = useRef(null);
  useLive('request.finished', () => {
    if (usageTimer.current) return;
    usageTimer.current = setTimeout(() => {
      usageTimer.current = null;
      keys.refresh();
    }, 2500);
  });
  useEffect(() => () => clearTimeout(usageTimer.current), []);

  // Frames sent while the connection was down, or dropped for a connection
  // that fell behind, are gone: refetch.
  useLiveGap(() => {
    refreshAll();
  });

  // #/keys?new=1 (a link, the command palette) opens the dialog once.
  useEffect(() => {
    if (!newParam) return;
    setCreating(true);
    setQuery({ new: null });
  }, [newParam]);

  const freshTimer = useRef(null);
  useEffect(() => () => clearTimeout(freshTimer.current), []);

  // ---- the list -----------------------------------------------------------

  const all = keys.data ?? NONE;
  const modelNames = useMemo(() => (models.data ? servedNames(models.data) : null), [models.data]);

  const needle = q.trim().toLowerCase();
  const filtered = useMemo(
    () => all.filter((entry) => (show === 'all' || (show === 'enabled') === !!entry.enabled) && matchesSearch(entry, needle)),
    [all, show, needle],
  );

  // ---- actions ------------------------------------------------------------

  // Opening from the list adds a history entry, so Back closes the drawer.
  // Closing it by hand steps back to the entry it was opened from instead of
  // leaving a second copy of the list in the history. A drawer reached by a
  // link has no such entry behind it: there the address is rewritten.
  const openKey = async (id) => {
    if (routeStore.get().query.open === id) return;
    // Unsaved edits in the drawer that is open now: ask first.
    if (!(await mayLeave())) return;
    if (routeStore.get().query.open) {
      setQuery({ open: id });
      return;
    }
    setQuery({ open: id }, { replace: false });
    const state = history.state && typeof history.state === 'object' ? history.state : {};
    history.replaceState({ ...state, [PUSHED]: true }, '');
  };
  const closeKey = () => {
    const id = routeStore.get().query.open;
    if (!id) return;
    if (!history.state?.[PUSHED]) {
      setQuery({ open: null });
      return;
    }
    // Should the step back not happen, the drawer still closes: take the
    // mark off this entry and rewrite its address.
    const rewrite = () => {
      const route = routeStore.get();
      if (route.path !== '/keys' || route.query.open !== id) return;
      const { [PUSHED]: _mark, ...state } = history.state ?? {};
      history.replaceState(state, '');
      setQuery({ open: null });
    };
    // Through the Navigation API where there is one: Chromium can ignore a
    // script's history.back() over an entry it counts as skippable.
    if (window.navigation?.canGoBack) {
      const step = window.navigation.back();
      step.committed.catch(rewrite);
      step.finished.catch(() => {});
    } else {
      history.back();
    }
    setTimeout(rewrite, 400);
  };

  const replaceEntry = (id, change) => keys.mutate((list) => list?.map((entry) => (entry.id === id ? change(entry) : entry)));

  const toggle = async (entry, enabled) => {
    if (toggling.has(entry.id)) return;
    const label = entry.name || entry.id;
    setToggling((set) => new Set(set).add(entry.id));
    // The switch moves at once; the gateway's answer confirms or reverts it.
    replaceEntry(entry.id, (current) => ({ ...current, enabled }));
    try {
      const updated = await api.patch(`/keys/${entry.id}`, { enabled });
      if (updated) replaceEntry(entry.id, () => updated);
      if (enabled) toast.success(`Key ${label} enabled`);
      else {
        toast.success(`Key ${label} disabled`, {
          description: refusalText(authRequired),
          action: { label: 'Undo', onClick: () => toggle({ ...entry, enabled: false }, true) },
        });
      }
    } catch (error) {
      replaceEntry(entry.id, (current) => ({ ...current, enabled: entry.enabled }));
      if (!error.aborted) toast.error(`Could not ${enabled ? 'enable' : 'disable'} ${label}`, { description: sentence(error.message) });
      if (error.status === 404) keys.refresh();
    } finally {
      setToggling((set) => {
        const next = new Set(set);
        next.delete(entry.id);
        return next;
      });
    }
  };

  const copyKey = async (entry) => {
    const what = entry.is_reference ? 'Reference' : 'Key';
    try {
      // The full key passes through this function and the clipboard only.
      const value = entry.is_reference ? entry.masked : (await api.post(`/keys/${entry.id}/reveal`)).key;
      if (!(await copyText(value))) throw new Error('The browser did not allow access to the clipboard.');
      toast.success(`${what} copied`, { description: entry.is_reference ? undefined : `The full key of ${entry.name || entry.id} is on the clipboard.` });
    } catch (error) {
      if (!error.aborted) toast.error(`Could not copy the ${what.toLowerCase()}`, { description: sentence(error.message) });
    }
  };

  // The rows on screen, for finding the neighbour of a deleted one.
  const shownRows = useRef([]);

  // The row that had the focus (or its menu button, or the drawer opened from
  // it) is gone with the key. Put the focus on the row that took its place,
  // so a keyboard user carries on from there and not from the top of the page.
  // Once, as soon as the list has rendered without the row: a closing dialog
  // or drawer leaves a focus the page has placed where it is.
  const focusAfterRemoval = (index) => {
    setTimeout(() => {
      const active = document.activeElement;
      const lost = !active || active === document.body || !document.contains(active) || active.closest('[data-state="closed"]');
      if (!lost) return;
      const rowEls = fitBox.current?.querySelectorAll('.table tbody tr[data-clickable]') ?? [];
      const target = rowEls[Math.min(Math.max(index, 0), rowEls.length - 1)] ?? document.querySelector('.keys-search input, .keys-create');
      target?.focus({ preventScroll: false });
    }, 0);
  };

  const forget = (id) => {
    const index = shownRows.current.findIndex((entry) => entry.id === id);
    keys.mutate((list) => list?.filter((entry) => entry.id !== id));
    if (routeStore.get().query.open === id) closeKey();
    status.refresh();
    focusAfterRemoval(index);
  };

  const remove = async (entry) => {
    if (await deleteKey(entry, authRequired)) forget(entry.id);
  };

  const onCreated = (result) => {
    keys.refresh();
    status.refresh();
    setFresh(new Set([result.id]));
    clearTimeout(freshTimer.current);
    freshTimer.current = setTimeout(() => setFresh(new Set()), 2400);
  };

  // ---- columns ------------------------------------------------------------

  // Every column there is. `stacked` (any set but the widest) puts failed
  // requests under Requests and tokens under Cost; `short` (the two narrowest)
  // heads the switch column "On", which is as wide as the switch.
  const columnsFor = (stacked, short = false) => [
    {
      key: 'name',
      header: 'Key',
      primary: true,
      sortable: true,
      sortValue: (entry) => entry.name || entry.id,
      render: (entry) => html`<${NameCell} entry=${entry} />`,
    },
    {
      key: 'enabled',
      header: short ? 'On' : 'Status',
      sortable: true,
      sortValue: (entry) => (entry.enabled ? 0 : 1),
      render: (entry) => html`
        <span class="keys-switch">
          <${Switch}
            checked=${entry.enabled}
            disabled=${toggling.has(entry.id)}
            aria-label=${`${entry.name || entry.id}: enabled`}
            onChange=${(on) => toggle(entry, on)}
          />
          <span class="keys-switch-text" aria-hidden="true">${entry.enabled ? 'Enabled' : 'Disabled'}</span>
        </span>
      `,
    },
    {
      key: 'models',
      header: 'Models',
      render: (entry) => html`<${PatternSummary} patterns=${entry.models} names=${modelNames} />`,
    },
    {
      key: 'limits',
      header: 'Limits',
      render: (entry) => html`
        <span class="keys-stack" data-align="start">
          <${PatternSummary} patterns=${entry.models} names=${modelNames} />
          <span class="keys-sub">${entry.rate_limit_rpm == null ? 'No rate limit' : html`<${RateLimit} rpm=${entry.rate_limit_rpm} />`}</span>
        </span>
      `,
    },
    {
      key: 'rate_limit_rpm',
      header: 'Rate limit',
      align: 'right',
      num: true,
      sortable: true,
      render: (entry) => html`<${RateLimit} rpm=${entry.rate_limit_rpm} />`,
    },
    {
      key: 'requests',
      header: 'Requests',
      align: 'right',
      num: true,
      sortable: true,
      sortValue: (entry) => entry.usage?.requests,
      render: (entry) => {
        const { requests, errors } = entry.usage ?? {};
        return html`
          <span class="keys-stack">
            <span class=${requests ? undefined : 'faint'} title=${`${plural(requests ?? 0, 'request')} in the last 30 days`}>${formatTokens(requests)}</span>
            ${stacked && errors > 0 && html`<span class="keys-sub keys-errors" title=${failedShare(requests, errors)}>${formatTokens(errors)} failed</span>`}
          </span>
        `;
      },
    },
    {
      key: 'errors',
      header: 'Errors',
      align: 'right',
      num: true,
      sortable: true,
      sortValue: (entry) => entry.usage?.errors,
      render: (entry) => {
        const { errors, requests } = entry.usage ?? {};
        if (!errors) return html`<span class="faint">${formatNumber(errors)}</span>`;
        return html`<span class="keys-errors" title=${failedShare(requests, errors)}>${formatTokens(errors)}</span>`;
      },
    },
    {
      key: 'tokens',
      header: 'Tokens',
      align: 'right',
      num: true,
      sortable: true,
      sortValue: (entry) => entry.usage?.tokens,
      render: (entry) => html`<span class=${entry.usage?.tokens ? undefined : 'faint'} title=${`${plural(entry.usage?.tokens ?? 0, 'token')}, prompt and output`}>${formatTokens(entry.usage?.tokens)}</span>`,
    },
    {
      key: 'cost',
      header: 'Cost',
      align: 'right',
      num: true,
      sortable: true,
      sortValue: (entry) => entry.usage?.cost,
      render: (entry) => {
        const { cost, tokens } = entry.usage ?? {};
        return html`
          <span class="keys-stack">
            <span class=${cost ? undefined : 'faint'}>${formatCurrency(cost)}</span>
            ${stacked && tokens > 0 && html`<span class="keys-sub" title=${`${plural(tokens, 'token')}, prompt and output`}>${formatTokens(tokens)} tokens</span>`}
          </span>
        `;
      },
    },
    {
      key: 'last_used_at',
      header: 'Last used',
      align: 'right',
      sortable: true,
      sortValue: (entry) => entry.usage?.last_used_at,
      render: (entry) => {
        const at = entry.usage?.last_used_at;
        return at ? html`<span class="keys-when" title=${formatDateTime(at)}>${formatRelativeTime(at, now)}</span>` : html`<span class="faint" title="No request with this key on record in the last 30 days">${DASH}</span>`;
      },
    },
    {
      key: 'actions',
      header: html`<span class="sr-only">Actions</span>`,
      label: 'Actions',
      hideOnPhone: true,
      align: 'right',
      render: (entry) => html`
        <${Menu}
          label=${`Actions for ${entry.name || entry.id}`}
          size="sm"
          items=${[
            { label: 'Edit', icon: 'edit', onSelect: () => openKey(entry.id) },
            { label: entry.is_reference ? 'Copy reference' : 'Copy key', icon: 'copy', onSelect: () => copyKey(entry) },
            { label: entry.enabled ? 'Disable' : 'Enable', icon: entry.enabled ? 'pause' : 'play', disabled: toggling.has(entry.id), onSelect: () => toggle(entry, !entry.enabled) },
            { separator: true },
            { label: 'Delete key', icon: 'trash', danger: true, onSelect: () => remove(entry) },
          ]}
        />
      `,
    },
  ];

  // ---- sort, page, fit ----------------------------------------------------

  // The sort is looked up among all columns, not only the ones on screen: a
  // link sorted by errors stays sorted by errors in a narrow window.
  const [sortKey, sortDir] = sortParam.split(':');
  const sortable = useMemo(() => columnsFor(false).filter((column) => column.sortable), []);
  const sortColumn = sortable.find((column) => column.key === sortKey) ?? sortable[0];
  const sort = { key: sortColumn.key, dir: sortDir === 'desc' ? 'desc' : 'asc' };
  const sorted = useMemo(() => sortRows(filtered, sortColumn, sort.dir), [filtered, sortColumn.key, sort.dir]);

  const pageCount = Math.max(1, Math.ceil(sorted.length / PAGE_SIZE));
  const page = Math.min(Math.max(1, Number.parseInt(pageParam, 10) || 1), pageCount);
  const rows = useMemo(() => (sorted.length > PAGE_SIZE ? sorted.slice((page - 1) * PAGE_SIZE, page * PAGE_SIZE) : sorted), [sorted, page]);
  shownRows.current = rows;

  // Phones show cards, which have room for the stacked cells and need no fitting.
  const [fitRef, fitBox, fitted] = useColumnFit(rows, { off: isPhone, ready: keys.data != null });
  const tier = isPhone ? 'compact' : fitted;

  const columns = useMemo(
    () => {
      const defs = columnsFor(tier !== 'full', tier === 'narrow' || tier === 'tight');
      return TIER_COLUMNS[tier].map((key) => defs.find((column) => column.key === key));
    },
    // `toggle`, `remove` and friends read the latest state through the resources.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [toggling, modelNames, now, authRequired, tier],
  );
  const sortShown = columns.some((column) => column.key === sort.key);

  const filtering = needle !== '' || show !== 'all';
  // Some buttons go away with what they undo: "Clear filters" with the empty
  // list, "Try again" with the error, "Sort by name" with its note. The
  // keyboard carries on from what took their place, the list (or the header
  // it is now sorted by), not from the top of the page.
  const focusList = () => {
    setTimeout(() => {
      const active = document.activeElement;
      if (active && active !== document.body && document.contains(active)) return;
      (fitBox.current?.querySelector('.table tbody tr[data-clickable]') ?? document.querySelector('.keys-create'))?.focus();
    }, 0);
  };
  const clearFilters = () => {
    setQuery({ q: null, show: null, page: null });
    focusList();
  };
  const sortByName = () => {
    setQuery({ sort: null, page: null });
    fitBox.current?.querySelector('.th-sort')?.focus();
  };
  const retry = async () => {
    await keys.refresh();
    focusList();
  };

  useCommands(
    () => [
      { id: 'keys:create', label: 'Create client key', group: 'API keys', icon: 'plus', keywords: 'new add api key', run: () => setCreating(true) },
      ...all.slice(0, 100).map((entry) => ({
        id: `keys:${entry.id}`,
        label: entry.name || entry.id,
        group: 'API keys',
        icon: 'key',
        hint: entry.masked,
        keywords: `${entry.id} ${(entry.models ?? []).join(' ')}`,
        run: () => openKey(entry.id),
      })),
    ],
    [keys.data],
  );

  // ---- render -------------------------------------------------------------

  const loaded = keys.data != null;
  const none = loaded && all.length === 0;
  const failed = keys.error && !loaded;
  const noneEnabled = loaded && all.length > 0 && all.every((entry) => !entry.enabled);
  const createButton = html`<${Button} class="keys-create" variant="primary" icon="plus" onClick=${() => setCreating(true)}>Create key<//>`;

  return html`
    <${Page}
      title="API keys"
      description="Keys that client applications present to call this gateway. Each can be limited to some models and to a request rate."
      actions=${none || failed ? null : createButton}
    >
      ${status.data &&
      !authRequired &&
      html`
        <${Notice} tone="caution" title="Authentication is not required" action=${html`<${Button} size="sm" href=${href('/settings')}>Open settings<//>`}>
          The gateway serves requests without a key, and requests with a key that is not listed here. Allow-lists, rate limits and usage numbers apply only to requests that send one of these keys. Turn on <span class="mono">auth.required</span> in Settings to refuse everything else.
        <//>
      `}
      ${authRequired &&
      noneEnabled &&
      html`<${Notice} tone="caution" title="No key is enabled">Authentication is required and every key is switched off, so the gateway refuses all client requests with 401. Enable a key or create one.<//>`}
      ${keys.error &&
      loaded &&
      html`
        <${Notice} tone="caution" title="Could not refresh the keys" action=${html`<${Button} size="sm" icon="refresh" onClick=${retry}>Try again<//>`}>
          ${sentence(keys.error.message)} The list below is from ${formatTime(keys.updatedAt)}.
        <//>
      `}

      ${failed
        ? html`<${Panel} flush><${ErrorState} title="Could not load the keys" error=${keys.error} onRetry=${retry} /><//>`
        : none
          ? html`
              <${Panel} flush>
                <${EmptyState}
                  icon="key"
                  title="No client keys yet"
                  description=${authRequired
                    ? 'A client key is what an application sends to this gateway in place of a provider key. Authentication is required and there is no key, so every client request is refused until you create one.'
                    : 'A client key is what an application sends to this gateway in place of a provider key. It gives the client a name in requests and usage, and can limit it to some models and to a request rate.'}
                  action=${createButton}
                />
              <//>
            `
          : html`
              <${Panel}
                flush
                footer=${sorted.length > PAGE_SIZE
                  ? html`<${Pagination} class="grow" page=${page} pageSize=${PAGE_SIZE} total=${sorted.length} noun="keys" onPage=${(next) => setQuery({ page: next === 1 ? null : String(next) })} />`
                  : null}
              >
                <div class="keys-toolbar">
                  <${Input}
                    class="keys-search"
                    size="sm"
                    type="search"
                    icon="search"
                    value=${q}
                    onChange=${(value) => setQuery({ q: value || null, page: null })}
                    placeholder="Search name, key or model pattern"
                    aria-label="Search keys"
                  />
                  <${Segmented} size="sm" label="Show keys by status" value=${show} onChange=${(value) => setQuery({ show: value === 'all' ? null : value, page: null })} options=${SHOW} />
                  <span class="keys-count" role="status">${!loaded ? '' : filtering ? `${formatNumber(filtered.length)} of ${plural(all.length, 'key')}` : plural(all.length, 'key')}</span>
                  ${loaded &&
                  !sortShown &&
                  html`
                    <span class="keys-sortnote">
                      Sorted by ${sortColumn.header.toLowerCase()}, ${sort.dir === 'desc' ? 'highest first' : 'lowest first'}
                      <${Button} size="sm" variant="ghost" onClick=${sortByName}>Sort by name<//>
                    </span>
                  `}
                  <span class="keys-range">Usage over the last 30 days</span>
                  <${IconButton}
                    icon="refresh"
                    label="Refresh"
                    size="sm"
                    loading=${reloading}
                    onClick=${async () => {
                      setReloading(true);
                      await refreshAll();
                      setReloading(false);
                    }}
                  />
                </div>
                <div class="keys-list" data-tier=${tier} ref=${fitRef}>
                  <${Table}
                    class="keys-table"
                    caption="Client keys with their limits and usage over the last 30 days"
                    columns=${columns}
                    rows=${loaded ? rows : undefined}
                    rowKey="id"
                    loading=${keys.loading}
                    sort=${sort}
                    onSort=${(next) => {
                      const value = next ? `${next.key}:${next.dir}` : DEFAULT_SORT;
                      setQuery({ sort: value === DEFAULT_SORT ? null : value, page: null });
                    }}
                    onRowClick=${(entry) => openKey(entry.id)}
                    selectedKey=${open || null}
                    freshKeys=${fresh}
                    empty=${{
                      icon: 'search',
                      title: 'No key matches',
                      description: needle ? `Nothing here has "${q.trim()}" in its name, key or model patterns${show !== 'all' ? ` among the ${show} keys` : ''}.` : `No key is ${show}.`,
                      action: html`<${Button} size="sm" onClick=${clearFilters}>Clear filters<//>`,
                    }}
                  />
                </div>
              <//>
            `}

      <${KeyDrawer}
        id=${open}
        keys=${keys}
        models=${models}
        listen=${listen}
        tls=${tls}
        authRequired=${authRequired}
        toggling=${toggling}
        onToggle=${toggle}
        onSaved=${(updated) => replaceEntry(updated.id, () => updated)}
        onDeleted=${forget}
        onClose=${closeKey}
      />

      <${CreateKeyModal} open=${creating} onClose=${() => setCreating(false)} existing=${keys.data} models=${models} listen=${listen} tls=${tls} onCreated=${onCreated} />
    <//>
  `;
}
