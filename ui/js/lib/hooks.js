// Shared hooks. Data hooks first, then small utilities.

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from '../../vendor/preact-htm.js';
import { api } from './api.js';
import { focusableWithin, isEditable, isFocusable, isTopOverlay, lockScroll, nextId, pushOverlay, topOverlay } from './dom.js';
import { createStore, useStore } from './store.js';

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------

/**
 * Run an async action on demand and track its state. For mutations: saving a
 * form, testing a provider, deleting a key.
 *
 *   const save = useAsync((body) => api.put('/aliases', body));
 *   <Button loading=${save.loading} onClick=${() => save.run(draft)}>Save</Button>
 *   save.error  // ApiError | null, feed it to <FormError> or useIssues()
 *
 * `run` never rejects, so call sites do not need try/catch. It resolves with
 *   - the action's result when it succeeded, or `true` when the action has
 *     no result (a 204, an empty body, a function that returns nothing);
 *   - `undefined` when it failed (the error is in `error`).
 * So `if (await save.run())` is right for everything the admin API returns.
 * Only an action that can itself resolve 0, '' or false needs
 * `(await x.run()) !== undefined`.
 *
 * `data` holds the raw result (null for a 204). A result that arrives after
 * a newer run started, or after unmount, is not stored.
 */
export function useAsync(fn) {
  const fnRef = useRef(fn);
  fnRef.current = fn;
  const seq = useRef(0);
  const alive = useRef(true);
  const [state, setState] = useState({ loading: false, error: null, data: undefined });

  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);

  const run = useCallback(async (...args) => {
    const mine = ++seq.current;
    setState((s) => ({ ...s, loading: true, error: null }));
    try {
      const data = await fnRef.current(...args);
      if (alive.current && mine === seq.current) setState({ loading: false, error: null, data });
      // Never undefined (and never null) on success: that is the failure signal.
      return data ?? true;
    } catch (error) {
      if (alive.current && mine === seq.current) setState((s) => ({ ...s, loading: false, error }));
      return undefined;
    }
  }, []);

  const reset = useCallback(() => {
    seq.current += 1;
    setState({ loading: false, error: null, data: undefined });
  }, []);

  return { ...state, run, reset };
}

/**
 * The refetch policy behind useResource, kept free of Preact so it can be
 * tested on its own (ui/tests/check.mjs).
 *
 * One request is in flight at a time. What a new call does while one is on
 * its way depends on why it was made:
 *
 *   restart()  the source changed: abort the request and load afresh
 *   poll()     a timer tick or the tab coming back: the request already on
 *              its way is fresh enough, so nothing new starts. Aborting it
 *              instead would starve a gateway that answers slower than the
 *              poll interval: no response would ever be allowed to land.
 *   refresh()  the caller knows something changed (a write, a live frame):
 *              the request on its way may predate that change, so it is left
 *              to finish and exactly one more follows it, however many
 *              refresh() calls arrive meanwhile.
 *   cancel()   abort and forget (unmount, disabled)
 *
 * Each returns a promise that resolves (never rejects) when the load it
 * stands for has settled.
 *
 * @param {{
 *   fetch: (signal: AbortSignal) => Promise<any>,
 *   onStart?: () => void,
 *   onData: (data: any) => void,
 *   onError: (error: any) => void,
 * }} handlers  onData/onError are called only for the current request; an
 *              aborted or superseded one reports nothing.
 */
export function createLoader({ fetch, onStart, onData, onError }) {
  let current = null; // { controller, promise } of the request in flight
  let trailing = null; // promise of the one queued follow-up, if any

  const start = () => {
    const mine = { controller: new AbortController(), promise: null };
    current = mine;
    onStart?.();
    mine.promise = (async () => {
      let data;
      let error;
      let failed = false;
      try {
        data = await fetch(mine.controller.signal);
      } catch (cause) {
        failed = true;
        error = cause;
      }
      if (current !== mine) return; // aborted or superseded: say nothing
      current = null;
      if (failed) onError(error);
      else onData(data);
    })();
    return mine.promise;
  };

  const cancel = () => {
    trailing = null;
    const was = current;
    current = null;
    was?.controller.abort();
  };

  return {
    restart() {
      cancel();
      return start();
    },
    poll() {
      return current ? current.promise : start();
    },
    refresh() {
      if (!current) return start();
      if (!trailing) {
        const queued = current.promise.then(() => {
          if (trailing !== queued) return undefined; // cancelled meanwhile
          trailing = null;
          // A request that began after the one we waited for is as fresh as
          // the follow-up would be.
          return current ? current.promise : start();
        });
        trailing = queued;
      }
      return trailing;
    },
    cancel,
    /** True while a request is in flight. */
    get busy() {
      return current !== null;
    },
  };
}

/**
 * Load a resource and keep it fresh.
 *
 *   const providers = useResource('/providers');
 *   const usage = useResource(['/usage/summary', { range }], { pollMs: 30_000 });
 *   const custom = useResource((signal) => api.get('/models', { signal }), { deps: [x] });
 *
 * First argument: an admin API path, a [path, query] pair, a function
 * (signal) => Promise, or null to stay idle.
 *
 * Options:
 *   pollMs        refetch on this interval while the tab is visible (0 = never)
 *   deps          extra dependencies for the function form
 *   enabled       set false to pause loading and polling
 *   keepPrevious  when the key changes, keep showing the previous key's data
 *                 (flagged with `isPrevious`) until the new key's has
 *                 arrived, instead of going back to `loading`. For a range
 *                 or group switch above a chart: the old plot stays, dimmed,
 *                 and nothing collapses into skeletons.
 *
 * Returns:
 *   data        last good value (kept while refetching and after an error)
 *   error       ApiError from the latest attempt, else null
 *   loading     true until the first load settles
 *   refreshing  a refetch is in flight while `data` is still on screen
 *   isPrevious  `data` belongs to the key before this one (only with
 *               keepPrevious; it stays true if the new key's load fails,
 *               with the failure in `error`)
 *   updatedAt   epoch ms of the last good load
 *   refresh()   refetch now; resolves when the data is as new as the call.
 *               Safe to call as often as you like (from every live frame,
 *               say): while a request is in flight the calls collapse into
 *               one follow-up request.
 *   mutate(v)   replace `data` locally (value or updater) after a write,
 *               so the UI updates before the next refetch. A refetch that
 *               was already in flight is dropped and started again: its
 *               answer predates the write and would undo the patch.
 *
 * The function form must settle: go through `api` (which times out after
 * 30 s) or reject on your own timeout. A request that never settles holds
 * back every later poll.
 *
 * A changed key resets `data` (unless keepPrevious) and aborts the request in
 * flight. The reset is visible from the very render that has the new key:
 * the hook never hands out one key's data as another's, so an effect that
 * depends on [key, data] can trust the pair. Polling never aborts: a tick
 * that finds a request still on its way is skipped, so a gateway slower than
 * `pollMs` still gets to answer. A response that lands after the key changed
 * or the component unmounted is dropped.
 */
export function useResource(source, { pollMs = 0, deps = [], enabled = true, keepPrevious = false } = {}) {
  const isFn = typeof source === 'function';
  const key = isFn || source == null ? null : JSON.stringify(source);
  const idle = source == null;
  const sourceRef = useRef(source);
  sourceRef.current = source;

  // Which source this render is for, as a number that changes whenever the
  // key or the deps do. State remembers the number it was loaded for.
  const identity = useRef(null);
  const parts = [key, idle, ...deps];
  if (identity.current === null) identity.current = { parts, id: 1 };
  else if (parts.length !== identity.current.parts.length || parts.some((part, i) => !Object.is(part, identity.current.parts[i]))) {
    identity.current = { parts, id: identity.current.id + 1 };
  }
  const id = identity.current.id;

  const [state, setState] = useState({ data: undefined, error: null, loading: !idle && enabled, refreshing: false, updatedAt: null, isPrevious: false, of: id });
  const hasData = useRef(false);
  const loadingFor = useRef(id);
  const shown = useRef(state);
  shown.current = state;

  const loader = useMemo(
    () =>
      createLoader({
        fetch(signal) {
          const src = sourceRef.current;
          if (typeof src === 'function') return src(signal);
          if (Array.isArray(src)) return api.get(src[0], { query: src[1], signal });
          return api.get(src, { signal });
        },
        onStart() {
          setState((s) => ({ ...s, loading: !hasData.current, refreshing: hasData.current }));
        },
        onData(data) {
          hasData.current = true;
          setState({ data, error: null, loading: false, refreshing: false, updatedAt: Date.now(), isPrevious: false, of: loadingFor.current });
        },
        onError(error) {
          setState((s) => ({ ...s, error, loading: false, refreshing: false }));
        },
      }),
    [],
  );

  // (Re)load when the key or deps change.
  useEffect(() => {
    if (!enabled || sourceRef.current == null) {
      setState((s) => (s.loading || s.refreshing ? { ...s, loading: false, refreshing: false } : s));
      return undefined;
    }
    // keepPrevious carries the data on screen over to the new key. Turning
    // `enabled` back on with the same key starts afresh, as it always has.
    const before = shown.current;
    const previous = keepPrevious && before.data !== undefined && (before.of !== id || before.isPrevious);
    loadingFor.current = id;
    hasData.current = previous;
    setState((s) =>
      previous
        ? { ...s, error: null, loading: false, refreshing: true, isPrevious: true, of: id }
        : { data: undefined, error: null, loading: true, refreshing: false, updatedAt: null, isPrevious: false, of: id },
    );
    loader.restart();
    return () => loader.cancel();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, idle, enabled, loader, ...deps]);

  // Poll while visible; refetch at once when the tab comes back.
  useEffect(() => {
    if (!enabled || !pollMs || sourceRef.current == null) return undefined;
    let timer = null;
    const tick = () => {
      if (document.visibilityState === 'visible') loader.poll();
    };
    const startTimer = () => {
      clearInterval(timer);
      timer = setInterval(tick, pollMs);
    };
    const onVisibility = () => {
      if (document.visibilityState === 'visible') {
        loader.poll();
        startTimer();
      }
    };
    startTimer();
    document.addEventListener('visibilitychange', onVisibility);
    return () => {
      clearInterval(timer);
      document.removeEventListener('visibilitychange', onVisibility);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, idle, enabled, pollMs, loader, ...deps]);

  // refresh() works while `enabled` is false, so the effect above may not be
  // the one that started the request in flight at unmount.
  useEffect(() => () => loader.cancel(), [loader]);

  // Takes no arguments on purpose: it is handed to onClick and onRetry.
  const refresh = useCallback(() => (sourceRef.current == null ? Promise.resolve() : loader.refresh()), [loader]);

  const mutate = useCallback(
    (next) => {
      setState((s) => ({ ...s, data: typeof next === 'function' ? next(s.data) : next }));
      // A response already on its way was produced before this change and
      // would put the old data back when it lands: drop it and ask again.
      if (loader.busy) loader.restart();
    },
    [loader],
  );

  // The effect that resets and reloads runs after this render. Until it has,
  // `state` still holds the answer for the previous source: say what the
  // effect is about to say instead of passing that answer off as this one's.
  let view = state;
  if (enabled && !idle && state.of !== id) {
    view =
      keepPrevious && state.data !== undefined
        ? { ...state, error: null, loading: false, refreshing: true, isPrevious: true }
        : { data: undefined, error: null, loading: true, refreshing: false, updatedAt: null, isPrevious: false };
  }

  return {
    data: view.data,
    error: view.error,
    loading: view.loading,
    refreshing: view.refreshing,
    isPrevious: view.isPrevious,
    updatedAt: view.updatedAt,
    refresh,
    mutate,
  };
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/** Call `fn` every `ms` milliseconds. Pass null to pause. */
export function useInterval(fn, ms) {
  const ref = useRef(fn);
  ref.current = fn;
  useEffect(() => {
    if (ms == null) return undefined;
    const timer = setInterval(() => ref.current(), ms);
    return () => clearInterval(timer);
  }, [ms]);
}

// One shared clock so every relative time on the page ticks together.
const clock = createStore(Date.now());
let clockUsers = 0;
let clockTimer = null;

/**
 * The current time, updated once a second (or slower: pass a larger step to
 * re-render less often). Use it for relative times and countdowns.
 *
 * The value is the clock's own reading at the tick that caused the render,
 * in epoch ms, not rounded to the step: `end - now` is the time that is
 * really left, so a countdown that rounds up (formatCountdown,
 * formatCountdownWords) never reads a second long. It is the same number
 * for every render until the next tick, so it is safe in dependency lists.
 */
export function useNow(stepMs = 1000) {
  useEffect(() => {
    clockUsers += 1;
    if (!clockTimer) {
      clock.replace(Date.now());
      clockTimer = setInterval(() => clock.replace(Date.now()), 1000);
    }
    return () => {
      clockUsers -= 1;
      if (clockUsers === 0) {
        clearInterval(clockTimer);
        clockTimer = null;
      }
    };
  }, []);
  // Re-render once per step; hand out the reading taken at that tick.
  const step = useStore(clock, (now) => Math.floor(now / stepMs));
  const reading = useRef(null);
  // A component that mounts between two ticks reads the time itself: the
  // shared clock's last reading is up to a second old by then.
  if (reading.current === null) reading.current = { step, now: Date.now() };
  else if (reading.current.step !== step) reading.current = { step, now: Math.max(clock.get(), reading.current.now) };
  return reading.current.now;
}

/** A value that trails `value` by `ms`: for search boxes that query as you type. */
export function useDebounced(value, ms = 250) {
  const [debounced, setDebounced] = useState(value);
  useEffect(() => {
    const timer = setTimeout(() => setDebounced(value), ms);
    return () => clearTimeout(timer);
  }, [value, ms]);
  return debounced;
}

// ---------------------------------------------------------------------------
// Browser state
// ---------------------------------------------------------------------------

/**
 * State persisted in localStorage as JSON. For view preferences (collapsed
 * sidebar, wrap toggle, table density). Never store secrets with it.
 * Keys are namespaced "sy.<key>".
 */
export function useLocalStorage(key, initial) {
  const storageKey = `sy.${key}`;
  const [value, setValue] = useState(() => {
    try {
      const raw = localStorage.getItem(storageKey);
      return raw == null ? initial : JSON.parse(raw);
    } catch {
      return initial;
    }
  });

  const set = useCallback(
    (next) => {
      setValue((prev) => {
        const resolved = typeof next === 'function' ? next(prev) : next;
        try {
          if (resolved === undefined) localStorage.removeItem(storageKey);
          else localStorage.setItem(storageKey, JSON.stringify(resolved));
        } catch {
          /* storage unavailable: the value lasts for this page load */
        }
        return resolved;
      });
    },
    [storageKey],
  );

  return [value, set];
}

/** True while the media query matches. useMediaQuery('(max-width: 720px)') */
export function useMediaQuery(query) {
  const [matches, setMatches] = useState(() => (typeof matchMedia === 'undefined' ? false : matchMedia(query).matches));
  useEffect(() => {
    const mq = matchMedia(query);
    const onChange = () => setMatches(mq.matches);
    onChange();
    mq.addEventListener('change', onChange);
    return () => mq.removeEventListener('change', onChange);
  }, [query]);
  return matches;
}

/** The phone breakpoint used across the layout CSS. */
export const PHONE_QUERY = '(max-width: 720px)';
export const useIsPhone = () => useMediaQuery(PHONE_QUERY);

const IS_MAC = typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent || '');

/** "mod+k" label for the current platform: "⌘K" or "Ctrl K". */
export function hotkeyLabel(combo) {
  return combo
    .split('+')
    .map((part) => {
      const p = part.trim().toLowerCase();
      if (p === 'mod') return IS_MAC ? '⌘' : 'Ctrl';
      if (p === 'shift') return IS_MAC ? '⇧' : 'Shift';
      if (p === 'alt') return IS_MAC ? '⌥' : 'Alt';
      if (p === 'escape') return 'Esc';
      if (p === 'enter') return 'Enter';
      return p.length === 1 ? p.toUpperCase() : p[0].toUpperCase() + p.slice(1);
    })
    .join(IS_MAC ? '' : ' ');
}

function matchCombo(event, combo) {
  const parts = combo.toLowerCase().split('+').map((p) => p.trim());
  const key = parts[parts.length - 1];
  const want = { mod: parts.includes('mod'), shift: parts.includes('shift'), alt: parts.includes('alt') };
  const mod = IS_MAC ? event.metaKey : event.ctrlKey;
  // Without "mod" in the combo neither Ctrl nor Cmd may be held.
  if (want.mod ? !mod : event.ctrlKey || event.metaKey) return false;
  if (want.alt !== event.altKey) return false;
  // Shift is only checked for letter and named keys; "?" already implies it.
  const named = key.length > 1 || /[a-z0-9]/.test(key);
  if (named && want.shift !== event.shiftKey) return false;
  return event.key.toLowerCase() === key;
}

/**
 * Bind a keyboard shortcut for as long as the component is mounted.
 *
 *   useHotkey('mod+k', openPalette);          // Cmd+K on macOS, Ctrl+K elsewhere
 *   useHotkey('/', focusSearch);              // ignored while typing in a field
 *   useHotkey('escape', close, { enabled: open, allowInInput: true });
 *
 * Shortcuts without "mod" are ignored while focus is in a text field unless
 * `allowInInput` is set. The handler receives the KeyboardEvent, which has
 * already had preventDefault() called.
 *
 * While a modal layer is open (drawer, modal, menu, command palette) the
 * keyboard belongs to that layer: a shortcut is ignored, so "/" on the page
 * behind a drawer does not pull focus out of it. A shortcut that is part of
 * what a layer shows (Ctrl+S in a drawer's form) passes `inLayer: true`; it
 * then works as usual with no layer open, and with one open only for keys
 * pressed inside the topmost layer.
 */
export function useHotkey(combo, handler, { enabled = true, allowInInput = false, inLayer = false } = {}) {
  const ref = useRef(handler);
  ref.current = handler;
  useEffect(() => {
    if (!enabled) return undefined;
    const hasMod = combo.toLowerCase().split('+').includes('mod');
    const onKey = (event) => {
      if (event.defaultPrevented || event.isComposing) return;
      if (!matchCombo(event, combo)) return;
      if (!hasMod && !allowInInput && isEditable(event.target)) return;
      const layer = topOverlay();
      if (layer && !(inLayer && layer.element && layer.element.contains(event.target))) return;
      event.preventDefault();
      ref.current(event);
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [combo, enabled, allowInInput, inLayer]);
}

// ---------------------------------------------------------------------------
// Component plumbing
// ---------------------------------------------------------------------------

/** A stable unique id for wiring labels to controls. */
export function useUid(prefix) {
  return useMemo(() => nextId(prefix), [prefix]);
}

/**
 * Track an element's content-box size. Returns [ref, { width, height }].
 * `ref` is a callback ref: put it on the element (ref=${ref}). It follows
 * the element if a re-render replaces it, which a ref object would not.
 */
export function useSize() {
  const [size, setSize] = useState({ width: 0, height: 0 });
  const watch = useRef({ el: null, observer: null, measure: null, fresh: false });
  const ref = useCallback((el) => {
    const w = watch.current;
    if (w.el === el) return;
    w.observer?.disconnect();
    w.observer = null;
    w.measure = null;
    w.el = el;
    if (!el) return;
    const measure = () => {
      const width = Math.round(el.clientWidth);
      const height = Math.round(el.clientHeight);
      setSize((s) => (s.width === width && s.height === height ? s : { width, height }));
    };
    w.measure = measure;
    w.fresh = true;
    measure();
    if (typeof ResizeObserver !== 'undefined') {
      w.observer = new ResizeObserver(measure);
      w.observer.observe(el);
    }
  }, []);
  // The ref is called while the tree is still being built, when the element
  // may not be in the document yet and measures 0. The ResizeObserver puts
  // that right in a tab that is being painted, and never in one that is not
  // (a background tab): so measure once more when the commit is done.
  useLayoutEffect(() => {
    const w = watch.current;
    if (!w.fresh) return;
    w.fresh = false;
    w.measure?.();
  });
  useEffect(() => () => watch.current.observer?.disconnect(), []);
  return [ref, size];
}

/**
 * Keep something mounted while it animates out.
 * Returns { mounted, state } where state is "open" or "closed"; put the state
 * on the element as data-state and let CSS transition between the two.
 */
export function usePresence(open, exitMs = 140) {
  const [mounted, setMounted] = useState(open);
  const [state, setState] = useState(open ? 'open' : 'closed');
  useEffect(() => {
    if (open) {
      setMounted(true);
      // Two frames: the element must paint in its closed state first so the
      // transition to "open" runs (and can be retargeted mid-flight).
      let raf2 = 0;
      const raf1 = requestAnimationFrame(() => {
        raf2 = requestAnimationFrame(() => setState('open'));
      });
      // A tab in the background gets no frames, and a layer opened there
      // would stay invisible while it already holds the focus. The timer
      // opens it without them; whichever comes first wins.
      const timer = setTimeout(() => setState('open'), PRESENCE_FALLBACK_MS);
      return () => {
        cancelAnimationFrame(raf1);
        cancelAnimationFrame(raf2);
        clearTimeout(timer);
      };
    }
    setState('closed');
    const timer = setTimeout(() => setMounted(false), exitMs);
    return () => clearTimeout(timer);
  }, [open, exitMs]);
  return { mounted: open || mounted, state: open ? state : 'closed' };
}

/** Longer than two frames at 30 Hz, short enough not to be noticed. */
const PRESENCE_FALLBACK_MS = 80;

/** Focus `el` and say whether it took. */
function focusOn(el) {
  if (!el || typeof el.focus !== 'function' || !document.contains(el)) return false;
  el.focus({ preventScroll: true });
  return document.activeElement === el;
}

/**
 * Where focus goes when the control that opened a layer is gone (a row menu
 * whose row was just deleted) or can no longer take it (disabled, hidden):
 * the nearest thing around the opener that still exists and can hold focus
 * (its row, its panel, the drawer it was in, the page's <main>). Not <body>:
 * a keyboard user would start again from the top of the document.
 * `trail` is the opener's ancestors, nearest first.
 */
function focusNear(trail) {
  for (const node of trail) {
    if (node === document.body) break;
    if (document.contains(node) && isFocusable(node) && focusOn(node)) return true;
  }
  return false;
}

/** How long after a layer closes its opener is watched for being removed. */
const OPENER_WATCH_MS = 2000;

/**
 * Behaviour every modal layer needs while `active`:
 * focus moves inside (to [data-autofocus], else the first focusable, else the
 * container), Tab cycles within, Escape calls onClose (topmost layer only),
 * the page behind stops scrolling, and focus returns to where it was.
 *
 * Options:
 *   onClose      (how) => void
 *   lock         false leaves the page scrollable (menus)
 *   dismissable  false: Escape does nothing, and the layer says so to the
 *                overlay stack (the command palette will not open over it)
 *   returnFocus  an element, a ref or a function returning an element: where
 *                focus goes if the opener cannot take it back. Without it,
 *                and when that is gone too, the nearest focusable ancestor
 *                of the opener that is still in the document gets it.
 *
 * Handing the focus back is an offer, not an order. When the layer closes
 * and the focus is neither inside it nor on <body>, someone has placed it
 * since (the page, on the neighbour of a row the dialog deleted; the user,
 * with a click): it is left there. And when the opener is gone, the
 * fallback waits FOCUS_SETTLE_MS for the page to place the focus itself
 * before it steps in, so a page that only looks for a lost focus afterwards
 * does not find the fallback's choice and take it for its own.
 */
export function useModalLayer(ref, active, { onClose, lock = true, dismissable = true, returnFocus } = {}) {
  const closeRef = useRef(onClose);
  closeRef.current = onClose;
  const dismissRef = useRef(dismissable);
  dismissRef.current = dismissable;
  const returnRef = useRef(returnFocus);
  returnRef.current = returnFocus;
  const activeRef = useRef(active);
  activeRef.current = active;

  useEffect(() => {
    if (!active) return undefined;
    const id = nextId('layer');
    const pop = pushOverlay(id, { element: () => ref.current, dismissable: () => dismissRef.current });
    const unlock = lock ? lockScroll() : null;
    const previous = document.activeElement;
    // The way back up from the opener, noted while it is still attached.
    const trail = [];
    for (let node = previous?.parentNode; node && node.nodeType === 1; node = node.parentNode) trail.push(node);
    const root = ref.current;
    if (root) handedBack.delete(root);

    const focusIn = () => {
      const root = ref.current;
      // `activeRef`: a layer told to close in the frame it opened in is
      // still waiting for this effect's clean-up, and must not pull the
      // focus back into itself meanwhile.
      if (!activeRef.current || !root || root.contains(document.activeElement)) return;
      // A layer that marks itself (menus, the palette list) takes focus as a whole.
      const target = root.matches('[data-autofocus]') ? root : (root.querySelector('[data-autofocus]') ?? focusableWithin(root)[0] ?? root);
      target.focus({ preventScroll: true });
    };
    // Focus now, so typing right after opening lands in the layer; and once
    // more on the next frame for layers whose content mounts a moment later
    // (or after a short wait, in a tab that gets no frames).
    focusIn();
    const raf = requestAnimationFrame(focusIn);
    const late = setTimeout(() => {
      // Not from under a layer that has opened on top meanwhile.
      if (isTopOverlay(id)) focusIn();
    }, PRESENCE_FALLBACK_MS);

    const onKey = (event) => {
      if (!isTopOverlay(id) || !activeRef.current) return;
      const root = ref.current;
      if (event.key === 'Escape') {
        if (event.defaultPrevented) return;
        event.preventDefault();
        if (dismissRef.current) closeRef.current?.('escape');
        return;
      }
      if (event.key !== 'Tab' || !root) return;
      const items = focusableWithin(root);
      if (items.length === 0) {
        event.preventDefault();
        root.focus();
        return;
      }
      const first = items[0];
      const last = items[items.length - 1];
      const current = document.activeElement;
      if (!root.contains(current)) {
        event.preventDefault();
        first.focus();
      } else if (event.shiftKey && current === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && current === last) {
        event.preventDefault();
        first.focus();
      }
    };
    document.addEventListener('keydown', onKey);

    return () => {
      cancelAnimationFrame(raf);
      clearTimeout(late);
      document.removeEventListener('keydown', onKey);
      pop();
      unlock?.();
      // Nothing had the focus when the layer opened: nothing to give it back to.
      if (!previous || previous === document.body) return;
      if (root) handedBack.set(root, { previous, trail, at: Date.now() });

      // The focus is this layer's to hand back only while it is still inside
      // the layer, or nowhere. Anywhere else, somebody put it there on
      // purpose after the layer was told to close (a page that moves the
      // keyboard to the neighbour of the row it just deleted, a click on
      // another control): that choice stands.
      const adrift = () => {
        const at = document.activeElement;
        return !at || at === document.body || !document.contains(at) || (root != null && root.contains(at));
      };
      if (!adrift()) return;

      const fallback = returnRef.current;
      const elsewhere = () => {
        if (focusOn(typeof fallback === 'function' ? fallback() : fallback && 'current' in fallback ? fallback.current : fallback)) return;
        // The opener may have been inside a layer that has closed since (the
        // menu item that opened this dialog): go where that layer sent focus.
        for (const node of [previous, ...trail]) {
          const earlier = handedBack.get(node);
          if (earlier && (focusOn(earlier.previous) || focusNear(earlier.trail))) return;
        }
        focusNear(trail);
      };
      // The opener is gone, and whatever removed it may be about to say
      // where the keyboard goes (those pages focus on a timer or a frame).
      // Wait a moment and step in only if the focus is still adrift: a
      // fallback that got there first would be taken for the page's choice.
      const elsewhereUnlessPlaced = () => {
        setTimeout(() => {
          if (adrift()) elsewhere();
        }, FOCUS_SETTLE_MS);
      };

      // An opener inside a layer that has itself just closed (a button in the
      // drawer that closes together with this dialog) is still in the
      // document while that layer fades out, and would take the focus with
      // it when it goes: it counts as gone.
      let dying = false;
      for (let node = previous; node && !dying; node = node.parentNode) {
        const closed = handedBack.get(node);
        dying = closed != null && Date.now() - closed.at < LAYER_FADE_MS;
      }
      if (dying || !focusOn(previous)) {
        elsewhereUnlessPlaced();
        return;
      }
      // The opener took the focus back. What the layer was for may remove it
      // a moment later (the list refetches after a delete, and the row with
      // its menu button goes): the browser then drops focus on <body>
      // without a word. Watch for that for a short while.
      if (typeof MutationObserver === 'undefined') return;
      let timer = null;
      const watcher = new MutationObserver(() => {
        if (document.contains(previous)) return;
        clearTimeout(timer);
        watcher.disconnect();
        const at = document.activeElement;
        if (!at || at === document.body) elsewhereUnlessPlaced();
      });
      timer = setTimeout(() => watcher.disconnect(), OPENER_WATCH_MS);
      watcher.observe(document.body, { childList: true, subtree: true });
    };
  }, [active, ref, lock]);
}

/**
 * Root element of a layer that has closed -> where it sent the focus. An
 * entry is dropped when the same element becomes a layer again (a menu
 * reopened while it fades out).
 */
const handedBack = new WeakMap();

/** Longer than any layer's exit animation (the drawer's is 200 ms). */
const LAYER_FADE_MS = 400;

/**
 * How long a closing layer waits for the page to place the focus itself
 * before it falls back. Longer than a frame at 30 Hz plus a zero timer,
 * which is when the pages that do it act; short enough not to be felt.
 */
const FOCUS_SETTLE_MS = 80;

/** Call `handler` when a pointer goes down outside every given ref. */
export function useOutsidePointer(refs, handler, enabled = true) {
  const handlerRef = useRef(handler);
  handlerRef.current = handler;
  const list = Array.isArray(refs) ? refs : [refs];
  const listRef = useRef(list);
  listRef.current = list;
  useEffect(() => {
    if (!enabled) return undefined;
    const onDown = (event) => {
      if (listRef.current.some((r) => r.current && r.current.contains(event.target))) return;
      handlerRef.current(event);
    };
    document.addEventListener('pointerdown', onDown, true);
    return () => document.removeEventListener('pointerdown', onDown, true);
  }, [enabled]);
}
