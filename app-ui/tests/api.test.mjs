import test from 'node:test';
import assert from 'node:assert/strict';
import { ApiError, TOKEN_STORAGE_KEY, bootstrapToken, createApi } from '../api.js';

function memoryStorage(initial = {}) {
  const items = new Map(Object.entries(initial));
  return {
    getItem: key => items.get(key) ?? null,
    setItem: (key, value) => items.set(key, value),
    removeItem: key => items.delete(key),
  };
}

function jsonResponse(data, status = 200) {
  return { ok: status >= 200 && status < 300, status, json: async () => data };
}

test('removes the fragment token before storing it and preserves the remaining URL/state', () => {
  const calls = [];
  const storage = memoryStorage();
  const state = { existing: true };
  const history = { state, replaceState: (...args) => calls.push(['history', ...args]) };
  const originalSet = storage.setItem;
  storage.setItem = (...args) => { calls.push(['storage', ...args]); originalSet(...args); };
  const token = bootstrapToken({
    location: { hash: '#token=host%2Btoken&tab=sessions', pathname: '/app/', search: '?view=workspace' },
    history, storage,
  });
  assert.equal(token, 'host+token');
  assert.deepEqual(calls, [
    ['history', state, '', '/app/?view=workspace#tab=sessions'],
    ['storage', TOKEN_STORAGE_KEY, 'host+token'],
  ]);
  assert.equal(storage.getItem(TOKEN_STORAGE_KEY), 'host+token');
});

test('reuses session storage on reload without rewriting ordinary fragments', () => {
  const storage = memoryStorage({ [TOKEN_STORAGE_KEY]: 'session-token' });
  const token = bootstrapToken({
    location: { hash: '#workspace' },
    history: { replaceState() { assert.fail('Unexpected history rewrite'); } }, storage,
  });
  assert.equal(token, 'session-token');
});

test('empty or duplicate launch tokens are removed and do not fall back to a stale token', () => {
  for (const hash of ['#token=', '#token=one&token=two', '#token=bad%0Atoken']) {
    const storage = memoryStorage({ [TOKEN_STORAGE_KEY]: 'stale-token' });
    let cleanUrl;
    assert.equal(bootstrapToken({
      location: { hash, pathname: '/' },
      history: { replaceState: (_, __, url) => { cleanUrl = url; } }, storage,
    }), '');
    assert.equal(cleanUrl, '/');
    assert.equal(storage.getItem(TOKEN_STORAGE_KEY), null);
  }
});

test('blocked session storage permits an initial token but missing storage returns no token', () => {
  const storage = { getItem() { throw new Error('blocked'); }, setItem() { throw new Error('blocked'); } };
  assert.equal(bootstrapToken({ location: { hash: '#token=valid-token' }, history: { replaceState() {} }, storage }), 'valid-token');
  assert.equal(bootstrapToken({ location: { hash: '' }, history: {}, storage }), '');
});

test('history cleanup failure does not persist or expose the launch token in its message', () => {
  const storage = memoryStorage();
  assert.throws(() => bootstrapToken({
    location: { hash: '#token=private-token' },
    history: { replaceState() { throw new Error('blocked'); } }, storage,
  }), error => error instanceof ApiError && error.code === 'token_cleanup_failed'
    && !error.message.includes('private-token'));
  assert.equal(storage.getItem(TOKEN_STORAGE_KEY), null);
});

test('sends an authenticated JSON mutation to /api with safe transport options', async () => {
  const controller = new AbortController();
  const body = { command_id: 'fixed-command', text: 'hello' };
  const api = createApi('host-token', { fetchImpl: async (url, options) => {
    assert.equal(url, '/api/sessions/session-1/turns');
    assert.equal(options.method, 'POST');
    assert.equal(options.headers.Authorization, 'Bearer host-token');
    assert.equal(options.headers.Accept, 'application/json');
    assert.equal(options.headers['Content-Type'], 'application/json');
    assert.equal(options.body, JSON.stringify(body));
    assert.equal(options.mode, 'same-origin');
    assert.equal(options.redirect, 'error');
    assert.equal(options.credentials, 'omit');
    assert.equal(options.cache, 'no-store');
    assert.equal(options.signal, controller.signal);
    return jsonResponse({ run: { id: 'run-1' } }, 201);
  } });
  assert.deepEqual(await api.request('/sessions/session-1/turns', { method: 'post', body, signal: controller.signal }), { run: { id: 'run-1' } });
});

test('defaults to GET and preserves query parameters without adding a body', async () => {
  const api = createApi('token', { fetchImpl: async (url, options) => {
    assert.equal(url, '/api/sessions?project_id=project%201');
    assert.equal(options.method, 'GET');
    assert.equal('body' in options, false);
    assert.equal('Content-Type' in options.headers, false);
    return jsonResponse({ sessions: [] });
  } });
  assert.deepEqual(await api.request('/sessions?project_id=project%201'), { sessions: [] });
});

test('missing or invalid tokens fail explicitly without making a request', async () => {
  for (const token of ['', undefined, 'bad\ntoken']) {
    const api = createApi(token, { fetchImpl: () => assert.fail('Unauthenticated request sent') });
    await assert.rejects(api.request('/status'), error => error.code === 'auth_required' && error.uncertain === false);
  }
});

test('rejects external URLs and paths escaping /api before dispatch', async () => {
  const api = createApi('token', { fetchImpl: () => assert.fail('Unsafe request sent') });
  for (const path of ['https://example.com/status', '//example.com/status', '/\\example.com', 'status', '/../status', '/%2e%2e/status', '/status#secret', '/status\n']) {
    await assert.rejects(api.request(path), error => error.code === 'invalid_path' && error.uncertain === false);
  }
});

test('preserves structured server error details and HTTP status', async () => {
  const api = createApi('token', { fetchImpl: async () => jsonResponse({ error: { code: 'conflict', message: 'A run is already active.' } }, 409) });
  await assert.rejects(api.request('/sessions/id/turns', { method: 'POST', body: {} }), error =>
    error instanceof ApiError && error.code === 'conflict' && error.message === 'A run is already active.'
    && error.status === 409 && error.uncertain === false);
});

test('network failures never retry and mutations report uncertainty while GET does not', async () => {
  for (const method of ['GET', 'POST']) {
    let count = 0;
    const api = createApi('token', { fetchImpl: async () => { count++; throw new TypeError('Connection lost'); } });
    await assert.rejects(api.request('/status', { method }), error =>
      error.code === 'network_error' && error.uncertain === (method === 'POST'));
    assert.equal(count, 1);
  }
});

test('pre-aborted requests do not dispatch; dispatched mutation aborts remain uncertain', async () => {
  const controller = new AbortController();
  controller.abort();
  const skipped = createApi('token', { fetchImpl: () => assert.fail('Cancelled request sent') });
  await assert.rejects(skipped.request('/sessions', { method: 'POST', signal: controller.signal }), error => error.code === 'aborted' && !error.uncertain);
  const active = createApi('token', { fetchImpl: async () => { throw new DOMException('Aborted', 'AbortError'); } });
  await assert.rejects(active.request('/sessions', { method: 'POST' }), error => error.code === 'aborted' && error.uncertain);
});

test('unreadable success or server failure leaves mutation outcomes uncertain', async () => {
  const unreadable = createApi('token', { fetchImpl: async () => ({ ok: true, status: 200, json: async () => { throw new SyntaxError('Invalid JSON'); } }) });
  await assert.rejects(unreadable.request('/sessions', { method: 'POST' }), error => error.code === 'invalid_response' && error.uncertain);
  const serverFailure = createApi('token', { fetchImpl: async () => jsonResponse({ error: { code: 'internal', message: 'Failed' } }, 500) });
  await assert.rejects(serverFailure.request('/sessions', { method: 'POST' }), error => error.code === 'internal' && error.uncertain);
  await assert.rejects(serverFailure.request('/sessions'), error => error.code === 'internal' && !error.uncertain);
});

test('handles empty success and rejects non-JSON bodies before dispatch', async () => {
  const empty = createApi('token', { fetchImpl: async () => ({ ok: true, status: 204, json() { assert.fail('Parsed an empty response'); } }) });
  assert.equal(await empty.request('/sessions/id/interrupt', { method: 'POST', body: { run_id: 'run' } }), null);
  const circular = {};
  circular.self = circular;
  const api = createApi('token', { fetchImpl: () => assert.fail('Invalid body dispatched') });
  await assert.rejects(api.request('/sessions', { method: 'POST', body: circular }), error => error.code === 'invalid_body' && !error.uncertain);
});
