// Where the keyboard goes when the control that had it goes away.
//
// Several controls of the playground remove or disable themselves by what
// they start: "Stop" turns into a "Send" that is disabled while there is no
// message, "Send result and continue" leaves with the form it belongs to,
// the key field is locked while a socket is open. A browser answers that by
// dropping the focus on <body>, and a keyboard user starts again from the
// top of the page. The page says where to go instead.

/** Nothing useful has the focus: <body>, a node that left the document, or a control that was just disabled. */
export function focusAdrift() {
  const at = document.activeElement;
  return !at || at === document.body || !document.contains(at) || at.disabled === true;
}

/** Focus the first of `targets` (elements, or null) that takes it. */
export function focusFirst(...targets) {
  for (const el of targets) {
    if (!el || typeof el.focus !== 'function' || el.disabled) continue;
    el.focus({ preventScroll: true });
    if (document.activeElement === el) return true;
  }
  return false;
}

/**
 * After the render that the current event causes: if the focus was dropped,
 * put it on what `pick()` returns (an element, or a list of candidates).
 * A zero timer, not a frame: a tab in the background gets no frames.
 */
export function rescueFocus(pick) {
  setTimeout(() => {
    if (!focusAdrift()) return;
    const found = pick();
    focusFirst(...(Array.isArray(found) ? found : [found]));
  }, 0);
}

/**
 * After the render that the current event causes, put the focus on what
 * `pick()` returns, wherever it is now. For a control that is known to have
 * removed itself (after a confirm dialog the focus may still be inside the
 * closing dialog, which is not "adrift" yet; a focus placed here stays).
 */
export function placeFocus(pick) {
  setTimeout(() => {
    const found = pick();
    focusFirst(...(Array.isArray(found) ? found : [found]));
  }, 0);
}

/** The page's own landmark: the last resort, as it is for a closing dialog. */
export const pageMain = () => document.getElementById('main');
