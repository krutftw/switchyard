// Modal layers: Modal, Drawer, ConfirmDialog and the imperative confirm().
//
// All of them trap focus, close on Escape, return focus to the control that
// opened them and stop the page behind from scrolling (lib/hooks.js,
// useModalLayer). They render through a Portal, so they can be declared
// anywhere.
//
// Reach for a Drawer to inspect or edit one record next to its list, and for
// a Modal only when the task must interrupt (a short create form, a
// confirmation). Most things need neither: prefer inline editing.

import { html, useEffect, useRef, useState } from '../../vendor/preact-htm.js';
import { cx } from '../lib/dom.js';
import { useModalLayer, usePresence, useUid } from '../lib/hooks.js';
import { createStore, useStore } from '../lib/store.js';
import { Button, IconButton } from './button.js';
import { Input } from './form.js';
import { Portal } from './portal.js';
import { Notice } from './surface.js';

// ---------------------------------------------------------------------------
// Modal
// ---------------------------------------------------------------------------

/**
 * A centred dialog (a bottom sheet on phones).
 *
 *   const [open, setOpen] = useState(false);
 *   html`<${Modal} open=${open} onClose=${() => setOpen(false)} title="Create client key"
 *         footer=${html`<${Button} onClick=${close}>Cancel<//>
 *                       <${Button} variant="primary" type="submit" form="key-form">Create key<//>`}>
 *     <${Form} id="key-form" onSubmit=${create}>...<//>
 *   <//>`
 *
 * open         whether it is shown
 * onClose      called on Escape, scrim click and the close button
 * title        dialog heading; description: a line under it
 * size         "sm" (440px, default) | "md" (580px) | "lg" (820px)
 * footer       button row; primary action last
 * dismissable  false blocks every way out the layer offers itself: Escape,
 *              the scrim and the close button (set it while saving). Your
 *              own buttons in the footer are yours to disable.
 *
 * returnFocus  where focus goes on closing if the control that opened the
 *              dialog cannot take it back (it was removed, or disabled): an
 *              element, a ref, or a function returning an element. Without
 *              it the nearest focusable thing around the opener is used
 *              (its row, its drawer, the page's main region). A focus the
 *              page has placed itself by then is never taken away.
 *
 * Focus goes to the element marked autoFocus (data-autofocus), else the
 * first control.
 */
export function Modal({ open, onClose, title, description, size = 'sm', footer, dismissable = true, returnFocus, class: className, children }) {
  const ref = useRef(null);
  const titleId = useUid('modal-title');
  const { mounted, state } = usePresence(open, 140);
  useModalLayer(ref, open, { onClose, dismissable, returnFocus });
  if (!mounted) return null;
  return html`
    <${Portal}>
      <div class="scrim" data-state=${state} onClick=${() => dismissable && onClose?.('scrim')}></div>
      <div class="modal-pos">
        <div
          ref=${ref}
          class=${cx('modal', className)}
          role="dialog"
          aria-modal="true"
          aria-labelledby=${title ? titleId : undefined}
          tabindex="-1"
          data-state=${state}
          data-size=${size === 'sm' ? undefined : size}
        >
          <header class="overlay-head">
            <div class="overlay-head-text">
              ${title && html`<h2 class="overlay-title" id=${titleId}>${title}</h2>`}
              ${description && html`<p class="overlay-desc">${description}</p>`}
            </div>
            <${IconButton} icon="x" label="Close" tooltip=${false} disabled=${!dismissable} onClick=${() => onClose?.('button')} />
          </header>
          <div class="overlay-body">${children}</div>
          ${footer && html`<footer class="overlay-foot">${footer}</footer>`}
        </div>
      </div>
    <//>
  `;
}

// ---------------------------------------------------------------------------
// Drawer
// ---------------------------------------------------------------------------

/**
 * A panel that slides in from the right: the detail view of a row.
 *
 *   html`<${Drawer} open=${!!id} onClose=${() => setQuery({ open: null })}
 *         title="Request" subtitle=${id} width="640px">...<//>`
 *
 * open, onClose, title, footer, dismissable, returnFocus: as Modal
 * subtitle  monospace line under the title (an id)
 * actions   controls in the header, before the close button
 * width     CSS width on desktop (default 560px); phones use the full width
 * side      "right" (default) | "bottom" (a bottom sheet)
 *
 * Keep the open record's id in the URL (useQueryParam) so the drawer can be
 * linked to and Back closes it.
 */
export function Drawer({ open, onClose, title, subtitle, actions, footer, width, side = 'right', dismissable = true, returnFocus, class: className, children }) {
  const ref = useRef(null);
  const titleId = useUid('drawer-title');
  const { mounted, state } = usePresence(open, 200);
  useModalLayer(ref, open, { onClose, dismissable, returnFocus });
  if (!mounted) return null;
  return html`
    <${Portal}>
      <div class="scrim" data-state=${state} onClick=${() => dismissable && onClose?.('scrim')}></div>
      <aside
        ref=${ref}
        class=${cx('drawer', className)}
        role="dialog"
        aria-modal="true"
        aria-labelledby=${title ? titleId : undefined}
        tabindex="-1"
        data-state=${state}
        data-side=${side === 'bottom' ? 'bottom' : undefined}
        style=${width && side !== 'bottom' ? `--drawer-w:${width}` : undefined}
      >
        ${side === 'bottom' && html`<div class="sheet-grip" aria-hidden="true"></div>`}
        <header class="overlay-head">
          <div class="overlay-head-text">
            ${title && html`<h2 class="overlay-title" id=${titleId}>${title}</h2>`}
            ${subtitle && html`<p class="overlay-desc mono">${subtitle}</p>`}
          </div>
          <div class="row" style="--gap:var(--space-1)">
            ${actions}
            <${IconButton} icon="x" label="Close" tooltip=${false} disabled=${!dismissable} onClick=${() => onClose?.('button')} />
          </div>
        </header>
        <div class="overlay-body">${children}</div>
        ${footer && html`<footer class="overlay-foot">${footer}</footer>`}
      </aside>
    <//>
  `;
}

// ---------------------------------------------------------------------------
// ConfirmDialog
// ---------------------------------------------------------------------------

/**
 * Asks before something that cannot be undone.
 *
 *   html`<${ConfirmDialog} open=${!!target} danger
 *         title=${`Delete provider ${target?.name}?`}
 *         message="Requests for its models will fail over to other providers or be rejected."
 *         confirmLabel="Delete provider"
 *         onConfirm=${() => api.del(`/providers/${target.name}`)}
 *         onClose=${() => setTarget(null)} />`
 *
 * title         a question naming the thing ("Delete key build-bot?")
 * message       what will happen, in one or two sentences
 * confirmLabel  the action as a verb phrase, never "OK" or "Yes"
 * danger        destructive: the confirm button is red and Cancel has focus
 * onConfirm     may return a promise; the dialog shows progress, stays open
 *               and shows the error if it rejects, and closes when it resolves
 * onClose       called after a successful confirm and on cancel
 * typeToConfirm when set, the user must type this text first (for wiping
 *               usage data, for example)
 * returnFocus   as Modal: where focus goes if the control that asked is gone
 *               when the dialog closes (the row that was just deleted)
 */
export function ConfirmDialog({ open, title, message, confirmLabel = 'Confirm', cancelLabel = 'Cancel', danger = false, onConfirm, onClose, typeToConfirm, returnFocus, children }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  const [typed, setTyped] = useState('');

  useEffect(() => {
    if (open) {
      setBusy(false);
      setError(null);
      setTyped('');
    }
  }, [open]);

  const blocked = typeToConfirm != null && typed.trim() !== String(typeToConfirm);

  const run = async () => {
    if (busy || blocked) return;
    setBusy(true);
    setError(null);
    try {
      await onConfirm?.();
      onClose?.('confirmed');
    } catch (cause) {
      setError(cause);
    } finally {
      setBusy(false);
    }
  };

  return html`
    <${Modal}
      open=${open}
      onClose=${() => !busy && onClose?.('cancelled')}
      title=${title}
      dismissable=${!busy}
      returnFocus=${returnFocus}
      footer=${html`
        <${Button} disabled=${busy} data-autofocus=${danger && typeToConfirm == null ? '' : undefined} onClick=${() => onClose?.('cancelled')}>${cancelLabel}<//>
        <${Button}
          variant=${danger ? 'danger' : 'primary'}
          loading=${busy}
          disabled=${blocked}
          data-autofocus=${danger ? undefined : ''}
          onClick=${run}
        >
          ${confirmLabel}
        <//>
      `}
    >
      <div class="stack" style="--gap:var(--space-3)">
        ${message && html`<p class="muted">${message}</p>`}
        ${children}
        ${typeToConfirm != null &&
        html`<${Input}
          label=${html`Type <span class="mono">${typeToConfirm}</span> to confirm`}
          value=${typed}
          onChange=${setTyped}
          onEnter=${run}
          mono
          autoFocus
          autocomplete="off"
        />`}
        ${error && html`<${Notice} tone="stop" title="That did not work">${error.message || String(error)}<//>`}
      </div>
    <//>
  `;
}

// ---------------------------------------------------------------------------
// confirm(): the same dialog, without keeping state in the page
// ---------------------------------------------------------------------------

const confirmStore = createStore({ request: null });

/**
 * Ask and wait for the answer.
 *
 *   const ok = await confirm({
 *     danger: true,
 *     title: `Revoke key ${key.name}?`,
 *     message: 'Applications using it will get 401 from now on.',
 *     confirmLabel: 'Revoke key',
 *     action: () => api.del(`/keys/${key.id}`),   // optional, runs in the dialog
 *   });
 *   if (ok) { toast.success('Key revoked'); keys.refresh(); }
 *
 * Resolves true once confirmed (and, with `action`, once it succeeded), false
 * when cancelled. Needs <ConfirmHost /> mounted, which the app shell does.
 *
 * `returnFocus` (an element, a ref, or a function returning an element) says
 * where focus goes when the control that asked is gone by the time the
 * dialog closes; without it the nearest focusable thing around that control
 * gets it.
 */
export function confirm(options) {
  return new Promise((resolve) => {
    const previous = confirmStore.get().request;
    previous?.resolve(false);
    confirmStore.set({ request: { options, resolve } });
  });
}

/** Mounted once by the shell; renders the dialog for confirm(). */
export function ConfirmHost() {
  const { request } = useStore(confirmStore);
  // Keep the last request while the dialog animates out.
  const last = useRef(null);
  if (request) last.current = request;
  const shown = request ?? last.current;
  if (!shown) return null;
  const { options } = shown;
  return html`
    <${ConfirmDialog}
      open=${!!request}
      title=${options.title}
      message=${options.message}
      confirmLabel=${options.confirmLabel}
      cancelLabel=${options.cancelLabel}
      danger=${options.danger}
      typeToConfirm=${options.typeToConfirm}
      returnFocus=${options.returnFocus}
      onConfirm=${options.action}
      onClose=${(reason) => {
        if (!request) return;
        confirmStore.set({ request: null });
        request.resolve(reason === 'confirmed');
      }}
    />
  `;
}
