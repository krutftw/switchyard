// Button, IconButton, CopyButton, Spinner.
//
//   html`<${Button} variant="primary" icon="plus" onClick=${add}>Add provider<//>`
//   html`<${Button} loading=${save.loading} type="submit">Save<//>`
//   html`<${IconButton} icon="refresh" label="Refresh" onClick=${refresh} />`
//   html`<${CopyButton} value=${key} label="Copy key" />`

import { html, useEffect, useRef, useState } from '../../vendor/preact-htm.js';
import { copyText, cx } from '../lib/dom.js';
import { Icon } from './icons.js';
import { Tooltip } from './tooltip.js';

/**
 * A spinning ring in the current text colour.
 * @param {{ size?: 'md'|'lg', label?: string }} props  `label` is announced to screen readers.
 */
export function Spinner({ size = 'md', label, class: className }) {
  return html`<span
    class=${cx('spinner', className)}
    data-size=${size === 'lg' ? 'lg' : undefined}
    role=${label ? 'status' : undefined}
    aria-label=${label || undefined}
    aria-hidden=${label ? undefined : 'true'}
  ></span>`;
}

/**
 * The one button.
 *
 * variant   "secondary" (default) | "primary" | "ghost" | "danger" | "danger-quiet"
 *           primary: the main action of a view, one per view
 *           danger: the confirming action of a destructive dialog
 *           danger-quiet: the button that opens such a dialog
 * size      "md" (default) | "sm" | "lg"
 * icon      icon name shown before the label; iconRight after it
 * loading   shows a spinner, blocks clicks, keeps the button's width
 * href      renders a link that looks like a button
 * block     full width
 * Any other prop (type, onClick, disabled, aria-*, title, form) is passed on.
 */
export function Button({
  variant = 'secondary',
  size = 'md',
  icon,
  iconRight,
  loading = false,
  disabled = false,
  block = false,
  href,
  type = 'button',
  class: className,
  children,
  ...rest
}) {
  const iconSize = size === 'sm' ? 14 : 16;
  const content = html`
    <span class="btn-content">
      ${icon && html`<${Icon} name=${icon} size=${iconSize} />`}
      ${children}
      ${iconRight && html`<${Icon} name=${iconRight} size=${iconSize} />`}
    </span>
    ${loading && html`<span class="btn-spinner"><${Spinner} /></span>`}
  `;
  const shared = {
    class: cx('btn', className),
    'data-variant': variant,
    'data-size': size === 'md' ? undefined : size,
    'data-block': block ? '' : undefined,
    'data-loading': loading ? '' : undefined,
    'aria-busy': loading ? 'true' : undefined,
  };
  if (href != null) {
    return html`<a ...${shared} href=${disabled ? undefined : href} aria-disabled=${disabled ? 'true' : undefined} ...${rest}>${content}</a>`;
  }
  return html`<button ...${shared} type=${type} disabled=${disabled || loading} ...${rest}>${content}</button>`;
}

/**
 * A square button that shows only an icon. `label` is required: it is the
 * accessible name and the tooltip text.
 *
 * variant defaults to "ghost". Pass tooltip=${false} when the label is
 * already visible next to the button.
 */
export function IconButton({ icon, label, variant = 'ghost', size = 'md', tooltip = true, tooltipSide = 'bottom', iconSize, class: className, ...rest }) {
  const button = html`
    <${Button} variant=${variant} size=${size} class=${cx('icon-btn', className)} aria-label=${label} ...${rest}>
      <${Icon} name=${icon} size=${iconSize ?? (size === 'sm' ? 14 : 16)} />
    <//>
  `;
  if (!tooltip) return button;
  // The label is already the accessible name: no aria-describedby needed.
  return html`<${Tooltip} content=${label} side=${tooltipSide} describe=${false}>${button}<//>`;
}

/**
 * Copies `value` to the clipboard and confirms with a tick for a moment.
 *
 * value     string, or a function returning a string / Promise<string>
 *           (for secrets that are fetched only when asked for)
 * label     accessible name and tooltip, default "Copy"
 * children  when given, renders a labelled button instead of an icon button
 */
export function CopyButton({ value, label = 'Copy', size = 'sm', variant = 'ghost', onCopied, children, ...rest }) {
  const [state, setState] = useState('idle'); // idle | done | failed
  const timer = useRef(null);
  useEffect(() => () => clearTimeout(timer.current), []);

  const copy = async (event) => {
    event.stopPropagation();
    let ok = false;
    try {
      const text = typeof value === 'function' ? await value() : value;
      ok = text != null && (await copyText(text));
    } catch {
      ok = false;
    }
    setState(ok ? 'done' : 'failed');
    if (ok) onCopied?.();
    clearTimeout(timer.current);
    timer.current = setTimeout(() => setState('idle'), 1600);
  };

  const icon = state === 'done' ? 'check' : state === 'failed' ? 'alert-circle' : 'copy';
  const text = state === 'done' ? 'Copied' : state === 'failed' ? 'Copy failed' : label;

  if (children != null) {
    return html`
      <${Button} variant=${variant} size=${size} icon=${icon} onClick=${copy} ...${rest}>
        ${state === 'idle' ? children : text}
        <span class="sr-only" role="status">${state === 'idle' ? '' : text}</span>
      <//>
    `;
  }
  return html`
    <${IconButton} icon=${icon} label=${text} variant=${variant} size=${size} onClick=${copy} ...${rest} />
    <span class="sr-only" role="status">${state === 'idle' ? '' : text}</span>
  `;
}
