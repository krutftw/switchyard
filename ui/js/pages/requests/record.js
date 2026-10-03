// Requests page: pure helpers for request records. Nothing here touches the
// DOM, so the logic can be exercised under Node.
//
// A request record is the object GET /requests and the request.finished
// frame carry (crates/admin/API.md, "A request record"). A request that is
// still in flight is the shorter request.started payload with `in_flight`
// set by the page.

import { API_BASE } from '../../lib/api.js';
import { DASH, formatCurrency, formatDuration, formatNumber } from '../../lib/format.js';

/** The query parameters that filter the list; the same names the API takes. */
export const FILTER_KEYS = ['status', 'model', 'client_model', 'since', 'provider', 'key', 'q'];

/**
 * The names the gateway files a request under when it has none of its own:
 * `model=unknown` and `provider=unknown` select the requests without a
 * model or a provider, `key=anonymous` the ones without a client key.
 */
export const NO_MODEL_FILTER = 'unknown';
export const NO_PROVIDER_FILTER = 'unknown';
const NO_KEY_FILTER = 'anonymous';

/** True for a record that names no model: it was refused before one was read. */
export function hasNoModel(record) {
  return !record.requested_model && !record.client_model;
}

const same = (a, b) => typeof a === 'string' && a.toLowerCase() === b;

function matchesStatus(record, status) {
  const want = status.trim().toLowerCase();
  if (want === 'ok' || want === 'success' || want === 'succeeded') return record.ok === true;
  if (want === 'error' || want === 'err' || want === 'failed' || want === 'failure') return record.ok === false;
  if (/^[1-5]xx$/.test(want)) return Math.floor(record.status / 100) === Number(want[0]);
  if (/^[1-5]\d\d$/.test(want)) return record.status === Number(want);
  // The gateway ignores a status it cannot parse (999, 6xx); so does the page.
  return true;
}

/**
 * Whether a record belongs in the list under `filters`, by the rules the
 * gateway applies to GET /requests. Used for records that arrive live.
 *
 * `partial` is for requests still in flight: they have no status and no
 * provider or resolved client-facing model yet, so a filter on any of
 * those leaves them out.
 */
export default function matchesFilters(record, filters, partial = false) {
  const { status, model, client_model, since, provider, key, q } = filters;
  if (since) {
    // A malformed value is refused by the API. Do not let live frames make
    // that failed query look like a valid, unfiltered list in the meantime.
    if (!/^[+-]?\d+$/.test(String(since)) || !Number.isSafeInteger(record.started_at)) return false;
    const start = BigInt(since);
    if (start < -9223372036854775808n || start > 9223372036854775807n || BigInt(record.started_at) < start) return false;
  }
  if (client_model) {
    if (partial) return false;
    const filed = record.client_model || record.requested_model || NO_MODEL_FILTER;
    if (!same(filed, client_model.toLowerCase())) return false;
  }
  if (status) {
    if (partial || !matchesStatus(record, status)) return false;
  }
  if (provider) {
    if (partial || !same(record.provider || NO_PROVIDER_FILTER, provider.toLowerCase())) return false;
  }
  if (model) {
    // Any of the three names, or the name the request is filed under: the
    // client-facing model, else the requested one, else "unknown".
    const want = model.toLowerCase();
    const filed = record.client_model || record.requested_model || NO_MODEL_FILTER;
    if (![record.requested_model, record.client_model, record.upstream_model, filed].some((name) => same(name, want))) return false;
  }
  if (key) {
    const want = key.toLowerCase();
    const client = record.client ?? {};
    const filed = client.key_name || client.key_id || NO_KEY_FILTER;
    if (![filed, client.key_id, client.key_name].some((name) => same(name, want))) return false;
  }
  if (q) {
    const want = q.toLowerCase();
    const haystack = [
      record.id,
      record.requested_model,
      record.client_model,
      record.upstream_model,
      record.provider,
      record.credential_label,
      record.client?.key_name,
      record.endpoint,
      // The gateway searches the error class ("rate_limit") as well as the
      // message.
      record.error?.kind,
      record.error?.message,
    ];
    if (!haystack.some((text) => typeof text === 'string' && text.toLowerCase().includes(want))) return false;
  }
  return true;
}

/** Newest first: by start time, then by id (UUIDv7, so ids sort by time too). */
export function compareRecords(a, b) {
  if (a.started_at !== b.started_at) return b.started_at - a.started_at;
  if (a.id === b.id) return 0;
  return a.id < b.id ? 1 : -1;
}

/** The `before=` cursor that continues the list after `record`. */
export function cursorOf(record) {
  return `${record.started_at}:${record.id}`;
}

/**
 * `rows` with `incoming` merged in, newest first. A record that is already
 * listed is replaced by the incoming copy.
 */
export function mergeRecords(rows, incoming) {
  if (incoming.length === 0) return rows;
  const byId = new Map();
  for (const record of incoming) byId.set(record.id, record);
  const kept = rows.filter((row) => !byId.has(row.id));
  const added = [...byId.values()].sort(compareRecords);
  // The common case, records newer than everything listed, needs no sort.
  if (kept.length === 0 || compareRecords(added[added.length - 1], kept[0]) < 0) return added.concat(kept);
  return kept.concat(added).sort(compareRecords);
}

// ---------------------------------------------------------------------------
// Words
// ---------------------------------------------------------------------------

const STATUS_WORDS = {
  // A WebSocket session the gateway relayed to the upstream, recorded when
  // it ends.
  101: 'WebSocket session',
  200: 'OK',
  400: 'Bad request',
  401: 'Unauthorized',
  403: 'Forbidden',
  404: 'Not found',
  408: 'Request timeout',
  409: 'Conflict',
  413: 'Body too large',
  422: 'Not valid',
  429: 'Rate limited',
  499: 'Client went away',
  500: 'Gateway error',
  502: 'Upstream failed',
  503: 'Unavailable',
  504: 'Upstream timed out',
};

/** "Rate limited" for 429; a class name for codes without their own words. */
export function statusWords(status) {
  if (STATUS_WORDS[status]) return STATUS_WORDS[status];
  if (status >= 200 && status < 300) return 'OK';
  if (status >= 400 && status < 500) return 'Client error';
  if (status >= 500) return 'Server error';
  return '';
}

// Every error class the gateway records (crates/admin/API.md, "A request
// record").
const KIND_WORDS = {
  invalid_request: 'Invalid request',
  authentication: 'Not authorised',
  permission: 'Model not allowed for this key',
  not_found: 'Not found',
  too_large: 'Too large',
  rate_limit: 'Rate limited',
  upstream: 'Upstream error',
  unavailable: 'No provider available',
  timeout: 'Timed out',
  internal: 'Gateway error',
  client_disconnect: 'The client went away',
  aborted: 'Session cut short',
};

/** An error class ("rate_limit") as words. Unknown classes are spelled out. */
export function errorKindWords(kind) {
  if (!kind) return 'Error';
  if (KIND_WORDS[kind]) return KIND_WORDS[kind];
  const text = String(kind).replace(/_/g, ' ');
  return text.charAt(0).toUpperCase() + text.slice(1);
}

export const MODES = {
  passthrough: { label: 'Passthrough', tone: 'neutral', hint: 'Sent upstream in the protocol the client used.' },
  translated: { label: 'Translated', tone: 'info', hint: 'The gateway translated between the client and the upstream protocol.' },
  mock: { label: 'Mock', tone: 'neutral', outline: true, hint: 'Answered by the built-in mock provider. Nothing left the gateway.' },
  raw: { label: 'Raw', tone: 'neutral', outline: true, hint: 'Proxied as raw JSON, without protocol handling.' },
};

const hasUsage = (usage) => !!usage && Object.values(usage).some((n) => n > 0);

/** The tokens cell: "1.2K / 166", or a dash when nothing was counted. */
export function usageCounted(record) {
  return hasUsage(record.usage);
}

// Tooltip texts. Each adds to what the cell shows; none is the only copy:
// the detail drawer has all of it.

export function statusTip(record) {
  const lines = [`${record.status} ${statusWords(record.status)}`.trim()];
  if (record.error) {
    lines.push(`${errorKindWords(record.error.kind)}: ${record.error.message}`);
    if (record.error.upstream_status) lines.push(`Upstream status ${record.error.upstream_status}`);
  }
  const attempts = record.attempts?.length ?? 0;
  if (attempts > 1) lines.push(`${attempts} upstream attempts; the last one decided the outcome`);
  return lines.join('\n');
}

/**
 * Shown in place of a model name. The gateway records "" (not null) for a
 * request it refused before the model was read, such as a body that is not
 * valid JSON.
 */
export const NO_MODEL = 'No model';

export function modelTip(record) {
  const lines = [record.requested_model ? `Requested ${record.requested_model}` : 'The request was refused before its model name was read'];
  if (record.client_model && record.client_model !== record.requested_model) lines.push(`Resolved to ${record.client_model}`);
  if (record.upstream_model && record.upstream_model !== record.requested_model) lines.push(`Sent upstream as ${record.upstream_model}`);
  if (record.reasoning) lines.push(`Reasoning ${record.reasoning}`);
  return lines.join('\n');
}

export function routeTip(record) {
  const lines = [`Client ${record.client_protocol ?? DASH} over ${record.transport ?? DASH}`];
  if (record.upstream_protocol) lines.push(`Upstream ${record.upstream_protocol}`);
  const mode = MODES[record.mode];
  if (mode) lines.push(mode.hint);
  return lines.join('\n');
}

/**
 * Shown in place of a provider name, and the name of the `provider=unknown`
 * filter: no provider served the request. It failed before routing (refused,
 * unknown model, the client key's own limit), or every credential of the
 * model was cooling down.
 */
export const NO_PROVIDER = 'No provider';

export function providerTip(record) {
  if (!record.provider) return 'No provider served this request: it failed before routing, or every credential of the model was cooling down.';
  const lines = [`Provider ${record.provider}`];
  if (record.credential_label) lines.push(`Credential ${record.credential_label}`);
  const attempts = record.attempts?.length ?? 0;
  if (attempts > 1) lines.push(`The last of ${attempts} attempts`);
  return lines.join('\n');
}

export function durationTip(record) {
  const lines = [`Total ${formatDuration(record.duration_ms)}`];
  lines.push(record.ttfb_ms == null ? 'Nothing was sent before the end' : `First byte after ${formatDuration(record.ttfb_ms)}`);
  return lines.join('\n');
}

export function usageTip(record) {
  const usage = record.usage ?? {};
  return [
    `Input ${formatNumber(usage.input_tokens ?? 0)}`,
    `Cache read ${formatNumber(usage.cache_read_tokens ?? 0)}`,
    `Cache write ${formatNumber(usage.cache_write_tokens ?? 0)}`,
    `Output ${formatNumber(usage.output_tokens ?? 0)}`,
    `Reasoning ${formatNumber(usage.reasoning_tokens ?? 0)}`,
    costTip(record),
  ].join('\n');
}

export function costTip(record) {
  if (record.cost != null) return `Estimated ${formatCurrency(record.cost)} from the configured prices`;
  return record.ok ? 'No price is configured for this model' : 'Failed requests have no cost estimate';
}

// ---------------------------------------------------------------------------
// Copy as curl
// ---------------------------------------------------------------------------

/** The gateway's own address: the dashboard lives at <root>/admin/. */
export function gatewayRoot() {
  const origin = typeof location === 'undefined' ? 'http://127.0.0.1:8317' : location.origin;
  return origin + API_BASE.replace(/\/admin\/api$/, '');
}

const PROTOCOL_PATHS = {
  'openai-chat': '/v1/chat/completions',
  'openai-responses': '/v1/responses',
  anthropic: '/v1/messages',
};

/**
 * Method and path of the client API call that reproduces a record, or null
 * when there is none (a WebSocket session, an unknown protocol).
 */
export function clientCall(record) {
  if (record.transport === 'websocket') return null;
  const match = /^([A-Z]+)\s+(\/\S*)/.exec(record.endpoint ?? '');
  let method = match?.[1] ?? 'POST';
  let path = match?.[2] ?? '';
  // A playground run is recorded under the admin route; replay it against
  // the client endpoint of its protocol instead.
  if (!path || path.startsWith(`${API_BASE}/`) || path.includes('/admin/')) {
    method = 'POST';
    if (record.client_protocol === 'gemini') path = `/v1beta/models/{model}:${record.stream ? 'streamGenerateContent' : 'generateContent'}`;
    else path = PROTOCOL_PATHS[record.client_protocol] ?? '';
  }
  if (!path) return null;
  if (path.includes('{model}')) {
    if (!record.requested_model) return null;
    path = path.replace('{model}', encodeURIComponent(record.requested_model).replace(/%2F/gi, '/'));
  }
  if (record.client_protocol === 'gemini' && path.endsWith(':streamGenerateContent') && record.transport === 'sse') path += '?alt=sse';
  return { method, path };
}

const shellQuote = (text) => `'${String(text).replace(/'/g, `'\\''`)}'`;

/**
 * A curl command that sends the captured client request again. The client
 * key is never part of it: the command reads it from $SWITCHYARD_KEY.
 * Returns null when the request cannot be replayed with curl.
 */
export function buildCurl(record, bodies) {
  const call = clientCall(record);
  if (!call || bodies?.client_request == null) return null;
  const lines = [`curl -sS${record.stream ? ' -N' : ''}${call.method === 'POST' ? '' : ` -X ${call.method}`} ${shellQuote(gatewayRoot() + call.path)}`];
  if (record.client_protocol === 'anthropic') {
    lines.push('-H "x-api-key: $SWITCHYARD_KEY"');
    const version = bodies.client_headers?.['anthropic-version'];
    lines.push(`-H ${shellQuote(`anthropic-version: ${version || '2023-06-01'}`)}`);
    const beta = bodies.client_headers?.['anthropic-beta'];
    if (beta) lines.push(`-H ${shellQuote(`anthropic-beta: ${beta}`)}`);
  } else if (record.client_protocol === 'gemini') {
    lines.push('-H "x-goog-api-key: $SWITCHYARD_KEY"');
  } else {
    lines.push('-H "Authorization: Bearer $SWITCHYARD_KEY"');
  }
  lines.push('-H "Content-Type: application/json"');
  lines.push(`-d ${shellQuote(bodies.client_request)}`);
  return lines.join(' \\\n  ');
}

// (Whether "Open in playground" is offered is canReplayInPlayground in
// lib/replay.js: the same check the playground makes on ?from=.)

/** True for a captured body that is a server-sent event stream, not JSON. */
export function isEventStream(text) {
  return typeof text === 'string' && /^\s*(?:event|data|id|retry)?:/.test(text);
}

/** Header map to "name: value" lines, in the order the gateway sent them. */
export function headerLines(headers) {
  return Object.entries(headers ?? {})
    .map(([name, value]) => `${name}: ${value}`)
    .join('\n');
}
