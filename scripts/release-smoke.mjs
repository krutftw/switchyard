// Native release smoke: no dependencies, external services or permanent state.
// Usage: node release-smoke.mjs /absolute/path/to/switchyard 0.1.0
import assert from 'node:assert/strict';
import { execFile, spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { basename, dirname, join, resolve } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { promisify } from 'node:util';

const [binaryArg, version] = process.argv.slice(2);
assert(binaryArg && /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version ?? ''),
  'Usage: node release-smoke.mjs BINARY VERSION');
const binary = resolve(binaryArg);
// CLI overrides from a developer shell must never change the fixture's auth,
// listener or log policy. The mock is the only configured provider.
const env = Object.fromEntries(Object.entries(process.env)
  .filter(([key]) => !key.toUpperCase().startsWith('SWITCHYARD_')));
const tempRoot = resolve(tmpdir());
const fixture = await mkdtemp(join(tempRoot, 'switchyard-release-smoke-'));
const config = join(fixture, 'switchyard.toml');
const adminSecret = randomBytes(32).toString('hex');
const clientKey = randomBytes(32).toString('hex');
let child;
let finished;
let result;
let childError = false;
let interrupted = false;
const onInterrupt = () => { interrupted = true; };
process.on('SIGINT', onInterrupt);
process.on('SIGTERM', onInterrupt);

async function command(args) {
  try {
    const { stdout } = await promisify(execFile)(binary, args, {
      cwd: fixture, env, windowsHide: true, timeout: 15_000, maxBuffer: 65_536,
    });
    assert(!interrupted, 'Smoke interrupted');
    return stdout.trim();
  } catch {
    // Do not print subprocess output: the configuration contains test secrets.
    throw new Error(`The ${args[0]} command failed or timed out`);
  }
}

async function until(predicate, timeout, message) {
  const deadline = Date.now() + timeout;
  while (!predicate()) {
    assert(Date.now() < deadline, message);
    await delay(50);
  }
}

async function stop() {
  if (!child || result) return;
  child.kill('SIGTERM');
  // SIGTERM is graceful on Unix; Node terminates the process on Windows.
  await until(() => result, 10_000, 'Gateway did not stop after SIGTERM').catch(async () => {
    child.kill('SIGKILL');
    await until(() => result, 5_000, 'Gateway did not stop after SIGKILL');
    throw new Error('Gateway required forced shutdown');
  });
  await finished;
}

try {
  assert.equal(await command(['version']), `switchyard ${version}`, 'Packaged version differs from tag');
  await writeFile(config, `[server]
host = "127.0.0.1"
port = 8317
data_dir = "data"
[admin]
enabled = true
secret = "${adminSecret}"
allow_remote = false
ui = true
[auth]
required = true
[[auth.keys]]
key = "${clientKey}"
name = "release-smoke"
[upstream]
proxy = "direct"
[logging]
level = "info"
file = false
request_log = "off"
[usage]
enabled = true
persist = true
[[providers]]
name = "mock"
kind = "mock"
discover = false
`, { mode: 0o600 });
  await command(['check', '--config', config]);
  // Port zero is supported by the CLI override, not the persisted config.
  child = spawn(binary, ['serve', '--config', config, '--port', '0'], {
    cwd: fixture, env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'],
  });
  finished = new Promise((done) => {
    child.once('error', () => { childError = true; });
    child.once('close', (code, signal) => { result = { code, signal }; done(); });
  });
  let output = '';
  let base;
  child.stdout.setEncoding('utf8');
  child.stdout.on('data', (chunk) => {
    output = (output + chunk).slice(-65_536);
    // This is the CLI's dedicated script-readiness line, not a log heuristic.
    const match = output.match(/(?:^|\n)listening on (http:\/\/127\.0\.0\.1:\d+)\r?\n/);
    if (match) base = match[1];
  });
  child.stderr.resume();
  await until(() => {
    assert(!interrupted, 'Smoke interrupted');
    assert(!childError, 'Gateway process could not start');
    assert(!result, 'Gateway exited before becoming ready');
    return base;
  }, 30_000, 'Gateway startup timed out');

  async function request(endpoint, options = {}) {
    assert(!interrupted && !result, 'Gateway stopped during smoke');
    const response = await fetch(new URL(endpoint, base), {
      ...options, redirect: 'error', signal: AbortSignal.timeout(10_000),
    });
    assert.equal(response.status, 200, `${endpoint} did not return HTTP 200`);
    return response;
  }
  assert.equal((await (await request('/healthz')).json()).status, 'ok');
  const ui = await request('/admin/');
  assert(ui.headers.get('content-type')?.includes('text/html'), 'Dashboard is not HTML');
  assert((await ui.text()).includes('js/app.js'), 'Embedded dashboard entry point is missing');
  assert((await (await request('/admin/js/app.js')).text()).includes('import '), 'Embedded dashboard module is missing');
  const marker = 'switchyard release smoke';
  const answer = await request('/v1/chat/completions', {
    method: 'POST',
    headers: { Authorization: `Bearer ${clientKey}`, 'Content-Type': 'application/json' },
    body: JSON.stringify({ model: 'mock-echo', messages: [{ role: 'user', content: marker }] }),
  });
  assert.equal(answer.headers.get('x-switchyard-provider'), 'mock', 'Request did not use the local mock');
  assert((await answer.json()).choices?.[0]?.message?.content?.includes(marker), 'Mock response did not echo the request');
  assert(!interrupted && !result, 'Gateway stopped during smoke');
  await stop();
  if (process.platform !== 'win32') assert.equal(result.code, 0, 'Gateway did not shut down cleanly');
  console.log(`PASS release smoke: ${version} (${process.platform}/${process.arch}); version, config, health, embedded UI, authenticated mock request, shutdown`);
} finally {
  try {
    await stop();
  } finally {
    process.off('SIGINT', onInterrupt);
    process.off('SIGTERM', onInterrupt);
    // Only remove the unique directory created by this invocation, and only
    // after the owned child has exited. Never delete a supplied binary path.
    assert.equal(dirname(resolve(fixture)), tempRoot, 'Refusing cleanup outside the temporary root');
    assert(basename(fixture).startsWith('switchyard-release-smoke-'), 'Unexpected cleanup directory');
    if (!child || result) await rm(fixture, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
  }
}
