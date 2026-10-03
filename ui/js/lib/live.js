// Live event client.
//
//   import { live, useLive, liveState } from '../lib/live.js';
//   const off = live.on('request.finished', (data, frame) => { ... });
//   useLive('stats', (data) => setStats(data));       // inside a component
//   const { status } = useStore(liveState);
//   useLiveGap(() => list.refresh());                 // frames may be missing
//
// The gateway pushes JSON text frames {"type": ..., "data": ...} over
// GET /admin/api/ws?ticket=... . Browsers cannot set headers on WebSockets,
// so each connection first buys a single-use ticket with POST /ws-ticket.
//
// The client reconnects on its own with exponential backoff, re-subscribes
// to the topics that have listeners, and reports its state in `liveState` so
// the top bar can show whether the page is live.
//
// A live stream can have holes. While the socket is down nothing arrives,
// and a connection that falls behind is sent a `lagged` frame ({ missed })
// in place of the events the gateway dropped for it. Either way a page that
// builds its view from frames has to load it again: that is what
// useLiveGap (or live.onGap) is for. A page that must also act the moment
// the connection is lost (stop treating what it shows as live, start
// polling) passes `onDown` to useLiveGap (or uses live.onDown) instead of
// watching liveState.status for transitions.

import { useEffect, useRef } from '../../vendor/preact-htm.js';
import { api, API_BASE, auth, ApiError } from './api.js';
import { createStore } from './store.js';

/**
 * Frame types the gateway emits (crates/admin/src/ws.rs). `lagged` is not
 * subscribed to: the gateway sends it to every connection that fell behind,
 * whatever its subscription. Listen with useLive('lagged', ({ missed }) => …)
 * or, for "reload what I show", with useLiveGap.
 */
export const TOPICS = ['hello', 'request.started', 'request.finished', 'log', 'credential', 'config.reloaded', 'stats', 'lagged'];

/** Sent whatever the subscription says; never asked for. */
const UNSOLICITED = new Set(['lagged']);

const BACKOFF_MS = [500, 1000, 2000, 4000, 8000, 15000];
/** After this many failed attempts in a row the state reads "offline". */
const OFFLINE_AFTER = 3;

/**
 * status:
 *   "idle"          not started (signed out)
 *   "connecting"    first connection in progress
 *   "open"          connected; frames are flowing
 *   "reconnecting"  lost the connection, retrying
 *   "offline"       several retries failed; still retrying
 *   "unavailable"   the gateway refused live events (403/404); not retrying
 * attempt: failed attempts since the last open connection
 * retryAt: epoch ms of the next attempt, or null
 * since:   epoch ms the current status began
 * hello:   payload of the last "hello" frame (server version and so on)
 */
export const liveState = createStore({
  status: 'idle',
  attempt: 0,
  retryAt: null,
  since: Date.now(),
  hello: null,
});

const listeners = new Map(); // pattern -> Set<fn>
const gapListeners = new Set();
const downListeners = new Set();
let socket = null;
let wanted = false;
/** This session has had an open connection before: the next one is a reconnect. */
let wasOpen = false;
let attempt = 0;
let retryTimer = null;
let generation = 0; // invalidates callbacks from superseded connections
let sentTopics = '';
let resubscribeQueued = false;

function setStatus(status, extra = {}) {
  const prev = liveState.get();
  liveState.set({ ...extra, status, since: prev.status === status ? prev.since : Date.now() });
}

function matches(pattern, type) {
  if (pattern === '*') return true;
  if (pattern.endsWith('.*')) return type.startsWith(pattern.slice(0, -1));
  return pattern === type;
}

/** The concrete topics the current listeners need. */
function wantedTopics() {
  const patterns = [...listeners.keys()].filter((p) => !UNSOLICITED.has(p));
  if (patterns.length === 0) return [];
  const topics = TOPICS.filter((topic) => !UNSOLICITED.has(topic) && (topic === 'hello' || patterns.some((p) => matches(p, topic))));
  // Unknown exact topics (a newer gateway) are passed through as written.
  for (const p of patterns) {
    if (p !== '*' && !p.endsWith('.*') && !topics.includes(p)) topics.push(p);
  }
  return topics;
}

/** Tell the gap listeners that frames may be missing. */
function announceGap(gap) {
  for (const fn of [...gapListeners]) {
    try {
      fn(gap);
    } catch (error) {
      console.error('live gap listener failed', error);
    }
  }
}

/** Tell the down listeners that the open connection was lost. */
function announceDown(down) {
  for (const fn of [...downListeners]) {
    try {
      fn(down);
    } catch (error) {
      console.error('live down listener failed', error);
    }
  }
}

function sendSubscription() {
  resubscribeQueued = false;
  if (!socket || socket.readyState !== 1) return;
  const topics = wantedTopics();
  // With no listeners the gateway's default (everything) is left alone.
  if (topics.length === 0) return;
  const key = topics.join(',');
  if (key === sentTopics) return;
  sentTopics = key;
  socket.send(JSON.stringify({ type: 'subscribe', topics }));
}

function queueResubscribe() {
  if (resubscribeQueued) return;
  resubscribeQueued = true;
  queueMicrotask(sendSubscription);
}

function dispatch(frame) {
  const type = frame.type;
  if (typeof type !== 'string') return;
  if (type === 'hello') liveState.set({ hello: frame.data ?? null });
  if (type === 'lagged') announceGap({ reason: 'lagged', missed: frame.data?.missed ?? null });
  for (const [pattern, fns] of listeners) {
    if (!matches(pattern, type)) continue;
    for (const fn of [...fns]) {
      try {
        fn(frame.data, frame);
      } catch (error) {
        // One broken listener must not stop the others.
        console.error(`live listener for "${pattern}" failed`, error);
      }
    }
  }
}

function scheduleRetry() {
  if (!wanted) return;
  attempt += 1;
  const base = BACKOFF_MS[Math.min(attempt - 1, BACKOFF_MS.length - 1)];
  const delay = Math.round(base * (0.8 + Math.random() * 0.4));
  setStatus(attempt >= OFFLINE_AFTER ? 'offline' : 'reconnecting', { attempt, retryAt: Date.now() + delay });
  clearTimeout(retryTimer);
  retryTimer = setTimeout(connect, delay);
}

async function connect() {
  clearTimeout(retryTimer);
  retryTimer = null;
  if (!wanted || socket) return;
  const mine = ++generation;
  if (attempt === 0) setStatus('connecting', { attempt: 0, retryAt: null });

  let ticket;
  try {
    ({ ticket } = await api.post('/ws-ticket', {}, { timeout: 10_000 }));
  } catch (error) {
    if (mine !== generation || !wanted) return;
    if (error instanceof ApiError && (error.status === 401 || error.status === 403 || error.status === 404)) {
      // 401 already ended the session; 403/404 mean live events are refused.
      wanted = false;
      setStatus(error.status === 401 ? 'idle' : 'unavailable', { attempt: 0, retryAt: null });
      return;
    }
    scheduleRetry();
    return;
  }
  if (mine !== generation || !wanted) return;

  const scheme = location.protocol === 'https:' ? 'wss:' : 'ws:';
  let ws;
  try {
    ws = new WebSocket(`${scheme}//${location.host}${API_BASE}/ws?ticket=${encodeURIComponent(ticket)}`);
  } catch {
    scheduleRetry();
    return;
  }
  socket = ws;
  sentTopics = '';
  let opened = false;

  ws.onopen = () => {
    if (mine !== generation) return;
    opened = true;
    // The attempt counter is cleared on the first frame, not here: a gateway
    // that accepts the socket and drops it at once must still back off.
    setStatus('open', { retryAt: null });
    sendSubscription();
    // Whatever happened while there was no connection was not seen.
    if (wasOpen) announceGap({ reason: 'reconnect', missed: null });
    wasOpen = true;
  };
  ws.onmessage = (event) => {
    if (mine !== generation || typeof event.data !== 'string') return;
    let frame;
    try {
      frame = JSON.parse(event.data);
    } catch {
      return;
    }
    if (!frame || typeof frame !== 'object') return;
    if (attempt !== 0) {
      attempt = 0;
      liveState.set({ attempt: 0 });
    }
    dispatch(frame);
  };
  ws.onclose = () => {
    if (socket === ws) socket = null;
    // A connection closed by stop() (signing out) belongs to an older
    // generation: that is not the connection going down.
    if (mine !== generation) return;
    scheduleRetry();
    // Only an open connection can go down: a socket that never opened is
    // one more failed attempt of an outage already announced (or of the
    // first connection, which nobody was relying on yet).
    if (opened) announceDown({ reason: 'down' });
  };
  // "error" is always followed by "close"; the retry is scheduled there.
  ws.onerror = () => {};
}

function start() {
  if (wanted) return;
  wanted = true;
  attempt = 0;
  connect();
}

function stop() {
  wanted = false;
  wasOpen = false;
  generation += 1;
  clearTimeout(retryTimer);
  retryTimer = null;
  attempt = 0;
  if (socket) {
    const ws = socket;
    socket = null;
    try {
      ws.close(1000);
    } catch {
      /* already closed */
    }
  }
  setStatus('idle', { attempt: 0, retryAt: null, hello: null });
}

/** Skip the backoff and try now (used when the tab or the network returns). */
function reconnectNow() {
  if (!wanted || socket) return;
  attempt = 0;
  connect();
}

/**
 * Listen for frames. `topic` is a frame type ("stats"), a prefix pattern
 * ("request.*") or "*" for everything. The handler receives (data, frame).
 * Returns an unsubscribe function.
 */
function on(topic, fn) {
  let set = listeners.get(topic);
  if (!set) {
    set = new Set();
    listeners.set(topic, set);
  }
  set.add(fn);
  queueResubscribe();
  return () => {
    set.delete(fn);
    if (set.size === 0 && listeners.get(topic) === set) listeners.delete(topic);
    queueResubscribe();
  };
}

/**
 * Listen for holes in the stream. `fn({ reason, missed })` is called when
 * frames may have been missed:
 *   reason "reconnect"  the connection is open again after being down
 *                       (not on the first connection of a session)
 *   reason "lagged"     the gateway dropped `missed` events for this
 *                       connection because it fell behind
 * Returns an unsubscribe function.
 */
function onGap(fn) {
  gapListeners.add(fn);
  return () => gapListeners.delete(fn);
}

/**
 * Listen for the connection going down. `fn({ reason: 'down' })` is called
 * once when a connection that was open is lost, before the client starts
 * retrying (liveState.status already reads "reconnecting"). It is not called
 * for a sign-out, nor again for each failed retry. Once a connection is open
 * again, the gap listeners hear `reason: "reconnect"`: the two come in pairs.
 * Returns an unsubscribe function.
 */
function onDown(fn) {
  downListeners.add(fn);
  return () => downListeners.delete(fn);
}

export const live = { start, stop, on, onGap, onDown, reconnectNow };

if (typeof window !== 'undefined') {
  window.addEventListener('online', reconnectNow);
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') reconnectNow();
  });
  // The live connection follows the session.
  auth.subscribe(({ status }) => {
    if (status === 'authenticated') start();
    else if (status === 'anonymous') stop();
  });
}

/**
 * Subscribe a component to a live topic for as long as it is mounted.
 * The handler may change between renders without re-subscribing.
 * Pass `enabled: false` to pause (for example while a table is paused).
 */
export function useLive(topic, handler, { enabled = true } = {}) {
  const ref = useRef(handler);
  ref.current = handler;
  useEffect(() => {
    if (!enabled) return undefined;
    return on(topic, (data, frame) => ref.current(data, frame));
  }, [topic, enabled]);
}

/**
 * Call `handler({ reason, missed })` when live frames may have been missed:
 * after a reconnect, and when the gateway says this connection lagged. A
 * page that keeps a list up to date from frames refetches here:
 *
 *   const requests = useResource('/requests');
 *   useLive('request.finished', prepend);
 *   useLiveGap(requests.refresh);
 *
 * `onDown({ reason: 'down' })` is called when the open connection is lost,
 * before anything is missed: the moment to stop calling the view live, or
 * to start polling until the gap handler says the connection is back.
 *
 *   useLiveGap(feed.resync, { onDown: feed.down });
 *
 * Both handlers may change between renders. `enabled: false` pauses both.
 */
export function useLiveGap(handler, { enabled = true, onDown: downHandler } = {}) {
  const ref = useRef(handler);
  ref.current = handler;
  const downRef = useRef(downHandler);
  downRef.current = downHandler;
  useEffect(() => {
    if (!enabled) return undefined;
    const offGap = onGap((gap) => ref.current?.(gap));
    const offDown = onDown((down) => downRef.current?.(down));
    return () => {
      offGap();
      offDown();
    };
  }, [enabled]);
}
