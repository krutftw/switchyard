// Dashboard self-check: no browser, no dependencies.
//
//   node ui/tests/check.mjs
//
// It syntax-checks every module, imports each one under Node (which catches
// broken import paths and missing exports), checks that every route loads a
// page component, asserts the pure logic in lib/ and the chart maths, then
// renders components into a stub document (dom.mjs) and exercises the dev
// server over real sockets (dev-server.mjs).
// Run it before handing a page over. It does not replace looking at the page:
// use tools/ui-dev.mjs for that.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { installFrameClock } from './dom-stub.mjs';

// Before anything loads Preact (see dom-stub.mjs). No document yet: the
// modules below are imported the way a plain Node process sees them.
installFrameClock();

const ui = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const url = (rel) => pathToFileURL(path.join(ui, rel)).href;

function walk(dir) {
  return fs.readdirSync(dir, { withFileTypes: true }).flatMap((e) => (e.isDirectory() ? walk(path.join(dir, e.name)) : [path.join(dir, e.name)]));
}

// 0. Every module parses.
const all = [
  ...walk(path.join(ui, 'js')).filter((f) => f.endsWith('.js')),
  ...walk(path.join(ui, 'tests')).filter((f) => f.endsWith('.mjs')),
  path.resolve(ui, '..', 'tools', 'ui-dev.mjs'),
].filter((f) => fs.existsSync(f));
let parsed = 0;
for (const file of all) {
  const result = spawnSync(process.execPath, ['--check', file], { encoding: 'utf8' });
  assert.equal(result.status, 0, `${file} does not parse:
${result.stderr}`);
  parsed += 1;
}
console.log(`parsed ${parsed} modules`);

// 1. Every module imports (app.js needs a document: it is only parsed here).
const modules = walk(path.join(ui, 'js')).filter((f) => f.endsWith('.js') && !f.endsWith(`${path.sep}app.js`));
let imported = 0;
for (const file of modules) {
  const mod = await import(pathToFileURL(file).href);
  if (file.includes(`${path.sep}pages${path.sep}`)) assert.equal(typeof mod.default, 'function', `${file} needs a default export`);
  imported += 1;
}
console.log(`imported ${imported} modules`);

// 2. Routes: every entry loads a module with a default component.
const { ROUTES, matchRoute } = await import(url('js/routes.js'));
for (const entry of ROUTES) {
  const mod = await entry.load();
  assert.equal(typeof mod.default, 'function', entry.path);
}
const expected = ['overview', 'requests', 'providers', 'models', 'keys', 'usage', 'playground', 'logs', 'settings', 'about', '_kit'];
assert.deepEqual(ROUTES.map((r) => r.path.slice(1)).sort(), [...expected].sort());
assert.equal(matchRoute({ segments: ['requests', 'req_1'] }).path, '/requests');
assert.equal(matchRoute({ segments: ['nope'] }), null);

// 3. Formatting.
const f = await import(url('js/lib/format.js'));
assert.equal(f.formatCompact(950), '950');
assert.equal(f.formatCompact(1234), '1.2K');
assert.equal(f.formatCompact(1_200_000), '1.2M');
assert.equal(f.formatCompact(999_950), '1M');
assert.equal(f.formatCompact(2_500_000_000), '2.5B');
assert.equal(f.formatCompact(null), '—');
assert.equal(f.formatCompact(NaN), '—');
assert.equal(f.formatTokens(48_200_000), '48.2M');
assert.equal(f.formatDuration(312), '312ms');
assert.equal(f.formatDuration(1240), '1.24s');
assert.equal(f.formatDuration(12_400), '12.4s');
assert.equal(f.formatDuration(65_000), '1m 05s');
assert.equal(f.formatDuration(3_720_000), '1h 02m');
assert.equal(f.formatDuration(0.4), '<1ms');
assert.equal(f.formatDuration(undefined), '—');
assert.equal(f.formatCountdown(1781), '29:41');
assert.equal(f.formatCountdown(59), '0:59');
assert.equal(f.formatBytes(0), '0 B');
assert.equal(f.formatBytes(1536), '1.5 KB');
assert.equal(f.formatBytes(5 * 1024 * 1024), '5 MB');
assert.equal(f.formatCurrency(0), '$0.00');
assert.equal(f.formatCurrency(1.5), '$1.50');
assert.equal(f.formatCurrency(0.0421), '$0.042');
assert.equal(f.formatCurrency(0.00042), '$0.00042');
assert.equal(f.formatCurrency(1204.5), '$1,205');
assert.equal(f.formatPercent(0.9934), '99.3%');
assert.equal(f.formatPercent(0.99999), '99.9%');
assert.equal(f.formatPercent(0.00001), '<0.1%');
assert.equal(f.formatPercent(1), '100%');
assert.equal(f.formatDelta(0.124), '+12.4%');
assert.equal(f.formatDelta(-0.03), '−3%');
assert.equal(f.formatNumber(1234567), '1,234,567');
const now = 1_800_000_000_000;
assert.equal(f.formatRelativeTime(now - 2000, now), 'just now');
assert.equal(f.formatRelativeTime(now - 12_000, now), '12s ago');
assert.equal(f.formatRelativeTime(now - 5 * 60_000, now), '5m ago');
assert.equal(f.formatRelativeTime(now + 5 * 60_000, now), 'in 5m');
assert.equal(f.formatRelativeTime(now / 1000 - 3 * 3600, now), '3h ago'); // epoch seconds accepted
assert.equal(f.plural(1, 'request'), '1 request');
assert.equal(f.plural(2, 'request'), '2 requests');

// 4. SSE parser: CRLF, split chunks, comments, multi-line data, [DONE], unterminated tail.
const { createSSEParser, ApiError, API_BASE } = await import(url('js/lib/api.js'));
assert.equal(API_BASE, '/admin/api');
const events = [];
const parser = createSSEParser((e) => events.push(e));
parser.feed(': keep-alive\r\n\r\nevent: message_start\r\ndata: {"a":');
parser.feed('1}\r');
parser.feed('\n\r\ndata: line one\ndata: line two\n\ndata: [DONE]\n\ndata: {"tail":true}');
parser.end();
assert.equal(events.length, 4);
assert.deepEqual(events[0], { event: 'message_start', data: '{"a":1}', id: '', json: { a: 1 } });
assert.equal(events[1].data, 'line one\nline two');
assert.equal(events[1].event, 'message');
assert.equal(events[2].data, '[DONE]');
assert.equal(events[2].json, undefined);
assert.deepEqual(events[3].json, { tail: true });
const err = new ApiError(422, 'bad', { issues: [{ path: 'a', message: 'm' }] });
assert.equal(err.status, 422);
assert.equal(err.issues.length, 1);
assert.equal(err.aborted, false);

// 5. Router.
const r = await import(url('js/lib/router.js'));
assert.deepEqual(r.parseHash('#/requests/req%201?status=error&q=a%20b'), { path: '/requests/req 1', segments: ['requests', 'req 1'], query: { status: 'error', q: 'a b' } });
assert.deepEqual(r.parseHash(''), { path: '/', segments: [], query: {} });
assert.equal(r.href('/requests/req 1', { status: 'error', empty: '', off: false, n: 0 }), '#/requests/req%201?status=error&n=0');

// 6. Store.
const { createStore, shallowEqual } = await import(url('js/lib/store.js'));
const store = createStore({ a: 1, b: 2 });
let calls = 0;
const off = store.subscribe(() => (calls += 1));
store.set({ a: 1 }); // no change: no notification
store.set({ a: 2 });
store.set((s) => ({ b: s.b + 1 }));
off();
store.set({ a: 9 });
assert.equal(calls, 2);
assert.deepEqual(store.get(), { a: 9, b: 3 });
assert.ok(shallowEqual([1, 2], [1, 2]));
assert.ok(!shallowEqual({ a: 1 }, { a: 1, b: 2 }));

// 7. Charts maths.
const c = await import(url('js/components/charts.js'));
assert.deepEqual(c.niceScale(0, 587).ticks, [0, 200, 400, 600]);
assert.deepEqual(c.niceScale(0, 1).ticks, [0, 0.5, 1]);
assert.deepEqual(c.niceScale(0, 0).ticks, [0, 0.5, 1]);
assert.equal(c.niceScale(0, 1_950_000).max, 2_000_000);
assert.equal(c.seriesColor({ color: 'red' }, 0), 'red');
assert.equal(c.seriesColor({}, 5), 'var(--series-6)');
assert.equal(c.seriesColor({}, 6), 'var(--series-other)');
const many = Array.from({ length: 9 }, (_, i) => ({ key: `s${i}`, values: [i, i] }));
const folded = c.foldSeries(many, 5);
assert.equal(folded.length, 6);
assert.deepEqual(folded.map((s) => s.key), ['s4', 's5', 's6', 's7', 's8', '__other']);
assert.deepEqual(folded[5].values, [0 + 1 + 2 + 3, 0 + 1 + 2 + 3]);
assert.equal(c.foldSeries(many.slice(0, 6), 5).length, 6); // nothing to fold

// 8. Table sorting.
const { sortRows } = await import(url('js/components/table.js'));
const rows = [{ n: 2, s: 'b10' }, { n: null, s: 'a' }, { n: 10, s: 'b9' }];
assert.deepEqual(sortRows(rows, { key: 'n' }, 'asc').map((x) => x.n), [2, 10, null]);
assert.deepEqual(sortRows(rows, { key: 'n' }, 'desc').map((x) => x.n), [10, 2, null]);
assert.deepEqual(sortRows(rows, { key: 's' }, 'asc').map((x) => x.s), ['a', 'b9', 'b10']);

// 9. JSON highlighting keeps every character and classifies tokens.
const { highlightJson } = await import(url('js/components/code.js'));
const sample = JSON.stringify({ model: 'gpt-4o', n: -1.5e3, ok: true, none: null, text: 'a "quoted" \\ string: {x}', list: [1, 2] }, null, 2);
const parts = highlightJson(sample);
const flat = parts.map((p) => (typeof p === 'string' ? p : p.props.children)).join('');
assert.equal(flat, sample);
const classOf = (text) => parts.find((p) => typeof p !== 'string' && p.props.children === text)?.props.class;
assert.equal(classOf('"model"'), 'tok-key');
assert.equal(classOf('"gpt-4o"'), 'tok-string');
assert.equal(classOf('-1.5e3') ?? classOf('-1500'), 'tok-number');
assert.equal(classOf('true'), 'tok-atom');
assert.equal(classOf('null'), 'tok-atom');
assert.equal(classOf('"a \\"quoted\\" \\\\ string: {x}"'), 'tok-string');

// 10. Status tone mapping.
const { toneForStatus } = await import(url('js/components/status.js'));
assert.deepEqual([200, 204, 302, 429, 400, 502, 0].map(toneForStatus), ['clear', 'clear', 'caution', 'caution', 'stop', 'stop', 'off']);

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// 11. The refetch policy of useResource. A response slower than the poll
// interval must still land: polling never aborts the request in flight.
const { createLoader } = await import(url('js/lib/hooks.js'));
{
  const seen = { calls: 0, aborted: 0, data: [], errors: [] };
  let delay = 60;
  let failNext = false;
  const loader = createLoader({
    fetch: (signal) =>
      new Promise((resolve, reject) => {
        const n = ++seen.calls;
        const fail = failNext;
        failNext = false;
        const timer = setTimeout(() => (fail ? reject(new Error(`failed ${n}`)) : resolve(n)), delay);
        signal.addEventListener('abort', () => {
          clearTimeout(timer);
          seen.aborted += 1;
          reject(new Error('aborted'));
        });
      }),
    onData: (data) => seen.data.push(data),
    onError: (error) => seen.errors.push(error.message),
  });

  // Polls every 10 ms against a 60 ms response.
  loader.restart();
  const poller = setInterval(() => loader.poll(), 10);
  for (const deadline = Date.now() + 3000; seen.data.length < 3 && seen.aborted === 0 && Date.now() < deadline; ) await sleep(5);
  clearInterval(poller);
  while (loader.busy) await sleep(5);
  assert.equal(seen.aborted, 0, 'polling must not abort the request in flight');
  assert.ok(seen.data.length >= 3, 'slow responses land while polling');
  assert.equal(seen.data.length, seen.calls, 'every request that was started completed');
  assert.deepEqual(seen.errors, []);

  // refresh() while a request is in flight: that one finishes, exactly one
  // more follows, and every caller waits for the follow-up.
  const before = seen.calls;
  const first = loader.refresh();
  const queued = [loader.refresh(), loader.refresh(), loader.refresh()];
  assert.ok(queued.every((p) => p === queued[0]) && queued[0] !== first, 'refreshes collapse into one follow-up');
  await first;
  assert.equal(seen.calls, before + 2, 'the follow-up starts as soon as the request in flight settles');
  await queued[0];
  assert.equal(seen.calls, before + 2, 'only one follow-up, however many refreshes');
  assert.equal(seen.data.at(-1), before + 2, 'refresh() resolves once the follow-up has landed');
  assert.equal(loader.busy, false);
  assert.equal(seen.aborted, 0);

  // restart() is the one that aborts: the source changed, the old answer is void.
  const landed = seen.data.length;
  loader.refresh();
  loader.restart();
  assert.equal(seen.aborted, 1);
  while (loader.busy) await sleep(5);
  assert.equal(seen.data.length, landed + 1, 'the aborted request reports nothing');
  assert.deepEqual(seen.errors, [], 'an abort is not an error');

  // A queued follow-up dies with cancel().
  loader.refresh();
  loader.refresh();
  loader.cancel();
  const callsAtCancel = seen.calls;
  await sleep(delay + 30);
  assert.equal(seen.calls, callsAtCancel, 'cancel() drops the queued follow-up');
  assert.equal(seen.data.length, landed + 1);
  assert.equal(seen.aborted, 2);

  // A failure is reported once and does not jam the loader.
  delay = 5;
  failNext = true;
  await loader.poll();
  assert.equal(seen.errors.length, 1);
  await loader.poll();
  assert.equal(seen.errors.length, 1);
  assert.equal(seen.data.at(-1), seen.calls);
}

// 12. api: whatever fails, and at whatever stage, the caller gets an ApiError.
const { api, isReportable, auth } = await import(url('js/lib/api.js'));
{
  const realFetch = globalThis.fetch;
  const text = (s) => new TextEncoder().encode(s);
  try {
    // Headers arrive, then the body stalls. The stream errors with a
    // TypeError when aborted, as wrapped body streams do.
    globalThis.fetch = async (_url, init) =>
      new Response(
        new ReadableStream({
          start(controller) {
            init.signal.addEventListener('abort', () => controller.error(new TypeError('terminated')));
          },
        }),
        { status: 200 },
      );
    await assert.rejects(api.get('/slow-body', { timeout: 40 }), (e) => e instanceof ApiError && e.status === 0 && e.code === 'timeout');
    const caller = new AbortController();
    setTimeout(() => caller.abort(), 20);
    await assert.rejects(api.get('/slow-body', { signal: caller.signal }), (e) => e instanceof ApiError && e.aborted && !isReportable(e));

    // The connection drops in the middle of the body.
    globalThis.fetch = async () =>
      new Response(
        new ReadableStream({
          start(controller) {
            controller.enqueue(text('{"a":'));
            controller.error(new TypeError('network error'));
          },
        }),
        { status: 200 },
      );
    await assert.rejects(api.get('/cut'), (e) => e instanceof ApiError && e.code === 'network' && /dropped/.test(e.message));

    // fetch itself fails.
    globalThis.fetch = async () => {
      throw new TypeError('fetch failed');
    };
    await assert.rejects(api.get('/down'), (e) => e instanceof ApiError && e.code === 'network' && /Cannot reach/.test(e.message));

    // Success without a body is null, with a body is the parsed JSON.
    globalThis.fetch = async () => new Response(null, { status: 204 });
    assert.equal(await api.put('/providers/probe', { a: 1 }), null);
    globalThis.fetch = async () => new Response('', { status: 200 });
    assert.equal(await api.del('/keys/k'), null);
    globalThis.fetch = async () => new Response('{"ok":true}', { status: 200 });
    assert.deepEqual(await api.get('/status'), { ok: true });
    globalThis.fetch = async () => new Response('<html>', { status: 200 });
    await assert.rejects(api.get('/status'), (e) => e instanceof ApiError && e.status === 200 && e.code === 'http');

    // An error status carries the gateway's message, issues and Retry-After.
    globalThis.fetch = async () =>
      new Response('{"error":{"message":"bad config","issues":[{"path":"providers[0].name","message":"required"}]}}', { status: 422, headers: { 'retry-after': '7' } });
    await assert.rejects(
      api.put('/config/raw', { text: '' }),
      (e) => e instanceof ApiError && e.status === 422 && e.message === 'bad config' && e.issues[0].path === 'providers[0].name' && e.retryAfter === 7,
    );

    // The secret travels as UTF-8 bytes in a header value fetch accepts, so a
    // pasted secret with a non-breaking hyphen or a zero-width space reaches
    // the gateway (and gets its 401) instead of failing as "cannot reach".
    const sent = [];
    globalThis.fetch = async (_url, init) => {
      const headers = new Headers(init.headers); // throws on a value fetch would refuse
      sent.push(headers.get('authorization'));
      return new Response('{"error":{"message":"Invalid admin secret."}}', { status: 401 });
    };
    for (const secret of ['sk\u2011admin\u20111234', 'gehe\u200bim', 'pässword', 'tab\tinside']) {
      await assert.rejects(api.login(secret), (e) => e instanceof ApiError && e.status === 401);
      assert.equal(Buffer.from(sent.at(-1).slice('Bearer '.length), 'latin1').toString('utf8'), secret);
    }
    await assert.rejects(api.login('plain-ascii'), (e) => e.status === 401);
    assert.equal(sent.at(-1), 'Bearer plain-ascii');
    assert.equal(auth.get().status, 'anonymous', 'a failed login stores nothing');

    // A control character cannot be sent at all: say so, and send nothing.
    const count = sent.length;
    for (const secret of ['pass\nword', 'pass\r\nword', 'nul\u0000byte']) {
      await assert.rejects(api.login(secret), (e) => e instanceof ApiError && e.status === 0 && e.code === 'invalid' && /control character/.test(e.message));
    }
    assert.equal(sent.length, count, 'an unsendable secret makes no request');

    // streamSSE: events arrive; a handler that throws surfaces its own error.
    const sse = () => new Response('data: {"n":1}\n\ndata: {"n":2}\n\n', { status: 200, headers: { 'content-type': 'text/event-stream' } });
    globalThis.fetch = async () => sse();
    const got = [];
    const meta = await api.streamSSE('/playground', {}, (event) => got.push(event.json.n));
    assert.deepEqual(got, [1, 2]);
    assert.equal(meta.streamed, true);
    const boom = new Error('handler bug');
    await assert.rejects(
      api.streamSSE('/playground', {}, () => {
        throw boom;
      }),
      (e) => e === boom,
    );
    globalThis.fetch = async () =>
      new Response(
        new ReadableStream({
          start(controller) {
            controller.enqueue(text('data: {"n":1}\n\n'));
            controller.error(new TypeError('terminated'));
          },
        }),
        { status: 200, headers: { 'content-type': 'text/event-stream' } },
      );
    await assert.rejects(api.streamSSE('/playground', {}, () => {}), (e) => e instanceof ApiError && e.code === 'network');
  } finally {
    globalThis.fetch = realFetch;
    api.logout();
  }
}

// 13. Sign-in: the odd character in a pasted secret is named.
const { oddCharacter } = await import(url('js/pages/login.js'));
assert.equal(oddCharacter('plain-ASCII_123 ~'), null);
assert.deepEqual(oddCharacter('sk\u2011admin'), { hex: 'U+2011', name: 'a non-breaking hyphen' });
assert.deepEqual(oddCharacter('gehe\u200bim'), { hex: 'U+200B', name: 'an invisible zero-width space' });
assert.deepEqual(oddCharacter('pässword'), { hex: 'U+00E4', name: null });
assert.deepEqual(oddCharacter('a\u{1F511}'), { hex: 'U+1F511', name: null });

// 14. CodeBlock re-indents JSON without touching a token.
const { formatJson } = await import(url('js/components/code.js'));
{
  const wire = '{"seed":12345678901234567890,"temperature":1.0,"big":9007199254740993,"e":1e3,"a":1,"a":2,"s":"\\u00e9 \\" {x}, [y]: z","empty":{},"none":[],"nested":[{"k":[1,-2.50,true,null]}]}';
  const pretty = formatJson(wire);
  // Nothing but white space differs from what was sent.
  const significant = (s) => s.replace(/("(?:\\.|[^"\\])*")|\s+/g, (m, str) => str ?? '');
  assert.equal(significant(pretty), wire);
  for (const token of ['12345678901234567890', '1.0', '9007199254740993', '1e3', '"a": 1', '"a": 2', '-2.50', '\\u00e9']) {
    assert.ok(pretty.includes(token), `${token} survives formatting`);
  }
  // Same layout as JSON.stringify(value, null, 2) where that is lossless.
  const value = { model: 'gpt-4o', messages: [{ role: 'user', content: 'hi: {there}, "you"' }], tools: [], meta: {}, n: [1, [2, [3]]], t: true, z: null };
  assert.equal(formatJson(JSON.stringify(value)), JSON.stringify(value, null, 2));
  assert.equal(formatJson(` \n${JSON.stringify(value, null, 4)}\n`), JSON.stringify(value, null, 2));
  assert.equal(formatJson('[]'), '[]');
  assert.equal(formatJson('{"truncated": tr'), null);
  assert.equal(formatJson('data: {"a":1}'), null);
  // A body with an inline image: megabytes inside one string.
  const image = `{"image":"${'A'.repeat(3_000_000)}\\n${'B'.repeat(3_000_000)}","n":1}`;
  assert.equal(formatJson(image).length, image.length + '\n  '.length * 2 + ' '.length * 2 + '\n'.length);
  assert.equal(highlightJson(`{"k":"${'x'.repeat(190_000)}"}`).length, 5);
}

// 15. Arrow keys in Tabs and Segmented skip disabled items and wrap.
const { rovingIndex } = await import(url('js/components/nav.js'));
{
  const lastDisabled = (i) => i === 3; // [Summary, Attempts, Bodies, Raw events (disabled)]
  assert.equal(rovingIndex('ArrowRight', 2, 4, lastDisabled), 0, 'wraps past a disabled last tab');
  assert.equal(rovingIndex('ArrowLeft', 0, 4, lastDisabled), 2);
  assert.equal(rovingIndex('End', 0, 4, lastDisabled), 2);
  assert.equal(rovingIndex('Home', 2, 4, (i) => i === 0), 1);
  const middle = (i) => i === 1 || i === 2;
  assert.equal(rovingIndex('ArrowRight', 0, 4, middle), 3, 'steps over disabled tabs');
  assert.equal(rovingIndex('ArrowLeft', 3, 4, middle), 0);
  assert.equal(rovingIndex('ArrowDown', 1, 3), 2);
  assert.equal(rovingIndex('ArrowUp', 0, 3), 2);
  assert.equal(rovingIndex('ArrowRight', 1, 3, (i) => i !== 1), 1, 'the only enabled item keeps focus');
  assert.equal(rovingIndex('ArrowRight', 0, 3, () => true), -1);
  assert.equal(rovingIndex('Enter', 0, 3), -1);
}

// 16. NumberInput: text the field cannot accept is named, never saved.
const { numberProblem } = await import(url('js/components/form.js'));
assert.equal(numberProblem('500', { min: 0, max: 1000, whole: true }), null);
assert.equal(numberProblem('5000', { min: 0, max: 1000, whole: true }), 'Enter a number from 0 to 1000.');
assert.equal(numberProblem('-1', { min: 0 }), 'Enter 0 or more.');
assert.equal(numberProblem('11', { max: 10 }), 'Enter 10 or less.');
assert.equal(numberProblem('1.5', { whole: true }), 'Enter a whole number.');
assert.equal(numberProblem('1.5', { whole: false, min: 0, max: 2 }), null);
assert.equal(numberProblem('abc'), 'Enter a number.');
assert.equal(numberProblem('1,200', { max: 2000 }), null);
assert.equal(numberProblem('', { min: 1 }), null);
assert.equal(numberProblem('  ', { min: 1 }), null);

// 17. Components in a document: see dom.mjs.
await import('./dom.mjs');

// 18. The dev server: tools/ui-dev.mjs.
await import('./dev-server.mjs');

console.log('all assertions passed');
