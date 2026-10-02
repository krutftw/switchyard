// Overview: state the sections share, kept in stores so one section updating
// (a stats frame every second, a burst of finished requests) does not
// re-render the others.
//
//   serverClock  how far the gateway's clock is from this browser's, so
//                countdowns and "12s ago" are right on a machine whose clock
//                is off
//   tail         requests that finished since the page loaded, for extending
//                the traffic chart between refetches
//   attempts     upstream attempts per provider (see model.js), kept across
//                visits to other pages
//   gauges       the in-flight count the vitals show, for the activity feed
//   contact      when the gateway last sent a live frame

import { useNow } from '../../lib/hooks.js';
import { createStore, useStore } from '../../lib/store.js';
import { createAttemptLog, mergeLiveAttempts, mergeLoadedAttempts } from './model.js';

export const serverClock = createStore({ offset: 0 });

// Below this the difference is network delay, not a clock that is off.
const CLOCK_SLACK_MS = 750;

/** Tell the page what time the gateway just reported. */
export function syncClock(serverNow) {
  if (typeof serverNow !== 'number' || !Number.isFinite(serverNow)) return;
  const offset = serverNow - Date.now();
  const known = serverClock.get().offset;
  if (Math.abs(offset) < CLOCK_SLACK_MS) {
    if (known !== 0) serverClock.set({ offset: 0 });
  } else if (Math.abs(offset - known) > CLOCK_SLACK_MS) {
    serverClock.set({ offset });
  }
}

/** The gateway's time right now, without a hook. */
export const serverNow = () => Date.now() + serverClock.get().offset;

/** The gateway's time, ticking every `stepMs` (see useNow). */
export function useServerNow(stepMs = 1000) {
  // useNow rounds down to the step, which would make every countdown read up
  // to a second long. The tick only schedules the render; the time is read
  // when the render happens.
  useNow(stepMs);
  const offset = useStore(serverClock, (s) => s.offset);
  return Date.now() + offset;
}

/** Finished requests newer than the loaded series: [{ at, ok }]. */
export const tail = createStore([]);

// ---- Upstream attempts ------------------------------------------------------

// The log itself is mutated in place (it can hold thousands of entries);
// the store only counts its versions, so readers know when to look again.
const attemptLog = createAttemptLog();
export const attempts = createStore({ version: 0 });
const bump = () => attempts.set((s) => ({ version: s.version + 1 }));

/** The log, for providerAttempts(). Read it when `attempts` changes. */
export const readAttempts = () => attemptLog;

/** A page of GET /requests has arrived. */
export function loadedAttempts(page) {
  mergeLoadedAttempts(attemptLog, page?.items ?? [], page?.has_more === true, serverNow());
  bump();
}

/** Finished requests from live frames, in one batch. */
export function liveAttempts(records) {
  if (records.length === 0) return;
  const now = serverNow();
  for (const record of records) mergeLiveAttempts(attemptLog, record, now);
  bump();
}

// ---- Gauges and contact -----------------------------------------------------

/** Requests in flight as the vitals last heard it (null: not known). */
export const gauges = createStore({ inFlight: null });

let frameAt = null;

/** A live frame with numbers in it has just arrived. */
export function markFrame() {
  frameAt = Date.now();
}

/** When the last such frame arrived (this browser's clock), or null. */
export const lastFrameAt = () => frameAt;

// check.mjs asks every module under pages/ for a default export that is a
// function; this module has no component.
export default useServerNow;
