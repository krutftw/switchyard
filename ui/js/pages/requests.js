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
import { formatNumber, formatTime, plural, sentence } from '../lib/format.js';
import { usePresence, useResource } from '../lib/hooks.js';
import { useLive } from '../lib/live.js';
import { navigate } from '../lib/router.js';
import { COLUMNS, HoverTips, memo } from './requests/cells.js';
import RequestDrawer from './requests/detail.js';
import FilterBar from './requests/filters.js';
import useRequestLog from './requests/log.js';
import matchesFilters, { FILTER_KEYS, NO_MODEL_FILTER, hasNoModel } from './requests/record.js';

await loadStyles('pages/requests.css');

/** The table re-renders when its rows or the open request change, and not
 *  when anything else on the page does. Table's own rows are memoised, so
 *  opening a request renders two of them again, not the whole list. */
const LogTable = memo(function LogTable({ rows, loading, freshKeys, selectedKey, onOpen, empty }) {
  return html`<${Table} class="req-table" dense columns=${COLUMNS} rows=${rows} rowKey="id" loading=${loading} skeletonRows=${12} freshKeys=${freshKeys} selectedKey=${selectedKey} onRowClick=${onOpen} empty=${empty} caption="Requests, newest first" />`;
});

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

  // Some controls remove themselves when they have done their work: "N new
  // requests", "Clear filters" in the empty state, "Load older requests" at
  // the end of the list, "Try again" once the list has loaded. The keyboard
  // then goes to the row that took the control's place (`index`), once the
  // list has it, unless the focus has moved on to something else meanwhile.
  const handOver = useRef(null); // { index, untilShown } | null
  useEffect(() => {
    const want = handOver.current;
    if (!want) return;
    if (log.loading || log.loadingMore || log.refreshing) {
      want.started = true;
      return;
    }
    if (want.untilShown ? log.pending > 0 : !want.started) return;
    handOver.current = null;
    const at = document.activeElement;
    if (at && at !== document.body && at.isConnected && !at.closest('.req-new')) return;
    const body = top.current?.parentElement;
    // (Failing a row: what the table shows in its place, the empty state's link.)
    (body?.querySelectorAll('.req-table tbody tr[data-clickable]')[want.index] ?? body?.querySelector('.req-table tbody a, .req-table tbody button'))?.focus({ preventScroll: true });
  });

  const retry = useCallback(() => {
    handOver.current = { index: 0 };
    log.refresh({ reveal: true });
  }, [log.refresh]);

  const showNew = useCallback(() => {
    handOver.current = { index: 0, untilShown: true };
    log.reveal();
    top.current?.scrollIntoView({ behavior: prefersReducedMotion() ? 'auto' : 'smooth', block: 'start' });
  }, [log.reveal]);

  const inFlight = useMemo(() => (live ? log.inFlight.filter((record) => matchesFilters(record, filters, true)) : []), [live, log.inFlight, filters]);
  const rows = useMemo(() => (inFlight.length > 0 ? [...inFlight, ...log.rows] : log.rows), [inFlight, log.rows]);

  // The first of the rows a page of older requests adds stands where the
  // button was.
  const listed = useRef(0);
  listed.current = rows.length;
  const loadOlder = useCallback(() => {
    handOver.current = { index: listed.current };
    log.loadMore();
  }, [log.loadMore]);

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

  // Requests that were refused before a model could be read have none. The
  // model filter offers them ("No model") while the gateway holds any: asked
  // once, again when one arrives that the answer did not know of, and again
  // with each reload of the list while the answer is yes (they leave the
  // gateway's memory like any other request).
  const unnamed = useResource(['/requests', { model: NO_MODEL_FILTER, limit: 1 }]);
  const unnamedKnown = (unnamed.data?.total ?? 0) > 0;
  useLive('request.finished', (record) => {
    if (record && hasNoModel(record)) unnamed.refresh();
  }, { enabled: !unnamedKnown });
  const reloadedAt = useRef(null);
  useEffect(() => {
    if (reloadedAt.current != null && unnamedKnown) unnamed.refresh();
    reloadedAt.current = log.updatedAt;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [log.updatedAt]);
  const noModel = useMemo(() => unnamedKnown || log.rows.some(hasNoModel), [unnamedKnown, log.rows]);
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
            action: html`<${Button} icon="x" onClick=${() => { handOver.current = { index: 0 }; clearFilters(); }}>Clear filters<//>`,
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
  // gateway only keeps so many (`capacity`, which comes with every page): at
  // that number the list is the newest ones, not all of them, and the title
  // says so.
  const capacity = log.capacity;
  const capped = !filtered && capacity != null && log.total != null && log.total >= capacity;
  let title = 'Requests';
  if (capped) title = `Newest ${formatNumber(capacity)} requests`;
  else if (log.total != null) title = `${formatNumber(log.total)} ${filtered ? 'matching ' : ''}${log.total === 1 ? 'request' : 'requests'}`;

  return html`
    <${Page} class="req-page" title="Requests" description="The most recent requests, newest first. Select one to see its attempts, usage and captured bodies.">
      <${FilterBar} filters=${filters} onChange=${setFilters} onClear=${clearFilters} models=${modelNames} noModel=${noModel} providers=${providerNames} keys=${keyList} />

      ${log.error &&
      !failed &&
      html`
        <${Notice} tone="caution" title="Could not refresh the list" action=${html`<${Button} size="sm" icon="refresh" loading=${log.refreshing} onClick=${retry}>Try again<//>`}>
          ${sentence(log.error.message)} The rows below are as of ${formatTime(log.updatedAt)}.
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
          ? html`<${ErrorState} title="Could not load requests" error=${log.error} onRetry=${retry} retrying=${log.refreshing} />`
          : html`
              <${HoverTips} class="req-tips">
                <${LogTable} rows=${rows} loading=${log.loading} freshKeys=${log.fresh} selectedKey=${id || null} onOpen=${open} empty=${empty} />
              <//>
            `}
        ${log.rows.length > 0 &&
        html`
          <div class="req-foot">
            ${log.moreError && html`<p class="req-foot-error" role="alert">Could not load older requests: ${log.moreError.message}</p>`}
            <${LoadMore} hasMore=${log.hasMore} loading=${log.loadingMore} onLoad=${loadOlder} shown=${log.rows.length} noun=${`${filtered ? 'matching ' : ''}${log.rows.length === 1 && !log.hasMore ? 'request' : 'requests'}`} />
            ${!log.hasMore && capped && html`<p class="req-foot-note">The gateway keeps its ${formatNumber(capacity)} most recent requests in memory, so the list ends here. Older requests still count on the <a href="#/usage">Usage</a> page.</p>`}
            ${!log.hasMore && filtered && capacity != null && html`<p class="req-foot-note">Filters search the ${formatNumber(capacity)} most recent requests, which is what the gateway keeps in memory.</p>`}
          </div>
        `}
      <//>

      <${RequestDrawer} id=${id} seed=${seed} onClose=${close} onNewer=${newer ? () => go({ id: newer.id }) : null} onOlder=${older ? () => go({ id: older.id }) : null} />
    <//>
  `;
}
