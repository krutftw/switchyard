// Logs: the gateway's own application log, as a live tail.
//
// The first page comes from GET /logs, new lines from "log" live frames
// (from polling while the live connection is down), older ones through the
// `before` cursor. The buffer and its rules are in logs/model.js, the
// scrolling list in logs/list.js; this module is the page around them:
// filters in the URL, pause, clear, download, and every state in between.

import { html, useEffect, useLayoutEffect, useMemo, useRef, useState } from '../../vendor/preact-htm.js';
import { Button, IconButton, Spinner } from '../components/button.js';
import { Input, Select } from '../components/form.js';
import { Icon } from '../components/icons.js';
import { Segmented } from '../components/nav.js';
import { StatusLamp } from '../components/status.js';
import { EmptyState, ErrorState, Notice, Page, Panel, Skeleton } from '../components/surface.js';
import { toast } from '../components/toast.js';
import { api } from '../lib/api.js';
import { useCommands } from '../lib/commands.js';
import { copyText, loadStyles } from '../lib/dom.js';
import { formatNumber, formatTime, plural, sentence } from '../lib/format.js';
import { useDebounced, useHotkey, useInterval, useLocalStorage, usePresence, useResource } from '../lib/hooks.js';
import { liveState, useLive, useLiveGap } from '../lib/live.js';
import { href, useQueryParam } from '../lib/router.js';
import { useStore } from '../lib/store.js';
import { LogList } from './logs/list.js';
import {
  GATEWAY_LINES,
  covers,
  createLogStore,
  downloadName,
  isFiltered,
  levelRank,
  lineToText,
  linesToText,
  lowerBound,
  makeFilter,
  matches,
  narrows,
  parseLevel,
  sameQuery,
  serverQuery,
  tailOf,
} from './logs/model.js';

await loadStyles('pages/logs.css');

/** Lines asked for on the first load, after a filter change and per older page. */
const PAGE_SIZE = 300;
/** Lines asked for when catching up (reconnect, lost frames, polling). */
const SYNC_SIZE = 500;
/** How often the log is fetched while the live connection is down. */
const POLL_MS = 3000;
/** Live frames are applied in batches this far apart, however fast they arrive. */
const FLUSH_MS = 80;
/** Older pages fetched in one go while none of their lines pass the display filter. */
const OLDER_ROUNDS = 6;
/** Targets offered in the target picker, most frequent first. */
const TARGET_CHOICES = 40;
/** Characters of a target shown in the picker; the end of a target says the most. */
const TARGET_LABEL = 48;
/** Frames kept until the next batch; beyond it the oldest go and are fetched instead. */
const QUEUE_MAX = 8000;

const LEVEL_OPTIONS = [
  { value: 'trace', label: 'All', title: 'Every level' },
  { value: 'debug', label: 'Debug', title: 'Debug and more severe' },
  { value: 'info', label: 'Info', title: 'Info and more severe' },
  { value: 'warn', label: 'Warn', title: 'Warnings and errors' },
  { value: 'error', label: 'Error', title: 'Errors only' },
];

// ---------------------------------------------------------------------------
// The feed: buffer + requests + live frames
// ---------------------------------------------------------------------------

/**
 * Keeps a log buffer filled for `filter`.
 *
 * Returns
 *   store      the buffer (logs/model.js); read it during render
 *   version    changes whenever the buffer does
 *   status     "loading" (nothing to show yet) | "ready" | "error"
 *   error      why the last load for the current filter failed
 *   retrying   a first load that failed is being tried again
 *   refreshing a load for a new filter is in flight, older data is on screen
 *   syncError  why the last catch-up failed (the lines on screen are kept)
 *   older      { loading, error } of "load older"
 *   reload(), sync(), loadOlder(), pause(), resume(), clear(), restore()
 */
function useLogFeed(filter) {
  const store = useMemo(() => createLogStore(), []);
  const [version, setVersion] = useState(0);
  const [load, setLoad] = useState({ status: 'loading', error: null, refreshing: false });
  const [syncError, setSyncError] = useState(null);
  const [older, setOlder] = useState({ loading: false, error: null });
  const filterRef = useRef(filter);
  filterRef.current = filter;
  const paused = store.paused !== null;

  const feed = useMemo(() => {
    const io = {
      alive: true,
      started: false,
      ready: false, // the first page has landed
      generation: 0, // a reset makes every older request stale
      controller: new AbortController(),
      pending: null, // the query a reset is loading, or null
      queue: [],
      timer: null,
      syncing: false,
      syncAgain: false,
      olderBusy: false,
      // The gateway process the buffer is filled from (its `started_at`). A
      // restarted gateway numbers its lines from 1 again, and both sources
      // say which process they are: every page of GET /logs, and the
      // "hello" frame a live connection begins with, before any of its lines.
      startedAt: null,
    };
    const bump = () => setVersion((v) => (v + 1) % 1_000_000_000);

    /** `started_at` as a page or a "hello" frame gives it, or null when it does not. */
    const processOf = (data) => {
      const at = Number(data?.started_at);
      return Number.isFinite(at) && at > 0 ? at : null;
    };

    /** A page of GET /logs. */
    const fetchPage = (query) => api.get('/logs', { query, signal: io.controller.signal });

    /** Put the frames received so far into the buffer. Returns true when it then needs a catch-up. */
    const apply = () => {
      if (io.queue.length === 0) return false;
      const batch = io.queue;
      io.queue = [];
      const result = store.live(batch);
      if (result.added > 0) bump();
      return result.needSync;
    };

    const flush = () => {
      io.timer = null;
      if (!io.alive) return;
      // Before the first page has landed, that page settles it (see reset).
      // A catch-up already on its way may close the hole: the buffer says
      // so when its page is in, and only then is another one sent.
      if (apply()) sync({ fresh: false });
    };

    /**
     * Take note of the process a page or a live connection belongs to
     * (`data` is the page or the "hello" frame). Returns true when the
     * gateway was restarted since the buffer was filled: the buffer has then
     * been told (logs/model.js) and the log must be loaded afresh. A page
     * that says so is dropped: its lines were asked for with the old
     * numbering in mind.
     */
    const settle = (data) => {
      const startedAt = processOf(data);
      if (startedAt === null) return false;
      const restarted = io.startedAt !== null && startedAt !== io.startedAt;
      io.startedAt = startedAt;
      if (restarted) {
        // Frames not yet in the buffer are lines of the process that is gone.
        apply();
        store.restart(startedAt);
        bump();
      }
      return restarted;
    };

    const startOver = () => reset(serverQuery(filterRef.current));

    /** With lines of an earlier run on screen, this run is fetched whole (see dropEarlierRuns in the model). */
    const pageSize = (normal) => (store.hasEarlierRuns() ? GATEWAY_LINES : normal);

    /**
     * Catch up: fetch the newest page and join it with what is held.
     * `fresh: false` when a page that is already on its way will do.
     */
    async function sync({ fresh = true } = {}) {
      if (!io.ready || !io.alive) return;
      // A catch-up may drop lines that sit behind a hole, and a paused view
      // must not change: the lines are fetched on resume. (A gateway that
      // restarts meanwhile says so in the "hello" of the next connection.)
      if (store.paused !== null) return;
      if (io.syncing) {
        if (fresh) io.syncAgain = true;
        return;
      }
      io.syncing = true;
      const generation = io.generation;
      let failed = false;
      try {
        // One request normally; another when frames were lost meanwhile.
        for (let round = 0; round < 4; round += 1) {
          io.syncAgain = false;
          const sentAt = store.lastSeq;
          const page = await fetchPage({ limit: pageSize(SYNC_SIZE), ...store.query });
          if (generation !== io.generation || !io.alive) return;
          if (settle(page)) {
            startOver();
            return;
          }
          // Paused while the request was on its way: leave the view alone.
          if (store.paused !== null) break;
          // So much was missed that the page does not reach back to the
          // lines held, which would have to go. With a filter on, give up
          // the lines it hides instead and ask again for the ones it
          // shows: far fewer were missed of those.
          const focus = store.focus;
          if (focus && !sameQuery(focus, store.query) && !store.joins(page)) {
            store.narrow(focus);
            bump();
            continue;
          }
          const result = store.sync(page, sentAt);
          setSyncError(null);
          bump();
          if (!result.needSync && !io.syncAgain) break;
        }
      } catch (error) {
        failed = true;
        if (generation === io.generation && io.alive && !error.aborted) setSyncError(error);
      } finally {
        const again = io.syncAgain;
        io.syncAgain = false;
        io.syncing = false;
        // A request that came in during the last round still gets its answer.
        if (again && !failed && io.alive && generation === io.generation) sync();
      }
    }

    /**
     * Load the newest page for `query` in place of the history held.
     * `uncleared`: the lines a "Clear view" hid come back with it. They do
     * when the page has arrived, not before: until then the cleared view
     * stays as it is, its button busy.
     */
    async function reset(query, { uncleared = false } = {}) {
      io.generation += 1;
      const generation = io.generation;
      io.controller.abort();
      io.controller = new AbortController();
      io.pending = query;
      setLoad((state) => {
        if (io.ready) return { status: 'ready', error: null, refreshing: true };
        // Trying again after a first load that failed: the error stays on
        // screen, its button busy, until there is something better to show.
        if (state.status === 'error') return { ...state, retrying: true };
        return { status: 'loading', error: null, refreshing: false };
      });
      setOlder({ loading: false, error: null });
      const sentAt = store.lastSeq;
      // A paused view gets the newest lines up to the pause, not up to now.
      const upTo = store.paused !== null && store.paused > store.base ? store.paused + 1 : null;
      try {
        const page = await fetchPage({ limit: pageSize(PAGE_SIZE), ...query, before: upTo === null ? null : upTo - store.base });
        if (generation !== io.generation || !io.alive) return;
        io.pending = null;
        if (uncleared) store.unclear();
        if (settle(page)) {
          startOver();
          return;
        }
        const result = store.reset(page, query, sentAt, upTo);
        io.ready = true;
        setLoad({ status: 'ready', error: null, refreshing: false });
        setSyncError(null);
        bump();
        // The filter moved on while this was loading, to one it does not cover.
        if (!covers(store.query, filterRef.current)) {
          startOver();
          return;
        }
        // Resumed while a page for the paused view was loading: it stops at the pause.
        if (result.needSync || (upTo !== null && store.paused === null)) sync();
      } catch (error) {
        if (generation !== io.generation || !io.alive || error.aborted) return;
        io.pending = null;
        setLoad({ status: io.ready ? 'ready' : 'error', error, refreshing: false });
      }
    }

    /** Give up the load on its way: what is held already serves the filter. */
    const cancelReset = () => {
      io.generation += 1;
      io.controller.abort();
      io.controller = new AbortController();
      io.pending = null;
      setLoad({ status: 'ready', error: null, refreshing: false });
      setOlder({ loading: false, error: null });
      // The cancelled load may have been the one that catches up (after a
      // restart of the gateway, say): do that for what is held.
      sync();
    };

    const loadOlder = async () => {
      if (io.olderBusy || !io.ready || store.floor === null) return;
      io.olderBusy = true;
      const generation = io.generation;
      setOlder({ loading: true, error: null });
      try {
        for (let round = 0; round < OLDER_ROUNDS; round += 1) {
          if (store.floor === null) break;
          // Ask for what the display filter shows when that is narrower than
          // what the buffer was loaded with: the page is then all matches.
          const wanted = serverQuery(filterRef.current);
          const query = covers(store.query, filterRef.current) && narrows(store.query, wanted) ? wanted : store.query;
          // A full buffer makes room by giving up the lines outside the filter.
          if (store.used() >= store.maxLines) store.narrow(query);
          const room = store.maxLines - store.used();
          if (room <= 0) break;
          const page = await fetchPage({ limit: Math.min(PAGE_SIZE, room), ...query, before: store.cursor() });
          if (generation !== io.generation || !io.alive) return;
          if (settle(page)) {
            startOver();
            return;
          }
          const { lines } = store.older(page, query);
          bump();
          if (lines.some((line) => matches(line, filterRef.current))) break;
        }
        setOlder({ loading: false, error: null });
      } catch (error) {
        if (generation === io.generation && io.alive && !error.aborted) setOlder({ loading: false, error });
      } finally {
        io.olderBusy = false;
      }
    };

    return {
      io,
      sync,
      loadOlder,
      /**
       * The filter changed (or the page has just opened): load what it needs,
       * unless what is held, or what is on its way, already covers it.
       */
      want(next) {
        if (!io.started) {
          io.started = true;
          reset(serverQuery(next));
          return;
        }
        if (io.pending !== null) {
          if (covers(io.pending, next)) return;
          // What is on its way is for a filter that has been left again.
          // Landing, it would replace lines this filter shows.
          if (io.ready && covers(store.query, next)) cancelReset();
          else reset(serverQuery(next));
          return;
        }
        if (!io.ready || !covers(store.query, next)) {
          reset(serverQuery(next));
          return;
        }
        // The buffer already holds everything this filter shows: a load for
        // the previous filter no longer matters, nor does its failure.
        setLoad((state) => (state.error && state.status === 'ready' ? { ...state, error: null } : state));
      },
      /** A "log" live frame. A timer, not a frame callback: hidden tabs get no frames. */
      push(line) {
        io.queue.push(line);
        // Dropping from the front leaves a gap in the numbering, which the
        // buffer notices and has fetched.
        if (io.queue.length > QUEUE_MAX) io.queue.splice(0, io.queue.length - QUEUE_MAX / 2);
        if (io.timer === null) io.timer = setTimeout(flush, FLUSH_MS);
      },
      /**
       * Frames are missing: the live connection is back after being down, or
       * the gateway dropped some for it. The next frame starts a new run,
       * and what lies in between is fetched.
       */
      resync() {
        // The frames not yet in the buffer came before the hole.
        apply();
        store.breakRun();
        sync();
      },
      /**
       * The live connection is down. It brings no frames: the run it brought
       * has ended, and what is fetched from here on is not joined to it.
       */
      down() {
        apply();
        store.breakRun();
      },
      /** A "hello" live frame: a connection has begun, to a gateway that may have been restarted. */
      hello(data) {
        if (settle(data)) startOver();
      },
      reload: startOver,
      pause() {
        store.pause();
        bump();
      },
      resume() {
        if (store.paused === null) return;
        store.release();
        bump();
        sync();
      },
      clear() {
        store.clear();
        bump();
      },
      restore() {
        return reset(serverQuery(filterRef.current), { uncleared: true });
      },
      stop() {
        io.alive = false;
        io.controller.abort();
        clearTimeout(io.timer);
      },
    };
  }, [store]);

  useEffect(() => () => feed.stop(), [feed]);

  // The first load, and a reload whenever the filter asks for more than the
  // buffer was loaded with. A narrower filter is applied in memory.
  useEffect(() => {
    feed.want(filter);
  }, [feed, filter]);

  useLive('log', feed.push);
  useLive('hello', feed.hello);
  useLiveGap(feed.resync);

  const liveStatus = useStore(liveState, (state) => state.status);
  const liveOpen = liveStatus === 'open';
  // A connection that is down brings no frames: the run it brought has
  // ended. This effect stands before the one that starts polling, because
  // what polling fetches must not be joined to that run.
  useEffect(() => {
    if (!liveOpen) feed.down();
  }, [feed, liveOpen]);

  // Without the live connection the log is polled, unless the view is
  // paused. "connecting" is the moment before the first connection: not down.
  const liveDown = liveStatus === 'reconnecting' || liveStatus === 'offline' || liveStatus === 'unavailable';
  const polling = liveDown && load.status === 'ready';
  useInterval(
    () => {
      if (document.visibilityState === 'visible') feed.sync();
    },
    polling && !paused ? POLL_MS : null,
  );
  useEffect(() => {
    if (!polling || paused) return undefined;
    feed.sync();
    // Back in view: do not wait for the next tick.
    const onVisible = () => {
      if (document.visibilityState === 'visible') feed.sync();
    };
    document.addEventListener('visibilitychange', onVisible);
    return () => document.removeEventListener('visibilitychange', onVisible);
  }, [feed, polling, paused]);

  return {
    store,
    version,
    status: load.status,
    error: load.error,
    refreshing: load.refreshing,
    retrying: load.retrying === true,
    syncError,
    older,
    liveOpen,
    polling: liveDown,
    paused,
    reload: feed.reload,
    sync: feed.sync,
    loadOlder: feed.loadOlder,
    pause: feed.pause,
    resume: feed.resume,
    clear: feed.clear,
    restore: feed.restore,
  };
}

// ---------------------------------------------------------------------------
// Pieces of the page
// ---------------------------------------------------------------------------

function downloadLines(lines) {
  const name = downloadName();
  const url = URL.createObjectURL(new Blob([linesToText(lines)], { type: 'text/plain;charset=utf-8' }));
  const link = document.createElement('a');
  link.href = url;
  link.download = name;
  document.body.appendChild(link);
  link.click();
  link.remove();
  // The download reads the blob after the click returns.
  setTimeout(() => URL.revokeObjectURL(url), 10_000);
  return name;
}

/** What the gateway records and where to change it, as part of the page description. */
function LevelHint({ config }) {
  const logging = config.data?.config?.logging;
  const settings = href('/settings', { tab: 'logging' });
  if (!logging) {
    if (config.loading) return null;
    return html`<span>The level and file logging are set in the <a href=${settings}>logging settings</a>.</span>`;
  }
  const level = parseLevel(logging.level, 'info');
  return html`
    <span>
      The gateway records ${level === 'trace' ? 'every level' : html`<span class="mono">${level}</span> and more severe`}${logging.file ? ' and also writes log files' : '; file logging is off'}. <a href=${settings}>Logging settings</a>
    </span>
  `;
}

/**
 * The target filter as a pick-one control. A native select: the log scrolls
 * under it all the time, and its list must stay open while it does.
 * `targets` is [[name, count], ...], most frequent first.
 */
function TargetPicker({ targets, value, onChange }) {
  // The choices stand still while the control has the focus: a list that
  // re-sorts itself under the pointer, or between two presses of an arrow
  // key, cannot be picked from.
  const [frozen, setFrozen] = useState(null);
  const freeze = () => setFrozen((held) => held ?? targets);
  const list = (frozen ?? targets).slice(0, TARGET_CHOICES);
  const options = [{ value: '', label: 'All targets' }];
  // A target from the URL that is not among the choices is still listed, so it can be seen and unset.
  if (value && !list.some(([name]) => name === value)) options.push({ value, label: tailOf(value, TARGET_LABEL) });
  // The number of loaded lines, where there are any: with a filter on, the lines of other targets are not loaded.
  for (const [name, count] of list) options.push({ value: name, label: count > 0 ? `${tailOf(name, TARGET_LABEL)} (${formatNumber(count)})` : tailOf(name, TARGET_LABEL) });
  return html`
    <${Select}
      class="logs-target-pick"
      aria-label="Target"
      title=${value ? `Target: ${value}` : 'Show the lines of one target'}
      value=${value}
      options=${options}
      onChange=${onChange}
      onFocus=${freeze}
      onPointerDown=${freeze}
      onBlur=${() => setFrozen(null)}
    />
  `;
}

const SKELETON_WIDTHS = [62, 48, 71, 39, 55, 80, 44, 67, 52, 74, 36, 59, 46, 69];

function SkeletonLines() {
  return html`
    <div class="logs-skeleton" aria-busy="true" aria-label="Loading log lines">
      ${SKELETON_WIDTHS.map(
        (width, i) => html`
          <div class="logs-skeleton-row" key=${i}>
            <${Skeleton} width="100%" />
            <${Skeleton} width="100%" />
            <${Skeleton} width="100%" class="logs-skeleton-target" />
            <${Skeleton} width=${`${width}%`} />
          </div>
        `,
      )}
    </div>
  `;
}

/** What sits above the first line: the way to older lines, or why there are none. */
function ListTop({ feed, filtered, full, onRestore, onOlder }) {
  const { store, older } = feed;
  if (store.cleared > 0 && store.floor === null) {
    return html`
      <div class="loadmore">
        <span>Earlier lines were cleared from this view. The gateway still has them.</span>
        <${Button} size="sm" loading=${feed.refreshing} onClick=${onRestore}>Show earlier lines<//>
      </div>
    `;
  }
  if (store.floor !== null) {
    if (full) {
      return html`
        <div class="loadmore">
          <span>${formatNumber(store.maxLines)} lines are loaded, the most this view keeps. Download them, or narrow the filters to look further back.</span>
        </div>
      `;
    }
    return html`
      <div class="loadmore">
        ${older.error && html`<span class="logs-top-error" role="alert">Could not load older lines. ${sentence(older.error.message)}</span>`}
        <${Button} size="sm" loading=${older.loading} onClick=${onOlder}>${older.error ? 'Try again' : 'Load older lines'}<//>
      </div>
    `;
  }
  let text = filtered ? 'No older lines match.' : 'Start of the log. The gateway keeps its most recent lines in memory.';
  if (store.hasEarlierRuns()) text = 'The first lines here were loaded before the gateway restarted. It no longer has older ones in memory.';
  return html`
    <div class="loadmore">
      <span>${text}</span>
    </div>
  `;
}

/** The way back to the end of the log, shown over the bottom of the list. */
function EndPill({ open, paused, count, onClick }) {
  const { mounted, state } = usePresence(open, 140);
  if (!mounted) return null;
  const news = count > 0 ? `${plural(count, 'new line')}` : null;
  return html`
    <div class="logs-pill-pos" data-state=${state}>
      <button type="button" class="logs-pill" onClick=${onClick} tabindex=${open ? 0 : -1}>
        <${Icon} name=${paused ? 'play' : 'arrow-down'} size=${14} />
        ${paused
          ? html`<span>${news ? `${news}. Resume` : 'Paused. Resume'}</span>`
          : html`<span>${news ? `${news}. Jump to latest` : 'Jump to latest'}</span>`}
      </button>
    </div>
  `;
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

export default function Logs() {
  // Filters live in the URL so a link reproduces the view.
  const [levelParam, setLevel] = useQueryParam('level', 'trace');
  const [qParam, setQParam] = useQueryParam('q', '');
  const [target, setTarget] = useQueryParam('target', '');
  const level = parseLevel(levelParam);

  // The search box writes to the URL after a pause in typing. A value that
  // arrives from the URL without having been typed here (Back, a link)
  // replaces the text; our own writes coming back must not, or they would
  // eat the keystrokes typed since.
  const [text, setText] = useState(qParam);
  const written = useRef(qParam);
  if (qParam !== written.current) {
    written.current = qParam;
    if (qParam !== text.trim()) setText(qParam);
  }
  const debounced = useDebounced(text, 250);
  const applySearch = (value) => {
    const next = value.trim();
    if (next === written.current) return;
    written.current = next;
    setQParam(next);
  };
  useEffect(() => {
    applySearch(debounced);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [debounced]);

  const filter = useMemo(() => makeFilter({ level, q: qParam, target }), [level, qParam, target]);
  const filtered = isFiltered(filter);

  const feed = useLogFeed(filter);
  const { store, paused } = feed;

  const config = useResource('/config');
  useLive('config.reloaded', config.refresh);
  const gatewayLevel = config.data?.config?.logging ? parseLevel(config.data.config.logging.level, 'info') : null;

  const [wrap, setWrap] = useLocalStorage('logs.wrap', false);
  const [follow, setFollow] = useState(true);
  const [leftAt, setLeftAt] = useState(null); // last line on screen when the user left the end
  const [expanded, setExpanded] = useState(() => new Set());
  const [activeSeq, setActiveSeq] = useState(null);
  const searchInput = useRef(null);
  const list = useRef(null);
  const viewEl = useRef(null);

  // The focus is never left on <body>. Controls of this page go away under
  // it: "Load older lines" at the start of the log, "Show earlier lines"
  // once they show, the buttons of an empty view when the list returns,
  // "Clear view" (disabled) when nothing is left to clear. The control that
  // has the focus before a render and cannot have it afterwards gives it to
  // what the view then holds: the list, where the arrow keys work, or the
  // first thing an empty view offers. (A button that loads keeps the focus
  // while it does; rows are looked after in logs/list.js.)
  const focusedBefore = useRef(null);
  const activeNow = document.activeElement;
  focusedBefore.current = activeNow && activeNow !== document.body && activeNow.closest('.logs') ? activeNow : null;
  useLayoutEffect(() => {
    const held = focusedBefore.current;
    focusedBefore.current = null;
    if (!held || (held.isConnected && !held.disabled)) return;
    // Something else has taken the focus meanwhile: it stays there.
    const active = document.activeElement;
    if (active && active !== document.body && active !== held) return;
    const next = viewEl.current?.querySelector('.logs-scroll, .empty button, .empty a') ?? searchInput.current;
    next?.focus({ preventScroll: true });
  });

  // What is on screen: the buffer through the display filter, up to the
  // line the view is paused at. `held` is what waits behind the pause.
  const { shown, held } = useMemo(() => {
    const out = [];
    let after = 0;
    const pausedAt = store.paused;
    for (const line of store.lines) {
      if (!matches(line, filter)) continue;
      if (pausedAt !== null && line.seq > pausedAt) after += 1;
      else out.push(line);
    }
    return { shown: out, held: after };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [store, feed.version, filter]);

  // Targets to pick from: every one met since the page opened, the ones
  // with the most loaded lines first. A filter by target loads only that
  // target's lines; the others must stay on offer.
  const targetsMet = useRef(new Set());
  const targets = useMemo(() => {
    const counts = new Map();
    for (const line of store.lines) {
      if (line.target) counts.set(line.target, (counts.get(line.target) ?? 0) + 1);
    }
    const met = targetsMet.current;
    for (const name of counts.keys()) met.add(name);
    // A flood at trace level brings many targets; the list need not keep them all.
    if (met.size > 500) {
      met.clear();
      for (const name of counts.keys()) met.add(name);
    }
    return [...met].map((name) => [name, counts.get(name) ?? 0]).sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [store, feed.version]);

  // Where the gateway restarted: a divider above the first line shown after it.
  const restarts = store.restarts.length;
  const marks = useMemo(() => {
    const out = new Map();
    for (const restart of store.restarts) {
      const at = lowerBound(shown, restart.after + 1);
      if (at > 0 && at < shown.length) out.set(shown[at].seq, restart);
    }
    return out;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [store, shown, restarts]);

  // Row handlers must keep their identity (rows skip renders by comparing
  // props), so they read what changes through this ref.
  const latest = useRef({});
  latest.current = { shown, target, setTarget };

  const onFollow = (value) => {
    if (!value) {
      const lines = latest.current.shown;
      setLeftAt(lines.length ? lines[lines.length - 1].seq : 0);
    }
    setFollow(value);
  };

  const actions = useMemo(
    () => ({
      toggle(seq) {
        setExpanded((previous) => {
          const next = new Set(previous);
          if (!next.delete(seq)) next.add(seq);
          return next;
        });
        setActiveSeq(seq);
      },
      target(name) {
        latest.current.setTarget(latest.current.target === name ? '' : name);
      },
      activate: setActiveSeq,
      async copy(line) {
        if (await copyText(lineToText(line))) toast.success('Line copied');
        else toast.error('Could not copy the line', { description: 'The browser did not allow access to the clipboard.' });
      },
    }),
    [],
  );

  const resume = () => {
    feed.resume();
    setFollow(true);
  };
  const jumpToEnd = () => {
    setFollow(true);
    list.current?.focus();
  };
  // The pill goes away with the pause: the focus it had moves to the list.
  const resumeFromPill = () => {
    resume();
    list.current?.focus();
  };
  const clearView = () => {
    feed.clear();
    setExpanded(new Set());
    setActiveSeq(null);
    setFollow(true);
  };
  const clearFilters = () => {
    setText('');
    written.current = '';
    // Filter changes replace the history entry, so Back still leaves the page.
    setLevel('trace');
    setQParam('');
    setTarget('');
  };
  const download = () => {
    if (shown.length === 0) return;
    const name = downloadLines(shown);
    toast.success('Log download started', { description: `${name}: ${plural(shown.length, 'line')}${filtered ? ', the ones that match the filters' : ''}.` });
  };

  useHotkey('/', () => searchInput.current?.focus());

  useCommands(
    () => [
      { id: 'logs:pause', label: paused ? 'Logs: resume the tail' : 'Logs: pause the tail', group: 'Logs', icon: paused ? 'play' : 'pause', run: paused ? resume : feed.pause },
      { id: 'logs:latest', label: 'Logs: jump to latest', group: 'Logs', icon: 'arrow-down', run: jumpToEnd },
      { id: 'logs:clear', label: 'Logs: clear the view', group: 'Logs', icon: 'x', run: clearView },
      { id: 'logs:download', label: 'Logs: download loaded lines', group: 'Logs', icon: 'download', keywords: 'save export file', run: () => latest.current.download() },
    ],
    [paused],
  );
  latest.current.download = download;

  // When the buffer fills up, lines the filter hides are dropped before
  // lines it shows, every time; "full" is when even that leaves no room for
  // older ones.
  const wanted = serverQuery(filter);
  const canNarrow = (wanted.level !== '' || wanted.q !== '' || wanted.target !== '') && covers(store.query, filter) && narrows(store.query, wanted);
  store.focus = canNarrow ? wanted : null;
  const total = store.used();
  const full = total >= store.maxLines && (!canNarrow || store.wouldKeep(wanted) >= store.maxLines);

  // New lines since the user scrolled away from the end.
  const unseen = !follow && leftAt !== null ? shown.length - lowerBound(shown, leftAt + 1) : 0;
  const hasLines = shown.length > 0;
  // A view without lines has no place to keep: the list that comes back
  // (a filter taken off again) follows the end, as a new one does.
  if (!hasLines && !follow) setFollow(true);

  // -------------------------------------------------------------------------
  // The view
  // -------------------------------------------------------------------------

  const retry = () => {
    if (config.error) config.refresh();
    feed.reload();
  };

  const waiting = held > 0 ? `${plural(held, 'new line')} ${held === 1 ? 'is' : 'are'} waiting.` : 'New lines will wait until you resume.';

  let view;
  if (feed.status === 'loading') {
    view = html`<${SkeletonLines} />`;
  } else if (feed.status === 'error') {
    view = html`<${ErrorState} title="Could not load the log" error=${feed.error} onRetry=${retry} retrying=${feed.retrying} />`;
  } else if (!hasLines && store.cleared > 0 && (total === 0 || !filtered)) {
    view = html`
      <${EmptyState}
        icon="logs"
        title=${paused ? 'View cleared and paused' : 'View cleared'}
        description=${`${paused ? waiting : 'New lines appear here as the gateway writes them.'} The lines from before are still in the gateway, and in its log files when file logging is on.`}
        action=${html`
          ${paused && html`<${Button} icon="play" onClick=${resume}>Resume<//>`}
          <${Button} variant=${paused ? 'ghost' : 'secondary'} loading=${feed.refreshing} onClick=${feed.restore}>Show earlier lines<//>
        `}
      />
    `;
  } else if (!hasLines && filtered) {
    // A level filter that asks for lines the gateway does not record.
    const unrecorded = gatewayLevel !== null && filter.rank > 0 && filter.rank < levelRank(gatewayLevel);
    let description;
    if (total === 0) description = 'The gateway has no lines for these filters.';
    else if (store.floor !== null && !full) description = `None of the ${plural(total, 'loaded line')} ${total === 1 ? 'matches' : 'match'} the filters. Older lines have not been searched yet.`;
    else if (store.floor !== null) description = `None of the ${plural(total, 'loaded line')} ${total === 1 ? 'matches' : 'match'} the filters, and this view cannot hold more.`;
    else description = 'No line the gateway holds matches the filters.';
    if (paused) description += held > 0 ? ` The view is paused: ${plural(held, 'matching line')} arrived since.` : ' The view is paused: new lines will wait until you resume.';
    if (unrecorded) description += ` The gateway records ${gatewayLevel} and more severe, so there are no ${filter.level} lines.`;
    view = html`
      <${EmptyState}
        icon="filter"
        title=${feed.refreshing ? 'Searching the log' : 'No lines match'}
        description=${description}
        action=${html`
          ${store.floor !== null && !full && html`<${Button} loading=${feed.older.loading} onClick=${feed.loadOlder}>Search older lines<//>`}
          ${paused && held > 0 && html`<${Button} icon="play" onClick=${resume}>Resume<//>`}
          <${Button} variant=${(store.floor !== null && !full) || (paused && held > 0) ? 'ghost' : 'secondary'} onClick=${clearFilters}>Clear filters<//>
        `}
      />
    `;
  } else if (!hasLines && paused) {
    view = html`
      <${EmptyState}
        icon="logs"
        title="Paused before the first line"
        description=${`The log was empty when the view was paused. ${waiting}`}
        action=${html`<${Button} icon="play" onClick=${resume}>Resume<//>`}
      />
    `;
  } else if (!hasLines) {
    view = html`
      <${EmptyState}
        icon="logs"
        title="No log lines yet"
        description=${`Lines appear here as the gateway writes them.${gatewayLevel && levelRank(gatewayLevel) > levelRank('info') ? ` It records only ${gatewayLevel === 'error' ? 'errors' : 'warnings and errors'} at the moment.` : ''}`}
        action=${store.floor !== null ? html`<${Button} loading=${feed.older.loading} onClick=${feed.loadOlder}>Load older lines<//>` : null}
      />
    `;
  } else {
    view = html`
      <${LogList}
        lines=${shown}
        needle=${filter.needle}
        expanded=${expanded}
        activeSeq=${activeSeq}
        targetOn=${filter.target}
        marks=${marks}
        follow=${follow}
        onFollow=${onFollow}
        layoutKey=${wrap ? 'wrap' : 'single'}
        actions=${actions}
        controls=${list}
        top=${html`<${ListTop} feed=${feed} filtered=${filtered} full=${full} onRestore=${feed.restore} onOlder=${feed.loadOlder} />`}
      />
    `;
  }

  // -------------------------------------------------------------------------
  // Chrome
  // -------------------------------------------------------------------------

  let stream;
  if (paused) {
    const detail = held > 0 ? `${plural(held, 'new line')} waiting${store.heldLost > 0 ? ', older ones dropped' : ''}` : 'new lines will wait';
    stream = html`<${StatusLamp} tone="off" label="Paused" detail=${detail} />`;
  } else if (feed.liveOpen) stream = html`<${StatusLamp} tone="clear" label="Live" pulse detail=${follow || !hasLines ? null : 'not following'} />`;
  else if (feed.polling) stream = html`<${StatusLamp} tone="caution" label=${`Checking every ${POLL_MS / 1000}s`} detail="live connection is down" />`;
  else stream = html`<${StatusLamp} tone="info" label="Connecting" />`;

  const first = shown[0];
  const footer = html`
    <div class="logs-foot-state">${stream}${feed.refreshing && html`<${Spinner} label="Loading lines for the new filter" />`}</div>
    <div class="logs-foot-count">
      <span class="num">${filtered ? `${formatNumber(shown.length)} of ${formatNumber(total)}` : formatNumber(shown.length)}</span> ${filtered ? (total === 1 ? 'line shown' : 'lines shown') : shown.length === 1 ? 'line' : 'lines'}${first && first.at != null ? html`<span class="logs-foot-since"> since <span class="num">${formatTime(first.at)}</span></span>` : null}
    </div>
  `;

  return html`
    <${Page}
      class="logs"
      title="Logs"
      description=${html`<span class="hide-phone">The gateway’s own log, as it is written. </span><${LevelHint} config=${config} />`}
      actions=${html`
        <${Button} icon=${paused ? 'play' : 'pause'} disabled=${feed.status !== 'ready'} onClick=${paused ? resume : feed.pause}>
          ${paused ? 'Resume' : 'Pause'}
        <//>
        <${Button} icon="x" disabled=${feed.status !== 'ready' || store.lines.length === 0} onClick=${clearView}>Clear view<//>
        <${Button} icon="download" disabled=${!hasLines} onClick=${download}>Download<//>
      `}
    >
      <${Panel} flush class="logs-panel" footer=${footer}>
        <div class="logs-toolbar">
          <${Segmented} label="Least severe level shown" value=${level} onChange=${setLevel} options=${LEVEL_OPTIONS} />
          <${Input}
            class="logs-search"
            type="search"
            icon="search"
            value=${text}
            onChange=${setText}
            onEnter=${() => applySearch(text)}
            onClear=${() => applySearch('')}
            clearLabel="Clear search"
            inputRef=${searchInput}
            placeholder="Search message, target and fields"
            aria-label="Search log lines"
            aria-keyshortcuts="/"
            autocomplete="off"
          />
          <${TargetPicker} targets=${targets} value=${target} onChange=${setTarget} />
          <div class="logs-tools">
            <${IconButton} icon="wrap" label=${wrap ? 'Keep each line on one row' : 'Wrap long lines'} aria-pressed=${wrap ? 'true' : 'false'} onClick=${() => setWrap(!wrap)} />
          </div>
        </div>
        ${feed.status === 'ready' &&
        feed.error &&
        html`
          <${Notice} class="logs-notice" tone="stop" title="Could not load the lines for these filters" action=${html`<${Button} size="sm" onClick=${retry}>Try again<//>`}>
            ${sentence(feed.error.message)} What is shown comes from the lines already loaded and may be incomplete.
          <//>
        `}
        ${feed.syncError &&
        !feed.error &&
        html`
          <${Notice} class="logs-notice" tone="caution" title="Could not refresh the log" action=${html`<${Button} size="sm" onClick=${feed.sync}>Try again<//>`}>
            ${sentence(feed.syncError.message)} The lines loaded so far stay on screen.
          <//>
        `}
        <div ref=${viewEl} class="logs-view" data-wrap=${wrap ? '' : undefined} data-stale=${feed.refreshing ? '' : undefined}>
          ${view}
          <${EndPill} open=${hasLines && feed.status === 'ready' && (paused || !follow)} paused=${paused} count=${paused ? held : unseen} onClick=${paused ? resumeFromPill : jumpToEnd} />
        </div>
        <p class="sr-only" id="logs-keys">Arrow keys move between lines. Enter opens a line, C copies it, Escape lets go of it. End jumps to the latest line.</p>
      <//>
    <//>
  `;
}
