// StatusLamp and Badge: the only places lamp colours appear.
//
//   html`<${StatusLamp} tone="clear" label="Serving" />`
//   html`<${StatusLamp} tone="caution" label="Cooling down" detail="2m 10s left" />`
//   html`<${Badge} tone="stop">429<//>`
//   html`<${Badge} mono>openai-chat<//>`
//
// Tones and what they mean (use them for nothing else):
//   clear    healthy, succeeded, serving
//   caution  cooling down, degraded, needs attention soon
//   stop     failed, blocked, destructive
//   info     in progress, selected, informational
//   off      disabled, unknown, not applicable (drawn hollow)

import { html } from '../../vendor/preact-htm.js';
import { cx } from '../lib/dom.js';

export const TONES = ['clear', 'caution', 'stop', 'info', 'off'];

/**
 * A signal lamp, usually with its label.
 *
 * tone    see above; default "off"
 * label   visible text next to the lamp. A lamp without a label must get
 *         `title` so the state is not carried by colour alone.
 * detail  quieter text after the label (a countdown, a count)
 * pulse   a slow ring for things that are live right now
 * size    "md" | "lg"
 */
export function StatusLamp({ tone = 'off', label, detail, pulse = false, size = 'md', title, class: className }) {
  const lamp = html`<span
    class="lamp"
    data-tone=${tone}
    data-size=${size === 'lg' ? 'lg' : undefined}
    data-pulse=${pulse ? '' : undefined}
    role=${label ? undefined : 'img'}
    aria-label=${label ? undefined : title || tone}
    aria-hidden=${label ? 'true' : undefined}
    title=${label ? undefined : title}
  ></span>`;
  if (label == null) return lamp;
  return html`
    <span class=${cx('status', className)} title=${title}>
      ${lamp}
      <span>${label}</span>
      ${detail != null && html`<span class="status-detail">${detail}</span>`}
    </span>
  `;
}

/**
 * A small tag for a state, a protocol, a count.
 *
 * tone     "neutral" (default) or a lamp tone
 * mono     monospace, for identifiers (protocols, HTTP status, model ids)
 * outline  hairline outline instead of a wash, for quieter metadata
 * lamp     show a lamp in the badge's tone before the text
 */
export function Badge({ tone = 'neutral', mono = false, outline = false, lamp = false, title, class: className, children }) {
  return html`
    <span
      class=${cx('badge', className)}
      data-tone=${tone === 'neutral' ? undefined : tone}
      data-mono=${mono ? '' : undefined}
      data-outline=${outline ? '' : undefined}
      title=${title}
    >
      ${lamp && html`<span class="lamp" data-tone=${tone === 'neutral' ? 'off' : tone} aria-hidden="true"></span>`}${children}
    </span>
  `;
}

/** Lamp tone for an HTTP status: 2xx clear, 429 and 3xx caution, other 4xx/5xx stop, 0 off. */
export function toneForStatus(status) {
  if (!status) return 'off';
  if (status >= 200 && status < 300) return 'clear';
  if (status === 429 || (status >= 300 && status < 400)) return 'caution';
  return 'stop';
}

/** A keyboard key cap: html`<${Kbd}>Esc<//>`. */
export function Kbd({ children }) {
  return html`<kbd class="kbd">${children}</kbd>`;
}
