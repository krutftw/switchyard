// DOM helpers shared by components: class names, focus management, floating
// placement, the overlay stack, clipboard.

/** Join class names; falsy values are skipped. cx('a', cond && 'b') */
export function cx(...parts) {
  let out = '';
  for (const p of parts) {
    if (!p) continue;
    out += out ? ` ${p}` : p;
  }
  return out;
}

let uid = 0;
/** A document-unique id for label/control wiring. */
export function nextId(prefix = 'sy') {
  uid += 1;
  return `${prefix}-${uid}`;
}

const FOCUSABLE = [
  'a[href]',
  'button:not([disabled])',
  'input:not([disabled]):not([type="hidden"])',
  'select:not([disabled])',
  'textarea:not([disabled])',
  '[tabindex]:not([tabindex="-1"])',
  '[contenteditable="true"]',
].join(',');

/**
 * True when focus can be put on `el` from script: a control, a link, or
 * anything with a tabindex (including -1, which keeps it out of the tab
 * order but lets focus() land on it).
 */
export function isFocusable(el) {
  if (!el || el.nodeType !== 1 || typeof el.matches !== 'function') return false;
  if (el.hasAttribute('inert')) return false;
  return el.hasAttribute('tabindex') || el.matches(FOCUSABLE);
}

/** Visible, focusable descendants in tab order. */
export function focusableWithin(root) {
  if (!root) return [];
  return [...root.querySelectorAll(FOCUSABLE)].filter(
    (el) =>
      !el.hasAttribute('inert') &&
      // tabindex="-1" is focusable by script but not part of the tab order.
      el.getAttribute('tabindex') !== '-1' &&
      (el.offsetWidth > 0 || el.offsetHeight > 0 || el === document.activeElement),
  );
}

/** True when the event target is somewhere the user types text. */
export function isEditable(target) {
  if (!target || !target.tagName) return false;
  const tag = target.tagName;
  return tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || target.isContentEditable === true;
}

// ---------------------------------------------------------------------------
// Overlay stack: Escape and outside clicks act on the topmost overlay only.
// ---------------------------------------------------------------------------

const overlayStack = []; // { id, element(), dismissable() }, the topmost last

/**
 * Put a modal layer on the stack. Returns the function that takes it off.
 *
 * @param {string} id
 * @param {{ element?: () => Element | null, dismissable?: () => boolean }} [about]
 *   element      the layer's root, so others can tell whether an event came
 *                from inside it (useHotkey does)
 *   dismissable  false while the layer may not be closed or covered (a save
 *                in flight, a secret shown once)
 */
export function pushOverlay(id, { element, dismissable } = {}) {
  const entry = { id, element: element ?? (() => null), dismissable: dismissable ?? (() => true) };
  overlayStack.push(entry);
  return () => {
    const i = overlayStack.lastIndexOf(entry);
    if (i !== -1) overlayStack.splice(i, 1);
  };
}

export function isTopOverlay(id) {
  return overlayStack[overlayStack.length - 1]?.id === id;
}

export function overlayCount() {
  return overlayStack.length;
}

/**
 * The topmost modal layer (drawer, modal, menu, palette) as
 * { id, element, dismissable }, or null when none is open.
 */
export function topOverlay() {
  const top = overlayStack[overlayStack.length - 1];
  if (!top) return null;
  return { id: top.id, element: top.element(), dismissable: top.dismissable() !== false };
}

/**
 * True while any open layer is not dismissable, wherever it sits on the
 * stack: a dialog that must be dealt with first stays in charge when a menu
 * (a layer of its own, and always dismissable) is open inside it. Ask this,
 * not topOverlay().dismissable, before opening something over the page.
 */
export function overlayLocked() {
  return overlayStack.some((entry) => entry.dismissable() === false);
}

// Body scroll lock, reference counted so nested overlays behave.
let locks = 0;
let savedOverflow = '';
let savedPadding = '';

export function lockScroll() {
  if (locks === 0) {
    const gap = window.innerWidth - document.documentElement.clientWidth;
    savedOverflow = document.body.style.overflow;
    savedPadding = document.body.style.paddingRight;
    document.body.style.overflow = 'hidden';
    // Keep the layout from shifting when the scrollbar disappears.
    if (gap > 0) document.body.style.paddingRight = `${gap}px`;
  }
  locks += 1;
  let released = false;
  return () => {
    if (released) return;
    released = true;
    locks -= 1;
    if (locks === 0) {
      document.body.style.overflow = savedOverflow;
      document.body.style.paddingRight = savedPadding;
    }
  };
}

// ---------------------------------------------------------------------------
// Floating placement (menus, tooltips)
// ---------------------------------------------------------------------------

/**
 * Position a floating box next to an anchor rectangle, flipping to the
 * opposite side and clamping to the viewport when it would not fit.
 *
 * @param {DOMRect} anchor  getBoundingClientRect() of the trigger
 * @param {{width:number,height:number}} size  measured size of the floating box
 * @param {{side?: 'top'|'bottom'|'left'|'right', align?: 'start'|'center'|'end', gap?: number, margin?: number}} opts
 * @returns {{ top: number, left: number, side: string, origin: string }}
 *          viewport coordinates for position: fixed, the side actually used,
 *          and a transform-origin that points back at the anchor.
 */
export function placeFloating(anchor, size, { side = 'bottom', align = 'start', gap = 6, margin = 8 } = {}) {
  const vw = document.documentElement.clientWidth;
  const vh = window.innerHeight;

  const fits = {
    bottom: anchor.bottom + gap + size.height <= vh - margin,
    top: anchor.top - gap - size.height >= margin,
    right: anchor.right + gap + size.width <= vw - margin,
    left: anchor.left - gap - size.width >= margin,
  };
  const opposite = { bottom: 'top', top: 'bottom', left: 'right', right: 'left' };
  let used = side;
  if (!fits[side] && fits[opposite[side]]) used = opposite[side];

  let top;
  let left;
  const vertical = used === 'top' || used === 'bottom';
  if (vertical) {
    top = used === 'bottom' ? anchor.bottom + gap : anchor.top - gap - size.height;
    if (align === 'start') left = anchor.left;
    else if (align === 'end') left = anchor.right - size.width;
    else left = anchor.left + anchor.width / 2 - size.width / 2;
  } else {
    left = used === 'right' ? anchor.right + gap : anchor.left - gap - size.width;
    if (align === 'start') top = anchor.top;
    else if (align === 'end') top = anchor.bottom - size.height;
    else top = anchor.top + anchor.height / 2 - size.height / 2;
  }

  left = Math.max(margin, Math.min(left, vw - margin - size.width));
  top = Math.max(margin, Math.min(top, vh - margin - size.height));

  // Scale-in should grow from the trigger, not from the box's own centre.
  const ox = Math.max(0, Math.min(size.width, anchor.left + anchor.width / 2 - left));
  const oy = Math.max(0, Math.min(size.height, anchor.top + anchor.height / 2 - top));
  let origin;
  if (vertical) origin = `${Math.round(ox)}px ${used === 'bottom' ? 'top' : 'bottom'}`;
  else origin = `${used === 'right' ? 'left' : 'right'} ${Math.round(oy)}px`;

  return { top: Math.round(top), left: Math.round(left), side: used, origin };
}

/**
 * True when a scroll event on `scrolled` moves `anchor` on screen: the
 * document scrolled, or a box that `anchor` is inside did. For floating
 * layers that close (or would have to be placed again) when their anchor
 * moves: scrolling somewhere else on the page is none of their business.
 *
 * @param {EventTarget} scrolled  event.target of the scroll event
 * @param {Element | null} anchor
 */
export function scrollMoves(scrolled, anchor) {
  if (!anchor) return false;
  if (scrolled === document || scrolled === document.documentElement || scrolled === document.body || scrolled === window) return true;
  return typeof scrolled?.contains === 'function' && scrolled.contains(anchor);
}

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

/** Copy text. Resolves true on success. Falls back for non-secure origins. */
export async function copyText(text) {
  const value = String(text ?? '');
  try {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(value);
      return true;
    }
  } catch {
    /* fall through to the legacy path */
  }
  // http://192.168.x.x has no Clipboard API: use a temporary textarea.
  const area = document.createElement('textarea');
  area.value = value;
  area.setAttribute('readonly', '');
  area.style.cssText = 'position:fixed;top:0;left:0;opacity:0;pointer-events:none';
  document.body.appendChild(area);
  const previous = document.activeElement;
  area.select();
  let ok = false;
  try {
    ok = document.execCommand('copy');
  } catch {
    ok = false;
  }
  area.remove();
  previous?.focus?.();
  return ok;
}

// ---------------------------------------------------------------------------
// Page stylesheets
// ---------------------------------------------------------------------------

const loadedStyles = new Map();

/**
 * Load a stylesheet from ui/css/ once. Page modules call it at the top level
 * so the page never renders unstyled:
 *
 *   await loadStyles('pages/requests.css');
 *
 * Resolves when the sheet has loaded. A sheet that fails to load resolves
 * too (the page still works, just plainer) and logs a warning.
 */
export function loadStyles(path) {
  if (typeof document === 'undefined') return Promise.resolve();
  const url = new URL(`css/${path}`, document.baseURI).href;
  let pending = loadedStyles.get(url);
  if (!pending) {
    pending = new Promise((resolve) => {
      const link = document.createElement('link');
      link.rel = 'stylesheet';
      link.href = url;
      link.onload = () => resolve();
      link.onerror = () => {
        console.warn(`Could not load stylesheet ${path}`);
        resolve();
      };
      document.head.appendChild(link);
    });
    loadedStyles.set(url, pending);
  }
  return pending;
}

/** True when the user asked the OS for reduced motion. */
export function prefersReducedMotion() {
  return typeof matchMedia !== 'undefined' && matchMedia('(prefers-reduced-motion: reduce)').matches;
}
