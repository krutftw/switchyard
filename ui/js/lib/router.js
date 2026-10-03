// Hash router. The dashboard is a static bundle served at /admin/, so routes
// live in the fragment ("#/requests/req_123?status=error") and need no server
// rewrites.
//
//   import { useRoute, navigate, href, useQueryParam } from '../lib/router.js';
//   const route = useRoute();                 // { path, segments, query }
//   navigate('/providers');                   // push
//   navigate('/requests', { query: { status: 'error' }, replace: true });
//   html`<a href=${href('/requests/' + id)}>Open</a>`
//   const [range, setRange] = useQueryParam('range', '24h', { push: true }); // Back returns to the last range
//
// Keep filters, tabs and the selected entity in the query so a link reproduces
// the view.
//
// Leave guards. A view that would lose something when the route changes
// (unsaved edits, a secret shown once) can object:
//
//   useLeaveGuard(dirty, { title: 'Discard unsaved changes to the provider?' });
//
// Every way of changing the route asks the guards first: navigate() and
// setQuery(), links, an address typed by hand, Back and Forward, and the
// shell's sign-out. See registerLeaveGuard below for how each is caught.

import { useCallback, useEffect, useMemo, useRef } from '../../vendor/preact-htm.js';
import { createStore, useStore } from './store.js';

/** "#/a/b?x=1" -> { path: "/a/b", segments: ["a","b"], query: { x: "1" } } */
export function parseHash(hash) {
  let raw = (hash || '').replace(/^#/, '');
  if (!raw.startsWith('/')) raw = `/${raw}`;
  const q = raw.indexOf('?');
  const pathPart = q === -1 ? raw : raw.slice(0, q);
  const query = {};
  if (q !== -1) {
    for (const [key, value] of new URLSearchParams(raw.slice(q + 1))) query[key] = value;
  }
  const segments = pathPart
    .split('/')
    .filter(Boolean)
    .map((s) => {
      try {
        return decodeURIComponent(s);
      } catch {
        return s;
      }
    });
  return { path: `/${segments.join('/')}`, segments, query };
}

/** Build a fragment URL for a path and optional query object. */
export function href(path, query) {
  const clean = `/${String(path).split('/').filter(Boolean).map(encodeURIComponent).join('/')}`;
  const params = new URLSearchParams();
  if (query) {
    for (const [key, value] of Object.entries(query)) {
      if (value == null || value === '' || value === false) continue;
      params.set(key, String(value));
    }
  }
  const qs = params.toString();
  return `#${clean}${qs ? `?${qs}` : ''}`;
}

/** True when two parsed routes name the same view: same path, same query (in any order). */
export function sameRoute(a, b) {
  if (a.path !== b.path) return false;
  const ka = Object.keys(a.query);
  const kb = Object.keys(b.query);
  return ka.length === kb.length && ka.every((key) => a.query[key] === b.query[key]);
}

const current = () => parseHash(typeof location === 'undefined' ? '' : location.hash);

export const routeStore = createStore(current());

/** Follow the address. A route equal to the one shown is not a change. */
function follow() {
  const next = current();
  if (!sameRoute(next, routeStore.get())) routeStore.replace(next);
}

// ---------------------------------------------------------------------------
// Leave guards
// ---------------------------------------------------------------------------

/** Registered guards, the most recent last: { fn, unload, state }. */
const guards = [];
/** The question being put to the user, so a second attempt does not ask twice. */
let asking = null;
/** A hash the guards have agreed to, and until when that holds. */
let approved = null;

const APPROVAL_MS = 3000;

function approve(hash) {
  approved = { hash, until: Date.now() + APPROVAL_MS };
}

function isApproved(hash) {
  return approved !== null && approved.hash === hash && Date.now() <= approved.until;
}

/**
 * Ask every guard, the newest first. Returns true (go), false (stay), or a
 * promise of one of them when a guard has to ask the user. A guard that
 * throws does not hold the user on the page.
 */
function consult(to, how) {
  const from = routeStore.get();
  const list = [...guards].reverse();
  let at = 0;
  const next = () => {
    while (at < list.length) {
      const guard = list[at];
      at += 1;
      // A guard taken off while an earlier one was asking has nothing to say.
      if (!guards.includes(guard)) continue;
      let verdict;
      try {
        verdict = guard.fn({ to, from, how });
      } catch (error) {
        console.error('leave guard failed', error);
        verdict = true;
      }
      if (verdict && typeof verdict.then === 'function') {
        return verdict.then(
          (ok) => (ok === false ? false : next()),
          (error) => {
            console.error('leave guard failed', error);
            return next();
          },
        );
      }
      if (verdict === false) return false;
    }
    return true;
  };
  return next();
}

/** consult(), with one question open at a time: while it is, nothing else leaves. */
function decide(to, how) {
  if (guards.length === 0) return true;
  if (asking) return false;
  const verdict = consult(to, how);
  if (verdict === true || verdict === false) return verdict;
  asking = verdict.finally(() => {
    asking = null;
  });
  return asking;
}

/**
 * May the current view be left? Resolves true when no guard objects (asking
 * the user where a guard needs to). For leaving in ways that are not a route
 * change: the shell asks before signing out (`to` is null, `how` is
 * "signout"), and a page asks before swapping what a drawer shows in place.
 *
 * @param {{ path: string, segments: string[], query: object } | null} [to]
 * @param {string} [how]
 * @returns {Promise<boolean>}
 */
export function mayLeave(to = null, how = 'other') {
  return Promise.resolve(decide(to, how));
}

/** Go to a history entry by its Navigation API key. A refused step is not worth a console error. */
function traverseTo(key) {
  const step = window.navigation.traverseTo(key);
  step.committed?.catch(() => {});
  step.finished?.catch(() => {});
}

// Where the Navigation API exists, a fragment navigation is announced before
// anything changes and can be cancelled: links, location.hash, an edited
// address and, after a click or key press, Back and Forward. Nothing moves
// until the guards have answered.
function onNavigate(event) {
  if (guards.length === 0 || !event.hashChange || !event.cancelable) return;
  let hash;
  try {
    hash = new URL(event.destination.url).hash;
  } catch {
    return;
  }
  if (isApproved(hash)) return;
  const to = parseHash(hash);
  if (sameRoute(to, routeStore.get())) return;
  const type = event.navigationType;
  const verdict = decide(to, type);
  if (verdict === true) return;
  event.preventDefault();
  if (verdict === false) return;
  const key = type === 'traverse' ? event.destination.key : null;
  verdict.then((ok) => {
    if (!ok) return;
    approve(hash);
    if (key) traverseTo(key);
    else if (type === 'replace') location.replace(hash);
    else location.hash = hash;
  });
}

// Everything else arrives here with the address already changed: a browser
// without the Navigation API, or a Back the browser would not let anyone
// cancel (a traversal without a user gesture). The router owns this
// listener, so the route store does not move until the guards agree; the
// address of the view that stays is written back as a new entry, which after
// a step back leaves the history as it was, and the step is made again if
// the user agrees.
function onHashChange() {
  const next = current();
  const shown = routeStore.get();
  if (guards.length === 0 || isApproved(location.hash) || sameRoute(next, shown)) {
    approved = null;
    follow();
    return;
  }
  const verdict = decide(next, 'traverse');
  if (verdict === true) {
    follow();
    return;
  }
  const wantedHash = location.hash;
  const wantedKey = window.navigation?.currentEntry?.key ?? null;
  history.pushState(guards[guards.length - 1]?.state ?? null, '', href(shown.path, shown.query));
  if (verdict === false) return;
  verdict.then((ok) => {
    if (!ok) return;
    approve(wantedHash);
    // The entry the user was heading for is one step behind the one just
    // written. Chromium can ignore a script's history.back() over an entry a
    // script pushed away from, so it is reached by its key where there is one.
    if (wantedKey && window.navigation.entries().some((entry) => entry.key === wantedKey)) traverseTo(wantedKey);
    else history.back();
  });
}

// Closing or reloading the tab: the browser shows its own prompt.
function onBeforeUnload(event) {
  if (!guards.some((guard) => (typeof guard.unload === 'function' ? guard.unload() : guard.unload))) return;
  event.preventDefault();
  // Chrome shows its prompt only when this is set.
  event.returnValue = '';
}

if (typeof window !== 'undefined') {
  window.addEventListener('hashchange', onHashChange);
}

/**
 * Register a leave guard. Returns the function that removes it.
 *
 *   const off = registerLeaveGuard(({ to, from, how }) => {
 *     if (to && to.path === from.path) return true;   // a filter change: fine
 *     return confirm({ title: 'Discard the draft?', … });
 *   }, { unload: true });
 *
 * `fn({ to, from, how })` answers true (leave), false (stay), or a promise
 * of either. `to` and `from` are parsed routes; `to` is null when the view
 * is left without a route change (sign-out, mayLeave()). `how` is "push",
 * "replace", "traverse" (Back, Forward, or a change the browser reported
 * after the fact), "signout" or "other".
 *
 * A guard is asked for every route change, query changes included, so it
 * decides for itself which ones matter. Guards are asked newest first; the
 * first refusal ends it. While a question is open, any other attempt to
 * leave is refused without asking again.
 *
 * Options:
 *   unload   true (or a function returning true) also makes the browser ask
 *            before the tab is closed or reloaded while the guard is
 *            registered
 *
 * What is caught, and how:
 *   navigate(), setQuery(), useQueryParam setters   asked before they act
 *   links, location.hash, the address bar           the Navigation API's
 *       `navigate` event, cancelled until the guards have answered
 *   Back and Forward                                 the same event when the
 *       browser lets it be cancelled; otherwise the route store stays where
 *       it is, the address is put back, and the step is made again on a yes
 *   sign-out from the shell                          mayLeave(null, 'signout')
 * A session that ends on its own (a 401) cannot be held back.
 *
 * Use useLeaveGuard in components; this is the layer below it.
 */
export function registerLeaveGuard(fn, { unload = false } = {}) {
  const guard = { fn, unload, state: typeof history === 'undefined' ? null : history.state };
  if (guards.length === 0 && typeof window !== 'undefined') {
    window.navigation?.addEventListener?.('navigate', onNavigate);
    window.addEventListener('beforeunload', onBeforeUnload);
  }
  guards.push(guard);
  return () => {
    const at = guards.indexOf(guard);
    if (at === -1) return;
    guards.splice(at, 1);
    if (guards.length === 0 && typeof window !== 'undefined') {
      window.navigation?.removeEventListener?.('navigate', onNavigate);
      window.removeEventListener('beforeunload', onBeforeUnload);
    }
  };
}

/** The standard question, asked with the shell's confirm dialog. */
async function askToDiscard(options) {
  // Loaded on demand: lib/ does not depend on components/ at load time.
  const { confirm } = await import('../components/overlay.js');
  return confirm({
    danger: options.danger ?? true,
    title: options.title ?? 'Discard unsaved changes?',
    message: options.message ?? 'What you changed here has not been saved.',
    confirmLabel: options.confirmLabel ?? 'Discard changes',
    cancelLabel: options.cancelLabel ?? 'Keep editing',
  });
}

/** Leaving the page (another path) or the session; filters and tabs of the same page pass. */
const leavesPage = (to, from) => to === null || to.path !== from.path;

/**
 * Guard the view while `when` is true: before the route changes, the user is
 * asked whether to discard what would be lost.
 *
 *   const guard = useLeaveGuard(form.dirty, {
 *     title: 'Discard unsaved changes to the provider?',
 *     message: 'What you entered in the form has not been saved.',
 *   });
 *
 * confirmOptions:
 *   title, message, confirmLabel, cancelLabel, danger
 *            the confirm dialog (defaults: "Discard unsaved changes?",
 *            "Discard changes", "Keep editing")
 *   ask      ({ to, from, how }) => boolean | Promise<boolean>: replaces the
 *            dialog. Answer false after telling the user why (a toast) to
 *            refuse outright.
 *   matters  (to, from, how) => boolean: which changes are guarded. Default:
 *            leaving the page (a different path) and signing out; a change
 *            of the query on the same page passes. A drawer kept in the
 *            query guards itself with
 *            `matters: (to, from) => !to || to.path !== from.path || to.query.edit !== from.query.edit`.
 *   unload   false leaves closing or reloading the tab unguarded (default
 *            true: the browser shows its "Leave site?" prompt)
 *
 * Returns { release() }. Call release() when the user has already chosen to
 * leave through the view's own controls (its Discard button), right before
 * navigating, so they are not asked twice. Once the user has agreed in the
 * dialog the guard stands down by itself until `when` has been false again.
 */
export function useLeaveGuard(when, confirmOptions = {}) {
  const options = useRef(confirmOptions);
  options.current = confirmOptions;
  const live = useRef(null);

  useEffect(() => {
    if (!when) return undefined;
    const guard = { released: false };
    live.current = guard;
    const off = registerLeaveGuard(
      ({ to, from, how }) => {
        if (guard.released) return true;
        const o = options.current;
        if (!(o.matters ?? leavesPage)(to, from, how)) return true;
        const answer = typeof o.ask === 'function' ? o.ask({ to, from, how }) : askToDiscard(o);
        if (answer && typeof answer.then === 'function') {
          return answer.then((ok) => {
            if (ok) guard.released = true;
            return !!ok;
          });
        }
        if (answer) guard.released = true;
        return !!answer;
      },
      { unload: () => !guard.released && options.current.unload !== false },
    );
    return () => {
      off();
      if (live.current === guard) live.current = null;
    };
  }, [when]);

  return useMemo(
    () => ({
      release() {
        if (live.current) live.current.released = true;
      },
    }),
    [],
  );
}

// ---------------------------------------------------------------------------
// Navigation
// ---------------------------------------------------------------------------

function go(target, replace) {
  if (replace) {
    const url = new URL(location.href);
    url.hash = target;
    history.replaceState(history.state, '', url);
    follow();
  } else {
    // The hash change this causes has been decided: let it through.
    if (guards.length > 0) approve(target);
    location.hash = target;
  }
}

/**
 * Go to a route. `replace` swaps the current history entry instead of adding
 * one; use it for filter changes so Back leaves the page.
 *
 * Leave guards are asked first; when one has to ask the user, the change is
 * made after the answer (or not at all). `force: true` skips them: for the
 * step a view takes after the user has already said "discard".
 */
export function navigate(path, { query, replace = false, force = false } = {}) {
  const target = href(path, query);
  if (target === location.hash) return;
  if (force) {
    go(target, replace);
    return;
  }
  const verdict = decide(parseHash(target), replace ? 'replace' : 'push');
  if (verdict === true) go(target, replace);
  else if (verdict !== false) {
    verdict.then((ok) => {
      if (ok && target !== location.hash) go(target, replace);
    });
  }
}

/** Merge `patch` into the current query. null, "" and false remove a key. */
export function setQuery(patch, { replace = true, force = false } = {}) {
  const route = routeStore.get();
  navigate(route.path, { query: { ...route.query, ...patch }, replace, force });
}

export function useRoute() {
  return useStore(routeStore);
}

/**
 * One query parameter as state. Reading returns `fallback` when the key is
 * absent; writing the fallback removes the key so default views keep clean
 * URLs.
 *
 *   const [q, setQ] = useQueryParam('q', '');                          // replaces
 *   const [tab, setTab] = useQueryParam('tab', 'general', { push: true }); // a history step
 *
 * By default a change replaces the current history entry: right for what is
 * typed (a search field would otherwise add an entry per keystroke). With
 * `push: true` every change is a step of its own, so Back returns to the
 * previous value: for what the user picks from a few choices (a tab, a time
 * range, a grouping). The setter also takes options of its own for one call:
 * `setTab('raw', { replace: true })`.
 */
export function useQueryParam(key, fallback = '', { push = false } = {}) {
  const value = useStore(routeStore, (r) => r.query[key]);
  const set = useCallback(
    (next, options) => {
      // Picking what is already shown is not a step.
      const current = routeStore.get().query[key] ?? fallback;
      if (next === current && push) return;
      // The setter is handed straight to onChange, and form controls pass
      // the DOM event as a second argument: only a plain object is options.
      const own = options && Object.getPrototypeOf(options) === Object.prototype ? options : null;
      setQuery({ [key]: next === fallback ? null : next }, { replace: own?.replace ?? !push, force: own?.force ?? false });
    },
    [key, fallback, push],
  );
  return [value ?? fallback, set];
}
