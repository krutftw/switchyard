// Requests (#/requests): the request log and the detail of one request.
//
//   ?status=&model=&provider=&key=&q=   filters, the same names GET /requests takes
//   ?live=off                           hold live updates back
//   ?id=<request id>&tab=<body tab>     the open detail drawer
//
// The pieces live in ./requests/: log.js (the list and its live updates),
// cells.js (columns), filters.js (the filter bar), detail.js (the drawer),
// record.js (pure helpers).

import { html, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from '../../vendor/preact-htm.js';
import { Button, ErrorState, IconButton, LoadMore, Notice, Page, Panel, StatusLamp, Switch, Table } from '../components/index.js';
import { useCommands } from '../lib/commands.js';
import { loadStyles, prefersReducedMotion } from '../lib/dom.js';
import { formatNumber, formatTime, plural } from '../lib/format.js';
import { usePresence, useResource } from '../lib/hooks.js';
import { useLive } from '../lib/live.js';
import { navigate } from '../lib/router.js';
import requestColumns, { HoverTips, memo } from './requests/cells.js';
import RequestDrawer from './requests/detail.js';
import FilterBar from './requests/filters.js';
import useRequestLog from './requests/log.js';
import matchesFilters, { FILTER_KEYS } from './requests/record.js';

await loadStyles('pages/requests.css');

const COLUMNS = requestColumns();

/** The table re-renders when its rows change, and not when anything else on
 *  the page does. Not even for the selection: see useSelectedRow. */
const LogTable = memo(function LogTable({ rows, loading, freshKeys, onOpen, empty }) {
  return html`<${Table} class="req-table" dense columns=${COLUMNS} rows=${rows} rowKey="id" loading=${loading} skeletonRows=${12} freshKeys=${freshKeys} onRowClick=${onOpen} empty=${empty} caption="Requests, newest first" />`;
});

/**
 * Mark the row of the open request. Table can do this itself (selectedKey),
 * but a changed prop renders every row again, and opening a request is the
 * thing done most on this page: with a thousand rows listed that is a
 * visible pause. Rows are keyed by id, so the mark stays with its request
 * when rows arrive above it.
 */
function useSelectedRow(box, index, rows) {
  useLayoutEffect(() => {
    const body = box.current?.querySelector('tbody');
    if (!body) return;
    const row = index >= 0 ? body.children[index] : null;
    for (const marked of body.querySelectorAll('tr[data-selected]')) if (marked !== row) marked.removeAttribute('data-selected');
    row?.setAttribute('data-selected', '');
  }, [box, index, rows]);
}

/** Width of an element, measured before the first paint and on every resize. */
function useWidth(ref) {
  const [width, setWidth] = useState(0);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return undefined;
    const read = () => setWidth(Math.round(el.clientWidth));
    read();
    const observer = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(read);
    observer?.observe(el);
    window.addEventListener('resize', read);
    return () => {
      observer?.disconnect();
      window.removeEventListener('resize', read);
    };
  }, [ref]);
  return width;
}

/** Panel widths, in px, from which each way of drawing the list fits. */
const FIT_FULL = 960;
const FIT_TIGHT = 880;
const FIT_CARDS = 600;

/**
 * How many requests the gateway keeps in memory (crates/admin/API.md: "the
 * most recent 2000"). The list cannot go further back than that, and the
 * API has no field that says so, so the page has to know the number.
 */
const MEMORY_LIMIT = 2000;

const MODE_LAMP = {
  streaming: { tone: 'info', pulse: true, label: 'Updating live', detail: 'new requests appear as they finish' },
  polling: { tone: 'caution', label: 'Updating every 5s', detail: 'the live connection is down' },
  paused: { tone: 'off', label: 'Paused', detail: 'new requests are counted, not shown' },
};

/** The "12 new requests" button. It stays put under the header while the
 *  reader is further down the list, and takes them back to the top. */
function NewRequests({ count, overflow, onShow }) {
  const { mounted, state } = usePresence(count > 0, 140);
  const last = useRef({ count, overflow });
  if (count > 0) last.current = { count, overflow };
  if (!mounted) return html`<div class="req-new" aria-live="polite"></div>`;
  const shown = last.current;
  return html`
    <div class="req-new" aria-live="polite">
      <${Button} class="req-new-button" variant="primary" size="sm" icon="arrow-up" data-state=${state} onClick=${onShow}>
        ${shown.overflow ? `${formatNumber(shown.count)}+ new requests` : plural(shown.count, 'new request')}
      <//>
    </div>
  `;
}

export default function Requests({ route }) {
  const query = route.query;
  const status = query.status ?? '';
  const model = query.model ?? '';
  const provider = query.provider ?? '';
  const key = query.key ?? '';
  const q = query.q ?? '';
  const filters = useMemo(() => ({ status, model, provider, key, q }), [status, model, provider, key, q]);
  const filtered = FILTER_KEYS.some((name) => filters[name]);
  const live = query.live !== 'off';
  const id = query.id ?? '';

  // Every change of view goes through the URL.
  const queryRef = useRef(query);
  queryRef.current = query;
  const go = useCallback((patch, { replace = true } = {}) => {
    navigate('/requests', { query: { ...queryRef.current, ...patch }, replace });
  }, []);

  // #/requests/<id> is the same view as #/requests?id=<id>.
  const pathId = route.segments[1];
  useEffect(() => {
    if (pathId) go({ id: queryRef.current.id || pathId });
  }, [pathId, go]);

  // ---- The list -----------------------------------------------------------

  // An empty element at the top of the table: where "the top of the list"
  // is, and how wide the table may be.
  const top = useRef(null);
  const width = useWidth(top);
  // How the list is drawn at this width (see requests.css): rows on a grid
  // of columns, without the client key when that is a squeeze, or cards.
  const fit = width >= FIT_FULL ? 'full' : width >= FIT_TIGHT ? 'tight' : width >= FIT_CARDS ? 'cards' : 'slim';
  const layout = width >= FIT_TIGHT ? 'rows' : 'cards';
  const atTop = useCallback(() => {
    const el = top.current;
    return !el || el.getBoundingClientRect().top >= 0;
  }, []);
  const log = useRequestLog(filters, { live, atTop });

  // Scrolling back up to the first row shows what was held back meanwhile.
  const liveRef = useRef(live);
  liveRef.current = live;
  useEffect(() => {
    const el = top.current;
    if (!el || typeof IntersectionObserver === 'undefined') return undefined;
    const observer = new IntersectionObserver((entries) => {
      if (entries.some((entry) => entry.isIntersecting) && liveRef.current) log.reveal();
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, [log.reveal]);

  const showNew = useCallback(() => {
    log.reveal();
    top.current?.scrollIntoView({ behavior: prefersReducedMotion() ? 'auto' : 'smooth', block: 'start' });
  }, [log.reveal]);

  const inFlight = useMemo(() => (live ? log.inFlight.filter((record) => matchesFilters(record, filters, true)) : []), [live, log.inFlight, filters]);
  const rows = useMemo(() => (inFlight.length > 0 ? [...inFlight, ...log.rows] : log.rows), [inFlight, log.rows]);

  // ---- Filter options -----------------------------------------------------

  const models = useResource('/models');
  const providers = useResource('/providers');
  const keys = useResource('/keys');
  useLive('config.reloaded', () => {
    models.refresh();
    providers.refresh();
    keys.refresh();
  });
  const modelNames = useMemo(() => models.data?.map((m) => m.name), [models.data]);
  const providerNames = useMemo(() => providers.data?.map((p) => p.name), [providers.data]);
  const keyList = useMemo(() => keys.data?.map((k) => ({ id: k.id, name: k.name })), [keys.data]);

  const setFilters = useCallback((patch) => go(patch), [go]);
  const clearFilters = useCallback(() => go(Object.fromEntries(FILTER_KEYS.map((name) => [name, null]))), [go]);

  // ---- The drawer ---------------------------------------------------------

  // Opening from the list adds a history entry, so Back closes the drawer.
  // That entry is marked: closing a marked entry goes back to the list entry
  // before it instead of stacking another one. An entry that was not opened
  // from the list (a link, a typed address) has nothing to go back to, so
  // closing it only drops the id.
  const open = useCallback(
    (row) => {
      const already = !!queryRef.current.id;
      go({ id: row.id }, { replace: already });
      if (!already) history.replaceState({ ...(history.state ?? {}), requestOpenedFromList: true }, '');
    },
    [go],
  );

  const close = useCallback(() => {
    if (history.state?.requestOpenedFromList) history.back();
    else go({ id: null, tab: null });
  }, [go]);

  const index = id ? rows.findIndex((row) => row.id === id) : -1;
  const seed = index === -1 ? null : rows[index];
  const newer = index > 0 ? rows[index - 1] : null;
  const older = index !== -1 && index < rows.length - 1 ? rows[index + 1] : null;
  const tableBox = useRef(null);
  useSelectedRow(tableBox, index, rows);

  // ---- Command palette ----------------------------------------------------

  useCommands(
    () => [
      { id: 'requests:failed', label: 'Show failed requests', group: 'Requests', icon: 'filter', keywords: 'errors 4xx 5xx', run: () => go({ status: 'error' }) },
      { id: 'requests:live', label: live ? 'Pause live requests' : 'Resume live requests', group: 'Requests', icon: live ? 'pause' : 'play', run: () => go({ live: live ? 'off' : null }) },
      ...(filtered ? [{ id: 'requests:clear', label: 'Clear request filters', group: 'Requests', icon: 'x', run: clearFilters }] : []),
    ],
    [live, filtered],
  );

  // ---- View ---------------------------------------------------------------

  const empty = useMemo(
    () =>
      filtered
        ? {
            icon: 'filter',
            title: 'No requests match these filters',
            description: 'Requests that match appear here as they finish. Clear the filters to see every request the gateway has in memory.',
            action: html`<${Button} icon="x" onClick=${clearFilters}>Clear filters<//>`,
          }
        : {
            icon: 'requests',
            title: 'No requests yet',
            description: 'Each request a client sends to the gateway is listed here when it finishes. Send one from the playground to see it arrive.',
            action: html`<${Button} href="#/playground" iconRight="arrow-right">Open playground<//>`,
          },
    [filtered, clearFilters],
  );

  const failed = log.error && rows.length === 0;
  const lamp = MODE_LAMP[log.mode];
  // The count of everything that matches, known once a page has loaded. The
  // gateway only keeps so many: at that number the list is the newest ones,
  // not all of them, and the title says so.
  const capped = !filtered && log.total != null && log.total >= MEMORY_LIMIT;
  let title = 'Requests';
  if (capped) title = `Newest ${formatNumber(MEMORY_LIMIT)} requests`;
  else if (log.total != null) title = `${formatNumber(log.total)} ${filtered ? 'matching ' : ''}${log.total === 1 ? 'request' : 'requests'}`;

  return html`
    <${Page} class="req-page" title="Requests" description="The most recent requests, newest first. Select one to see its attempts, usage and captured bodies.">
      <${FilterBar} filters=${filters} onChange=${setFilters} onClear=${clearFilters} models=${modelNames} providers=${providerNames} keys=${keyList} />

      ${log.error &&
      !failed &&
      html`
        <${Notice} tone="caution" title="Could not refresh the list" action=${html`<${Button} size="sm" icon="refresh" loading=${log.refreshing} onClick=${() => log.refresh({ reveal: true })}>Try again<//>`}>
          ${log.error.message} The rows below are as of ${formatTime(log.updatedAt)}.
        <//>
      `}

      <${Panel}
        flush
        class="req-panel"
        data-layout=${layout}
        data-fit=${fit}
        title=${title}
        description=${html`
          <span class="req-meta">
            <${StatusLamp} tone=${lamp.tone} pulse=${lamp.pulse} label=${lamp.label} detail=${lamp.detail} />
            ${inFlight.length > 0 && html`<span class="req-meta-item"><span class="num">${formatNumber(inFlight.length)}</span> in flight</span>`}
          </span>
        `}
        actions=${html`
          <${Switch} class="req-live" label="Live" checked=${live} onChange=${(on) => go({ live: on ? null : 'off' })} />
          <${IconButton} icon="refresh" label="Refresh" loading=${log.refreshing} disabled=${log.loading} onClick=${() => log.refresh({ reveal: true })} />
        `}
      >
        <div class="req-top" ref=${top}></div>
        <${NewRequests} count=${log.pending} overflow=${log.pendingOverflow} onShow=${showNew} />
        ${failed
          ? html`<${ErrorState} title="Could not load requests" error=${log.error} onRetry=${() => log.refresh({ reveal: true })} retrying=${log.refreshing} />`
          : html`
              <${HoverTips} class="req-tips" boxRef=${tableBox}>
                <${LogTable} rows=${rows} loading=${log.loading} freshKeys=${log.fresh} onOpen=${open} empty=${empty} />
              <//>
            `}
        ${log.rows.length > 0 &&
        html`
          <div class="req-foot">
            ${log.moreError && html`<p class="req-foot-error" role="alert">Could not load older requests: ${log.moreError.message}</p>`}
            <${LoadMore} hasMore=${log.hasMore} loading=${log.loadingMore} onLoad=${log.loadMore} shown=${log.rows.length} noun=${filtered ? 'matching requests' : 'requests'} />
            ${!log.hasMore && capped && html`<p class="req-foot-note">The gateway keeps its ${formatNumber(MEMORY_LIMIT)} most recent requests in memory, so the list ends here. Older requests still count on the <a href="#/usage">Usage</a> page.</p>`}
            ${!log.hasMore && filtered && html`<p class="req-foot-note">Filters search the ${formatNumber(MEMORY_LIMIT)} most recent requests, which is what the gateway keeps in memory.</p>`}
          </div>
        `}
      <//>

      <${RequestDrawer} id=${id} seed=${seed} onClose=${close} onNewer=${newer ? () => go({ id: newer.id }) : null} onOlder=${older ? () => go({ id: older.id }) : null} />
    <//>
  `;
}
