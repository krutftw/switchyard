// Requests page: the detail drawer of one request.
//
// The drawer is opened by id (#/requests?id=...). It shows the row's record
// at once when the list has it, and fetches GET /requests/{id} for the
// captured bodies; for a link to a request that is no longer in the list
// that fetch is also where the record comes from.

import { html, useEffect, useMemo, useRef, useState } from '../../../vendor/preact-htm.js';
import {
  Badge,
  Button,
  CodeBlock,
  CopyButton,
  Drawer,
  EmptyState,
  ErrorState,
  IconButton,
  KeyValue,
  Notice,
  Skeleton,
  StatusLamp,
  Tabs,
  Timeline,
  toast,
  toneForStatus,
} from '../../components/index.js';
import { DASH, formatBytes, formatCurrency, formatDuration, formatNumber, formatRelativeTime, formatTimestamp, plural, sentence } from '../../lib/format.js';
import { useNow, useResource } from '../../lib/hooks.js';
import { useLive } from '../../lib/live.js';
import { canReplayInPlayground } from '../../lib/replay.js';
import { href } from '../../lib/router.js';
import { Elapsed, ProtocolRoute, statusTone } from './cells.js';
import { NO_PROVIDER, buildCurl, errorKindWords, headerLines, isEventStream, statusWords } from './record.js';

/**
 * A safety net, not a wait. The gateway can hand out the bodies of a request
 * from the moment its request.finished frame is sent, so the first answer
 * has them. Should a record that only just ended still say it has bodies
 * and come without them, the detail is asked for again after these delays
 * before the drawer says they are missing. A record that ended longer than
 * BODY_RETRY_WINDOW_MS ago is believed at once: its bodies are not on their
 * way, they have been removed from the request log.
 */
const BODY_RETRY_MS = [150, 500, 1500];
const BODY_RETRY_WINDOW_MS = 5000;

/** A WebSocket session the gateway relayed frame by frame (status 101). */
const isRelayedSession = (record) => record.status === 101;

const BODY_TABS = [
  { id: 'client_request', label: 'Client request' },
  { id: 'upstream_request', label: 'Upstream request' },
  { id: 'upstream_response', label: 'Upstream response' },
  { id: 'client_response', label: 'Client response' },
  { id: 'headers', label: 'Headers' },
];

function Section({ title, aside, children }) {
  return html`
    <section class="req-section">
      <div class="req-section-head">
        <h3>${title}</h3>
        ${aside}
      </div>
      ${children}
    </section>
  `;
}

// ---------------------------------------------------------------------------
// Headline, error
// ---------------------------------------------------------------------------

function Headline({ record }) {
  if (record.in_flight) {
    return html`
      <div class="req-hero">
        <div class="req-hero-status">
          <${StatusLamp} size="lg" tone="info" pulse title="In flight" />
          <span class="req-hero-words">In flight</span>
        </div>
        <div class="req-hero-timing">
          <span class="req-hero-duration num"><${Elapsed} since=${record.started_at} /></span>
          <span class="faint">so far</span>
        </div>
      </div>
    `;
  }
  return html`
    <div class="req-hero">
      <div class="req-hero-status">
        <${StatusLamp} size="lg" tone=${statusTone(record)} title=${record.ok ? 'Succeeded' : 'Failed'} />
        <span class="req-hero-code num">${record.status}</span>
        <span class="req-hero-words">${statusWords(record.status)}</span>
      </div>
      <div class="req-hero-timing">
        <span class="req-hero-duration num">${formatDuration(record.duration_ms)}</span>
        <span class="faint">${record.ttfb_ms == null ? 'nothing sent before the end' : html`first byte after <span class="num">${formatDuration(record.ttfb_ms)}</span>`}</span>
      </div>
    </div>
  `;
}

// What an operator can do about each class of failure.
const NEXT_STEP = {
  not_found: 'Check the model name the client sends, or add a provider or an alias that serves it.',
  permission: 'The client key is not allowed to use this model. The API keys page lists the models each key may use.',
  too_large: 'The request, or the answer to it, was larger than the gateway or the upstream accepts.',
  internal: 'The gateway failed while it served this request. The Logs page has what it wrote at that time.',
  client_disconnect: 'The client closed the connection before the answer was complete. Nothing failed on the gateway or upstream; if this keeps happening, look at the timeout the client sets.',
  aborted: 'The gateway stopped relaying this WebSocket session before either side had closed it. The client has to open a new session.',
  rate_limit: 'The credential rests for its cooldown and other credentials are tried meanwhile. Add credentials or lower the request rate if this keeps happening.',
  unavailable: 'Every credential that could serve this model was cooling down, disabled or unusable. The Providers page shows which.',
  timeout: 'The upstream took longer than the request timeout. Check the provider, or raise the timeout under Settings.',
  invalid_request: 'The request was refused before it was sent upstream. The client request body shows what was sent.',
};

function ErrorBlock({ record }) {
  const error = record.error;
  if (!error) return null;
  const upstream = error.upstream_status;
  let next = NEXT_STEP[error.kind];
  if (error.kind === 'upstream') {
    if (isRelayedSession(record)) next = 'The upstream\'s side of the WebSocket session failed after the session had been opened. The client has to open a new session.';
    else next = upstream ? 'The provider refused or failed the request. The attempts below show each call.' : 'No upstream answered. Check the provider\'s base URL and that the gateway can reach it.';
  } else if (error.kind === 'client_disconnect' && isRelayedSession(record)) {
    next = 'The client\'s side of the WebSocket session broke off without closing it. Nothing failed on the gateway or upstream.';
  }
  return html`
    <${Notice} tone=${statusTone(record) === 'caution' ? 'caution' : 'stop'} title=${errorKindWords(error.kind)}>
      <p class="req-error-message">${error.message}</p>
      ${upstream != null && html`<p>${upstream === 0 ? 'The upstream did not answer.' : html`Upstream status <span class="num">${upstream}</span>.`}</p>`}
      ${next && html`<p class="req-error-next">${next}</p>`}
    <//>
  `;
}

// ---------------------------------------------------------------------------
// Summary, attempts, usage
// ---------------------------------------------------------------------------

function Summary({ record }) {
  const now = useNow(10_000);
  const client = record.client ?? {};
  const differs = (name) => name && name !== record.requested_model;
  const items = [
    { label: 'Request id', value: record.id, mono: true, copy: true },
    {
      label: 'Started',
      value: html`<span class="num">${formatTimestamp(record.started_at, { zone: true })}</span> <span class="faint">${formatRelativeTime(record.started_at, Math.max(now, record.started_at))}</span>`,
    },
    { label: 'Endpoint', value: record.endpoint, mono: true },
    { label: 'Protocol', value: html`<${ProtocolRoute} record=${record} />` },
    {
      label: 'Transport',
      value: html`<span class="mono">${record.transport ?? DASH}</span> <span class="faint">${isRelayedSession(record) ? 'a session relayed to the upstream' : record.stream ? 'the client asked for a stream' : 'single response'}</span>`,
    },
    { label: 'Requested model', value: record.requested_model, mono: true, copy: true },
    { label: 'Resolved model', value: record.client_model, mono: true, hidden: !differs(record.client_model) },
    { label: 'Upstream model', value: record.upstream_model, mono: true, copy: true, hidden: !differs(record.upstream_model) },
    { label: 'Reasoning', value: record.reasoning, hidden: !record.reasoning },
    {
      label: 'Provider',
      hidden: record.in_flight,
      value: record.provider ? html`<a class="mono" href=${href('/providers', { open: record.provider })}>${record.provider}</a>` : html`<span class="faint">${NO_PROVIDER}</span>`,
    },
    {
      label: 'Credential',
      hidden: !record.credential_id,
      value: html`<span class="mono">${record.credential_label || record.credential_id}</span> <span class="faint mono">${record.credential_label ? record.credential_id : ''}</span>`,
    },
    {
      label: 'Client key',
      value: client.key_name ? html`<span>${client.key_name}</span> <span class="faint mono">${client.key_id ?? ''}</span>` : html`<span class="faint">No client key</span>`,
    },
    { label: 'Client address', value: client.ip, mono: true, hidden: !client.ip },
    { label: 'User agent', value: client.user_agent, mono: true, hidden: !client.user_agent },
  ];
  return html`<${KeyValue} items=${items} />`;
}

function Attempts({ record }) {
  const attempts = record.attempts ?? [];
  if (attempts.length === 0) {
    return html`<p class="muted req-quiet">No upstream was called: the request failed before routing, or every credential of the model was cooling down.</p>`;
  }
  return html`
    <${Timeline}
      items=${attempts.map((attempt, index) => ({
        key: index,
        tone: attempt.ok ? 'clear' : attempt.status === 429 ? 'caution' : 'stop',
        toneLabel: attempt.ok ? 'Succeeded' : attempt.status === 429 ? 'Rate limited' : 'Failed',
        title: html`
          <span class="mono">${attempt.provider}</span>
          ${attempt.credential_label && attempt.credential_label !== attempt.provider && html`<span class="faint mono">${attempt.credential_label}</span>`}
        `,
        badges: html`
          <${Badge} mono tone=${attempt.ok ? 'clear' : attempt.status ? toneForStatus(attempt.status) : 'stop'}>${attempt.status || 'no response'}<//>
          ${attempt.upstream_protocol && html`<${Badge} mono>${attempt.upstream_protocol}<//>`}
        `,
        time: formatDuration(attempt.duration_ms),
        description: html`
          <div class="req-attempt">
            ${attempt.upstream_model && html`<span>Model <span class="mono">${attempt.upstream_model}</span></span>`}
            ${attempt.error && html`<span class="req-attempt-error">${attempt.error}</span>`}
          </div>
        `,
      }))}
    />
  `;
}

function Usage({ record }) {
  const usage = record.usage ?? {};
  const cells = [
    ['Input', usage.input_tokens],
    ['Cache read', usage.cache_read_tokens],
    ['Cache write', usage.cache_write_tokens],
    ['Output', usage.output_tokens],
    ['Reasoning', usage.reasoning_tokens],
  ];
  const costNote = record.cost != null ? 'from the configured prices' : record.ok ? 'no price for this model' : 'not estimated for failures';
  return html`
    <dl class="req-usage">
      ${cells.map(
        ([label, value]) => html`
          <div class="req-usage-cell" key=${label}>
            <dt class="plate-label">${label}</dt>
            <dd class="num">${formatNumber(value ?? 0)}</dd>
          </div>
        `,
      )}
      <div class="req-usage-cell">
        <dt class="plate-label">Estimated cost</dt>
        <dd><span class="num">${formatCurrency(record.cost)}</span> <span class="req-usage-note">${costNote}</span></dd>
      </div>
    </dl>
  `;
}

// ---------------------------------------------------------------------------
// Captured bodies
// ---------------------------------------------------------------------------

function whyMissing(which, record) {
  const attempts = record.attempts ?? [];
  if (which === 'upstream_request') {
    return attempts.length === 0 ? 'Nothing was sent upstream: the request ended before a provider was chosen.' : 'The upstream request body was not captured.';
  }
  if (which === 'upstream_response') {
    if (record.mode === 'mock') return 'The mock provider answers inside the gateway, so there is no upstream response to capture.';
    if (attempts.length === 0) return 'No upstream was called, so there is no upstream response.';
    if (attempts[attempts.length - 1].status === 0) return 'The upstream did not answer, so there is no response body.';
    return 'The upstream response body was not captured. The client response shows what the gateway sent on.';
  }
  if (which === 'client_response') return 'No response body was captured for the client.';
  return 'The client sent no body, or it was not captured.';
}

function Body({ which, label, record, bodies, limitKb }) {
  const text = bodies[which];
  const size = useMemo(() => (typeof text === 'string' ? new TextEncoder().encode(text).length : 0), [text]);
  if (text == null || text === '') {
    return html`<p class="muted req-quiet">${whyMissing(which, record)}</p>`;
  }
  const stream = isEventStream(text);
  const cut = limitKb > 0 && size >= limitKb * 1024;
  const note = [
    formatBytes(size),
    stream ? 'server-sent events, shown as they were sent' : null,
    cut ? `cut at the body size cap of ${formatNumber(limitKb)} KB` : null,
  ]
    .filter(Boolean)
    .join(' · ');
  return html`<${CodeBlock} title=${label} value=${text} language=${stream ? 'text' : 'auto'} note=${note} maxHeight="520px" />`;
}

function Headers({ bodies }) {
  const sides = [
    ['Client request headers', bodies.client_headers, 'The gateway recorded no headers from the client.'],
    ['Upstream request headers', bodies.upstream_headers, 'The gateway recorded no headers sent upstream.'],
  ];
  return html`
    <div class="stack" style="--gap:var(--space-3)">
      ${sides.map(([title, headers, none]) =>
        headers && Object.keys(headers).length > 0
          ? html`<${CodeBlock} key=${title} title=${title} value=${headerLines(headers)} language="text" note="Credentials in headers are masked by the gateway." />`
          : html`<p key=${title} class="muted req-quiet">${none}</p>`,
      )}
    </div>
  `;
}

function NotCaptured({ record, requestLog }) {
  // The frames of a relayed session pass through as they are; none are kept,
  // whatever the request log is set to.
  if (isRelayedSession(record)) {
    return html`<p class="muted req-quiet">The gateway relays the frames of a WebSocket session as they are and does not store them, so there are no bodies to show.</p>`;
  }
  // Named as the Settings page names them: the Capture setting (Off, Failed
  // requests, Every request) under Request bodies, tab Logging and usage.
  const where = 'under Request bodies, on the Logging and usage tab of Settings';
  let text = `Bodies are stored only while Capture is on. To capture them, set Capture to Failed requests or Every request ${where}.`;
  if (requestLog === 'off') {
    text = `Capture is set to Off, so request and response bodies are not stored. To capture them, set Capture to Failed requests or Every request ${where}.`;
  } else if (requestLog === 'errors' && record.ok) {
    text = `Capture is set to Failed requests, and this request succeeded. To capture successful requests too, set Capture to Every request ${where}.`;
  } else if (requestLog) {
    text = `The bodies were not stored when this request ran, or they have since been removed. Capture is set ${where}.`;
  }
  const needs = canReplayInPlayground(record) ? 'Copy curl (bash/zsh) and Open in playground need' : 'Copy curl (bash/zsh) needs';
  return html`
    <${EmptyState}
      compact
      icon="logs"
      title="Bodies were not captured"
      description=${`${text} It applies to requests made from then on. ${needs} the captured client request.`}
      action=${html`<${Button} href=${href('/settings', { tab: 'logging' })} iconRight="arrow-right">Open logging settings<//>`}
    />
  `;
}

/**
 * bodies   undefined while they load (or are still being stored), null when
 *          the gateway has none, else the captured bodies
 * missing  the record says bodies were captured, yet none came back
 * onRetry  fetch the detail again
 */
function Bodies(props) {
  const { detail, onRetry } = props;
  const busy = detail.refreshing || detail.loading;

  // "Try again" goes away when the bodies arrive. The keyboard then moves to
  // what took its place (the selected tab, or the button again if the retry
  // failed), unless the focus has gone elsewhere meanwhile.
  const box = useRef(null);
  const retried = useRef(false);
  const retry = () => {
    retried.current = true;
    onRetry();
  };
  useEffect(() => {
    if (!retried.current || busy) return;
    retried.current = false;
    const at = document.activeElement;
    if (at && at !== document.body && at.isConnected) return;
    box.current?.querySelector('[role="tab"][aria-selected="true"], button, a')?.focus({ preventScroll: true });
  });

  return html`<div ref=${box}><${BodiesView} ...${props} busy=${busy} onRetry=${retry} /></div>`;
}

function BodiesView({ record, bodies, missing, detail, busy, logging, onRetry, tab, onTab }) {
  const active = BODY_TABS.some((t) => t.id === tab) ? tab : BODY_TABS[0].id;
  // The first tab is the default: it leaves ?tab= out of the address.
  const pick = (id) => onTab(id === BODY_TABS[0].id ? null : id);

  if (record.in_flight) {
    return html`<p class="muted req-quiet">Bodies are stored when the request finishes.</p>`;
  }
  if (bodies === undefined) {
    // A request that was in flight a moment ago answered 404 until now;
    // that is not a failure to show while the finished record is fetched.
    if (detail.error && !busy) {
      return html`
        <${Notice} tone="caution" title="Could not load the captured bodies" action=${html`<${Button} size="sm" icon="refresh" onClick=${onRetry}>Try again<//>`}>
          ${sentence(detail.error.message)}
        <//>
      `;
    }
    return html`<div aria-busy="true"><${Skeleton} lines=${5} /></div>`;
  }
  if (bodies === null && missing) {
    return html`
      <${Notice} tone="caution" title="The captured bodies are not available" action=${html`<${Button} size="sm" icon="refresh" loading=${busy} onClick=${onRetry}>Try again<//>`}>
        The gateway captured the bodies of this request and no longer has them. It removes captured bodies that are older than the usage retention, and the oldest ones first when they outgrow the size limit of the logs.
      <//>
    `;
  }
  if (bodies === null) return html`<${NotCaptured} record=${record} requestLog=${logging?.request_log} />`;

  const current = BODY_TABS.find((t) => t.id === active);
  return html`
    <div class="stack" style="--gap:var(--space-3)">
      <${Tabs} label="Captured bodies" tabs=${BODY_TABS} value=${active} onChange=${pick} />
      <div role="tabpanel" aria-label=${current.label}>
        ${active === 'headers'
          ? html`<${Headers} bodies=${bodies} />`
          : html`<${Body} key=${active} which=${active} label=${current.label} record=${record} bodies=${bodies} limitKb=${logging?.request_log_max_body_kb ?? 0} />`}
      </div>
    </div>
  `;
}

// ---------------------------------------------------------------------------
// Drawer
// ---------------------------------------------------------------------------

function Content({ id, record, bodies, missing, detail, logging, onRetry, tab, onTab }) {
  if (!record) {
    if (detail.error?.status === 404) {
      return html`
        <${EmptyState}
          icon="search"
          title="No request with this id"
          description=${html`The gateway has no finished request <span class="mono">${id}</span>. It may be older than the usage retention, the statistics may have been cleared, or the id is incomplete. A request that is still running appears here when it finishes.`}
        />
      `;
    }
    if (detail.error) return html`<${ErrorState} title="Could not load the request" error=${detail.error} onRetry=${detail.refresh} retrying=${detail.loading || detail.refreshing} />`;
    return html`
      <div class="stack" style="--gap:var(--space-5)" aria-busy="true">
        <${Skeleton} width="45%" height="26px" />
        <${Skeleton} lines=${8} />
        <${Skeleton} lines=${4} />
      </div>
    `;
  }
  return html`
    <div class="req-detail">
      <${Headline} record=${record} />
      ${record.in_flight
        ? html`<${Notice} tone="info" title="This request has not finished">The attempts, the usage and the captured bodies appear here when it does.<//>`
        : html`<${ErrorBlock} record=${record} />`}
      <${Summary} record=${record} />
      ${!record.in_flight &&
      html`
        <${Section} title="Attempts" aside=${(record.attempts?.length ?? 0) > 0 && html`<span class="faint req-section-aside">${plural(record.attempts.length, 'upstream call')}, in order</span>`}>
          <${Attempts} record=${record} />
        <//>
        <${Section} title="Usage">
          <${Usage} record=${record} />
        <//>
      `}
      <${Section} title="Captured bodies">
        <${Bodies} record=${record} bodies=${bodies} missing=${missing} detail=${detail} logging=${logging} onRetry=${onRetry} tab=${tab} onTab=${onTab} />
      <//>
    </div>
  `;
}

/**
 * id       request id from the URL; '' closes the drawer
 * seed     the list's record for that id, when it has one (shown at once)
 * tab      the body tab from the URL (?tab=); anything else shows the first
 * onTab    (tab | null) => void: null for the first tab. The page makes each
 *          pick a step in the history, so Back returns to the previous tab
 * onClose  () => void
 * onNewer, onOlder  step to the neighbouring row, or null at the ends
 */
export default function RequestDrawer({ id, seed, tab, onTab, onClose, onNewer, onOlder }) {
  // The drawer keeps its content while it slides out, after the id is gone.
  const last = useRef({ id: '', seed: null });
  if (id) last.current = { id, seed: seed ?? (last.current.id === id ? last.current.seed : null) };
  const shownId = id || last.current.id;
  const known = last.current.seed;

  // Keyed by the open id, not the shown one: closing the drawer lets go of
  // the request (the hook keeps what it had, for the slide out) and opening
  // it again, even on the same request, asks the gateway again.
  //
  // A request the list shows as in flight is not asked for until it ends:
  // the gateway only knows finished requests, and would answer 404.
  const [ended, setEnded] = useState('');
  const inFlight = !!id && known?.in_flight === true && ended !== id;
  const resource = useResource(id && !inFlight ? `/requests/${encodeURIComponent(id)}` : null);
  // The request log settings explain a missing body and give the size limit.
  const config = useResource(id ? '/config' : null);
  const logging = config.data?.config?.logging;

  // useResource starts clean on a new key, so what it holds is about this
  // request from the first render that has its id. (A closing drawer has no
  // id; the hook then keeps what it had, and so does the drawer.)
  const detail = resource;
  const data = detail.data?.record?.id === shownId ? detail.data : undefined;

  // A record that says it has bodies comes with them. If one does not, and
  // it only just ended, the detail is asked for again a few times before the
  // drawer says the bodies are missing (see BODY_RETRY_MS).
  const pendingBodies = !!data && data.bodies == null && data.record.has_bodies === true;
  const retry = useRef({ id: '', n: 0 });
  if (retry.current.id !== shownId) retry.current = { id: shownId, n: 0 };
  const justEnded = pendingBodies && (retry.current.n > 0 || Date.now() - data.record.finished_at < BODY_RETRY_WINDOW_MS);
  const delay = justEnded && id ? BODY_RETRY_MS[retry.current.n] : undefined;
  useEffect(() => {
    if (delay == null) return undefined;
    const timer = setTimeout(() => {
      retry.current.n += 1;
      detail.refresh();
    }, delay);
    return () => clearTimeout(timer);
    // updatedAt: each answer without bodies schedules the next try.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [delay, shownId, detail.updatedAt]);
  const retryNow = () => {
    retry.current = { id: shownId, n: 0 };
    detail.refresh();
  };

  useLive(
    'request.finished',
    (record) => {
      if (record?.id !== id) return;
      // Either starts the first fetch (the request was in flight) or
      // replaces a 404 from a link to a request that had not finished.
      setEnded(id);
      resource.refresh();
    },
    { enabled: !!id },
  );
  useLive('config.reloaded', () => config.refresh(), { enabled: !!id });

  const record = data?.record ?? known;
  const missing = pendingBodies && delay == null;
  const bodies = !data || (pendingBodies && !missing) ? undefined : data.bodies;
  const curl = useMemo(() => (record && bodies ? buildCurl(record, bodies) : null), [record, bodies]);
  // Only what the playground will load: it refuses embeddings, token counts
  // and other non-generation endpoints.
  const replayable = canReplayInPlayground(record, bodies);

  return html`
    <${Drawer}
      class="req-drawer"
      open=${!!id}
      onClose=${onClose}
      title="Request"
      subtitle=${shownId}
      width="760px"
      actions=${html`
        <${IconButton} icon="chevron-up" label="Newer request" disabled=${!onNewer} onClick=${() => onNewer?.()} />
        <${IconButton} icon="chevron-down" label="Older request" disabled=${!onOlder} onClick=${() => onOlder?.()} />
      `}
      footer=${html`
        <${CopyButton} variant="secondary" size="md" value=${shownId}>Copy request id<//>
        ${curl &&
        html`
          <${CopyButton}
            variant="secondary"
            size="md"
            value=${curl}
            onCopied=${() => toast.success('Copied curl for bash/zsh', { description: 'The client key is not included. In bash or zsh, set SWITCHYARD_KEY to a client key before you run it.' })}
          >
            Copy curl (bash/zsh)
          <//>
        `}
        ${replayable && html`<${Button} href=${href('/playground', { from: shownId })} iconRight="arrow-right">Open in playground<//>`}
      `}
    >
      ${shownId && html`<${Content} id=${shownId} record=${record} bodies=${bodies} missing=${missing} detail=${detail} logging=${logging} onRetry=${retryNow} tab=${tab} onTab=${onTab} />`}
    <//>
  `;
}
