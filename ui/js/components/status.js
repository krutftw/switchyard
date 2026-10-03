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

const TONE_WORDS = { clear: 'Healthy', caution: 'Warning', stop: 'Critical', info: 'In progress', off: 'Inactive' };

/**
 * A tone in a word a person would say: the name of a lamp that was given no
 * words of its own. The tone names themselves ("caution", "stop") are the
 * design system's vocabulary, not the reader's.
 */
export function toneWord(tone) {
  return TONE_WORDS[tone] ?? TONE_WORDS.off;
}

/**
 * A signal lamp, usually with its label.
 *
 * tone    see above; default "off"
 * label   visible text next to the lamp. A lamp without a label must get
 *         `title` so the state is not carried by colour alone (without one
 *         its accessible name is toneWord(tone): "Warning", "Critical").
 * detail  quieter text after the label (a countdown, a count)
 * pulse   a slow ring for things that are live right now
 * size    "md" | "lg"
 * Other props (data-*, id, aria-describedby) and `class` go to the root
 * element: the lamp itself when there is no label, else the wrapper.
 */
export function StatusLamp({ tone = 'off', label, detail, pulse = false, size = 'md', title, class: className, ...rest }) {
  const alone = label == null;
  const lamp = html`<span
    class=${cx('lamp', alone && className)}
    data-tone=${tone}
    data-size=${size === 'lg' ? 'lg' : undefined}
    data-pulse=${pulse ? '' : undefined}
    role=${label ? undefined : 'img'}
    aria-label=${label ? undefined : title || toneWord(tone)}
    aria-hidden=${label ? 'true' : undefined}
    title=${label ? undefined : title}
    ...${alone ? rest : null}
  ></span>`;
  if (alone) return lamp;
  return html`
    <span class=${cx('status', className)} title=${title} ...${rest}>
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
 * Other props (data-*, id, aria-*) go to the badge's element.
 */
export function Badge({ tone = 'neutral', mono = false, outline = false, lamp = false, title, class: className, children, ...rest }) {
  return html`
    <span
      class=${cx('badge', className)}
      data-tone=${tone === 'neutral' ? undefined : tone}
      data-mono=${mono ? '' : undefined}
      data-outline=${outline ? '' : undefined}
      title=${title}
      ...${rest}
    >
      ${lamp && html`<span class="lamp" data-tone=${tone === 'neutral' ? 'off' : tone} aria-hidden="true"></span>`}${children}
    </span>
  `;
}

/**
 * Lamp tone for an HTTP status alone: 1xx info (a 101 is a WebSocket that was
 * switched to, not a failure), 2xx clear, 429 and 3xx caution, other 4xx/5xx
 * stop, 0 or missing off.
 *
 * The status is not the whole story. A request record also says whether it
 * succeeded (`ok`): a stream that broke after its 200 is a failure with a
 * 2xx status, and a relayed WebSocket that ended well is a success with a
 * 101. A caller holding `ok` lets it decide, and uses this for the rest:
 *   record.ok === false ? 'stop' : toneForStatus(record.status)
 */
export function toneForStatus(status) {
  if (!status) return 'off';
  if (status >= 100 && status < 200) return 'info';
  if (status >= 200 && status < 300) return 'clear';
  if (status === 429 || (status >= 300 && status < 400)) return 'caution';
  return 'stop';
}

/** A keyboard key cap: html`<${Kbd}>Esc<//>`. */
export function Kbd({ children }) {
  return html`<kbd class="kbd">${children}</kbd>`;
}
