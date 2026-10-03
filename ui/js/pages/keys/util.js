// API keys page: pure helpers. Nothing here touches the DOM at import time,
// so the module loads under Node for ui/tests/check.mjs.
//
//   wildcardMatch(pattern, text)      the gateway's own model-pattern matching
//   servedNames(models)               the names a request can be routed by
//   matchModels(patterns, names)      which current models an allow-list admits
//   gatewayAddresses(listen, tls)     base URLs a client can be pointed at
//   buildExamples({ ... })            ready-to-paste snippets for a key
//   referenceName(key)                "TEAM_KEY" for "env:TEAM_KEY" / "${TEAM_KEY}"

import { API_BASE } from '../../lib/api.js';

/**
 * The gateway's wildcard matching (crates/core/src/util.rs, wildcard_match):
 * `*` matches any run of characters, including none; everything else matches
 * itself; case is ignored. There is no `?` and no character class.
 */
export function wildcardMatch(pattern, text) {
  const p = [...String(pattern).toLowerCase()];
  const t = [...String(text).toLowerCase()];
  let pi = 0;
  let ti = 0;
  let star = -1;
  let mark = 0;
  while (ti < t.length) {
    if (pi < p.length && p[pi] === '*') {
      star = pi;
      mark = ti;
      pi += 1;
    } else if (pi < p.length && p[pi] === t[ti]) {
      pi += 1;
      ti += 1;
    } else if (star !== -1) {
      pi = star + 1;
      mark += 1;
      ti = mark;
    } else {
      return false;
    }
  }
  while (pi < p.length && p[pi] === '*') pi += 1;
  return pi === p.length;
}

/** True when a key with this allow-list may use the model. An empty list allows everything. */
export function allowsModel(patterns, name) {
  return !patterns || patterns.length === 0 || patterns.some((pattern) => wildcardMatch(pattern, name));
}

/**
 * The client-facing names the gateway serves: the entries of GET /models
 * without the ignored ones (an alias with no routable target, under which
 * no request can be served). The same set `counts.models` of /status counts.
 */
export function servedNames(models) {
  return (models ?? []).filter((model) => !model.ignored).map((model) => model.name);
}

/**
 * What an allow-list admits out of the models the gateway serves right now.
 *
 * @param {string[]} patterns  the key's patterns; [] means every model
 * @param {string[]} names     client-facing model names (servedNames)
 * @returns {{ matched: string[], unmatched: string[], total: number, all: boolean }}
 *          `matched`: names the key may use; `unmatched`: patterns that match
 *          no current model (they may match one added later).
 */
export function matchModels(patterns, names) {
  const list = (patterns ?? []).filter((pattern) => pattern.trim() !== '');
  if (list.length === 0) return { matched: names, unmatched: [], total: names.length, all: true };
  const used = new Set();
  const matched = names.filter((name) => {
    let hit = false;
    for (const pattern of list) {
      if (wildcardMatch(pattern, name)) {
        used.add(pattern);
        hit = true;
      }
    }
    return hit;
  });
  return { matched, unmatched: list.filter((pattern) => !used.has(pattern)), total: names.length, all: false };
}

/** Same patterns in the same order. */
export function sameList(a, b) {
  return a.length === b.length && a.every((value, index) => value === b[index]);
}

/** The variable a key reference names, or null for a literal key. */
export function referenceName(key) {
  const text = String(key ?? '').trim();
  const env = /^env:(.+)$/.exec(text);
  if (env) return env[1];
  const braces = /^\$\{(.+)\}$/.exec(text);
  return braces ? braces[1] : null;
}

/** True for a reference that names no variable: "env:" or "${}". */
export function emptyReference(key) {
  return /^(env:\s*|\$\{\s*\})$/.test(String(key ?? '').trim());
}

// ---------------------------------------------------------------------------
// Addresses
// ---------------------------------------------------------------------------

const LOOPBACK = /^(localhost|127(?:\.\d{1,3}){3}|\[?::1\]?)$/i;
const WILDCARD = /^(0\.0\.0\.0|\[?::\]?)$/;

/** "127.0.0.1:8317" / "[::]:9000" -> { host, port }, or null. */
export function parseListen(listen) {
  const match = /^(\[[^\]]+\]|[^:]+):(\d+)$/.exec(String(listen ?? '').trim());
  return match ? { host: match[1], port: match[2] } : null;
}

/**
 * Base URLs of the client API that a snippet can use.
 *
 * `page` is where this dashboard was loaded from (with any path prefix a
 * reverse proxy adds): the browser reached the gateway there, so it works
 * from this device. `listen` is the address the gateway is bound to
 * (GET /status), offered when it is a concrete address that differs from the
 * page's: behind a tunnel or a proxy that is the one other machines on the
 * gateway's network would use.
 *
 * @param {string | null | undefined} listen  status.listen
 * @param {boolean} tls  status.tls: the gateway's own listener serves HTTPS
 * @param {{ origin: string, protocol: string, hostname: string, port: string }} [loc]
 * @returns {{ page: string, listen: string | null, wildcard: string | null }}
 *          `wildcard`: the bind address when the gateway listens on every
 *          interface while the page was opened on loopback.
 */
export function gatewayAddresses(listen, tls = false, loc = typeof location === 'undefined' ? null : location) {
  const prefix = API_BASE.replace(/\/admin\/api$/, '');
  const page = loc ? `${loc.origin}${prefix}` : `http://127.0.0.1:8317${prefix}`;
  const bound = parseListen(listen);
  if (!bound || !loc) return { page, listen: null, wildcard: null };

  const pagePort = loc.port || (loc.protocol === 'https:' ? '443' : '80');
  const pageLoopback = LOOPBACK.test(loc.hostname);
  if (WILDCARD.test(bound.host)) {
    return { page, listen: null, wildcard: pageLoopback ? `${bound.host}:${bound.port}` : null };
  }
  const sameHost = bound.host.toLowerCase() === loc.hostname.toLowerCase() || (LOOPBACK.test(bound.host) && pageLoopback);
  if (sameHost && bound.port === pagePort) return { page, listen: null, wildcard: null };
  return { page, listen: `${tls ? 'https' : 'http'}://${bound.host}:${bound.port}`, wildcard: null };
}

// ---------------------------------------------------------------------------
// Snippets
// ---------------------------------------------------------------------------

const SH_PLAIN = /^[A-Za-z0-9_\-.:/@%+=,]+$/;

/** A value as one POSIX shell word. */
export function shQuote(value) {
  const text = String(value);
  if (text !== '' && SH_PLAIN.test(text)) return text;
  return `'${text.replace(/'/g, `'\\''`)}'`;
}

/** A value as a PowerShell single-quoted string. */
export function psQuote(value) {
  return `'${String(value).replace(/'/g, "''")}'`;
}

function envBlock(shell, pairs) {
  return pairs
    .map(([name, value]) => (shell === 'powershell' ? `$env:${name} = ${psQuote(value)}` : `export ${name}=${shQuote(value)}`))
    .join('\n');
}

/**
 * A model to put in the test request: the first current model the key may
 * use that has a credential able to serve it now.
 */
export function exampleModel(patterns, models) {
  const allowed = (models ?? []).filter((model) => !model.ignored && allowsModel(patterns, model.name));
  const serving = (model) => (model.routes ?? []).some((route) => route.credentials_available > 0);
  // The mock provider's "mock-error-*" models fail on purpose.
  const usable = allowed.filter((model) => serving(model) && !/(^|\/)mock-(error|slow)/.test(model.name));
  // A model the gateway has metadata for is the likelier one to be in use.
  const known = usable.find((model) => model.info?.known !== false);
  return (known ?? usable[0] ?? allowed.find(serving) ?? allowed[0])?.name ?? null;
}

export const SHELLS = [
  { value: 'sh', label: 'bash / zsh' },
  { value: 'powershell', label: 'PowerShell' },
];

export function defaultShell() {
  const platform = typeof navigator === 'undefined' ? '' : navigator.platform || navigator.userAgent || '';
  return /Win/i.test(platform) ? 'powershell' : 'sh';
}

/**
 * Ready-to-paste snippets for one key.
 *
 * @param {{ base: string, key: string, model: string | null, shell: 'sh' | 'powershell' }} options
 *        `key` is the text to put where the key goes: the real key, or a
 *        placeholder when it is not known here.
 * @returns {Array<{ id: string, label: string, code: string, note: string }>}
 */
export function buildExamples({ base, key, model, shell }) {
  const root = String(base).replace(/\/+$/, '');
  const body = JSON.stringify({ model: model ?? 'MODEL', messages: [{ role: 'user', content: 'Hello' }] });
  const request =
    shell === 'powershell'
      ? [
          `Invoke-RestMethod -Method Post -Uri ${psQuote(`${root}/v1/chat/completions`)} \``,
          `  -Headers @{ Authorization = ${psQuote(`Bearer ${key}`)} } \``,
          `  -ContentType 'application/json' \``,
          `  -Body ${psQuote(body)}`,
        ].join('\n')
      : [
          `curl ${shQuote(`${root}/v1/chat/completions`)} \\`,
          `  -H ${shQuote(`Authorization: Bearer ${key}`)} \\`,
          `  -H 'Content-Type: application/json' \\`,
          `  -d ${shQuote(body)}`,
        ].join('\n');
  return [
    {
      id: 'request',
      label: shell === 'powershell' ? 'Test request' : 'curl',
      code: request,
      note: model
        ? 'Sends one chat completion. The key also works as x-api-key and x-goog-api-key.'
        : 'Replace MODEL with a model this key may use. The key also works as x-api-key and x-goog-api-key.',
    },
    {
      id: 'openai',
      label: 'OpenAI',
      code: envBlock(shell, [
        ['OPENAI_BASE_URL', `${root}/v1`],
        ['OPENAI_API_KEY', key],
      ]),
      note: 'For the OpenAI SDKs and tools that read these variables. Chat Completions and Responses are both served.',
    },
    {
      id: 'anthropic',
      label: 'Anthropic',
      code: envBlock(shell, [
        ['ANTHROPIC_BASE_URL', root],
        ['ANTHROPIC_API_KEY', key],
      ]),
      note: 'For Claude Code and the Anthropic SDKs.',
    },
    {
      id: 'gemini',
      label: 'Gemini',
      code: envBlock(shell, [
        ['GOOGLE_GEMINI_BASE_URL', root],
        ['GEMINI_API_KEY', key],
      ]),
      note: 'For Gemini CLI and the Google Gen AI SDKs.',
    },
  ];
}
