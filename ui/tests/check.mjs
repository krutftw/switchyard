// Dashboard self-check: no browser, no dependencies.
//
//   node ui/tests/check.mjs
//
// It syntax-checks every module, imports each one under Node (which catches
// broken import paths and missing exports), checks that every route loads a
// page component, asserts the pure logic in lib/ and the chart maths, checks
// index.html against the gateway's Content-Security-Policy, then renders
// components into a stub document (dom.mjs).
// Run it before handing a page over. It does not replace looking at the page:
// run the gateway (cargo run -p switchyard) and open /admin/ for that.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
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
];
let parsed = 0;
for (const file of all) {
  const result = spawnSync(process.execPath, ['--check', file], { encoding: 'utf8' });
  assert.equal(result.status, 0, `${file} does not parse:
${result.stderr}`);
  parsed += 1;
}
console.log(`parsed ${parsed} modules`);

// 1. Every module imports. Two files are only parsed above: app.js needs a
// document, and theme-boot.js is a classic script for the browser (it is run
// against a stub in step 18). A route module, a file directly in js/pages/,
// must default-export its component. Sub-modules in js/pages/<name>/ are a
// page's own business and may export whatever they like.
const pagesDir = path.join(ui, 'js', 'pages');
const notImported = new Set([path.join(ui, 'js', 'app.js'), path.join(ui, 'js', 'theme-boot.js')]);
const modules = walk(path.join(ui, 'js')).filter((f) => f.endsWith('.js') && !notImported.has(f));
/** Is this file a route module (and so needs a default-exported component)? */
const isRouteModule = (file) => path.dirname(file) === pagesDir;
assert.equal(isRouteModule(path.join(pagesDir, 'keys.js')), true);
assert.equal(isRouteModule(path.join(pagesDir, 'keys', 'util.js')), false, 'a sub-module of a page is not a route module');
let imported = 0;
let routeModules = 0;
for (const file of modules) {
  const mod = await import(pathToFileURL(file).href);
  if (isRouteModule(file)) {
    assert.equal(typeof mod.default, 'function', `${file} needs a default export`);
    routeModules += 1;
  }
  imported += 1;
}
assert.ok(routeModules >= 11, 'the route modules were found');
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
// A countdown in words ticks in whole seconds and rounds up: never "0s" with time left.
assert.equal(f.formatCountdownWords(42), '42s');
assert.equal(f.formatCountdownWords(41.2), '42s');
assert.equal(f.formatCountdownWords(0.3), '1s');
assert.equal(f.formatCountdownWords(0), '0s');
assert.equal(f.formatCountdownWords(-5), '0s');
assert.equal(f.formatCountdownWords(60), '1m 00s');
assert.equal(f.formatCountdownWords(1781), '29m 41s');
assert.equal(f.formatCountdownWords(3720), '1h 02m');
assert.equal(f.formatCountdownWords(3600), '1h 00m');
assert.equal(f.formatCountdownWords(90_000), '1d 1h');
assert.equal(f.formatCountdownWords(undefined), '—');
// A duration in words: the two largest units.
assert.equal(f.formatDurationWords(30_000), '30 seconds');
assert.equal(f.formatDurationWords(1000), '1 second');
assert.equal(f.formatDurationWords(1_800_000), '30 minutes');
assert.equal(f.formatDurationWords(5_400_000), '1 hour 30 minutes');
assert.equal(f.formatDurationWords(86_400_000), '1 day');
assert.equal(f.formatDurationWords(90_061_000), 'about 1 day 1 hour');
assert.equal(f.formatDurationWords(0), '0 seconds');
assert.equal(f.formatDurationWords(null), '—');
// The whole instant: year and milliseconds always, the zone on request.
{
  const at = Date.UTC(2026, 9, 2, 14, 3, 27, 512);
  assert.equal(f.formatTimestamp(at, { utc: true }), '2 Oct 2026 14:03:27.512 UTC');
  const local = new Date(2026, 9, 2, 14, 3, 27, 512);
  assert.equal(f.formatTimestamp(local), '2 Oct 2026 14:03:27.512');
  assert.match(f.formatTimestamp(local, { zone: true }), /^2 Oct 2026 14:03:27\.512 UTC[+−]\d\d:\d\d$/);
  assert.equal(f.formatTimestamp(null), '—');
  // UTC variants of the existing formatters: a UTC day bucket keeps its date everywhere.
  const midnight = Date.UTC(2026, 9, 2);
  assert.equal(f.formatDate(midnight, new Date(Date.UTC(2026, 0, 1)), { utc: true }), '2 Oct');
  assert.equal(f.formatTime(midnight, { utc: true }), '00:00:00');
  assert.equal(f.formatTime(at, { utc: true, ms: true }), '14:03:27.512');
  assert.match(f.formatDateTime(midnight, { utc: true }), /^2 Oct( 2026)? 00:00:00$/);
}
// A gateway message as a sentence, for text that is followed by more text.
assert.equal(f.sentence('the request body is not valid JSON'), 'The request body is not valid JSON.');
assert.equal(f.sentence('Already a sentence.'), 'Already a sentence.');
assert.equal(f.sentence('is that so?'), 'Is that so?');
assert.equal(f.sentence('routing.max_attempts: must be at least 1'), 'routing.max_attempts: must be at least 1.');
assert.equal(f.sentence('rate_limit_rpm must be a whole number'), 'rate_limit_rpm must be a whole number.');
assert.equal(f.sentence('`fast` has no routable target'), '`fast` has no routable target.');
assert.equal(f.sentence(''), '');
assert.equal(f.sentence(f.sentence('name must not be empty')), 'Name must not be empty.', 'applying it twice changes nothing');

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
assert.ok(r.sameRoute(r.parseHash('#/keys?a=1&b=2'), r.parseHash('#/keys?b=2&a=1')), 'the order of the query does not make a different view');
assert.ok(!r.sameRoute(r.parseHash('#/keys?a=1'), r.parseHash('#/keys?a=2')));
assert.ok(!r.sameRoute(r.parseHash('#/keys?a=1'), r.parseHash('#/keys')));
assert.ok(!r.sameRoute(r.parseHash('#/keys'), r.parseHash('#/models')));
assert.equal(typeof r.registerLeaveGuard, 'function');
assert.equal(await r.mayLeave(), true, 'nothing objects when no guard is registered');
// (The guards themselves are exercised in dom.mjs, against a stub history.)

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
// Axes that count things: whole-number ticks, however small the maximum.
assert.deepEqual(c.niceScale(0, 1, 4, { integer: true }).ticks, [0, 1], 'a maximum of 1 has no 0.5 tick');
assert.deepEqual(c.niceScale(0, 0, 4, { integer: true }).ticks, [0, 1]);
assert.deepEqual(c.niceScale(0, 3, 4, { integer: true }).ticks, [0, 1, 2, 3]);
assert.deepEqual(c.niceScale(0, 1, 2, { integer: true }).ticks, [0, 1], 'also on a short chart, which asks for fewer ticks');
assert.deepEqual(c.niceScale(0, 587, 4, { integer: true }).ticks, [0, 200, 400, 600], 'large counts are unchanged');
for (const max of [1, 2, 3, 5, 7, 12, 99]) {
  for (const target of [2, 4]) assert.ok(c.niceScale(0, max, target, { integer: true }).ticks.every(Number.isInteger), `whole ticks up to ${max}`);
}
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

// 10. Status tone mapping. A 101 is a WebSocket switched to: information, not a failure.
const { toneForStatus } = await import(url('js/components/status.js'));
assert.deepEqual([200, 204, 302, 429, 400, 502, 0].map(toneForStatus), ['clear', 'clear', 'caution', 'caution', 'stop', 'stop', 'off']);
assert.deepEqual([100, 101, 199, null, undefined].map(toneForStatus), ['info', 'info', 'info', 'off', 'off']);
const { statusTone } = await import(url('js/pages/requests/cells.js'));
assert.equal(statusTone({ status: 101, ok: false }), 'stop', 'a broken WebSocket relay is a failed request despite its successful upgrade');
assert.equal(statusTone({ status: 101, ok: true }), 'clear', 'an orderly WebSocket relay remains successful');
assert.equal(statusTone({ status: 200, ok: false }), 'stop', 'a stream failure overrides the initial HTTP success');
assert.equal(statusTone({ status: 429, ok: false }), 'caution', 'rate-limit refusals retain their caution tone');

// 10b. Chart nouns and names: one HealthStrip noun for one attempt, the full
// name of a shortened series for its title.
assert.equal(c.countNoun('requests', 1), 'request');
assert.equal(c.countNoun('requests', 2), 'requests');
assert.equal(c.countNoun('upstream attempts', 1), 'upstream attempt', 'a string noun is a plural; its singular drops the s');
assert.equal(c.countNoun(['person', 'people'], 1), 'person');
assert.equal(c.countNoun(['person', 'people'], 0), 'people');
assert.equal(c.countNoun(['upstream attempt'], 3), 'upstream attempts');
assert.equal(c.countNoun('access', 1), 'access', 'a word ending in ss is left alone');
assert.equal(c.seriesTitle({ key: 'k', label: 'gpt-4o…', title: 'openai/gpt-4o-2024-08-06' }), 'openai/gpt-4o-2024-08-06');
assert.equal(c.seriesTitle({ key: 'k', label: 'gpt-4o' }), 'gpt-4o');
assert.equal(c.seriesTitle({ key: 'k' }), 'k');
assert.equal(c.seriesTitle({ key: 'k', label: { type: 'span' } }), undefined, 'a label that is markup has no title of its own');

// 10c. Replay: one answer to "can the playground send this record again",
// for the request drawer and the playground alike.
{
  const replay = await import(url('js/lib/replay.js'));
  const { PROTOCOL_IDS } = await import(url('js/pages/playground/protocols.js'));
  assert.deepEqual(replay.REPLAY_PROTOCOLS, PROTOCOL_IDS, 'the protocols the playground replays are the ones it speaks');
  const rec = (client_protocol, endpoint) => ({ client_protocol, endpoint });
  const body = { client_request: '{"model":"m"}' };
  for (const [protocol, endpoint] of [
    ['openai-chat', 'POST /v1/chat/completions'],
    ['openai-responses', 'POST /v1/responses'],
    ['openai-responses', 'GET /v1/responses (WebSocket)'],
    ['anthropic', 'POST /v1/messages'],
    ['gemini', 'POST /v1beta/models/gemini-2.0-flash:generateContent'],
    ['gemini', 'POST /v1beta/models/x:streamGenerateContent'],
    ['openai-chat', 'POST /admin/api/playground'],
    ['anthropic', undefined],
  ]) {
    assert.equal(replay.canReplayInPlayground(rec(protocol, endpoint), body), true, `${protocol} ${endpoint}`);
    assert.equal(replay.replayProblem(rec(protocol, endpoint), body), null);
  }
  assert.equal(replay.replayProblem(rec('openai-chat', 'POST /v1/embeddings'), body), 'endpoint');
  assert.equal(replay.replayProblem(rec('anthropic', 'POST /v1/messages/count_tokens'), body), 'endpoint');
  assert.equal(replay.replayProblem(rec('openai-chat', 'POST /v1/realtime'), body), 'endpoint');
  assert.equal(replay.replayProblem(rec('ollama', 'POST /api/chat'), body), 'protocol');
  assert.equal(replay.replayProblem(null, body), 'protocol');
  // The body: asked about only when bodies are passed, and checked first (the playground's order).
  assert.equal(replay.replayProblem(rec('ollama', 'POST /api/chat'), null), 'no-body');
  assert.equal(replay.replayProblem(rec('openai-chat', 'POST /v1/chat/completions'), { client_request: '' }), 'no-body');
  assert.equal(replay.canReplayInPlayground(rec('openai-chat', 'POST /v1/chat/completions'), undefined), false, 'bodies passed but not loaded: no');
  assert.equal(replay.canReplayInPlayground(rec('openai-chat', 'POST /v1/chat/completions')), true, 'without bodies only the kind of request is judged');
  assert.equal(replay.canReplayInPlayground(rec('openai-chat', 'POST /v1/embeddings')), false);
  // The two pages use it, and keep no copy of their own.
  const source = (rel) => fs.readFileSync(path.join(ui, rel), 'utf8');
  assert.match(source('js/pages/requests/detail.js'), /canReplayInPlayground\(record, bodies\)/);
  assert.match(source('js/pages/playground.js'), /replayProblem\(record, source\.data\.bodies\)/);
  for (const rel of ['js/pages/playground.js', 'js/pages/requests/record.js', 'js/pages/requests/detail.js']) {
    assert.doesNotMatch(source(rel), /generateContent\$/, `${rel} keeps no copy of the endpoint list`);
  }
}

// 10d. The providers page model: a failure on one model is that model's
// trouble, not the provider's; discovery that is off says so.
{
  const { providerHealth, discoveryInfo } = await import(url('js/pages/providers/model.js'));
  const at = 1_000_000;
  const cred = (extra) => ({ id: 'p:1', status: 'ready', disabled: false, disabled_by: null, usable: true, cooldown_until: null, model_cooldowns: [], requests: 0, successes: 0, failures: 0, consecutive_failures: 0, last_error: null, ...extra });
  const prov = (credentials, extra = {}) => ({ name: 'p', kind: 'mock', enabled: true, model_count: 8, discover: true, config: { models: [] }, discovery: { state: 'off' }, credentials, ...extra });
  const failure = (cls, model) => ({ status: 500, class: cls, message: 'x', at: at - 10, model });
  // One model-scoped failure: Ready, and the model is named, resting or not.
  let h = providerHealth(prov([cred({ requests: 1, failures: 1, consecutive_failures: 1, last_error: failure('server', 'mock-error-500'), model_cooldowns: [{ model: 'mock-error-500', until: at + 30_000, reason: 'server' }] })]), at);
  assert.deepEqual([h.label, h.detail], ['Ready', 'mock-error-500 resting']);
  h = providerHealth(prov([cred({ requests: 1, failures: 1, consecutive_failures: 1, last_error: failure('server', 'mock-error-500') })]), at);
  assert.deepEqual([h.label, h.detail], ['Ready', 'mock-error-500 failed']);
  // A transport failure is the upstream as a whole.
  h = providerHealth(prov([cred({ requests: 1, failures: 1, consecutive_failures: 1, last_error: failure('transport', 'm'), model_cooldowns: [{ model: 'm', until: at + 30_000, reason: 'transport' }] })]), at);
  assert.equal(h.label, 'Failing');
  // Failures on two models and no success: Failing (one credential, or two).
  h = providerHealth(prov([cred({ requests: 2, failures: 2, consecutive_failures: 2, last_error: failure('server', 'b'), model_cooldowns: [{ model: 'a', until: at + 30_000, reason: 'server' }] })]), at);
  assert.equal(h.label, 'Failing');
  h = providerHealth(prov([cred({ id: 'p:1', requests: 1, failures: 1, last_error: failure('server', 'a') }), cred({ id: 'p:2', requests: 1, failures: 1, last_error: failure('server', 'b') })]), at);
  assert.equal(h.label, 'Failing');
  // openai-compat with discovery off and no models: says so, in both places.
  const compat = prov([cred({})], { kind: 'openai-compat', discover: false, model_count: 0 });
  assert.equal(discoveryInfo(compat, at).short, 'discovery off');
  h = providerHealth(compat, at);
  assert.equal(h.detail, 'discovery is off');
}

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
  // Ordinary inline strings are formatted; bounded input/depth/output work
  // preserves raw text for larger documents instead of expanding it.
  const image = `{"image":"${'A'.repeat(1000)}\\n${'B'.repeat(1000)}","n":1}`;
  assert.equal(formatJson(image).length, image.length + '\n  '.length * 2 + ' '.length * 2 + '\n'.length);
  assert.equal(highlightJson(`{"k":"${'x'.repeat(190_000)}"}`).length, 5);
  const large = JSON.stringify({ value: 'x'.repeat(200_000) });
  assert.equal(formatJson(large), null);
  assert.deepEqual(highlightJson(large), [large]);
  assert.equal(formatJson('['.repeat(65) + '0' + ']'.repeat(65)), null);
  const incomplete = '{"value":"unfinished';
  assert.deepEqual(highlightJson(incomplete), [incomplete]);
  const expanding = '['.repeat(64) + Array(8000).fill('0').join(',') + ']'.repeat(64);
  assert.ok(formatJson(expanding) === null, 'indentation has a fixed output budget');
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

// 17. Icons the pages draw by hand until now.
const { ICON_NAMES } = await import(url('js/components/icons.js'));
for (const name of ['stop', 'arrow-left', 'grip']) assert.ok(ICON_NAMES.includes(name), `icon "${name}"`);

// 18. index.html under the gateway's Content-Security-Policy
// (crates/admin/src/assets.rs: script-src 'self'). No inline script and no
// inline event handler survives that policy, so there must be none; and the
// theme has to be stamped by a blocking script before any stylesheet loads.
{
  const page = fs.readFileSync(path.join(ui, 'index.html'), 'utf8').replace(/<!--[\s\S]*?-->/g, '');
  const scripts = [...page.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/gi)].map((m) => ({ attrs: m[1], body: m[2].trim(), at: m.index }));
  assert.ok(scripts.length >= 2);
  for (const script of scripts) {
    assert.match(script.attrs, /\bsrc="[^":]+"/, 'every script is a same-origin file (script-src \'self\')');
    assert.equal(script.body, '', 'no inline script: the policy blocks it');
  }
  assert.doesNotMatch(page, /<[^>]+\son[a-z]+\s*=/i, 'no inline event handlers');
  assert.doesNotMatch(page, /javascript:/i);
  const boot = scripts.find((s) => /src="js\/theme-boot\.js"/.test(s.attrs));
  assert.ok(boot, 'the theme bootstrap is loaded from a file');
  assert.doesNotMatch(boot.attrs, /\b(type="module"|async|defer)\b/, 'it blocks: the theme is set before the first paint');
  assert.ok(boot.at < page.indexOf('rel="stylesheet"'), 'and it comes before the stylesheets');
  assert.ok(boot.at < page.indexOf('<body'), 'in the head');

  // Fonts: each preload is in CORS mode and is what an @font-face in this
  // document fetches. A face declared in a stylesheet file is not: on a
  // reload Chrome reuses that sheet with the font it fetched for the previous
  // page, and the console reports the preload as unused.
  const preloads = [...page.matchAll(/<link\b[^>]*\brel="preload"[^>]*>/g)].map((m) => m[0]).filter((tag) => /\bas="font"/.test(tag));
  assert.equal(preloads.length, 2, 'both fonts are preloaded');
  const faces = [...page.matchAll(/@font-face\s*\{[^}]*src:\s*url\("([^"]+)"\)/g)].map((m) => m[1]);
  for (const tag of preloads) {
    assert.match(tag, /\scrossorigin(?:[\s=/>])/, 'a font preload is crossorigin: fonts are fetched in CORS mode');
    assert.ok(faces.includes(/href="([^"]+)"/.exec(tag)[1]), `${tag} is the url() of an @font-face in index.html`);
  }
  assert.equal(faces.length, preloads.length);
  for (const sheet of walk(path.join(ui, 'css')).filter((file) => file.endsWith('.css'))) {
    assert.doesNotMatch(fs.readFileSync(sheet, 'utf8').replace(/\/\*[\s\S]*?\*\//g, ''), /@font-face/, `${path.basename(sheet)} declares no font face: index.html does`);
  }

  // The bootstrap itself, against the least it needs of a browser.
  const source = fs.readFileSync(path.join(ui, 'js', 'theme-boot.js'), 'utf8');
  const run = ({ stored, prefersLight = false, storageThrows = false }) => {
    const attrs = {};
    const meta = { content: '#101316', setAttribute: (name, value) => (meta[name] = value) };
    vm.runInNewContext(source, {
      document: {
        documentElement: { setAttribute: (name, value) => (attrs[name] = value) },
        querySelector: (selector) => (selector === 'meta[name="theme-color"]' ? meta : null),
      },
      localStorage: {
        getItem: (key) => {
          if (storageThrows) throw new Error('blocked');
          return key === 'sy.theme' ? (stored ?? null) : null;
        },
      },
      window: { matchMedia: true },
      matchMedia: (query) => ({ matches: prefersLight && /prefers-color-scheme: light/.test(query) }),
    });
    return { theme: attrs['data-theme'], color: meta.content };
  };
  assert.deepEqual(run({ stored: 'light' }), { theme: 'light', color: '#eff1f4' }, 'a stored light theme is light from the first frame');
  assert.deepEqual(run({ stored: 'dark', prefersLight: true }), { theme: 'dark', color: '#101316' }, 'a stored choice beats the system');
  assert.deepEqual(run({ prefersLight: true }), { theme: 'light', color: '#eff1f4' }, 'without one the system decides');
  assert.deepEqual(run({}), { theme: 'dark', color: '#101316' });
  assert.deepEqual(run({ stored: 'purple' }), { theme: 'dark', color: '#101316' }, 'a value that is not ours is ignored');
  assert.deepEqual(run({ storageThrows: true, prefersLight: true }), { theme: 'dark', color: '#101316' }, 'blocked storage is survived');
  // The same key and colours as lib/theme.js, which takes over once the app runs.
  const themeModule = fs.readFileSync(path.join(ui, 'js', 'lib', 'theme.js'), 'utf8');
  for (const literal of ["'sy.theme'", "'#101316'", "'#eff1f4'"]) {
    assert.ok(source.includes(literal) && themeModule.includes(literal), `theme-boot.js and lib/theme.js agree on ${literal}`);
  }
}

// 19. Shared CSS: rules a page relies on and a browser-less check can still
// hold on to. (How they look is checked in a browser.)
{
  const css = (name) => fs.readFileSync(path.join(ui, 'css', name), 'utf8').replace(/\/\*[\s\S]*?\*\//g, '');
  const base = css('base.css');
  const components = css('components.css');
  /** The declarations of the first rule whose selector is exactly `selector`. */
  const rule = (sheet, selector) => {
    const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    const found = new RegExp(`(?:^|[}\\n])\\s*${escaped}\\s*\\{([^}]*)\\}`).exec(sheet);
    assert.ok(found, `a rule for ${selector}`);
    return found[1];
  };
  // The focus ring follows the element's own corners: no radius of its own.
  assert.doesNotMatch(rule(base, ':focus-visible'), /border-radius/);
  // The native clear button of a search field is hidden.
  assert.match(base, /input\[type="search"\]::-webkit-search-cancel-button[^{]*\{[^}]*display:\s*none/);
  // Fresh rows flash only when motion is welcome.
  assert.match(components, /@media \(prefers-reduced-motion: no-preference\)\s*\{\s*\.table tbody tr\[data-fresh\]\s*\{[^}]*animation:/);
  assert.doesNotMatch(components.replace(/@media \(prefers-reduced-motion: no-preference\)\s*\{[^{}]*\{[^{}]*\}\s*\}/g, ''), /tr\[data-fresh\]\s*\{[^}]*animation:/, 'the flash exists only inside that media query');
  // Toasts can be lifted by a page; drawers and modals say what colour they are.
  assert.match(rule(components, '.toasts'), /--toast-lift/);
  assert.match(rule(components, '.track-bed'), /stroke:\s*var\(--surface-bg, var\(--bg-surface\)\)/);
  assert.match(components, /\.modal,\s*\.drawer\s*\{[^}]*--surface-bg:\s*var\(--bg-raised\)/);
  // Sticky headers: a variable for where they stop, 0 inside an overlay's body.
  assert.match(rule(components, '.table[data-sticky] th'), /top:\s*var\(--table-sticky-top, 0px\)/);
  assert.match(rule(components, '.table-wrap[data-page-sticky]'), /--table-sticky-top:\s*var\(--sticky-top, var\(--topbar-h\)\)/);
  assert.match(rule(components, '.overlay-body'), /--sticky-top:\s*0px/);
  // A focused row stops below the top bar and the sticky header, not under them.
  assert.match(components, /\.table-wrap\[data-page-sticky\] tbody :is\(tr,[^)]*\[tabindex\]\)\s*\{[^}]*scroll-margin-top:\s*calc\(var\(--table-sticky-top\) \+/);
  // No scroll-padding on the root: pages compensate for the top bar themselves (scroll-margin-top on their anchors).
  assert.doesNotMatch(base + components + css('layout.css'), /scroll-padding/);
  // The phone sort menu can be hidden by a page with one class.
  assert.match(components, /:where\(\.table-wrap\[data-collapse\]\) \.table-sortbar\s*\{[^}]*display:\s*flex/);
  assert.doesNotMatch(components, /(?<!:where\()\.table-wrap\[data-collapse\] \.table-sortbar/);
  // Only icon buttons are squared inside an input; a segmented control in a field keeps its width.
  assert.doesNotMatch(rule(components, '.input-actions .btn'), /width/);
  assert.match(rule(components, '.input-actions .icon-btn'), /width/);
  assert.match(rule(components, '.field > .seg'), /align-self:\s*flex-start/);
  // A notice wraps on phones; legend labels are cut, not pushed out.
  assert.match(components, /@media \(max-width: 720px\)\s*\{\s*\.notice\s*\{[^}]*flex-wrap:\s*wrap/);
  assert.match(rule(components, '.chart-legend-label'), /text-overflow:\s*ellipsis/);
  // So is a long series name heading a column of a chart's table view.
  assert.match(rule(components, '.chart-table-head'), /text-overflow:\s*ellipsis/);
  assert.match(rule(components, '.chart-table-head'), /max-width/);
  // A stale StatGroup dims its readings, as the guide's example implies.
  assert.match(components, /\.stat-group\[data-stale\] \.stat-value,\s*\.stat-group\[data-stale\] \.stat-trend\s*\{[^}]*opacity:/);
  // The trend gives way in a narrow cell; the value does not.
  assert.match(rule(components, '.stat-trend'), /flex:\s*0 1 auto/);
  assert.match(rule(components, '.stat-trend'), /min-width:\s*0/);
  // A CodeBlock without a title still has a bar for its tools: nothing floats over the code.
  assert.match(rule(components, '.code-bar[data-untitled]'), /justify-content:\s*flex-end/);
  assert.doesNotMatch(components, /\.code-floating/);
  // Touch targets: on coarse pointers the small controls get a 44px hit area.
  const coarse = /@media \(pointer: coarse\)\s*\{([\s\S]*?)\n\}/g;
  const coarseRules = [...components.matchAll(coarse)].map((m) => m[1]).join('\n');
  for (const selector of ['.icon-btn::before', '.switch::before', '.check::before', '.tag-x::before', '.th-sort::before', '.seg-opt::before']) {
    assert.ok(coarseRules.includes(selector), `${selector} gives a hit area on coarse pointers`);
  }
  assert.match(coarseRules, /height:\s*max\(100%, var\(--tap-min\)\)/);
  assert.match(css('tokens.css'), /--tap-min:\s*44px/);
  // A text field's input fills the field, so a tap anywhere in the outline lands in it.
  assert.match(rule(components, '.input-el'), /align-self:\s*stretch/);
}

// 20. The guide tells the truth about the things this check can see.
{
  const guide = fs.readFileSync(path.join(ui, 'UI_GUIDE.md'), 'utf8');
  assert.doesNotMatch(guide, /providers\.data\?\.providers/, 'the admin API returns bare arrays: rows=${providers.data}');
  assert.match(guide, /rows=\$\{providers\.data\}/);
  assert.doesNotMatch(guide, /ui-dev|dev-server/, 'the dev server is gone: the gateway serves ui/ from disk');
  assert.match(guide, /cargo run -p switchyard/);
  assert.equal(fs.existsSync(path.resolve(ui, '..', 'tools', 'ui-dev.mjs')), false);
  assert.equal(fs.existsSync(path.join(ui, 'tests', 'dev-server.mjs')), false);
  for (const name of ['useLeaveGuard', 'registerLeaveGuard', 'mayLeave', 'useLiveGap', 'keepPrevious', 'isPrevious', 'inLayer', 'returnFocus', 'errorTitle', 'sortMenu', 'data-row-key', 'lampLabel', 'tipFormat', 'minPoints', '--sticky-top', '--toast-lift', '--surface-bg', 'formatCountdownWords', 'formatDurationWords', 'formatTimestamp', 'sentence(', 'clearable', 'onClear', 'toneWord', 'toneLabel', 'overlayLocked', 'canReplayInPlayground', 'replayProblem', 'live.onDown', '{ onDown', '--tap-min', 'never sit over the code']) {
    assert.ok(guide.includes(name), `the guide documents ${name}`);
  }
  // The touch-target sentence says what the CSS does, not more.
  assert.doesNotMatch(guide, /Touch targets are 44px on coarse pointers; the control tokens already grow/, 'controls are 34 to 46px tall on touch screens, not 44');
  assert.match(guide, /34px \(`sm`\), 40px \(`md`\) and 46px\s+\(`lg`\)/);
  const tokens = fs.readFileSync(path.join(ui, 'css', 'tokens.css'), 'utf8');
  const coarseTokens = /@media \(pointer: coarse\)\s*\{\s*:root\s*\{([^}]*)\}/.exec(tokens)[1];
  assert.match(coarseTokens, /--control-h-sm:\s*34px/);
  assert.match(coarseTokens, /--control-h-md:\s*40px/);
  assert.match(coarseTokens, /--control-h-lg:\s*46px/);
}

// The client-model drill-down, and the API's explicit model/credential fields.
{
  const { default: matchesFilters, FILTER_KEYS } = await import(url('js/pages/requests/record.js'));
  const record = { started_at: 1000, client_model: 'alias', requested_model: 'alias(high)', upstream_model: 'target', status: 200, ok: true };
  assert.ok(FILTER_KEYS.includes('client_model') && FILTER_KEYS.includes('since'));
  assert.equal(matchesFilters(record, { client_model: 'ALIAS', since: '1000' }), true);
  assert.equal(matchesFilters(record, { client_model: 'target' }), false);
  assert.equal(matchesFilters(record, { client_model: 'alias', model: 'target' }), true);
  assert.equal(matchesFilters(record, { since: '1001' }), false);
  assert.equal(matchesFilters(record, { since: '1e3' }), false);
  assert.equal(matchesFilters(record, { client_model: 'alias' }, true), false);
  assert.equal(matchesFilters({ started_at: 1000 }, { client_model: 'unknown' }), true);
  const { GROUPS } = await import(url('js/pages/usage/data.js'));
  assert.equal(GROUPS.find((group) => group.value === 'model').param, 'client_model');
  const { buildRows } = await import(url('js/pages/models/logic.js'));
  assert.equal(buildRows([{ name: 'alias', alias_targets: ['base'], shadows_model: true, routes: [], info: {} }], [])[0].shadows_model, true);
  const { draftFromConfig, configFromDraft } = await import(url('js/pages/providers/model.js'));
  const draft = draftFromConfig({ name: 'local', kind: 'openai-compat', credentials: [{ api_key: null }, { api_key: null, label: 'second' }] });
  assert.deepEqual(configFromDraft(draft).config.credentials.map((row) => row.api_key), [null, null]);
  draft.credentials[1].label = 'renamed';
  draft.credentials.reverse();
  assert.equal(configFromDraft(draft).config.credentials[0].label, 'renamed');
  draft.credentials.pop();
  assert.deepEqual(configFromDraft(draft).config.credentials.map((row) => row.api_key), [null]);
}

// Text exports preserve one field and shell snippets preserve one literal.
{
  const { psQuote } = await import(url('js/pages/keys/util.js'));
  for (const quote of ["'", '\u2018', '\u2019', '\u201a', '\u201b']) {
    assert.equal(psQuote(`left${quote}right`), `'left${quote}${quote}right'`);
  }
  assert.equal(psQuote('plain-name'), "'plain-name'");
  const { csvCell, toCsv } = await import(url('js/pages/usage/export.js'));
  assert.equal(csvCell('plain name'), '"plain name"');
  assert.equal(csvCell('one;two\tthree'), '"one;two\tthree"');
  assert.equal(csvCell('a "quoted" label'), '"a ""quoted"" label"');
  assert.equal(csvCell('=total'), '"\'=total"');
  assert.equal(csvCell(12), '12');
  assert.equal(csvCell(null), '');
  assert.equal(toCsv([['name', 'count'], ['one;two', 2]]), '"name","count"\r\n"one;two",2\r\n');
}

// 21. Components in a document: see dom.mjs.
await import('./dom.mjs');

console.log('all assertions passed');
