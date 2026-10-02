#!/usr/bin/env node
// Dashboard dev server. No dependencies; needs Node 18 or newer.
//
//   node tools/ui-dev.mjs                              mock admin API
//   node tools/ui-dev.mjs --api http://127.0.0.1:8317  proxy to a running gateway
//   node tools/ui-dev.mjs --port 5173 --host 0.0.0.0   listen elsewhere
//
// Serves ui/ at /admin/ with caching off, so a browser reload always shows
// the files on disk.
//
// With --api, /admin/api (HTTP and the WebSocket upgrade) is forwarded to a
// running switchyard, so the dashboard talks to the real admin API.
//
// Without --api, a built-in mock answers the routes the shell and the
// component kit need:
//   POST /admin/api/login       the secret is "dev"
//   GET  /admin/api/status
//   POST /admin/api/ws-ticket
//   GET  /admin/api/ws          emits hello, then stats, request.started,
//                               request.finished and log frames every second
//   POST /admin/api/playground  a canned JSON or SSE answer
// Other routes answer 404 with the admin API's error envelope.
//
// The mock also plays the sign-in failures, so each state of the sign-in
// page can be seen without a gateway:
//   secret "remote"    -> 403 (remote access disabled)
//   secret "disabled"  -> 404 (admin API disabled)
//   secret "locked"    -> 429 with Retry-After: 90
//   five wrong secrets in a row -> 429 for 60 seconds
//
// The mock's response shapes follow docs/DESIGN.md section 11 and the
// telemetry crate's record types. Where the design leaves a shape open
// (/status, the "stats" and "hello" frames) the mock's shape is a
// placeholder: check pages against a real gateway with --api.

import { createHash, randomBytes } from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import https from 'node:https';
import net from 'node:net';
import path from 'node:path';
import tls from 'node:tls';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const UI_DIR = path.resolve(HERE, '..', 'ui');
const MOUNT = '/admin/';
const API_PREFIX = '/admin/api';

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

function parseArgs(argv) {
  const options = { port: 5173, host: '127.0.0.1', api: null, quiet: false };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const value = () => {
      const next = arg.includes('=') ? arg.slice(arg.indexOf('=') + 1) : argv[(i += 1)];
      if (next === undefined) fail(`${arg} needs a value`);
      return next;
    };
    if (arg === '--help' || arg === '-h') {
      console.log('Usage: node tools/ui-dev.mjs [--port 5173] [--host 127.0.0.1] [--api http://127.0.0.1:8317] [--quiet]');
      process.exit(0);
    } else if (arg === '--port' || arg.startsWith('--port=')) {
      options.port = Number(value());
      if (!Number.isInteger(options.port) || options.port < 0 || options.port > 65535) fail('--port must be a number between 0 and 65535');
    } else if (arg === '--host' || arg.startsWith('--host=')) {
      options.host = value();
    } else if (arg === '--api' || arg.startsWith('--api=')) {
      try {
        options.api = new URL(value());
      } catch {
        fail('--api must be a URL such as http://127.0.0.1:8317');
      }
      if (options.api.protocol !== 'http:' && options.api.protocol !== 'https:') fail('--api must start with http:// or https://');
    } else if (arg === '--quiet') {
      options.quiet = true;
    } else {
      fail(`Unknown argument ${arg}. Try --help.`);
    }
  }
  return options;
}

function fail(message) {
  console.error(message);
  process.exit(2);
}

const options = parseArgs(process.argv.slice(2));
const log = (...parts) => {
  if (!options.quiet) console.log(...parts);
};

// ---------------------------------------------------------------------------
// Static files
// ---------------------------------------------------------------------------

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.ico': 'image/x-icon',
  '.woff2': 'font/woff2',
  '.txt': 'text/plain; charset=utf-8',
  '.md': 'text/markdown; charset=utf-8',
  '.map': 'application/json; charset=utf-8',
};

const NO_CACHE = { 'cache-control': 'no-store, max-age=0', pragma: 'no-cache' };

function sendText(res, status, text, headers = {}) {
  res.writeHead(status, { 'content-type': 'text/plain; charset=utf-8', ...NO_CACHE, ...headers });
  res.end(text);
}

function serveStatic(req, res, pathname) {
  let relative;
  try {
    relative = decodeURIComponent(pathname.slice(MOUNT.length));
  } catch {
    return sendText(res, 400, 'Bad path');
  }
  // fs throws synchronously on a path with a NUL byte ("%00").
  if (relative.includes('\0')) return sendText(res, 400, 'Bad path');
  if (relative === '' || relative.endsWith('/')) relative += 'index.html';
  const file = path.resolve(UI_DIR, relative);
  // Stay inside ui/ whatever the URL says.
  if (file !== UI_DIR && !file.startsWith(UI_DIR + path.sep)) return sendText(res, 403, 'Forbidden');

  fs.stat(file, (statError, stat) => {
    if (statError || !stat.isFile()) return sendText(res, 404, `Not found: ${pathname}`);
    const type = MIME[path.extname(file).toLowerCase()] ?? 'application/octet-stream';
    res.writeHead(200, { 'content-type': type, 'content-length': stat.size, ...NO_CACHE });
    if (req.method === 'HEAD') return res.end();
    fs.createReadStream(file)
      .on('error', () => res.destroy())
      .pipe(res);
    return undefined;
  });
  return undefined;
}

// ---------------------------------------------------------------------------
// Proxy to a running gateway
// ---------------------------------------------------------------------------

function upstreamPath(target, url) {
  return target.pathname.replace(/\/$/, '') + url;
}

function proxyHttp(req, res, target) {
  const client = target.protocol === 'https:' ? https : http;
  const upstream = client.request(
    {
      protocol: target.protocol,
      hostname: target.hostname,
      port: target.port || (target.protocol === 'https:' ? 443 : 80),
      method: req.method,
      path: upstreamPath(target, req.url),
      headers: { ...req.headers, host: target.host },
    },
    (answer) => {
      res.writeHead(answer.statusCode ?? 502, answer.headers);
      // Flush each chunk: the playground streams server-sent events.
      answer.on('data', (chunk) => res.write(chunk));
      answer.on('end', () => res.end());
      // The gateway went away after its headers (a crash or restart in the
      // middle of a stream): there will be no "end". Cut the browser's
      // connection too, so the dashboard sees a dropped response, as it
      // would without the proxy, instead of waiting for ever.
      answer.on('error', () => res.destroy());
      answer.on('close', () => {
        if (!answer.complete) res.destroy();
      });
    },
  );
  upstream.on('error', (error) => {
    if (res.headersSent) return res.destroy();
    return sendJson(res, 502, { error: { message: `The dev server could not reach ${target.origin}: ${error.message}` } });
  });
  res.on('close', () => upstream.destroy());
  req.pipe(upstream);
}

function proxyUpgrade(req, socket, head, target) {
  const secure = target.protocol === 'https:';
  const port = Number(target.port) || (secure ? 443 : 80);
  const upstream = secure ? tls.connect({ host: target.hostname, port, servername: target.hostname }) : net.connect({ host: target.hostname, port });
  const close = () => {
    socket.destroy();
    upstream.destroy();
  };
  upstream.once(secure ? 'secureConnect' : 'connect', () => {
    const lines = [`${req.method} ${upstreamPath(target, req.url)} HTTP/1.1`];
    for (let i = 0; i < req.rawHeaders.length; i += 2) {
      const name = req.rawHeaders[i];
      lines.push(`${name}: ${name.toLowerCase() === 'host' ? target.host : req.rawHeaders[i + 1]}`);
    }
    upstream.write(`${lines.join('\r\n')}\r\n\r\n`);
    if (head?.length) upstream.write(head);
    socket.pipe(upstream);
    upstream.pipe(socket);
  });
  upstream.on('error', close);
  socket.on('error', close);
  upstream.on('close', () => socket.destroy());
  socket.on('close', () => upstream.destroy());
}

// ---------------------------------------------------------------------------
// Mock admin API
// ---------------------------------------------------------------------------

const MOCK_SECRET = 'dev';
const startedAt = Date.now();

function sendJson(res, status, body, headers = {}) {
  const text = JSON.stringify(body);
  res.writeHead(status, { 'content-type': 'application/json', 'content-length': Buffer.byteLength(text), ...NO_CACHE, ...headers });
  res.end(text);
}

const apiError = (res, status, message, headers) => sendJson(res, status, { error: { message } }, headers);

function readBody(req) {
  return new Promise((resolve) => {
    const chunks = [];
    req.on('data', (chunk) => chunks.push(chunk));
    req.on('end', () => {
      const text = Buffer.concat(chunks).toString('utf8');
      try {
        resolve(text ? JSON.parse(text) : {});
      } catch {
        resolve(null);
      }
    });
    req.on('error', () => resolve(null));
  });
}

// Lockout, like the real thing but short enough to watch end.
const BAD_LIMIT = 5;
const LOCK_MS = 60_000;
const attempts = new Map(); // ip -> { bad, lockedUntil }

function bearer(req) {
  const header = req.headers.authorization ?? '';
  return header.toLowerCase().startsWith('bearer ') ? header.slice(7).trim() : '';
}

/** Returns true when the request may proceed; otherwise it has been answered. */
function authorize(req, res) {
  const ip = req.socket.remoteAddress ?? 'unknown';
  const state = attempts.get(ip) ?? { bad: 0, lockedUntil: 0 };
  attempts.set(ip, state);
  const now = Date.now();
  if (state.lockedUntil > now) {
    apiError(res, 429, 'Too many failed attempts. Try again later.', { 'retry-after': String(Math.ceil((state.lockedUntil - now) / 1000)) });
    return false;
  }
  const secret = bearer(req);
  if (secret === MOCK_SECRET) {
    state.bad = 0;
    return true;
  }
  if (secret === 'remote') {
    apiError(res, 403, 'Remote administration is disabled. Set admin.allow_remote = true to allow it.');
    return false;
  }
  if (secret === 'disabled') {
    apiError(res, 404, 'Not found.');
    return false;
  }
  if (secret === 'locked') {
    apiError(res, 429, 'Too many failed attempts. Try again later.', { 'retry-after': '90' });
    return false;
  }
  state.bad += 1;
  if (state.bad >= BAD_LIMIT) {
    state.bad = 0;
    state.lockedUntil = now + LOCK_MS;
    apiError(res, 429, 'Too many failed attempts. Try again later.', { 'retry-after': String(LOCK_MS / 1000) });
    return false;
  }
  apiError(res, 401, 'Invalid admin secret.');
  return false;
}

// ---- Fake traffic ----------------------------------------------------------

const pick = (list) => list[Math.floor(Math.random() * list.length)];
const between = (lo, hi) => Math.round(lo + Math.random() * (hi - lo));
const requestId = () => `req_${randomBytes(9).toString('base64url')}`;

const ROUTES = [
  { requested: 'gpt-4o', upstream: 'gpt-4o-2024-11-20', provider: 'openai-main', protocol: 'openai-chat', credential: ['cred_7f3a', 'key 1'] },
  { requested: 'gpt-4o', upstream: 'gpt-4o', provider: 'azure-east', protocol: 'openai-chat', credential: ['cred_19bc', 'eastus'] },
  { requested: 'claude-sonnet-4-5', upstream: 'claude-sonnet-4-5-20250929', provider: 'anthropic', protocol: 'anthropic', credential: ['cred_c2e0', 'team key'] },
  { requested: 'gemini-2.5-pro', upstream: 'gemini-2.5-pro', provider: 'google', protocol: 'gemini', credential: ['cred_55d1', 'key 1'] },
  { requested: 'fast', upstream: 'gpt-4o-mini', provider: 'openai-main', protocol: 'openai-responses', credential: ['cred_7f3a', 'key 1'] },
];
const CLIENTS = [
  { key_id: 'key_a81f', key_name: 'build-bot', protocol: 'openai-chat', endpoint: '/v1/chat/completions' },
  { key_id: 'key_3c07', key_name: 'support-app', protocol: 'anthropic', endpoint: '/v1/messages' },
  { key_id: 'key_d94e', key_name: 'notebooks', protocol: 'openai-responses', endpoint: '/v1/responses' },
];

const gauges = { in_flight: 0, active_streams: 0, requests: 0, errors: 0, tokens: 0, latencies: [] };
const minute = []; // { at, ok, tokens, duration }

function makeStart() {
  const client = pick(CLIENTS);
  const route = pick(ROUTES);
  const stream = Math.random() < 0.6;
  return {
    start: {
      id: requestId(),
      started_at: Date.now(),
      client: { key_id: client.key_id, key_name: client.key_name, ip: '10.0.4.' + between(2, 40), user_agent: 'mock-client/1.0' },
      client_protocol: client.protocol,
      endpoint: client.endpoint,
      transport: stream ? 'sse' : 'http',
      stream,
      requested_model: route.requested,
    },
    route,
  };
}

function finish(start, route) {
  const failed = Math.random() < 0.08;
  const rateLimited = failed && Math.random() < 0.5;
  const duration = failed ? between(120, 900) : between(380, 5200);
  const input = between(200, 9000);
  const output = failed ? 0 : between(40, 1800);
  const translated = route.protocol !== start.client_protocol;
  const attemptsList = [];
  if (failed) {
    attemptsList.push({
      provider: route.provider,
      credential_id: route.credential[0],
      credential_label: route.credential[1],
      upstream_model: route.upstream,
      upstream_protocol: route.protocol,
      status: rateLimited ? 429 : 502,
      ok: false,
      error: rateLimited ? 'Rate limit reached for requests' : 'upstream connect error: connection reset',
      duration_ms: duration,
    });
  } else {
    attemptsList.push({
      provider: route.provider,
      credential_id: route.credential[0],
      credential_label: route.credential[1],
      upstream_model: route.upstream,
      upstream_protocol: route.protocol,
      status: 200,
      ok: true,
      error: null,
      duration_ms: duration,
    });
  }
  return {
    ...start,
    finished_at: start.started_at + duration,
    duration_ms: duration,
    ttfb_ms: failed ? null : between(180, Math.min(duration, 1400)),
    client_model: start.requested_model,
    upstream_model: route.upstream,
    provider: route.provider,
    credential_id: route.credential[0],
    credential_label: route.credential[1],
    upstream_protocol: route.protocol,
    mode: translated ? 'translated' : 'passthrough',
    status: failed ? (rateLimited ? 429 : 502) : 200,
    ok: !failed,
    error: failed
      ? { kind: rateLimited ? 'rate_limit' : 'upstream', message: attemptsList[0].error, upstream_status: attemptsList[0].status }
      : null,
    usage: {
      input_tokens: input,
      cache_read_tokens: Math.random() < 0.3 ? between(100, 4000) : 0,
      cache_write_tokens: 0,
      output_tokens: output,
      reasoning_tokens: Math.random() < 0.25 ? Math.round(output * 0.4) : 0,
    },
    cost: failed ? null : Number(((input * 2.5 + output * 10) / 1e6).toFixed(6)),
    reasoning: Math.random() < 0.25 ? 'medium' : null,
    attempts: attemptsList,
    has_bodies: false,
  };
}

function percentile(sorted, p) {
  if (sorted.length === 0) return 0;
  return sorted[Math.min(sorted.length - 1, Math.floor(p * sorted.length))];
}

function statsFrame() {
  const cutoff = Date.now() - 60_000;
  while (minute.length && minute[0].at < cutoff) minute.shift();
  const durations = minute.map((m) => m.duration).sort((a, b) => a - b);
  return {
    at: Date.now(),
    in_flight: gauges.in_flight,
    active_streams: gauges.active_streams,
    requests_per_min: minute.length,
    errors_per_min: minute.filter((m) => !m.ok).length,
    tokens_per_min: minute.reduce((sum, m) => sum + m.tokens, 0),
    latency_ms: { p50: percentile(durations, 0.5), p90: percentile(durations, 0.9), p99: percentile(durations, 0.99) },
  };
}

function statusBody() {
  return {
    name: 'switchyard',
    version: '0.1.0-mock',
    mock: true,
    started_at: startedAt,
    uptime_ms: Date.now() - startedAt,
    in_flight: gauges.in_flight,
    active_streams: gauges.active_streams,
    ws_connections: sockets.size,
    totals: { requests: gauges.requests, errors: gauges.errors, tokens: gauges.tokens },
    config_path: './switchyard.toml',
    providers: 4,
    models: 4,
    client_keys: CLIENTS.length,
  };
}

// ---- WebSocket (RFC 6455, text frames only) -------------------------------

const tickets = new Map(); // ticket -> expiry
const sockets = new Set(); // { socket, topics: Set | null }

function encodeFrame(opcode, payload) {
  const length = payload.length;
  let header;
  if (length < 126) {
    header = Buffer.from([0x80 | opcode, length]);
  } else if (length < 65536) {
    header = Buffer.alloc(4);
    header[0] = 0x80 | opcode;
    header[1] = 126;
    header.writeUInt16BE(length, 2);
  } else {
    header = Buffer.alloc(10);
    header[0] = 0x80 | opcode;
    header[1] = 127;
    header.writeBigUInt64BE(BigInt(length), 2);
  }
  return Buffer.concat([header, payload]);
}

function sendFrame(peer, type, data) {
  if (peer.socket.destroyed) return;
  if (peer.topics && type !== 'hello' && !peer.topics.has(type)) return;
  peer.socket.write(encodeFrame(0x1, Buffer.from(JSON.stringify({ type, data }))));
}

function broadcast(type, data) {
  for (const peer of sockets) sendFrame(peer, type, data);
}

function mockUpgrade(req, socket, query) {
  const ticket = new URLSearchParams(query).get('ticket') ?? '';
  const expiry = tickets.get(ticket);
  tickets.delete(ticket); // single use
  const key = req.headers['sec-websocket-key'];
  if (!expiry || expiry < Date.now() || !key || (req.headers.upgrade ?? '').toLowerCase() !== 'websocket') {
    socket.end('HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n');
    return;
  }
  const accept = createHash('sha1').update(`${key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11`).digest('base64');
  socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
  socket.setNoDelay(true);

  const peer = { socket, topics: null };
  sockets.add(peer);
  log(`ws    open   (${sockets.size} connected)`);
  sendFrame(peer, 'hello', { version: '0.1.0-mock', server_time: Date.now(), mock: true });

  let buffer = Buffer.alloc(0);
  socket.on('data', (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);
    for (;;) {
      if (buffer.length < 2) return;
      const opcode = buffer[0] & 0x0f;
      const masked = (buffer[1] & 0x80) !== 0;
      let length = buffer[1] & 0x7f;
      let offset = 2;
      if (length === 126) {
        if (buffer.length < 4) return;
        length = buffer.readUInt16BE(2);
        offset = 4;
      } else if (length === 127) {
        if (buffer.length < 10) return;
        length = Number(buffer.readBigUInt64BE(2));
        offset = 10;
      }
      const maskLength = masked ? 4 : 0;
      if (buffer.length < offset + maskLength + length) return;
      const mask = masked ? buffer.subarray(offset, offset + 4) : null;
      const payload = Buffer.from(buffer.subarray(offset + maskLength, offset + maskLength + length));
      if (mask) for (let i = 0; i < payload.length; i += 1) payload[i] ^= mask[i % 4];
      buffer = buffer.subarray(offset + maskLength + length);

      if (opcode === 0x8) {
        socket.end(encodeFrame(0x8, payload.subarray(0, 2)));
        return;
      }
      if (opcode === 0x9) socket.write(encodeFrame(0xa, payload));
      if (opcode === 0x1) {
        try {
          const message = JSON.parse(payload.toString('utf8'));
          if (message?.type === 'subscribe' && Array.isArray(message.topics)) peer.topics = new Set(message.topics.map(String));
        } catch {
          /* not JSON: ignore, as the gateway does */
        }
      }
    }
  });
  const drop = () => {
    if (sockets.delete(peer)) log(`ws    close  (${sockets.size} connected)`);
  };
  socket.on('close', drop);
  socket.on('error', drop);
}

const LOG_LINES = [
  ['INFO', 'switchyard_gateway::pipeline', 'request finished'],
  ['DEBUG', 'switchyard_scheduler::select', 'picked credential by round-robin'],
  ['INFO', 'switchyard_upstream::discover', 'model discovery found 42 models'],
  ['WARN', 'switchyard_scheduler::cooldown', 'credential cooling down for 30s after 429'],
  ['ERROR', 'switchyard_upstream::transport', 'upstream connect error: connection reset'],
];

function tick() {
  if (sockets.size === 0) return;
  const { start, route } = makeStart();
  gauges.in_flight += 1;
  if (start.stream) gauges.active_streams += 1;
  broadcast('request.started', start);

  const record = finish(start, route);
  setTimeout(() => {
    gauges.in_flight = Math.max(0, gauges.in_flight - 1);
    if (start.stream) gauges.active_streams = Math.max(0, gauges.active_streams - 1);
    gauges.requests += 1;
    if (!record.ok) gauges.errors += 1;
    const tokens = record.usage.input_tokens + record.usage.cache_read_tokens + record.usage.output_tokens;
    gauges.tokens += tokens;
    minute.push({ at: Date.now(), ok: record.ok, tokens, duration: record.duration_ms });
    broadcast('request.finished', { ...record, finished_at: Date.now() });
    const line = record.ok ? LOG_LINES[Math.random() < 0.7 ? 0 : 1] : LOG_LINES[record.status === 429 ? 3 : 4];
    broadcast('log', { at: Date.now(), level: line[0], target: line[1], message: line[2], request_id: record.id });
  }, Math.min(record.duration_ms, 3000));

  broadcast('stats', statsFrame());
}

// ---- Playground -----------------------------------------------------------

function mockPlayground(res, body) {
  const text = 'This is the mock gateway. Start the dev server with --api to send real requests.';
  if (!body?.stream && !body?.body?.stream) {
    return sendJson(res, 200, {
      id: 'chatcmpl-mock',
      object: 'chat.completion',
      model: body?.model ?? body?.body?.model ?? 'mock',
      choices: [{ index: 0, message: { role: 'assistant', content: text }, finish_reason: 'stop' }],
      usage: { prompt_tokens: 12, completion_tokens: 17, total_tokens: 29 },
    });
  }
  res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache', 'x-accel-buffering': 'no' });
  const words = text.split(' ');
  let i = 0;
  const timer = setInterval(() => {
    if (i < words.length) {
      const chunk = { id: 'chatcmpl-mock', object: 'chat.completion.chunk', choices: [{ index: 0, delta: { content: (i ? ' ' : '') + words[i] } }] };
      res.write(`data: ${JSON.stringify(chunk)}\n\n`);
      i += 1;
    } else {
      res.write('data: [DONE]\n\n');
      clearInterval(timer);
      res.end();
    }
  }, 60);
  res.on('close', () => clearInterval(timer));
  return undefined;
}

async function mockApi(req, res, route) {
  if (!authorize(req, res)) return undefined;
  const { method } = req;

  if (method === 'POST' && route === '/login') return sendJson(res, 200, { ok: true });
  if (method === 'GET' && route === '/status') return sendJson(res, 200, statusBody());
  if (method === 'POST' && route === '/ws-ticket') {
    const ticket = randomBytes(18).toString('base64url');
    // Tickets that were bought and never used would otherwise pile up.
    for (const [old, expiry] of tickets) if (expiry < Date.now()) tickets.delete(old);
    tickets.set(ticket, Date.now() + 30_000);
    return sendJson(res, 200, { ticket, expires_in: 30 });
  }
  if (method === 'POST' && route === '/playground') {
    const body = await readBody(req);
    if (body === null) return apiError(res, 400, 'The request body is not valid JSON.');
    return mockPlayground(res, body);
  }
  return apiError(res, 404, `The mock admin API has no ${method} ${route}. Start the dev server with --api to use a real gateway.`);
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/**
 * Path and query of a request target, or null when the target is not in
 * origin form ("/path?query"). Taken apart by hand: `new URL(target, base)`
 * throws on "//" and reads "//admin/x" as a host name followed by "/x".
 */
function splitTarget(target) {
  if (typeof target !== 'string' || !target.startsWith('/')) return null;
  const hash = target.indexOf('#');
  const clean = hash === -1 ? target : target.slice(0, hash);
  const mark = clean.indexOf('?');
  return mark === -1 ? { pathname: clean, query: '' } : { pathname: clean.slice(0, mark), query: clean.slice(mark + 1) };
}

function handleRequest(req, res) {
  const target = splitTarget(req.url);
  if (!target) return sendText(res, 400, 'Bad request target');
  const { pathname } = target;

  if (pathname === API_PREFIX || pathname.startsWith(`${API_PREFIX}/`)) {
    if (options.api) return proxyHttp(req, res, options.api);
    return mockApi(req, res, pathname.slice(API_PREFIX.length) || '/').catch((error) => {
      console.error(error);
      if (!res.headersSent) apiError(res, 500, 'The mock admin API failed. See the dev server output.');
    });
  }
  if (pathname === '/' || pathname === '/admin') {
    res.writeHead(302, { location: MOUNT, ...NO_CACHE });
    return res.end();
  }
  if (pathname.startsWith(MOUNT)) {
    if (req.method !== 'GET' && req.method !== 'HEAD') return sendText(res, 405, 'Method not allowed', { allow: 'GET, HEAD' });
    return serveStatic(req, res, pathname);
  }
  return sendText(res, 404, `Not found: ${pathname}\nThe dashboard is at ${MOUNT}`);
}

// Whatever a request does, it must not take the server down: anything that
// can reach the port (a scanner, another device with --host 0.0.0.0) could
// otherwise stop it with one malformed line.
const server = http.createServer((req, res) => {
  res.on('finish', () => log(`${String(req.method).padEnd(5)} ${String(res.statusCode)} ${req.url}`));
  try {
    handleRequest(req, res);
  } catch (error) {
    console.error(`${req.method} ${req.url} failed:`, error);
    if (res.headersSent) res.destroy();
    else sendText(res, 500, 'The dev server failed on this request. See its output.');
  }
});

server.on('upgrade', (req, socket, head) => {
  socket.on('error', () => socket.destroy());
  try {
    const target = splitTarget(req.url);
    if (!target || target.pathname !== `${API_PREFIX}/ws`) {
      socket.end(`HTTP/1.1 ${target ? '404 Not Found' : '400 Bad Request'}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n`);
      return;
    }
    if (options.api) proxyUpgrade(req, socket, head, options.api);
    else mockUpgrade(req, socket, target.query);
  } catch (error) {
    console.error(`upgrade ${req.url} failed:`, error);
    socket.destroy();
  }
});

// A request the HTTP parser itself rejects.
server.on('clientError', (error, socket) => {
  if (socket.writable) socket.end('HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n');
  else socket.destroy();
});

server.on('error', (error) => {
  if (error.code === 'EADDRINUSE') fail(`Port ${options.port} is already in use. Pass --port to choose another.`);
  fail(`Could not start the dev server: ${error.message}`);
});

if (!fs.existsSync(path.join(UI_DIR, 'index.html'))) fail(`No index.html in ${UI_DIR}`);

server.listen(options.port, options.host, () => {
  const { port } = server.address();
  const shown = options.host === '0.0.0.0' || options.host === '::' ? 'localhost' : options.host;
  console.log(`Switchyard dashboard: http://${shown}:${port}${MOUNT}`);
  console.log(options.api ? `Admin API: proxied to ${options.api.origin}` : `Admin API: built-in mock (sign in with the secret "${MOCK_SECRET}")`);
  console.log(`Component kit: http://${shown}:${port}${MOUNT}#/_kit`);
});

if (!options.api) setInterval(tick, 1000).unref?.();

for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => {
    for (const peer of sockets) peer.socket.destroy();
    server.close(() => process.exit(0));
    setTimeout(() => process.exit(0), 300).unref();
  });
}
