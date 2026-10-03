// Toasts: brief confirmation that something happened.
//
//   import { toast } from '../components/toast.js';
//   toast.success('Provider saved');
//   toast.error('Could not delete the key', { description: error.message });
//   toast.info('Config reloaded from disk');
//   toast.warning('Restart needed', { description: 'The listener address changed.' });
//   toast.success('Key revoked', { action: { label: 'Undo', onClick: restore } });
//
// Call it from anywhere; no hook, no context. <Toaster /> is mounted once by
// the app shell.
//
// A toast is for the result of something the user just did. Problems the
// user must act on belong on the page (Notice, FormError, ErrorState), where
// they do not disappear.
//
// A toast that closes while it has the keyboard focus (its action, its
// Dismiss button, Escape) hands the focus to the next toast, else back to
// where it was before it came into the toasts, else to the page's <main>.

import { html, useEffect, useLayoutEffect, useRef } from '../../vendor/preact-htm.js';
import { focusableWithin, isFocusable } from '../lib/dom.js';
import { usePresence } from '../lib/hooks.js';
import { createStore, useStore } from '../lib/store.js';
import { Button, IconButton } from './button.js';
import { Icon } from './icons.js';
import { Portal } from './portal.js';

const MAX_VISIBLE = 4;
const DEFAULT_MS = { clear: 4000, info: 4000, caution: 7000, stop: 8000 };
const ICONS = { clear: 'check-circle', info: 'info', caution: 'alert', stop: 'alert-circle' };

const store = createStore([]);
let counter = 0;

function push(tone, title, { description, action, duration, id } = {}) {
  const toastId = id ?? `toast-${++counter}`;
  const entry = {
    id: toastId,
    tone,
    title,
    description,
    action,
    duration: duration ?? (action ? 9000 : DEFAULT_MS[tone]),
    closing: false,
  };
  const list = store.get();
  const existing = list.findIndex((t) => t.id === toastId);
  // Re-using an id updates the toast in place (progress, then result).
  const next = existing === -1 ? [...list, entry] : list.map((t, i) => (i === existing ? entry : t));
  // Over the cap, the oldest leave first.
  const open = next.filter((t) => !t.closing);
  const overflow = open.length - MAX_VISIBLE;
  store.replace(overflow > 0 ? next.map((t) => (open.indexOf(t) > -1 && open.indexOf(t) < overflow ? { ...t, closing: true } : t)) : next);
  return toastId;
}

function dismiss(id) {
  store.replace(store.get().map((t) => (id == null || t.id === id ? { ...t, closing: true } : t)));
}

function remove(id) {
  store.replace(store.get().filter((t) => t.id !== id));
}

/**
 * toast(title, options) is an info toast. Options:
 *   description  second line with the detail (an error message)
 *   action       { label, onClick }: one button, e.g. Undo or View
 *   duration     ms on screen; 0 keeps it until dismissed
 *   id           reuse an id to replace a toast instead of adding one
 * Returns the id. toast.dismiss(id) closes one; toast.dismiss() closes all.
 */
export function toast(title, options) {
  return push('info', title, options);
}
toast.info = (title, options) => push('info', title, options);
toast.success = (title, options) => push('clear', title, options);
toast.warning = (title, options) => push('caution', title, options);
toast.error = (title, options) => push('stop', title, options);
toast.dismiss = dismiss;

// ---------------------------------------------------------------------------
// Focus: a toast that closes while it has the focus hands it on
// ---------------------------------------------------------------------------

/** The element of each toast on screen, by id. */
const elements = new Map();

/** The last element outside the toasts that had the focus. */
let focusedBefore = null;

/** Focus `el` and say whether it took. */
function focusOn(el) {
  if (!el || typeof el.focus !== 'function' || !document.contains(el) || !isFocusable(el)) return false;
  el.focus({ preventScroll: true });
  return document.activeElement === el;
}

/**
 * The toast with `id` is closing (its own action or Dismiss button, Escape,
 * toast.dismiss) and has the focus, which would fall to <body> when it
 * leaves. Hand it on: to the next toast still open (else the one before),
 * else to the element that had the focus before the toasts got it, else to
 * the page's <main>.
 */
function handOn(id) {
  const list = store.get();
  const at = list.findIndex((t) => t.id === id);
  const others = [...list.slice(at + 1), ...list.slice(0, Math.max(at, 0)).reverse()].filter((t) => !t.closing);
  for (const other of others) {
    const el = elements.get(other.id);
    if (el && focusOn(focusableWithin(el)[0])) return;
  }
  if (focusOn(focusedBefore)) return;
  focusOn(document.querySelector('main'));
}

function ToastItem({ item }) {
  const { mounted, state } = usePresence(!item.closing, 140);
  const root = useRef(null);

  // Registered while it is on screen, for handOn.
  useLayoutEffect(() => {
    const el = root.current;
    if (el) elements.set(item.id, el);
    return () => {
      if (elements.get(item.id) === el) elements.delete(item.id);
    };
  }, [item.id, mounted]);

  // Closing with the focus inside: the browser would drop it on <body> once
  // the toast has faded out. Runs right after the closing render.
  useLayoutEffect(() => {
    if (!item.closing) return;
    const el = root.current;
    if (el && el.contains(document.activeElement)) handOn(item.id);
  }, [item.closing, item.id]);
  const remaining = useRef(item.duration);
  const startedAt = useRef(0);
  const timer = useRef(null);
  const held = useRef(false);

  const stop = () => {
    if (timer.current == null) return;
    clearTimeout(timer.current);
    timer.current = null;
    remaining.current -= Date.now() - startedAt.current;
  };

  const start = () => {
    if (item.closing || !item.duration || held.current || document.visibilityState !== 'visible') return;
    clearTimeout(timer.current);
    startedAt.current = Date.now();
    timer.current = setTimeout(() => dismiss(item.id), Math.max(remaining.current, 1000));
  };

  // The clock only runs while the toast can be read: not while hovered,
  // focused, or while the tab is in the background.
  useEffect(() => {
    remaining.current = item.duration;
    start();
    const onVisibility = () => (document.visibilityState === 'visible' ? start() : stop());
    document.addEventListener('visibilitychange', onVisibility);
    return () => {
      clearTimeout(timer.current);
      timer.current = null;
      document.removeEventListener('visibilitychange', onVisibility);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [item.id, item.duration, item.title, item.closing]);

  useEffect(() => {
    if (!mounted) remove(item.id);
  }, [mounted, item.id]);

  if (!mounted) return null;

  const hold = () => {
    held.current = true;
    stop();
  };
  const release = () => {
    held.current = false;
    start();
  };

  return html`
    <div
      ref=${root}
      class="toast"
      data-tone=${item.tone}
      data-state=${state}
      role=${item.tone === 'stop' ? 'alert' : 'status'}
      onPointerEnter=${hold}
      onPointerLeave=${release}
      onFocusCapture=${hold}
      onBlurCapture=${release}
      onKeyDown=${(event) => {
        if (event.key === 'Escape') {
          event.stopPropagation();
          dismiss(item.id);
        }
      }}
    >
      <${Icon} name=${ICONS[item.tone]} />
      <div class="toast-body">
        <div class="toast-title">${item.title}</div>
        ${item.description && html`<div class="toast-desc">${item.description}</div>`}
      </div>
      <div class="toast-actions">
        ${item.action &&
        html`<${Button}
          size="sm"
          onClick=${() => {
            item.action.onClick?.();
            dismiss(item.id);
          }}
        >
          ${item.action.label}
        <//>`}
        <${IconButton} icon="x" label="Dismiss" size="sm" tooltip=${false} onClick=${() => dismiss(item.id)} />
      </div>
    </div>
  `;
}

function ToastList() {
  const items = useStore(store);
  const list = useRef(null);
  // Remember where the focus was before it came into the toasts, to give it
  // back there when the last toast holding it closes. Focus events do not
  // bubble; they do pass the document on their way down.
  useEffect(() => {
    const onFocus = (event) => {
      const target = event.target;
      if (target && target.nodeType === 1 && !list.current?.contains(target)) focusedBefore = target;
    };
    document.addEventListener('focus', onFocus, true);
    return () => {
      document.removeEventListener('focus', onFocus, true);
      focusedBefore = null;
    };
  }, []);
  return html`
    <div ref=${list} class="toasts" role="region" aria-label="Notifications">
      ${items.map((item) => html`<${ToastItem} key=${item.id} item=${item} />`)}
    </div>
  `;
}

/**
 * Mounted once by the app shell.
 *
 * The list is rendered through a Portal, into <body>. #app is its own
 * stacking context (base.css), and modals, drawers and their scrims are
 * portalled to <body> above it; a toaster left inside #app would be painted
 * underneath all of them however large its z-index. In <body> the toasts
 * rank by --z-toast, above --z-overlay: a toast raised while a drawer is
 * open (the result of saving in it) is seen, and can be dismissed.
 */
export function Toaster() {
  return html`<${Portal}><${ToastList} /><//>`;
}
