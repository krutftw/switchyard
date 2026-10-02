// Tests for tools/ui-dev.mjs, run by check.mjs (or alone: node ui/tests/dev-server.mjs).
//
// They start the real server as a child process on a free port and talk to
// it over sockets, because the failures worth guarding against are the ones
// a well-behaved HTTP client cannot produce: a request target the URL parser
// rejects, a NUL byte in the path, an upstream that dies mid-response.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import http from 'node:http';
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const SERVER = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..', 'tools', 'ui-dev.mjs');
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** Start the dev server on a free port. Resolves { port, child, exited() }. */
function start(args = []) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [SERVER, '--port', '0', '--quiet', ...args], { stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '';
    let err = '';
    let exit = null;
    child.on('exit', (code) => {
      exit = code;
      reject(new Error(`dev server exited with ${code} before listening:\n${err}`));
    });
    child.stderr.on('data', (chunk) => (err += chunk));
    child.stdout.on('data', (chunk) => {
      out += chunk;
      const match = /http:\/\/[^:\s]+:(\d+)\/admin\//.exec(out);
      if (match) resolve({ port: Number(match[1]), child, exited: () => exit, stderr: () => err });
    });
  });
}

/** Send raw bytes; resolves with everything the server sent back before closing. */
function raw(port, text, { waitMs = 8000 } = {}) {
  return new Promise((resolve) => {
    const socket = net.connect({ host: '127.0.0.1', port });
    let data = '';
    const done = (how) => {
      socket.destroy();
      resolve({ data, how });
    };
    const timer = setTimeout(() => done('timeout'), waitMs);
    socket.on('connect', () => socket.write(text));
    socket.on('data', (chunk) => (data += chunk));
    socket.on('end', () => {
      clearTimeout(timer);
      done('end');
    });
    socket.on('error', (error) => {
      clearTimeout(timer);
      done(error.code);
    });
  });
}

const statusOf = (reply) => Number(/^HTTP\/1\.1 (\d{3})/.exec(reply.data)?.[1] ?? 0);
const get = (port, target, headers = '') => raw(port, `GET ${target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n${headers}\r\n`);

// ---- Mock mode -----------------------------------------------------------

const mock = await start();
try {
  // The app and its modules are served, uncached, from /admin/.
  const index = await get(mock.port, '/admin/');
  assert.equal(statusOf(index), 200);
  assert.match(index.data, /cache-control: no-store/i);
  assert.match(index.data, /<div id="app"/);
  assert.equal(statusOf(await get(mock.port, '/admin/js/app.js')), 200);
  assert.equal(statusOf(await get(mock.port, '/admin')), 302);

  // Malformed targets get an answer; none of them may stop the server.
  assert.equal(statusOf(await get(mock.port, '/admin/%00')), 400, 'NUL byte in the path');
  assert.equal(statusOf(await get(mock.port, '/admin/js/%00app.js')), 400);
  assert.equal(statusOf(await get(mock.port, '/admin/%zz')), 400, 'broken percent-encoding');
  assert.equal(statusOf(await get(mock.port, '//')), 404, 'a target the URL parser rejects');
  assert.equal(statusOf(await get(mock.port, '//admin/api/status')), 404, '"//host/path" is a path, not a host');
  assert.equal(statusOf(await get(mock.port, 'http://[bad/')), 400, 'absolute-form target');
  assert.equal(statusOf(await get(mock.port, '*')), 400);
  assert.notEqual(statusOf(await get(mock.port, '/admin/../../Cargo.toml')), 200, 'no escape from ui/');
  assert.notEqual(statusOf(await get(mock.port, '/admin/..%2f..%2fCargo.toml')), 200);
  assert.equal(statusOf(await get(mock.port, '/admin/..%5c..%5cCargo.toml')) === 200, false);
  assert.ok([400, 404].includes(statusOf(await get(mock.port, '//', 'Upgrade: websocket\r\nConnection: Upgrade\r\n'))), 'bad target on the upgrade path');
  await raw(mock.port, 'NOT HTTP AT ALL\r\n\r\n', { waitMs: 500 });

  // Still alive, and the mock API answers in the admin error envelope.
  assert.equal(mock.exited(), null, `the dev server died:\n${mock.stderr()}`);
  const unauth = await get(mock.port, '/admin/api/status');
  assert.equal(statusOf(unauth), 401);
  assert.match(unauth.data, /"error":\{"message":/);
  const status = await get(mock.port, '/admin/api/status', 'Authorization: Bearer dev\r\n');
  assert.equal(statusOf(status), 200);
  assert.match(status.data, /"version":/);
  const login = await raw(mock.port, 'POST /admin/api/login HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer dev\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}');
  assert.equal(statusOf(login), 200);
  assert.equal(statusOf(await get(mock.port, '/admin/api/status', 'Authorization: Bearer remote\r\n')), 403);
  assert.equal(statusOf(await get(mock.port, '/admin/api/status', 'Authorization: Bearer disabled\r\n')), 404);
  const locked = await get(mock.port, '/admin/api/status', 'Authorization: Bearer locked\r\n');
  assert.equal(statusOf(locked), 429);
  assert.match(locked.data, /retry-after: 90/i);
  assert.equal(mock.exited(), null);
} finally {
  mock.child.kill();
}

// ---- Proxy mode: the gateway dies in the middle of a response -----------

const upstream = http.createServer((req, res) => {
  if (req.url === '/admin/api/playground') {
    res.writeHead(200, { 'content-type': 'text/event-stream' });
    res.write('data: {"n":1}\n\n');
    setTimeout(() => res.socket.destroy(), 100);
    return;
  }
  res.writeHead(200, { 'content-type': 'application/json' });
  res.end('{"ok":true}');
});
await new Promise((resolve) => upstream.listen(0, '127.0.0.1', resolve));
const proxy = await start(['--api', `http://127.0.0.1:${upstream.address().port}`]);
try {
  const ok = await get(proxy.port, '/admin/api/status');
  assert.equal(statusOf(ok), 200);
  assert.match(ok.data, /\{"ok":true\}/);

  // The browser's request must end when the upstream response is cut off,
  // not hang: streamSSE has no timeout of its own.
  const cut = await raw(proxy.port, 'POST /admin/api/playground HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}', { waitMs: 4000 });
  assert.notEqual(cut.how, 'timeout', 'the proxied response was left hanging after the upstream dropped');
  assert.match(cut.data, /data: \{"n":1\}/, 'what arrived before the drop was passed on');
  assert.doesNotMatch(cut.data, /\r\n0\r\n\r\n$/, 'a cut-off response must not look complete');

  // The proxy survives it, and reports an unreachable upstream as 502.
  assert.equal(statusOf(await get(proxy.port, '/admin/api/status')), 200);
  await new Promise((resolve) => {
    upstream.close(resolve);
    upstream.closeAllConnections();
  });
  const down = await get(proxy.port, '/admin/api/status');
  assert.equal(statusOf(down), 502);
  assert.match(down.data, /"error":\{"message":"The dev server could not reach/);
  assert.equal(proxy.exited(), null, `the dev server died:\n${proxy.stderr()}`);
} finally {
  proxy.child.kill();
  upstream.close();
}

await sleep(0);
console.log('dev server checks passed');
