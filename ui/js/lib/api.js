// Admin API client.
//
//   import { api, ApiError } from '../lib/api.js';
//   const { providers } = await api.get('/providers');
//   await api.post('/providers', body);          // throws ApiError
//   await api.streamSSE('/playground', body, (ev) => { ... }, signal);
//
// Every request carries `Authorization: Bearer <admin secret>`. The secret is
// kept in sessionStorage (this tab only) and, when the user ticks "Remember
// on this device", also in localStorage. A 401 from any endpoint clears it
// and flips the auth store to "anonymous", which sends the app back to the
// login page.

import { createStore } from './store.js';

/**
 * The one place the admin API location is defined. The dashboard is served
 * at <root>/admin/ and the API lives at <root>/admin/api, so the base is the
 * directory of the current document plus "api". Deriving it from the location
 * keeps the dashboard working behind a reverse proxy that adds a path prefix.
 */
export const API_BASE = (() => {
  if (typeof location === 'undefined') return '/admin/api';
  const dir = location.pathname.replace(/[^/]*$/, '');
  return `${dir.endsWith('/') ? dir : `${dir}/`}api`;
})();

const TOKEN_KEY = 'sy.admin.token';
const DEFAULT_TIMEOUT_MS = 30_000;

// ---------------------------------------------------------------------------
// Storage (never throws: private mode and blocked storage are tolerated)
// ---------------------------------------------------------------------------

function storageGet(kind, key) {
  try {
    return globalThis[kind]?.getItem(key) ?? null;
  } catch {
    return null;
  }
}

function storageSet(kind, key, value) {
  try {
    if (value == null) globalThis[kind]?.removeItem(key);
    else globalThis[kind]?.setItem(key, value);
  } catch {
    /* storage unavailable: the secret then lives only in memory */
  }
}

let memoryToken = null;

export function getToken() {
  return memoryToken ?? storageGet('sessionStorage', TOKEN_KEY) ?? storageGet('localStorage', TOKEN_KEY);
}

/** True when the secret is stored for future sessions on this device. */
export function isRemembered() {
  return storageGet('localStorage', TOKEN_KEY) != null;
}

function setToken(token, remember) {
  memoryToken = token;
  storageSet('sessionStorage', TOKEN_KEY, token);
  storageSet('localStorage', TOKEN_KEY, remember ? token : null);
}

function clearToken() {
  memoryToken = null;
  storageSet('sessionStorage', TOKEN_KEY, null);
  storageSet('localStorage', TOKEN_KEY, null);
}

// ---------------------------------------------------------------------------
// Auth state
// ---------------------------------------------------------------------------

/**
 * status: "unknown" (a stored secret has not been checked yet),
 *         "authenticated", or "anonymous".
 * reason: why the session ended, shown on the login page:
 *         "expired" (a request got 401), "signed-out", or null.
 */
export const auth = createStore({
  status: getToken() ? 'unknown' : 'anonymous',
  reason: null,
});

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/**
 * Every failed request rejects with an ApiError, whichever stage failed:
 * sending, waiting for the answer, or reading its body.
 *
 *   status      HTTP status; 0 when there is no usable response (network
 *               down, timeout, aborted, connection lost while the body was
 *               still arriving, request not sendable)
 *   message     human-readable, from the server's {"error":{"message"}} when
 *               present
 *   issues      [{ path, message }] validation issues, [] when none
 *   retryAfter  seconds from a Retry-After header, or null
 *   code        "http"     the gateway answered with an error status, or
 *                          with a body that is not JSON
 *               "network"  no connection, or it dropped mid-response
 *               "timeout"  no complete answer within the time limit
 *               "aborted"  cancelled through the caller's AbortSignal
 *               "invalid"  never sent: the admin secret cannot be put in a
 *                          header (it contains a control character)
 */
export class ApiError extends Error {
  constructor(status, message, { issues = [], retryAfter = null, code = 'http', body = null } = {}) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.issues = Array.isArray(issues) ? issues : [];
    this.retryAfter = retryAfter;
    this.code = code;
    this.body = body;
  }

  /** Aborted by the caller (navigation, unmount). Usually not worth showing. */
  get aborted() {
    return this.code === 'aborted';
  }
}

const STATUS_TEXT = {
  400: 'The gateway rejected the request.',
  401: 'The admin secret was not accepted.',
  403: 'The gateway refused this connection.',
  404: 'The gateway has no such admin route.',
  409: 'The change conflicts with the current state.',
  413: 'The request body is too large.',
  422: 'The gateway could not validate the request.',
  429: 'Too many attempts. Wait before trying again.',
  500: 'The gateway hit an internal error.',
  502: 'The gateway could not reach the upstream.',
  503: 'The gateway is not ready.',
  504: 'The gateway timed out waiting for the upstream.',
};

function parseRetryAfter(value) {
  if (!value) return null;
  const seconds = Number(value);
  if (Number.isFinite(seconds)) return Math.max(0, seconds);
  const date = Date.parse(value);
  return Number.isNaN(date) ? null : Math.max(0, Math.round((date - Date.now()) / 1000));
}

async function errorFromResponse(res) {
  let body = null;
  let message = '';
  let issues = [];
  const text = await res.text().catch(() => '');
  if (text) {
    try {
      body = JSON.parse(text);
    } catch {
      body = null;
    }
  }
  if (body && typeof body === 'object') {
    const err = body.error;
    if (err && typeof err === 'object') {
      message = typeof err.message === 'string' ? err.message : '';
      if (Array.isArray(err.issues)) {
        issues = err.issues
          .filter((i) => i && typeof i.message === 'string')
          .map((i) => ({ path: typeof i.path === 'string' ? i.path : '', message: i.message }));
      }
    } else if (typeof err === 'string') {
      message = err;
    } else if (typeof body.message === 'string') {
      message = body.message;
    }
  }
  if (!message) message = STATUS_TEXT[res.status] ?? `The gateway answered with HTTP ${res.status}.`;
  return new ApiError(res.status, message, {
    issues,
    retryAfter: parseRetryAfter(res.headers.get('retry-after')),
    body,
  });
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

function buildUrl(path, query) {
  let url = API_BASE + (path.startsWith('/') ? path : `/${path}`);
  if (query) {
    const params = new URLSearchParams();
    for (const [key, value] of Object.entries(query)) {
      if (value == null || value === '') continue;
      params.set(key, String(value));
    }
    const qs = params.toString();
    if (qs) url += `?${qs}`;
  }
  return url;
}

/** Combine the caller's signal with a timeout. Returns { signal, done, timedOut }. */
function linkSignal(external, timeoutMs) {
  const controller = new AbortController();
  let timedOut = false;
  const onAbort = () => controller.abort();
  if (external) {
    if (external.aborted) controller.abort();
    else external.addEventListener('abort', onAbort, { once: true });
  }
  const timer = timeoutMs > 0
    ? setTimeout(() => {
        timedOut = true;
        controller.abort();
      }, timeoutMs)
    : null;
  return {
    signal: controller.signal,
    done() {
      if (timer) clearTimeout(timer);
      external?.removeEventListener('abort', onAbort);
    },
    timedOut: () => timedOut,
  };
}

const NOT_SENT = 'Cannot reach the gateway. Check that Switchyard is running and that this device can connect to it.';
const CUT_OFF = 'The connection dropped before the response finished.';

/**
 * The ApiError for something thrown by fetch() or by reading a body. Every
 * stage of a request goes through here, so callers only ever see ApiError:
 * the timeout and the caller's abort can fire after the headers arrived just
 * as well as before, and then it is the body read that rejects.
 */
function transportError(cause, link, timeout, fallback) {
  if (cause instanceof ApiError) return cause;
  if (link.timedOut()) {
    return new ApiError(0, `The gateway did not answer within ${Math.round(timeout / 1000)} seconds.`, { code: 'timeout' });
  }
  // Browsers reject an aborted body read with AbortError, but a stream that
  // wraps the body may surface its own error type: trust the signal.
  if (link.signal.aborted || cause?.name === 'AbortError') return new ApiError(0, 'The request was cancelled.', { code: 'aborted' });
  return new ApiError(0, fallback, { code: 'network' });
}

/**
 * The Authorization header value for a secret.
 *
 * fetch() only accepts header values made of code points up to U+00FF and
 * without CR, LF or NUL; anything else makes it throw a TypeError before a
 * request is sent, which would read as "cannot reach the gateway".
 *
 * - Control characters cannot be part of a header at all: say so.
 * - Other characters go out as their UTF-8 bytes (one code unit per byte),
 *   the encoding the gateway's config file uses. So a secret with "é" in it
 *   works, and a pasted secret that picked up a non-breaking hyphen or a
 *   zero-width space gets an honest 401 from the gateway.
 */
function bearerHeader(secret) {
  const text = String(secret);
  // Tab is the one control character HTTP allows inside a header value.
  if (/[\u0000-\u0008\u000a-\u001f\u007f]/.test(text)) {
    throw new ApiError(0, 'The admin secret contains a line break or another control character. Enter it again as a single line.', { code: 'invalid' });
  }
  if (!/[^\u0000-\u007f]/.test(text)) return `Bearer ${text}`;
  let bytes = '';
  for (const byte of new TextEncoder().encode(text)) bytes += String.fromCharCode(byte);
  return `Bearer ${bytes}`;
}

async function send(method, path, { body, query, signal, timeout = DEFAULT_TIMEOUT_MS, token, headers, keepSessionOn401 = false } = {}) {
  const init = {
    method,
    headers: { accept: 'application/json', ...headers },
    cache: 'no-store',
    credentials: 'same-origin',
  };
  const bearer = token ?? getToken();
  if (bearer) init.headers.authorization = bearerHeader(bearer);
  if (body !== undefined) {
    init.headers['content-type'] = 'application/json';
    init.body = JSON.stringify(body);
  }
  const link = linkSignal(signal, timeout);
  init.signal = link.signal;

  let res;
  try {
    res = await fetch(buildUrl(path, query), init);
  } catch (cause) {
    link.done();
    throw transportError(cause, link, timeout, NOT_SENT);
  }

  if (res.ok) return { res, link, timeout };

  // An error status. Its body is read under the same timeout and signal.
  let error;
  try {
    error = await errorFromResponse(res);
  } finally {
    link.done();
  }
  if (res.status === 401 && !keepSessionOn401) endSession('expired');
  throw error;
}

async function request(method, path, options) {
  const { res, link, timeout } = await send(method, path, options);
  let text;
  try {
    // A 204 has no body to wait for.
    text = res.status === 204 ? '' : await res.text();
  } catch (cause) {
    throw transportError(cause, link, timeout, CUT_OFF);
  } finally {
    link.done();
  }
  if (!text) return null;
  try {
    return JSON.parse(text);
  } catch {
    throw new ApiError(res.status, 'The gateway sent a response that is not valid JSON.', { body: text.slice(0, 2000) });
  }
}

// ---------------------------------------------------------------------------
// Server-sent events
// ---------------------------------------------------------------------------

/**
 * Incremental SSE parser. Feed it decoded text chunks; it calls `onEvent`
 * with { event, data, id, json } for every dispatched event. `json` is the
 * parsed data when it is valid JSON, otherwise undefined. Comment lines
 * (": keep-alive") are ignored. Exported for tests.
 */
export function createSSEParser(onEvent) {
  let buffer = '';
  let event = '';
  let data = [];
  let id = '';

  const dispatch = () => {
    if (data.length === 0 && !event) {
      id = '';
      return;
    }
    const payload = data.join('\n');
    let json;
    if (payload && payload !== '[DONE]') {
      try {
        json = JSON.parse(payload);
      } catch {
        json = undefined;
      }
    }
    onEvent({ event: event || 'message', data: payload, id, json });
    event = '';
    data = [];
    id = '';
  };

  const line = (text) => {
    if (text === '') return dispatch();
    if (text.startsWith(':')) return undefined;
    const colon = text.indexOf(':');
    const field = colon === -1 ? text : text.slice(0, colon);
    let value = colon === -1 ? '' : text.slice(colon + 1);
    if (value.startsWith(' ')) value = value.slice(1);
    if (field === 'event') event = value;
    else if (field === 'data') data.push(value);
    else if (field === 'id') id = value;
    return undefined;
  };

  return {
    feed(chunk) {
      buffer += chunk;
      // Hold back a trailing "\r": the "\n" of a CRLF may be in the next chunk.
      const end = buffer.endsWith('\r') ? buffer.length - 1 : buffer.length;
      const lines = buffer.slice(0, end).split(/\r\n|\n|\r/);
      const partial = lines.pop();
      buffer = partial + buffer.slice(end);
      for (const text of lines) line(text);
    },
    /** Call at end of stream: flushes a final event that lacks a blank line. */
    end() {
      const rest = buffer.replace(/\r$/, '');
      buffer = '';
      if (rest) line(rest);
      dispatch();
    },
  };
}

/**
 * POST `body` to `path` and read the response as a stream of server-sent
 * events, calling `onEvent({ event, data, id, json })` for each. Resolves
 * when the stream ends; rejects with ApiError on HTTP errors and when the
 * connection breaks mid-stream. If `onEvent` itself throws, the stream is
 * closed and that error is rethrown as it is: a bug in the handler is not a
 * network failure.
 *
 * The playground endpoint answers with plain JSON when the request is not a
 * streaming one. In that case `onEvent` is called once with
 * { event: "response", data, json } so callers handle both shapes the same way.
 *
 * Returns { status, headers, streamed }.
 */
async function streamSSE(path, body, onEvent, signal) {
  // No overall timeout: a stream is open for as long as the model talks.
  const { res, link } = await send('POST', path, {
    body,
    signal,
    timeout: 0,
    headers: { accept: 'text/event-stream, application/json' },
  });
  const meta = { status: res.status, headers: res.headers, streamed: false };
  let handlerFailure = null;
  const emit = (event) => {
    try {
      onEvent(event);
    } catch (error) {
      handlerFailure = { error };
      throw error;
    }
  };
  let reader = null;
  try {
    const type = res.headers.get('content-type') ?? '';
    if (!type.includes('text/event-stream')) {
      const text = await res.text();
      let json;
      try {
        json = text ? JSON.parse(text) : undefined;
      } catch {
        json = undefined;
      }
      emit({ event: 'response', data: text, id: '', json });
      return meta;
    }
    meta.streamed = true;
    const parser = createSSEParser(emit);
    reader = res.body.getReader();
    const decoder = new TextDecoder();
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      parser.feed(decoder.decode(value, { stream: true }));
    }
    parser.feed(decoder.decode());
    parser.end();
    return meta;
  } catch (cause) {
    // Stop reading: nobody is listening any more.
    reader?.cancel().catch(() => {});
    if (handlerFailure) throw handlerFailure.error;
    throw transportError(cause, link, 0, CUT_OFF);
  } finally {
    link.done();
  }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/**
 * Check a secret with POST /login and, when accepted, store it.
 * Rejects with ApiError: 401 wrong secret, 403 remote access disabled,
 * 404 admin API disabled, 429 locked out (see error.retryAfter), and
 * status 0 with code "invalid" when the secret contains a control character
 * (nothing is sent), "network" or "timeout" when the gateway is unreachable.
 */
async function login(secret, { remember = false } = {}) {
  await request('POST', '/login', { token: secret, body: {}, keepSessionOn401: true });
  setToken(secret, remember);
  auth.set({ status: 'authenticated', reason: null });
}

/**
 * Validate the stored secret at boot. Resolves true when it is still good.
 * A network failure rejects so the caller can offer a retry instead of
 * throwing the stored secret away.
 */
async function resume() {
  const token = getToken();
  if (!token) {
    auth.set({ status: 'anonymous' });
    return false;
  }
  try {
    await request('POST', '/login', { token, body: {}, keepSessionOn401: true, timeout: 10_000 });
    auth.set({ status: 'authenticated', reason: null });
    return true;
  } catch (error) {
    // "invalid": the stored value could never have been accepted by login(),
    // so it is not ours; drop it like a rejected secret.
    if (error.status === 401 || error.code === 'invalid') {
      endSession('expired');
      return false;
    }
    if (error.status === 403 || error.status === 404 || error.status === 429) {
      // Not a bad secret, but the gateway will not talk to us: show the
      // login page, which explains each of these.
      auth.set({ status: 'anonymous', reason: null });
      return false;
    }
    throw error;
  }
}

function endSession(reason) {
  clearToken();
  auth.set({ status: 'anonymous', reason });
}

/** Forget the secret on this device and return to the login page. */
function logout() {
  endSession('signed-out');
}

export const api = {
  get: (path, options) => request('GET', path, options),
  post: (path, body, options) => request('POST', path, { ...options, body: body === undefined ? {} : body }),
  put: (path, body, options) => request('PUT', path, { ...options, body }),
  patch: (path, body, options) => request('PATCH', path, { ...options, body }),
  del: (path, options) => request('DELETE', path, options),
  streamSSE,
  login,
  resume,
  logout,
};

/** True for errors worth showing: anything except a caller-initiated abort. */
export function isReportable(error) {
  return !(error instanceof ApiError && error.aborted);
}
