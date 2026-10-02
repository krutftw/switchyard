// One call to POST /admin/api/playground, with everything the inspector
// shows: the status and headers the moment they arrive, each server-sent
// event with its time offset, and the body text of a call that was not
// streamed.
//
// lib/api.js has streamSSE for this endpoint, but it reports the status and
// headers only once the stream has ended, and not at all for an error status,
// and it serialises the body itself. The playground needs the headers first
// (time to first byte, the request id of a failed call) and sends body text
// exactly as it was written, so it makes the call itself with the same
// session secret, the same SSE parser and the same ApiError.

import { API_BASE, ApiError, api, createSSEParser, getToken } from '../../lib/api.js';

/** Authorization value for the admin secret; non-ASCII secrets go out as UTF-8 bytes (as lib/api.js does). */
function bearer(secret) {
  const text = String(secret ?? '');
  if (!/[^\u0000-\u007f]/.test(text)) return `Bearer ${text}`;
  let bytes = '';
  for (const byte of new TextEncoder().encode(text)) bytes += String.fromCharCode(byte);
  return `Bearer ${bytes}`;
}

const now = () => (typeof performance !== 'undefined' ? performance.now() : Date.now());

function transportError(cause, signal, fallback) {
  if (cause instanceof ApiError) return cause;
  if (signal?.aborted || cause?.name === 'AbortError') return new ApiError(0, 'The request was stopped.', { code: 'aborted' });
  return new ApiError(0, fallback, { code: 'network' });
}

/**
 * @param {string} payload  the envelope, as JSON text (see buildEnvelope)
 * @param {{
 *   signal?: AbortSignal,
 *   onResponse?: (meta: object) => void,   headers have arrived
 *   onEvent?: (event: {event: string, data: string, id: string, json: any}, offsetMs: number) => void,
 *   onBody?: (text: string, json: any) => void,   body of a call that was not streamed
 * }} handlers
 * @returns {Promise<object>} meta: { status, ok, contentType, streamed, requestId,
 *   provider, upstreamModel, retryAfter, ttfbMs, totalMs }
 * Rejects with ApiError (code "aborted" or "network") when there is no
 * complete answer. An HTTP error status is not a rejection: it is an answer,
 * reported through onResponse and onBody like any other.
 */
export default async function runPlayground(payload, { signal, onResponse, onEvent, onBody } = {}) {
  const started = now();
  let res;
  try {
    res = await fetch(`${API_BASE}/playground`, {
      method: 'POST',
      headers: {
        accept: 'text/event-stream, application/json',
        'content-type': 'application/json',
        authorization: bearer(getToken()),
      },
      body: payload,
      cache: 'no-store',
      credentials: 'same-origin',
      signal,
    });
  } catch (cause) {
    throw transportError(cause, signal, 'Cannot reach the gateway. Check that Switchyard is running and that this device can connect to it.');
  }

  const contentType = res.headers.get('content-type') ?? '';
  const retryAfter = Number(res.headers.get('retry-after'));
  const meta = {
    status: res.status,
    ok: res.ok,
    contentType,
    streamed: contentType.includes('text/event-stream'),
    requestId: res.headers.get('x-request-id'),
    provider: res.headers.get('x-switchyard-provider'),
    upstreamModel: res.headers.get('x-switchyard-model'),
    retryAfter: res.headers.get('retry-after') != null && Number.isFinite(retryAfter) ? retryAfter : null,
    ttfbMs: now() - started,
    totalMs: null,
  };
  onResponse?.(meta);

  // The admin API's own 401 (not an upstream's: those arrive as 502) means the
  // session is over. Let the shared client find that out its own way, so the
  // sign-in page explains it as it does for every other page.
  if (res.status === 401) api.get('/status').catch(() => {});

  let reader = null;
  let handlerFailure = null;
  try {
    if (!meta.streamed) {
      const text = await res.text();
      let json;
      try {
        json = text ? JSON.parse(text) : undefined;
      } catch {
        json = undefined;
      }
      meta.totalMs = now() - started;
      onBody?.(text, json);
      return meta;
    }
    const parser = createSSEParser((event) => {
      try {
        onEvent?.(event, now() - started);
      } catch (error) {
        handlerFailure = { error };
        throw error;
      }
    });
    reader = res.body.getReader();
    const decoder = new TextDecoder();
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      parser.feed(decoder.decode(value, { stream: true }));
    }
    parser.feed(decoder.decode());
    parser.end();
    meta.totalMs = now() - started;
    return meta;
  } catch (cause) {
    reader?.cancel().catch(() => {});
    if (handlerFailure) throw handlerFailure.error;
    throw transportError(cause, signal, 'The connection dropped before the response finished.');
  }
}
