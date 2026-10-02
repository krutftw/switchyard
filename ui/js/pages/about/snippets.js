// About page: everything that is text. Addresses, the client snippets, the
// diagnostics block and the reading of vendor/LICENSES.txt.
//
// All of it is pure (no DOM, no fetch), so it can be imported and checked
// under Node. Every snippet was run against a gateway before it was written
// down here; keep it that way when changing one.

import { formatDuration, formatNumber } from '../../lib/format.js';

/** Stands where the client key goes. Never a real key. */
export const KEY_PLACEHOLDER = 'YOUR_CLIENT_KEY';
/** Stands where the model goes while the gateway serves none. */
export const MODEL_PLACEHOLDER = 'MODEL_NAME';

// ---------------------------------------------------------------------------
// Addresses
// ---------------------------------------------------------------------------

/**
 * The gateway's root as this browser reaches it. The dashboard is served at
 * <root>/admin/, so the root is one directory up from the page; a reverse
 * proxy's path prefix and its scheme survive.
 *
 *   http://127.0.0.1:8317/admin/#/about        -> http://127.0.0.1:8317
 *   https://ai.example.com/gw/admin/index.html -> https://ai.example.com/gw
 */
export function browserBase(pageUrl) {
  try {
    const root = new URL('../', pageUrl);
    return root.origin + root.pathname.replace(/\/+$/, '');
  } catch {
    return '';
  }
}

/** "127.0.0.1:8317", "[::1]:8317" -> { host, port, wildcard }, or null. */
export function parseListen(listen) {
  const match = /^(\[[^\]]+\]|[^:\s]+):(\d+)$/.exec(String(listen ?? '').trim());
  if (!match) return null;
  const host = match[1];
  return { host, port: match[2], wildcard: host === '0.0.0.0' || host === '[::]' };
}

const LOOPBACK = new Set(['localhost', '127.0.0.1', '[::1]']);
const sameHost = (a, b) => a === b || (LOOPBACK.has(a) && LOOPBACK.has(b));

/**
 * The base URLs worth offering in the snippets.
 *
 * "browser" is always there: it is the one address known to work from where
 * the operator sits. "listen" is added when the gateway is bound to a
 * concrete address that differs from it (the dashboard is open through a
 * proxy or a tunnel): that is what a client on the gateway's own machine
 * would use. A wildcard bind (0.0.0.0) names no address a client could use.
 *
 * @returns {{ id: 'browser' | 'listen', label: string, base: string }[]}
 */
export function addressChoices(listen, pageUrl) {
  const browser = browserBase(pageUrl);
  const choices = [{ id: 'browser', label: 'This browser’s address', base: browser }];
  const bound = parseListen(listen);
  if (!bound || bound.wildcard) return choices;
  let page = null;
  try {
    page = new URL(browser);
  } catch {
    page = null;
  }
  if (page) {
    const pagePort = page.port || (page.protocol === 'https:' ? '443' : '80');
    const direct = page.pathname.replace(/\/+$/, '') === '' && pagePort === bound.port && sameHost(page.hostname.toLowerCase(), bound.host.toLowerCase());
    if (direct) return choices;
  }
  // The scheme of the listener is not part of /status. Through a proxy the
  // gateway itself almost always speaks plain HTTP.
  choices.push({ id: 'listen', label: 'Listen address', base: `http://${bound.host}:${bound.port}` });
  return choices;
}

/** http://host -> ws://host, https://host -> wss://host. */
export function wsBase(base) {
  return base.replace(/^http/i, 'ws');
}

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

/**
 * The entries of GET /models a client can call: a name and at least one
 * route. An alias whose targets match nothing is listed with `routes: []`
 * and answers 404, so it is not one of them.
 *
 * @param {{ name: string, hidden?: boolean, routes?: { credentials_available: number }[] }[] | undefined} models  GET /models
 */
export function callableModels(models) {
  return (Array.isArray(models) ? models : []).filter((m) => m && typeof m.name === 'string' && m.name !== '' && Array.isArray(m.routes) && m.routes.length > 0);
}

/** Whether a credential could serve the model right now. */
export const modelReady = (model) => (model?.routes ?? []).some((route) => route.credentials_available > 0);

/**
 * The model the examples use until the operator picks one: the mock's echo
 * model when it is there (it answers at once and costs nothing), else the
 * first model that could be served right now, else the first one listed.
 *
 * @returns {string | null}
 */
export function defaultModel(models) {
  const listed = callableModels(models).filter((m) => !m.hidden);
  const ready = listed.filter(modelReady);
  if (ready.some((m) => m.name === 'mock-echo')) return 'mock-echo';
  return ready[0]?.name ?? listed[0]?.name ?? null;
}

/**
 * "name(suffix)" split where the gateway splits it (parse_model_suffix in
 * crates/core/src/reasoning.rs): the last "(" and a ")" that ends the name.
 */
export function splitSuffix(name) {
  const text = String(name ?? '');
  const open = text.lastIndexOf('(');
  if (!text.endsWith(')') || open <= 0) return { base: text, suffix: null };
  return { base: text.slice(0, open), suffix: text.slice(open + 1, -1) };
}

const SUFFIX_WORDS = new Set(['none', 'off', 'disabled', 'auto', 'dynamic', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max']);

/** A reasoning suffix the gateway acts on: a level, none, auto or a budget. */
function knownSuffix(suffix) {
  const text = String(suffix).trim().toLowerCase();
  return SUFFIX_WORDS.has(text) || /^(0|-1|[1-9]\d{0,8})$/.test(text);
}

/**
 * Whether the gateway serves `name`: it is a callable entry of GET /models
 * (hidden ones count, they still route), or such an entry followed by a
 * reasoning suffix the gateway recognises.
 *
 * A model named in a link is checked with this before it is written into a
 * command somebody will paste into a terminal.
 */
export function servesModel(models, name) {
  if (typeof name !== 'string' || name === '') return false;
  const names = new Set(callableModels(models).map((m) => m.name));
  if (names.has(name)) return true;
  const { base, suffix } = splitSuffix(name);
  return suffix != null && names.has(base) && knownSuffix(suffix);
}

// ---------------------------------------------------------------------------
// Shell quoting
// ---------------------------------------------------------------------------

export const SHELLS = [
  { value: 'posix', label: 'bash / zsh' },
  { value: 'powershell', label: 'PowerShell' },
];

// PowerShell (5.1 and 7) reads the typographic quotes as quote characters
// too: “ ” „ end a "…" string and ‘ ’ ‚ ‛ end a '…' string, exactly as the
// ASCII ones do. A value that carries one must not be able to close the
// string it is written into, so they are escaped along with the ASCII ones.
const PS_DOUBLE = /[`"$“”„]/g;
const PS_SINGLE = /['‘’‚‛]/g;

/** A single-quoted string (no expansion at all). */
export function sq(shell, text) {
  // PowerShell: a quote is doubled. bash: close, an escaped quote, reopen.
  const body = shell === 'powershell' ? String(text).replace(PS_SINGLE, '$&$&') : String(text).replace(/'/g, "'\\''");
  return `'${body}'`;
}

/** A double-quoted string that the shell passes on exactly as `text`. */
export function dq(shell, text) {
  const value = String(text);
  if (shell === 'powershell') return `"${value.replace(PS_DOUBLE, '`$&')}"`;
  // An interactive bash or zsh expands "!" inside double quotes (history),
  // and no escape removes it cleanly; single quotes do not expand anything.
  if (value.includes('!')) return sq(shell, value);
  return `"${value.replace(/[\\"$`]/g, '\\$&')}"`;
}

/**
 * A model name as part of a URL path. The gateway decodes percent-escapes,
 * so anything that would end the path (?, #, a space) is escaped; a slash
 * stays, as prefixed names ("lab/mock-echo") are routed with it.
 */
export function modelPath(name) {
  return encodeURIComponent(String(name)).replace(/%2F/gi, '/');
}

function envLine(shell, name, value) {
  return shell === 'powershell' ? `$env:${name} = ${dq(shell, value)}` : `export ${name}=${dq(shell, value)}`;
}

/**
 * One JSON request from the command line.
 *
 * bash: curl, with the body as a single-quoted argument.
 *
 * PowerShell: Invoke-RestMethod, not curl.exe. Windows PowerShell 5 strips
 * the quotes inside an argument to a native program and PowerShell 7 does
 * not, and 5 may put a byte-order mark in front of what it pipes to one, so
 * no curl.exe line is right in both. Invoke-RestMethod behaves the same in
 * both; ConvertTo-Json prints the answer as the JSON it was.
 *
 * `headers` are "Name: value" strings; Content-Type is added here.
 */
function request(shell, { url, headers, body }) {
  const json = JSON.stringify(body);
  if (shell === 'powershell') {
    const table = headers.map((header) => {
      const colon = header.indexOf(':');
      return `${dq(shell, header.slice(0, colon))} = ${dq(shell, header.slice(colon + 1).trim())}`;
    });
    return [
      `Invoke-RestMethod -Method Post -Uri ${dq(shell, url)} \``,
      `  -Headers @{ ${table.join('; ')} } \``,
      '  -ContentType "application/json" `',
      `  -Body ${sq(shell, json)} | ConvertTo-Json -Depth 20`,
    ].join('\n');
  }
  return [`curl -s ${dq(shell, url)} \\`, ...[...headers, 'Content-Type: application/json'].map((h) => `  -H ${dq(shell, h)} \\`), `  -d ${sq(shell, json)}`].join('\n');
}

const shellTitle = (shell) => (shell === 'powershell' ? 'PowerShell' : 'bash / zsh');

// ---------------------------------------------------------------------------
// Client snippets
// ---------------------------------------------------------------------------

export const CLIENTS = [
  { id: 'openai', label: 'OpenAI SDKs' },
  { id: 'codex', label: 'Codex CLI' },
  { id: 'claude', label: 'Claude Code' },
  { id: 'gemini', label: 'Gemini CLI' },
  { id: 'curl', label: 'curl / HTTP' },
  { id: 'websocket', label: 'WebSocket' },
];

/**
 * The snippets of one client.
 *
 * @param {string} client  an id from CLIENTS
 * @param {{ base: string, model: string | null, shell: 'posix' | 'powershell' }} options
 *        base   the gateway root, without a trailing slash
 *        model  a model the gateway serves, or null for the placeholder
 * @returns {{ lead: string, blocks: { id: string, title: string, code: string, note?: string }[] }}
 *          `lead` says what the client needs; each block is one thing to copy.
 */
export function clientSnippets(client, { base, model, shell = 'posix' }) {
  const name = model || MODEL_PLACEHOLDER;
  const key = KEY_PLACEHOLDER;
  const session = shell === 'powershell' ? 'Set for this PowerShell session only.' : 'Set for this shell session. Put the lines in your shell profile to keep them.';

  switch (client) {
    case 'openai':
      return {
        lead: 'Every OpenAI SDK and most OpenAI-compatible tools take a base URL. It ends in /v1.',
        blocks: [
          {
            id: 'env',
            title: `Environment, ${shellTitle(shell)}`,
            code: [envLine(shell, 'OPENAI_BASE_URL', `${base}/v1`), envLine(shell, 'OPENAI_API_KEY', key)].join('\n'),
            note: `The official SDKs read both variables. ${session}`,
          },
          {
            id: 'python',
            title: 'Python',
            code: [
              'from openai import OpenAI',
              '',
              `client = OpenAI(base_url=${JSON.stringify(`${base}/v1`)}, api_key=${JSON.stringify(key)})`,
              'reply = client.chat.completions.create(',
              `    model=${JSON.stringify(name)},`,
              '    messages=[{"role": "user", "content": "Hello"}],',
              ')',
              'print(reply.choices[0].message.content)',
            ].join('\n'),
          },
        ],
      };

    case 'codex':
      return {
        lead: 'Codex CLI talks to the Responses API. Add the gateway as a model provider and keep the key in an environment variable.',
        blocks: [
          {
            id: 'toml',
            title: '~/.codex/config.toml',
            code: [
              `model = ${JSON.stringify(name)}`,
              'model_provider = "switchyard"',
              '',
              '[model_providers.switchyard]',
              'name = "Switchyard"',
              `base_url = ${JSON.stringify(`${base}/v1`)}`,
              'env_key = "SWITCHYARD_API_KEY"',
              'wire_api = "responses"',
            ].join('\n'),
          },
          {
            id: 'env',
            title: `Environment, ${shellTitle(shell)}`,
            code: envLine(shell, 'SWITCHYARD_API_KEY', key),
            note: session,
          },
        ],
      };

    case 'claude':
      return {
        lead: 'Claude Code and the Anthropic SDKs take the gateway’s root: they add /v1/messages themselves.',
        blocks: [
          {
            id: 'env',
            title: `Environment, ${shellTitle(shell)}`,
            code: [envLine(shell, 'ANTHROPIC_BASE_URL', base), envLine(shell, 'ANTHROPIC_API_KEY', key)].join('\n'),
            note: session,
          },
          {
            id: 'run',
            title: 'Start Claude Code with a model the gateway serves',
            code: `claude --model ${dq(shell, name)}`,
          },
        ],
      };

    case 'gemini':
      return {
        lead: 'Gemini CLI and the google-genai SDK take the gateway’s root: they add /v1beta themselves.',
        blocks: [
          {
            id: 'env',
            title: `Environment, ${shellTitle(shell)}`,
            code: [envLine(shell, 'GOOGLE_GEMINI_BASE_URL', base), envLine(shell, 'GEMINI_API_KEY', key)].join('\n'),
            note: session,
          },
          {
            id: 'run',
            title: 'Start Gemini CLI with a model the gateway serves',
            code: `gemini -m ${dq(shell, name)}`,
          },
        ],
      };

    case 'curl':
      return {
        lead:
          shell === 'powershell'
            ? 'One request in each of the four protocols. Any of them reaches any model. PowerShell 5 and 7 pass quotes to curl.exe differently, so these use Invoke-RestMethod, which behaves the same in both.'
            : 'One request in each of the four protocols. Any of them reaches any model: the gateway translates when the provider speaks another protocol.',
        blocks: [
          {
            id: 'chat',
            title: 'OpenAI Chat Completions',
            code: request(shell, {
              url: `${base}/v1/chat/completions`,
              headers: [`Authorization: Bearer ${key}`],
              body: { model: name, messages: [{ role: 'user', content: 'Hello' }] },
            }),
          },
          {
            id: 'responses',
            title: 'OpenAI Responses',
            code: request(shell, {
              url: `${base}/v1/responses`,
              headers: [`Authorization: Bearer ${key}`],
              body: { model: name, input: 'Hello' },
            }),
          },
          {
            id: 'messages',
            title: 'Anthropic Messages',
            code: request(shell, {
              url: `${base}/v1/messages`,
              headers: [`x-api-key: ${key}`, 'anthropic-version: 2023-06-01'],
              body: { model: name, max_tokens: 1024, messages: [{ role: 'user', content: 'Hello' }] },
            }),
          },
          {
            id: 'gemini',
            title: 'Gemini generateContent',
            code: request(shell, {
              url: `${base}/v1beta/models/${modelPath(name)}:generateContent`,
              headers: [`x-goog-api-key: ${key}`],
              body: { contents: [{ role: 'user', parts: [{ text: 'Hello' }] }] },
            }),
          },
        ],
      };

    case 'websocket':
      return {
        lead: 'The Responses API over one WebSocket. The key goes in a header of the upgrade request; each message you send is one JSON object, each frame you get back is one streaming event.',
        blocks: [
          {
            id: 'url',
            title: 'Endpoint',
            code: `${wsBase(base)}/v1/responses`,
          },
          {
            id: 'wscat',
            title: 'Connect with wscat',
            code: `wscat -c ${dq(shell, `${wsBase(base)}/v1/responses`)} -H ${dq(shell, `Authorization: Bearer ${key}`)}`,
          },
          {
            id: 'message',
            title: 'First message',
            code: JSON.stringify({ type: 'response.create', model: name, input: [{ role: 'user', content: 'Hello' }] }),
            note: 'Later turns may leave out the model and send previous_response_id with only the new input.',
          },
        ],
      };

    default:
      return { lead: '', blocks: [] };
  }
}

// ui/tests/check.mjs asks every file under js/pages/ for a default export,
// sub-modules included.
export default clientSnippets;

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

const yesNo = (value) => (value == null ? 'unknown' : value ? 'yes' : 'no');

/**
 * The block to paste when asking for help. Only facts from GET /status that
 * say nothing about the machine or the configuration: no paths, no
 * addresses, no keys, no user agent.
 *
 * @param {object | undefined} status   GET /status
 * @param {{ uptimeMs?: number | null, error?: { message?: string, status?: number } | null }} extra
 *        uptimeMs  the uptime to print (the page keeps it ticking)
 *        error     why the status could not be loaded, when it could not
 */
export function diagnosticsText(status, { uptimeMs = null, error = null } = {}) {
  if (!status) {
    const reason = error ? `${error.message ?? 'no reason given'}${error.status > 0 ? ` (HTTP ${error.status})` : ''}` : 'not loaded yet';
    return ['Switchyard', `Status: unavailable (${reason})`].join('\n');
  }
  const counts = status.counts ?? {};
  const live = status.live ?? {};
  const totals = live.totals ?? {};
  const restart = Array.isArray(status.restart_required) ? status.restart_required : [];
  const warnings = Array.isArray(status.warnings) ? status.warnings : [];
  const n = (value) => (typeof value === 'number' ? formatNumber(value) : 'unknown');

  const lines = [
    `Switchyard ${status.version ?? 'unknown version'}`,
    `Uptime: ${uptimeMs == null ? 'unknown' : formatDuration(uptimeMs)}`,
    `Providers: ${n(counts.providers)}`,
    `Credentials: ${n(counts.credentials)}, ${n(counts.credentials_ready)} ready`,
    `Models: ${n(counts.models)}`,
    `Client keys: ${n(counts.client_keys)}`,
    `Key required: ${yesNo(status.auth_required)}`,
    `Remote admin: ${status.admin ? (status.admin.allow_remote ? 'allowed' : 'off') : 'unknown'}`,
    `Requests: ${n(totals.requests)} since start, ${n(totals.errors)} failed`,
    `In flight: ${n(live.in_flight)} requests, ${n(live.active_streams)} streams, ${n(live.ws_connections)} WebSockets`,
    `Restart required: ${restart.length ? restart.join(', ') : 'no'}`,
    `Warnings: ${warnings.length ? warnings.length : 'none'}`,
    ...warnings.map((warning) => `  - ${warning}`),
  ];
  if (error) lines.push(`Note: the last status refresh failed: ${error.message ?? 'no reason given'}`);
  return lines.join('\n');
}

// ---------------------------------------------------------------------------
// vendor/LICENSES.txt
// ---------------------------------------------------------------------------

/**
 * The bundled pieces named in vendor/LICENSES.txt. Sections start with a
 * line like "===== preact (MIT) =====".
 *
 * @returns {{ name: string, licence: string }[]}
 */
export function parseLicenceSections(text) {
  const out = [];
  for (const line of String(text ?? '').split(/\r?\n/)) {
    const match = /^=====\s*(.+?)\s*=====\s*$/.exec(line);
    if (!match) continue;
    const inner = /^(.*?)\s*\(([^()]+)\)$/.exec(match[1]);
    out.push(inner ? { name: inner[1], licence: inner[2] } : { name: match[1], licence: '' });
  }
  return out;
}
