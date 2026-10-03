// The inspector: what goes over the wire for the request being written and
// for the one that was last sent.
//
//   Request   the JSON body, exactly; editable by hand in raw mode
//   Events    the server-sent events of a streamed answer, with time offsets
//   Response  the body of an answer that was not streamed
//   Info      status, request id, provider, timings, token usage
//   Code      the same request as curl, and base-URL setup for the SDKs
//
// A run (the last request; built and updated by playground.js):
//   { status: 'waiting' | 'streaming' | 'done' | 'stopped' | 'error',
//     protocol, model, stream, path, bodyText, startedAt,
//     meta (see run.js; null until the headers arrive), firstContentMs,
//     events, eventTotal, responseText, usage, finish, error, record }

import { html, useRef, useState } from '../../../vendor/preact-htm.js';
import {
  Badge,
  Button,
  CodeBlock,
  EmptyState,
  KeyValue,
  Notice,
  Segmented,
  Select,
  Skeleton,
  StatusLamp,
  Switch,
  Tabs,
  Textarea,
  toneForStatus,
} from '../../components/index.js';
import { formatCurrency, formatDuration, formatNumber, formatTime } from '../../lib/format.js';
import { href } from '../../lib/router.js';
import { placeFocus } from './focus.js';
import FrameList from './framelist.js';
import { KEY_VARIABLE, protocolInfo, snippetFamily, statusName } from './protocols.js';

/** Events kept per request. */
export const EVENT_CAP = 500;

export const INSPECTOR_TABS = ['request', 'events', 'response', 'info', 'code'];

function RequestTab({ run, view, onView, next, raw, onRawToggle, onRawChange, onRawReset, loading }) {
  const sent = view === 'sent' && run;
  return html`
    <div class="play-insp-section">
      <div class="play-insp-bar">
        <${Segmented}
          size="sm"
          label="Which request to show"
          value=${sent ? 'sent' : 'next'}
          onChange=${onView}
          options=${[
            { value: 'next', label: 'Next request' },
            { value: 'sent', label: 'Last sent', disabled: !run, title: run ? undefined : 'Nothing was sent yet' },
          ]}
        />
        <${Switch} label="Edit as raw JSON" checked=${raw.on} onChange=${onRawToggle} disabled=${loading} class="play-insp-raw" />
      </div>
      <p class="play-insp-path">
        <span class="mono">POST ${sent ? run.path : next.path}</span>
        <span class="faint">The public endpoint this body belongs to. The playground sends it through the admin API as the dashboard client.</span>
      </p>
      ${loading
        ? html`<${Skeleton} lines=${6} />`
        : sent
          ? html`
              <${CodeBlock} title=${`Body sent at ${formatTime(run.startedAt)}`} value=${run.bodyText} maxHeight="none" />
              ${raw.on && html`<p class="faint play-insp-foot">Read-only: this is what was sent. The raw body you are editing is under Next request.</p>`}
            `
          : raw.on
            ? html`
                <${Textarea}
                  label="Request body"
                  mono
                  rows=${14}
                  autoGrow
                  maxRows=${40}
                  value=${raw.text}
                  onChange=${onRawChange}
                  error=${raw.error}
                  hint="Sent as written. The form on the left no longer changes it."
                  class="play-insp-editor"
                />
                <div class="row row-wrap">
                  <${Button} size="sm" onClick=${onRawReset}>Rebuild from the form<//>
                </div>
              `
            : html`<${CodeBlock} title="Body" value=${next.text} maxHeight="none" />`}
    </div>
  `;
}

const eventText = (frame) => `event: ${frame.name}\ndata: ${frame.data}\n`;

function EventsTab({ run, onTab }) {
  if (!run) {
    return html`<${EmptyState} compact icon="logs" title="No events yet" description="Send a streamed request. Each server-sent event is listed here as it arrives, with its time since the request began." />`;
  }
  if (run.events.length === 0) {
    if (run.status === 'waiting' && !run.meta) return html`<div class="play-insp-section"><${Skeleton} lines=${5} /></div>`;
    if (run.meta && !run.meta.streamed) {
      return html`<${EmptyState}
        compact
        icon="logs"
        title="This answer was not streamed"
        description="It arrived as one JSON body, so there are no events."
        action=${html`<${Button} size="sm" onClick=${() => onTab('response')}>Show the response<//>`}
      />`;
    }
    if (run.status === 'waiting' || run.status === 'streaming') return html`<div class="play-insp-section"><${Skeleton} lines=${5} /></div>`;
    return html`<${EmptyState} compact icon="logs" title="No events arrived" description=${run.error?.message ?? 'The stream ended before its first event.'} />`;
  }
  return html`
    <${FrameList}
      frames=${run.events}
      total=${run.eventTotal}
      cap=${EVENT_CAP}
      noun="event"
      label="Server-sent events of the last request"
      toText=${eventText}
    />
  `;
}

function ResponseTab({ run, onTab }) {
  if (!run) {
    return html`<${EmptyState} compact icon="requests" title="No response yet" description="Send a request with streaming off. Its JSON body is shown here as the gateway returned it." />`;
  }
  if (run.meta?.streamed) {
    return html`<${EmptyState}
      compact
      icon="requests"
      title="This answer was streamed"
      description="A stream has no single body. Every event it sent is under Events."
      action=${html`<${Button} size="sm" onClick=${() => onTab('events')}>Show the events<//>`}
    />`;
  }
  if (run.responseText == null) {
    if (run.status === 'error' || run.status === 'stopped') {
      return html`<${EmptyState} compact icon="requests" title="No response body" description=${run.error?.message ?? 'The request was stopped before the gateway answered.'} />`;
    }
    return html`<div class="play-insp-section"><${Skeleton} lines=${6} /></div>`;
  }
  const status = run.meta?.status;
  return html`
    <div class="play-insp-section">
      <${CodeBlock}
        title=${`${status ?? ''} ${run.meta?.contentType || 'no content type'}`.trim()}
        value=${run.responseText || '(empty body)'}
        maxHeight="none"
      />
    </div>
  `;
}

function statusValue(run) {
  if (run.meta) {
    const name = statusName(run.meta.status);
    return html`<span class="row" style="--gap:var(--space-2)"><${Badge} mono tone=${toneForStatus(run.meta.status)}>${run.meta.status}<//>${name && html`<span>${name}</span>`}</span>`;
  }
  if (run.status === 'waiting') return html`<${StatusLamp} tone="info" pulse label="Waiting for the gateway" />`;
  if (run.status === 'stopped') return 'Stopped before the gateway answered';
  return 'No response';
}

function streamValue(run) {
  if (!run.meta) return null;
  if (!run.meta.streamed) return 'Not streamed: one JSON body';
  if (run.status === 'streaming') return html`<${StatusLamp} tone="info" pulse label="Receiving events" />`;
  if (run.status === 'stopped') return 'Stopped by you';
  if (run.error) return 'Failed after it started';
  return 'Ended normally';
}

function InfoTab({ run }) {
  if (!run) {
    return html`<${EmptyState} compact icon="about" title="Nothing sent yet" description="After a request: its status, request id, the provider that served it, timings and token usage." />`;
  }
  const meta = run.meta;
  const usage = run.usage;
  const record = run.record;
  const count = (n) => (n == null ? null : formatNumber(n));
  const translated = record && record.upstream_protocol && record.client_protocol && record.upstream_protocol !== record.client_protocol;
  return html`
    <div class="play-insp-section">
      <${KeyValue}
        items=${[
          { label: 'Status', value: statusValue(run) },
          {
            label: 'Request id',
            value: meta?.requestId ? html`<a class="mono" href=${href('/requests', { id: meta.requestId })}>${meta.requestId}</a>` : null,
            copy: meta?.requestId || false,
          },
          { label: 'Protocol', value: protocolInfo(run.protocol).label },
          { label: 'Model asked for', value: run.model, mono: true, hidden: !run.model },
          { label: 'Provider', value: meta?.provider ?? record?.provider, mono: true },
          { label: 'Upstream model', value: meta?.upstreamModel ?? record?.upstream_model, mono: true },
          {
            label: 'Upstream protocol',
            value: record?.upstream_protocol ? html`<span class="mono">${record.upstream_protocol}</span> <span class="faint">${translated ? 'translated' : 'same as the client'}</span>` : null,
            hidden: !record?.upstream_protocol,
          },
          { label: 'Credential', value: record?.credential_label, hidden: !record?.credential_label },
          // What the gateway applied after fitting a suffix, an alias target or the body to the model.
          { label: 'Reasoning applied', value: record?.reasoning, mono: true, hidden: !record?.reasoning },
          { label: 'Attempts', value: record?.attempts ? formatNumber(record.attempts.length) : null, hidden: !record?.attempts },
          { label: 'Stream', value: streamValue(run) },
          { label: 'Time to first byte', value: meta ? formatDuration(meta.ttfbMs) : null },
          { label: 'First content', value: run.firstContentMs != null ? formatDuration(run.firstContentMs) : null, hidden: !meta?.streamed },
          { label: 'Total time', value: meta?.totalMs != null ? formatDuration(meta.totalMs) : null },
          { label: 'Finish reason', value: run.finish, mono: true },
          { label: 'Input tokens', value: count(usage?.input) },
          { label: 'Output tokens', value: count(usage?.output) },
          { label: 'Reasoning tokens', value: count(usage?.reasoning), hidden: usage?.reasoning == null },
          { label: 'Cached tokens', value: count(usage?.cached), hidden: !usage?.cached },
          { label: 'Total tokens', value: count(usage?.total) },
          { label: 'Cost', value: record?.cost != null ? formatCurrency(record.cost) : null, hidden: record?.cost == null },
          { label: 'Retry after', value: meta?.retryAfter != null ? formatDuration(meta.retryAfter * 1000) : null, hidden: meta?.retryAfter == null },
        ]}
      />
      ${run.status === 'done' && !usage && meta?.ok && html`<p class="faint play-insp-foot">The response did not report token usage.</p>`}
    </div>
  `;
}

function CodeTab({ protocol, curl, snippets }) {
  const family = snippetFamily(protocol);
  const preferred = snippets.find((s) => s.family === family)?.id ?? snippets[0]?.id;
  // The reader's own choice wins; until there is one, follow the protocol.
  const [picked, setPicked] = useState(null);
  const id = picked && snippets.some((s) => s.id === picked) ? picked : preferred;
  const snippet = snippets.find((s) => s.id === id);
  return html`
    <div class="play-insp-section">
      <${CodeBlock} title="curl, public client API" language="text" value=${curl} maxHeight="320px" />
      <p class="faint play-insp-foot">Set <span class="mono">${KEY_VARIABLE}</span> to a client key first. Keys are on the <a href=${href('/keys')}>API keys</a> page.</p>
      <hr />
      <${Select}
        label="SDK setup"
        hint="Only the base URL and the key change. The rest of your code stays as it is."
        value=${id}
        onChange=${setPicked}
        options=${snippets.map((s) => ({ value: s.id, label: s.label }))}
      />
      ${snippet && html`<${CodeBlock} title=${snippet.label} language="text" value=${snippet.code} maxHeight="none" />`}
      ${snippet && snippet.family !== family &&
      html`<${Notice} tone="info">This SDK speaks a different protocol than the one selected. The gateway translates, so the same models answer either way.<//>`}
    </div>
  `;
}

/**
 * tab, onTab          the selected tab (kept in the URL by the page)
 * run                 the last request, or null
 * view, onView        'next' | 'sent': which body the Request tab shows
 * next                { text, path }: the body the next send would carry
 * raw                 { on, text, error }
 * onRawToggle(on), onRawChange(text), onRawReset()
 * loading             a captured request is being loaded into raw mode
 * protocol, curl, snippets
 */
export default function Inspector({ tab, onTab, run, view, onView, next, raw, onRawToggle, onRawChange, onRawReset, loading = false, protocol, curl, snippets }) {
  const current = INSPECTOR_TABS.includes(tab) ? tab : 'request';
  const eventCount = run && run.meta?.streamed ? run.eventTotal : null;
  const root = useRef(null);
  // "Show the response" and "Show the events" leave with the empty state
  // they are in: the keyboard goes to the tab they opened.
  const jump = (id) => {
    onTab(id);
    placeFocus(() => root.current?.querySelector('[role="tab"][aria-selected="true"]'));
  };
  return html`
    <section ref=${root} class="panel play-insp" aria-label="Inspector">
      <div class="play-insp-tabs">
        <${Tabs}
          label="Inspector"
          value=${current}
          onChange=${onTab}
          tabs=${[
            { id: 'request', label: 'Request' },
            { id: 'events', label: 'Events', count: eventCount != null ? formatNumber(eventCount) : undefined },
            { id: 'response', label: 'Response' },
            { id: 'info', label: 'Info' },
            { id: 'code', label: 'Code' },
          ]}
        />
      </div>
      <div class="play-insp-body" role="tabpanel" aria-label=${current}>
        ${current === 'request' &&
        html`<${RequestTab} run=${run} view=${view} onView=${onView} next=${next} raw=${raw} onRawToggle=${onRawToggle} onRawChange=${onRawChange} onRawReset=${onRawReset} loading=${loading} />`}
        ${current === 'events' && html`<${EventsTab} run=${run} onTab=${jump} />`}
        ${current === 'response' && html`<${ResponseTab} run=${run} onTab=${jump} />`}
        ${current === 'info' && html`<${InfoTab} run=${run} />`}
        ${current === 'code' && html`<${CodeTab} protocol=${protocol} curl=${curl} snippets=${snippets} />`}
      </div>
    </section>
  `;
}
