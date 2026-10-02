// API keys page: keeping the view when leaving it would lose something.
//
// Two things on this page must not vanish behind the user's back: unsaved
// edits in the key drawer, and the one showing of a newly created key. Their
// own close paths ask already; this covers the ways around them:
//
//   - Back, Forward, a link, an edited address or the command palette change
//     the hash. The change is undone before the router sees it, the guard is
//     asked, and if it agrees the change is made again.
//   - Navigation the page does itself without a hash change (switching the
//     drawer to another key) asks through mayLeave() first.
//   - Closing or reloading the tab gets the browser's own "Leave site?".
//   - A guard may keep the command palette shut (the created key's dialog
//     has one way out, and the palette would be a second).
//
//   const guard = useLeaveGuard(dirty, { ask: () => confirm({ ... }) });
//   guard.release();          // the user chose to leave: stop guarding
//   if (await mayLeave()) …   // before navigating without a hash change
//
// How a hash change is undone. The router (lib/router.js) follows the
// address on `hashchange` and reads location.hash when the event arrives, so
// the address has to be right again by then:
//
//   1. Where the browser has the Navigation API, its `navigate` event comes
//      before anything changes and can be cancelled. That covers links,
//      location.hash and, after a click or key press, Back and Forward.
//   2. What could not be cancelled (a traversal without a user gesture, a
//      browser without the API) has happened by the time `popstate` fires,
//      which is still before `hashchange`. The address of the guarded view
//      is written again as a new entry: after a step back that puts the
//      history exactly as it was.

import { useEffect, useMemo, useRef } from '../../../vendor/preact-htm.js';
import { href, parseHash, routeStore } from '../../lib/router.js';

/** Active guards, the most recent last. */
const guards = [];
/** The question being asked, so a second attempt does not ask twice. */
let pending = null;

const live = () => guards.filter((guard) => !guard.released);

function sameRoute(a, b) {
  if (a.path !== b.path) return false;
  const pairs = (query) => JSON.stringify(Object.entries(query).sort(([x], [y]) => (x < y ? -1 : x > y ? 1 : 0)));
  return pairs(a.query) === pairs(b.query);
}

/** Ask every guard, the newest first. Resolves with the guard that refused, or null. */
async function askAll() {
  for (const guard of [...guards].reverse()) {
    if (guard.released) continue;
    if (!(await guard.ask())) return guard;
    guard.released = true;
  }
  return null;
}

function ask() {
  if (!pending) {
    pending = askAll().finally(() => {
      pending = null;
    });
  }
  return pending;
}

/**
 * True when nothing on the page objects to leaving the current view. Asks
 * the user where a guard needs to. Call it before a navigation that does not
 * change the hash through the browser (history.replaceState).
 */
export async function mayLeave() {
  if (live().length === 0) return true;
  return (await ask()) === null;
}

/** The change was undone: ask, and make it again if the guards agree. */
function settle(redo) {
  // A second attempt while the question is open changes nothing: the first
  // one is made when the user agrees.
  if (pending) return;
  ask().then((refused) => {
    if (!refused) redo();
  });
}

function onNavigate(event) {
  if (live().length === 0) return;
  // Same-document hash changes only. history.pushState/replaceState (the
  // router's own filter changes) are not fragment navigations; leaving the
  // document is beforeunload's business.
  if (!event.hashChange || !event.cancelable) return;
  let target;
  try {
    target = new URL(event.destination.url).hash;
  } catch {
    return;
  }
  if (sameRoute(parseHash(target), routeStore.get())) return;
  event.preventDefault();
  const key = event.navigationType === 'traverse' ? event.destination.key : null;
  settle(() => {
    if (key) traverseTo(key);
    else location.hash = target;
  });
}

/** Go to a history entry by its Navigation API key. A refused step is not worth a console error. */
function traverseTo(key) {
  const step = window.navigation.traverseTo(key);
  step.committed.catch(() => {});
  step.finished.catch(() => {});
}

function onPopState() {
  const guarding = live();
  if (guarding.length === 0) return;
  const shown = routeStore.get();
  if (sameRoute(parseHash(location.hash), shown)) return;
  // The address has moved and the router's `hashchange` handler runs next.
  // Write the guarded view's address back first, with the entry's own state.
  const wanted = window.navigation?.currentEntry?.key;
  history.pushState(guarding[guarding.length - 1].state, '', href(shown.path, shown.query));
  // The entry the user was heading for is now one step behind this one. It
  // is reached by its key where the browser gives one: Chromium can ignore a
  // script's history.back() over an entry that a script pushed away from.
  settle(() => {
    if (wanted && window.navigation.entries().some((entry) => entry.key === wanted)) traverseTo(wanted);
    else history.back();
  });
}

function onBeforeUnload(event) {
  if (live().length === 0) return;
  event.preventDefault();
  // Chrome shows its prompt only when this is set.
  event.returnValue = '';
}

function onKeyDown(event) {
  if (!live().some((guard) => guard.blockPalette)) return;
  // The shell's shortcut handler leaves a prevented event alone.
  if ((event.ctrlKey || event.metaKey) && !event.altKey && event.key.toLowerCase() === 'k') event.preventDefault();
}

function add(guard) {
  if (guards.length === 0) {
    window.navigation?.addEventListener?.('navigate', onNavigate);
    window.addEventListener('popstate', onPopState);
    window.addEventListener('beforeunload', onBeforeUnload);
    window.addEventListener('keydown', onKeyDown, true);
  }
  guards.push(guard);
}

function remove(guard) {
  const at = guards.indexOf(guard);
  if (at !== -1) guards.splice(at, 1);
  if (guards.length === 0) {
    window.navigation?.removeEventListener?.('navigate', onNavigate);
    window.removeEventListener('popstate', onPopState);
    window.removeEventListener('beforeunload', onBeforeUnload);
    window.removeEventListener('keydown', onKeyDown, true);
  }
}

/**
 * Guard the current view while `active`.
 *
 * ask           () => boolean | Promise<boolean>: may the user leave? Ask
 *               them (confirm) or tell them why not (toast) and answer false.
 * blockPalette  keep Ctrl/Cmd+K from opening the command palette meanwhile
 *
 * Returns { release() }: call it when the user has chosen to leave through
 * the view's own controls, right before navigating.
 */
export function useLeaveGuard(active, { ask: askUser, blockPalette = false }) {
  const askRef = useRef(askUser);
  askRef.current = askUser;
  const current = useRef(null);

  useEffect(() => {
    if (!active) return undefined;
    const guard = { released: false, blockPalette, state: history.state, ask: () => askRef.current() };
    current.current = guard;
    add(guard);
    return () => {
      remove(guard);
      if (current.current === guard) current.current = null;
    };
  }, [active, blockPalette]);

  return useMemo(
    () => ({
      release() {
        if (current.current) current.current.released = true;
      },
    }),
    [],
  );
}

// Every module under js/pages/ has a default export (ui/tests/check.mjs).
export default useLeaveGuard;
