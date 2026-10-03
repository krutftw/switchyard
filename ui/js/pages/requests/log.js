// Requests page: the list behind the table.
//
// useRequestLog loads the newest page of GET /requests, pages backwards with
// the `before` cursor, and keeps the list current:
//
//   - request.finished frames are merged in as they arrive, in batches, so a
//     burst of traffic costs one render and not one per request;
//   - while the reader is looking further down, or has switched live updates
//     off, arrivals wait in a queue and are only counted ("12 new requests");
//   - request.started frames are kept as the set of requests in flight;
//   - without a live connection the newest page is polled instead;
//   - after a reconnect, or when the gateway says frames were dropped
//     (useLiveGap), the newest page is fetched again, because frames in
//     between are gone.

import { useCallback, useEffect, useRef, useState } from '../../../vendor/preact-htm.js';
import { api } from '../../lib/api.js';
import { useInterval } from '../../lib/hooks.js';
import { liveState, useLive, useLiveGap } from '../../lib/live.js';
import { useStore } from '../../lib/store.js';
import matchesFilters, { cursorOf, mergeRecords } from './record.js';

const FIRST_PAGE = 100;
const MORE_PAGE = 200;
/** Live arrivals stop growing the list here: the oldest rows give way. */
const SOFT_MAX_ROWS = 1000;
/** More waiting arrivals than this are fetched again rather than kept. */
const MAX_PENDING = 500;
const POLL_MS = 5000;
const BATCH_MS = 250;
/** A long list costs more to render again, so it takes arrivals less often. */
const SLOW_BATCH_MS = 1000;
const LONG_LIST = 400;
const FRESH_MS = 1500;
const IN_FLIGHT_MAX_AGE_MS = 30 * 60_000;
/** How soon after live arrivals the total is read back from the gateway. */
const COUNT_SYNC_MS = 8000;

const NO_KEYS = new Set();

const BLANK = {
  rows: [],
  total: null,
  // How many finished requests the gateway keeps in memory; the list, and
  // `total`, cannot go beyond it. Known once a page has loaded.
  capacity: null,
  cursor: null,
  hasMore: false,
  loading: true,
  refreshing: false,
  error: null,
  loadingMore: false,
  moreError: null,
  pending: 0,
  pendingOverflow: false,
  inFlight: [],
  fresh: NO_KEYS,
  updatedAt: null,
};

/**
 * @param {{status: string, model: string, client_model: string, since: string, provider: string, key: string, q: string}} filters
 * @param {{ live: boolean, atTop: () => boolean }} options
 *   live   show arrivals as they come; false queues them
 *   atTop  whether the top of the list is on screen. Arrivals are queued
 *          while it is not, so the rows being read do not move.
 */
export default function useRequestLog(filters, { live, atTop }) {
  const [state, setState] = useState(BLANK);

  // The same state, readable from callbacks that outlive a render. Every
  // write goes through commit(), so the two never disagree.
  const current = useRef(BLANK);
  const commit = useCallback((patch) => {
    const next = { ...current.current, ...(typeof patch === 'function' ? patch(current.current) : patch) };
    current.current = next;
    setState(next);
  }, []);

  const filtersRef = useRef(filters);
  filtersRef.current = filters;
  const liveRef = useRef(live);
  liveRef.current = live;
  const atTopRef = useRef(atTop);
  atTopRef.current = atTop;

  const generation = useRef(0); // bumps when the filters change
  const pending = useRef([]); // matching arrivals not on screen yet
  const incoming = useRef([]); // finished frames since the last batch
  const inFlight = useRef(new Map());
  const freshAt = useRef(new Map());
  const timers = useRef({ batch: null, fresh: null, count: null });
  // The refetch of the newest page: whether one is on its way, the one to
  // send after it, and the ids of requests that finished meanwhile.
  const head = useRef({ busy: false, again: null, arrived: new Set() });

  const filterKey = JSON.stringify(filters);

  const query = (extra) => {
    const f = filtersRef.current;
    return { ...extra, status: f.status, model: f.model, client_model: f.client_model, since: f.since, provider: f.provider, key: f.key, q: f.q };
  };

  const inFlightList = () => {
    const cutoff = Date.now() - IN_FLIGHT_MAX_AGE_MS;
    for (const [id, record] of inFlight.current) if (record.started_at < cutoff) inFlight.current.delete(id);
    return [...inFlight.current.values()].reverse();
  };

  const markFresh = (records) => {
    const now = Date.now();
    for (const [id, at] of freshAt.current) if (now - at > FRESH_MS) freshAt.current.delete(id);
    for (const record of records) freshAt.current.set(record.id, now);
    clearTimeout(timers.current.fresh);
    timers.current.fresh = setTimeout(() => {
      freshAt.current.clear();
      commit({ fresh: NO_KEYS });
    }, FRESH_MS);
    return new Set(freshAt.current.keys());
  };

  /** Put records on screen, newest first, and let the oldest rows go. */
  const show = (s, records) => {
    const listed = new Set(s.rows.map((r) => r.id));
    let rows = mergeRecords(s.rows, records);
    let { cursor, hasMore } = s;
    const grew = rows.length - s.rows.length;
    if (grew > 0 && rows.length > SOFT_MAX_ROWS) {
      rows = rows.slice(0, Math.max(SOFT_MAX_ROWS, rows.length - grew));
      cursor = cursorOf(rows[rows.length - 1]);
      hasMore = true;
    }
    // Only rows that were not there before flash.
    return { rows, cursor, hasMore, fresh: markFresh(records.filter((r) => !listed.has(r.id))) };
  };

  /** Queue records behind the "N new requests" button. */
  const park = (records) => {
    const queued = new Set(pending.current.map((r) => r.id));
    for (const record of records) if (!queued.has(record.id)) pending.current.push(record);
    let overflow = current.current.pendingOverflow;
    if (pending.current.length > MAX_PENDING) {
      pending.current = pending.current.slice(-MAX_PENDING);
      overflow = true;
    }
    return { pending: pending.current.length, pendingOverflow: overflow };
  };

  const canShowNow = () => liveRef.current && atTopRef.current();

  // ---- Newest page --------------------------------------------------------

  const applyHead = (page, reveal) => {
    for (const item of page.items) inFlight.current.delete(item.id);
    commit((s) => {
      const base = { total: page.total, capacity: page.capacity ?? s.capacity, error: null, loading: false, refreshing: false, updatedAt: Date.now(), inFlight: inFlightList() };
      const listed = new Set(s.rows.map((r) => r.id));
      const unseen = page.items.filter((r) => !listed.has(r.id));
      // Requests that finished while this page was on its way are newer than
      // it: their absence from it does not mean the gateway dropped them.
      const late = head.current.arrived;
      const lateRows = s.rows.filter((r) => late.has(r.id));

      // Nothing behind this page: it is every request the gateway has. Rows
      // it does not hold are gone (the statistics were cleared, or the
      // gateway restarted without them), so the list becomes the page.
      if (!page.has_more && s.rows.length > 0) {
        pending.current = pending.current.filter((r) => late.has(r.id));
        if (reveal || canShowNow()) {
          const queued = pending.current;
          pending.current = [];
          return { ...base, rows: mergeRecords(page.items, [...lateRows, ...queued]), cursor: page.next_before, hasMore: false, pending: 0, pendingOverflow: false, fresh: markFresh(unseen) };
        }
        // The reader is further down: what is new still waits for them.
        return { ...base, rows: mergeRecords(page.items.filter((r) => listed.has(r.id)), lateRows), cursor: page.next_before, hasMore: false, ...park(unseen), pendingOverflow: false };
      }

      // The list no longer follows on from this page, so start over from it:
      // nothing in common with what is listed (requests are missing in
      // between), or more rows listed than the gateway has at all.
      const gap = s.rows.length > 0 && unseen.length === page.items.length;
      const shrunk = s.rows.length - lateRows.length > page.total;
      if (s.rows.length === 0 || gap || shrunk || s.pendingOverflow) {
        if (s.rows.length === 0 || reveal || canShowNow()) {
          pending.current = [];
          return { ...base, rows: mergeRecords(page.items, lateRows), cursor: page.next_before, hasMore: page.has_more, pending: 0, pendingOverflow: false, fresh: s.rows.length ? markFresh(unseen) : NO_KEYS };
        }
        // The reader is further down: keep their rows, queue the page.
        return { ...base, ...park(unseen), pendingOverflow: true };
      }
      const known = page.items.filter((r) => listed.has(r.id));
      if (reveal || canShowNow()) {
        const queued = pending.current;
        pending.current = [];
        return { ...base, ...show(s, [...queued, ...page.items]), pending: 0, pendingOverflow: false };
      }
      return { ...base, rows: mergeRecords(s.rows, known), ...park(unseen) };
    });
  };

  /**
   * Fetch the newest page again and merge it in. `reveal` also puts every
   * queued arrival on screen (the Refresh button, the "new requests" button).
   * Calls made while a fetch is in flight collapse into one follow-up.
   */
  const refresh = useCallback(async ({ reveal = false } = {}) => {
    if (current.current.loading && !current.current.error) return;
    if (head.current.busy) {
      head.current.again = { reveal: reveal || head.current.again?.reveal === true };
      return;
    }
    head.current.busy = true;
    head.current.arrived = new Set();
    const mine = generation.current;
    commit({ refreshing: true });
    try {
      const page = await api.get('/requests', { query: query({ limit: FIRST_PAGE }) });
      if (mine === generation.current) applyHead(page, reveal);
    } catch (error) {
      if (mine === generation.current && !error.aborted) commit({ error, loading: false, refreshing: false });
    } finally {
      head.current.busy = false;
      const again = head.current.again;
      head.current.again = null;
      if (again && mine === generation.current) refresh(again);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // First page: on mount and whenever the filters change.
  useEffect(() => {
    generation.current += 1;
    const mine = generation.current;
    const controller = new AbortController();
    pending.current = [];
    incoming.current = [];
    freshAt.current.clear();
    // (The capacity is the gateway's, not the filters': it stays.)
    commit({ ...BLANK, capacity: current.current.capacity, inFlight: inFlightList() });
    api.get('/requests', { query: query({ limit: FIRST_PAGE }), signal: controller.signal }).then(
      (page) => {
        if (mine !== generation.current) return;
        for (const item of page.items) inFlight.current.delete(item.id);
        commit({ rows: page.items, total: page.total, capacity: page.capacity ?? null, cursor: page.next_before, hasMore: page.has_more, loading: false, error: null, updatedAt: Date.now(), inFlight: inFlightList() });
      },
      (error) => {
        if (mine !== generation.current || error.aborted) return;
        commit({ loading: false, error });
      },
    );
    return () => controller.abort();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [filterKey]);

  // ---- Older pages --------------------------------------------------------

  const loadMore = useCallback(async () => {
    const s = current.current;
    if (s.loadingMore || !s.hasMore || !s.cursor) return;
    const mine = generation.current;
    commit({ loadingMore: true, moreError: null });
    try {
      const page = await api.get('/requests', { query: query({ limit: MORE_PAGE, before: s.cursor }) });
      if (mine !== generation.current) return;
      commit((now) => ({ rows: mergeRecords(now.rows, page.items), cursor: page.next_before, hasMore: page.has_more, total: page.total, loadingMore: false }));
    } catch (error) {
      if (mine === generation.current && !error.aborted) commit({ loadingMore: false, moreError: error });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // ---- Live frames --------------------------------------------------------

  // The gateway keeps a bounded number of requests in memory, so the total
  // cannot simply be counted up as requests arrive: ask for it, at most once
  // per interval while traffic flows.
  const syncCount = () => {
    timers.current.count = null;
    const mine = generation.current;
    api.get('/requests', { query: query({ limit: 1 }) }).then(
      (page) => {
        if (mine !== generation.current || current.current.loading) return;
        commit({ total: page.total });
        // Fewer than are listed: the gateway has let go of rows shown here
        // (statistics cleared while requests kept arriving). Read the list
        // again so it shows what exists.
        if (page.total < current.current.rows.length) refresh();
      },
      () => {
        /* the next refresh corrects it */
      },
    );
  };

  const flush = () => {
    timers.current.batch = null;
    const s = current.current;
    // The first page is still on its way; it may or may not include these.
    if (s.loading) {
      if (incoming.current.length > 0 || inFlight.current.size > 0) timers.current.batch = setTimeout(flush, BATCH_MS);
      return;
    }
    const batch = incoming.current;
    incoming.current = [];
    for (const record of batch) inFlight.current.delete(record.id);
    const matched = batch.filter((record) => matchesFilters(record, filtersRef.current));
    if (matched.length === 0) {
      commit({ inFlight: inFlightList() });
      return;
    }
    const known = new Set(s.rows.map((r) => r.id));
    for (const record of pending.current) known.add(record.id);
    const added = matched.filter((record) => !known.has(record.id)).length;
    const total = s.total == null ? null : s.total + added;
    if (added > 0 && timers.current.count == null) timers.current.count = setTimeout(syncCount, COUNT_SYNC_MS);
    if (canShowNow()) {
      const queued = pending.current;
      pending.current = [];
      commit({ ...show(s, [...queued, ...matched]), total, pending: 0, inFlight: inFlightList() });
    } else {
      commit({ ...park(matched), total, inFlight: inFlightList() });
    }
  };

  const schedule = () => {
    if (timers.current.batch == null) timers.current.batch = setTimeout(flush, current.current.rows.length > LONG_LIST ? SLOW_BATCH_MS : BATCH_MS);
  };

  useLive('request.started', (data) => {
    if (!data?.id) return;
    inFlight.current.set(data.id, { ...data, in_flight: true });
    schedule();
  });

  useLive('request.finished', (record) => {
    if (!record?.id) return;
    if (head.current.busy) head.current.arrived.add(record.id);
    incoming.current.push(record);
    schedule();
  });

  // Frames may be missing: the gateway dropped some for this connection, or
  // the connection was down for a while. The list has holes then, and a
  // request whose end was missed would look in flight for ever.
  useLiveGap(() => {
    inFlight.current.clear();
    refresh();
  });

  // The same guard for an idle gateway: its gauge says nothing is in flight,
  // so anything still listed as such (and not brand new) has ended.
  useLive('stats', (stats) => {
    if (stats?.in_flight !== 0 || inFlight.current.size === 0) return;
    const cutoff = Date.now() - 2000;
    let dropped = false;
    for (const [id, record] of inFlight.current) {
      if (record.started_at < cutoff) {
        inFlight.current.delete(id);
        dropped = true;
      }
    }
    if (dropped) commit({ inFlight: inFlightList() });
  });

  /** Put the queued arrivals on screen. */
  const reveal = useCallback(() => {
    const s = current.current;
    if (s.pendingOverflow) {
      refresh({ reveal: true });
      return;
    }
    if (pending.current.length === 0) return;
    const queued = pending.current;
    pending.current = [];
    commit({ ...show(s, queued), pending: 0 });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Switching live updates back on shows what was held back.
  useEffect(() => {
    if (live && atTopRef.current()) reveal();
  }, [live, reveal]);

  // ---- Connection ---------------------------------------------------------

  const connection = useStore(liveState, (s) => s.status);
  const connected = connection === 'open';

  // No live connection: ask for the newest page every few seconds instead.
  useInterval(
    () => {
      if (typeof document === 'undefined' || document.visibilityState === 'visible') refresh();
    },
    live && !connected ? POLL_MS : null,
  );

  useEffect(
    () => () => {
      clearTimeout(timers.current.batch);
      clearTimeout(timers.current.fresh);
      clearTimeout(timers.current.count);
      generation.current += 1;
    },
    [],
  );

  return {
    ...state,
    /** "streaming" | "polling" | "paused" */
    mode: !live ? 'paused' : connected ? 'streaming' : 'polling',
    refresh,
    loadMore,
    reveal,
  };
}
