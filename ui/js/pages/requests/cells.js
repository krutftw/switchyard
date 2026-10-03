// Requests page: the table's columns and the small pieces its cells are
// made of.
//
// A row is two lines tall. The first line of a cell is what the column is
// about; the second, smaller line is the detail that used to need a column
// of its own or a tooltip: the clock time, the number of attempts, the
// upstream model, the mode and transport, the credential, the time to first
// byte, the cost. That way every field of the list is on screen at laptop
// widths, and nothing depends on hovering.
//
// The table can hold a few thousand rows, so a cell is as cheap as it can
// be: plain markup, no component with hooks per cell except the two that
// must tick (When, Elapsed). Hover detail is one delegated tooltip for the
// whole table (HoverTips) reading `data-tip` attributes, instead of a
// Tooltip component in every cell.

import { Component, h, html, useEffect, useLayoutEffect, useRef, useState } from '../../../vendor/preact-htm.js';
import { Badge, Icon, Portal, StatusLamp, toneForStatus } from '../../components/index.js';
import { placeFloating } from '../../lib/dom.js';
import { DASH, formatCurrency, formatDuration, formatRelativeTime, formatTime, formatTimestamp, formatTokens, plural } from '../../lib/format.js';
import { useNow } from '../../lib/hooks.js';
import { shallowEqual } from '../../lib/store.js';
import { MODES, NO_MODEL, durationTip, modelTip, providerTip, routeTip, statusTip, usageCounted, usageTip } from './record.js';

// ---------------------------------------------------------------------------
// Pieces
// ---------------------------------------------------------------------------

/**
 * A component that re-renders only when its props change (shallow
 * comparison). The vendored Preact has no memo().
 */
export function memo(Inner) {
  return class Memo extends Component {
    shouldComponentUpdate(next) {
      return !shallowEqual(this.props, next);
    }

    render(props) {
      return h(Inner, props);
    }
  };
}

/**
 * "12s ago" over the clock time, ticking. Each instance follows the shared
 * clock at the pace its age needs: every second in the first minute, every
 * minute after an hour.
 */
function When({ at }) {
  const age = Date.now() - at;
  const step = age < 60_000 ? 1000 : age < 3_600_000 ? 10_000 : 60_000;
  const now = useNow(step);
  return html`
    <span class="req-two req-when" data-tip=${formatTimestamp(at)}>
      <span class="req-l1"><time class="req-time" datetime=${new Date(at).toISOString()}>${formatRelativeTime(at, Math.max(now, at))}</time></span>
      <span class="req-l2 req-clock num">${formatTime(at)}</span>
    </span>
  `;
}

/** Time since `since`, ticking: the duration of a request still in flight. */
export function Elapsed({ since }) {
  const now = useNow(1000);
  return formatDuration(Math.max(0, now - since));
}

/**
 * Lamp tone of a finished request. `ok` decides, not the status alone: a
 * relayed WebSocket session ends with status 101 whether it was closed in
 * order (clear) or broke off (stop), and a stream that failed after its 200
 * is not a success either.
 */
export function statusTone(record) {
  if (record.ok) return 'clear';
  const tone = toneForStatus(record.status);
  return tone === 'clear' ? 'stop' : tone;
}

export function ModeBadge({ mode }) {
  const meta = MODES[mode];
  if (!meta) return null;
  return html`<${Badge} tone=${meta.tone} outline=${meta.outline}>${meta.label}<//>`;
}

/** The upstream protocol of a record when it is not the client's, else null. */
const translatedTo = (record) => (record.upstream_protocol && record.upstream_protocol !== record.client_protocol ? record.upstream_protocol : null);

/** client protocol, then the upstream one when it differs. */
function Protocols({ record }) {
  const upstream = translatedTo(record);
  return html`
    <span class="req-protocols mono">
      <span class="req-protocol-client">${record.client_protocol ?? DASH}</span>
      ${upstream && html`<${Icon} name="arrow-right" size=${12} class="req-arrow" label="to" /><span class="req-clip">${upstream}</span>`}
    </span>
  `;
}

/** One line: the protocols, then the mode. Used by the detail drawer. */
export function ProtocolRoute({ record }) {
  return html`
    <span class="req-route">
      <${Protocols} record=${record} />
      <${ModeBadge} mode=${record.mode} />
    </span>
  `;
}

// ---------------------------------------------------------------------------
// Columns
// ---------------------------------------------------------------------------

/** A cell with nothing to say (yet). The card layout leaves these out. */
const none = () => html`<span class="req-two req-none"><span class="req-l1 faint">${DASH}</span></span>`;

// Column widths are not set here: css/pages/requests.css lays every row out
// on one grid (--req-cols) and decides what fits the panel. The stylesheet
// addresses the cells by position, so keep the order in step with it.
/** The columns of the request table, in display order. */
export const COLUMNS = [
  {
    key: 'started_at',
    header: 'Time',
    render: (r) => html`<${When} at=${r.started_at} />`,
  },
  {
    key: 'status',
    header: 'Status',
    render: (r) => {
      if (r.in_flight) {
        return html`<span class="req-two req-status"><span class="req-l1"><${StatusLamp} tone="info" pulse label="In flight" /></span></span>`;
      }
      const attempts = r.attempts?.length ?? 0;
      return html`
        <span class="req-two req-status" data-tip=${statusTip(r)}>
          <span class="req-l1"><${StatusLamp} tone=${statusTone(r)} label=${html`<span class="num">${r.status}</span>`} /></span>
          ${attempts > 1 && html`<span class="req-l2"><${Badge} outline class="req-attempts">${plural(attempts, 'attempt')}<//></span>`}
        </span>
      `;
    },
  },
  {
    key: 'requested_model',
    header: 'Model',
    primary: true,
    render: (r) => {
      const upstream = r.upstream_model && r.upstream_model !== r.requested_model ? r.upstream_model : null;
      return html`
        <span class="req-two req-model" data-tip=${modelTip(r)}>
          <span class="req-l1">${r.requested_model ? html`<span class="req-clip mono">${r.requested_model}</span>` : html`<span class="req-clip faint">${NO_MODEL}</span>`}</span>
          ${upstream && html`<span class="req-l2"><${Icon} name="arrow-right" size=${12} class="req-arrow" label="sent upstream as" /><span class="req-clip mono">${upstream}</span></span>`}
        </span>
      `;
    },
  },
  {
    key: 'protocol',
    header: 'Protocol',
    render: (r) => html`
      <span class="req-two req-how" data-tip=${routeTip(r)}>
        <span class="req-l1"><${Protocols} record=${r} /></span>
        <span class="req-l2"><${ModeBadge} mode=${r.mode} /><span class="req-transport mono">${r.transport ?? DASH}</span></span>
      </span>
    `,
  },
  {
    key: 'provider',
    header: 'Provider',
    render: (r) => {
      if (r.in_flight) return html`<span class="req-two"><span class="req-l1 faint">Routing</span></span>`;
      if (!r.provider) return html`<span class="req-two" data-tip=${providerTip(r)}><span class="req-l1 faint">Not routed</span></span>`;
      const credential = r.credential_label && r.credential_label !== r.provider ? r.credential_label : null;
      return html`
        <span class="req-two req-provider" data-tip=${providerTip(r)}>
          <span class="req-l1"><span class="req-clip mono">${r.provider}</span></span>
          ${credential && html`<span class="req-l2"><span class="req-clip mono">${credential}</span></span>`}
        </span>
      `;
    },
  },
  {
    key: 'key',
    header: 'Client key',
    render: (r) =>
      r.client?.key_name
        ? html`<span class="req-two req-key" data-tip=${`Client key ${r.client.key_name}`}><span class="req-l1"><span class="req-clip">${r.client.key_name}</span></span></span>`
        : html`<span class="req-two req-key"><span class="req-l1 faint">No key</span></span>`,
  },
  {
    key: 'duration_ms',
    header: 'Duration · TTFB',
    align: 'right',
    num: true,
    render: (r) =>
      r.in_flight
        ? html`
            <span class="req-two req-nums">
              <span class="req-l1"><${Elapsed} since=${r.started_at} /></span>
              <span class="req-l2 req-word">so far</span>
            </span>
          `
        : html`
            <span class="req-two req-nums" data-tip=${durationTip(r)}>
              <span class="req-l1">${formatDuration(r.duration_ms)}</span>
              <span class=${r.ttfb_ms == null ? 'req-l2 req-absent' : 'req-l2'}><span class="req-unit">first byte</span>${formatDuration(r.ttfb_ms)}</span>
            </span>
          `,
  },
  {
    key: 'tokens',
    header: 'Tokens in/out',
    align: 'right',
    num: true,
    render: (r) =>
      r.in_flight || !usageCounted(r)
        ? none()
        : html`
            <span class="req-two req-nums" data-tip=${usageTip(r)}>
              <span class="req-l1">
                <span>${formatTokens(r.usage.input_tokens)}<span class="req-slash"> / </span>${formatTokens(r.usage.output_tokens)}</span>
                <span class="req-unit">tokens</span>
              </span>
              ${r.cost != null && html`<span class="req-l2"><span class="sr-only">estimated cost </span>${formatCurrency(r.cost)}</span>`}
            </span>
          `,
  },
];

// ---------------------------------------------------------------------------
// One tooltip for the whole table
// ---------------------------------------------------------------------------

const TIP_DELAY_MS = 400;

/**
 * Shows the `data-tip` text of whatever the pointer rests on inside it. One
 * listener and one floating element serve every cell. The text may contain
 * line breaks. Touch pointers are ignored: there, the detail drawer is the
 * way to the same information.
 */
export function HoverTips({ class: className, children }) {
  const [tip, setTip] = useState(null); // { text, rect, warm }
  const [pos, setPos] = useState(null);
  const box = useRef(null);
  const target = useRef(null);
  const timer = useRef(null);

  const hide = () => {
    clearTimeout(timer.current);
    target.current = null;
    setTip(null);
    setPos(null);
  };

  const onOver = (event) => {
    if (event.pointerType === 'touch') return;
    const el = event.target.closest?.('[data-tip]') ?? null;
    if (el === target.current) return;
    clearTimeout(timer.current);
    target.current = el;
    if (!el) {
      setTip(null);
      setPos(null);
      return;
    }
    const show = (warm) => {
      if (target.current !== el || !el.isConnected) return;
      setPos(null);
      setTip({ text: el.getAttribute('data-tip'), rect: el.getBoundingClientRect(), warm });
    };
    // Moving from one cell to the next swaps the text at once.
    if (tip) show(true);
    else timer.current = setTimeout(() => show(false), TIP_DELAY_MS);
  };

  useLayoutEffect(() => {
    if (!tip || !box.current) return;
    setPos(placeFloating(tip.rect, { width: box.current.offsetWidth, height: box.current.offsetHeight }, { side: 'top', align: 'center', gap: 6 }));
  }, [tip]);

  const shown = tip !== null;
  useEffect(() => {
    if (!shown) return undefined;
    window.addEventListener('scroll', hide, true);
    document.addEventListener('keydown', hide);
    return () => {
      window.removeEventListener('scroll', hide, true);
      document.removeEventListener('keydown', hide);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [shown]);

  useEffect(() => () => clearTimeout(timer.current), []);

  return html`
    <div class=${className} onPointerOver=${onOver} onPointerLeave=${hide} onPointerDown=${hide}>${children}</div>
    ${tip &&
    tip.text &&
    html`
      <${Portal}>
        <div
          ref=${box}
          class="tip req-tip"
          role="tooltip"
          data-state=${pos ? 'open' : 'closed'}
          data-instant=${tip.warm ? '' : undefined}
          style=${pos ? `top:${pos.top}px;left:${pos.left}px;transform-origin:${pos.origin}` : 'top:0;left:0;visibility:hidden'}
        >
          ${tip.text}
        </div>
      <//>
    `}
  `;
}
