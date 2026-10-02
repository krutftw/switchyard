// Providers page: everything that is not markup.
//
// Kinds and presets, the health of a credential and of a provider as the
// page words it, cooldown reasons in plain language, the live-frame patch,
// and the two-way mapping between a provider's configuration entry and the
// editor's draft. Pure functions: no DOM, no network.

import { formatDuration, plural } from '../../lib/format.js';

// ---------------------------------------------------------------------------
// Kinds and presets
// ---------------------------------------------------------------------------

/** Provider kinds, in the order the editor offers them. */
export const KINDS = [
  {
    value: 'openai',
    label: 'OpenAI',
    blurb: 'api.openai.com, over the Responses and Chat Completions APIs.',
    defaultUrl: 'https://api.openai.com/v1',
    keyless: false,
    envVar: 'OPENAI_API_KEY',
  },
  {
    value: 'anthropic',
    label: 'Anthropic',
    blurb: 'api.anthropic.com, over the Messages API.',
    defaultUrl: 'https://api.anthropic.com',
    keyless: false,
    envVar: 'ANTHROPIC_API_KEY',
  },
  {
    value: 'gemini',
    label: 'Google Gemini',
    blurb: 'The Gemini API, with an API key from Google AI Studio.',
    defaultUrl: 'https://generativelanguage.googleapis.com',
    keyless: false,
    envVar: 'GEMINI_API_KEY',
  },
  {
    value: 'vertex',
    label: 'Vertex AI',
    blurb: 'Gemini on Google Cloud, signed in with a service-account file.',
    defaultUrl: 'https://aiplatform.googleapis.com',
    keyless: false,
    envVar: 'VERTEX_API_KEY',
  },
  {
    value: 'openai-compat',
    label: 'OpenAI-compatible',
    blurb: 'Any server that speaks Chat Completions: OpenRouter, Groq, Ollama, vLLM.',
    defaultUrl: '',
    keyless: true,
    envVar: 'API_KEY',
  },
  {
    value: 'mock',
    label: 'Mock',
    blurb: 'Built in. Answers without a network or a key, for trying clients and this dashboard.',
    defaultUrl: 'mock://local',
    keyless: true,
    envVar: 'API_KEY',
  },
];

export function kindInfo(kind) {
  return KINDS.find((k) => k.value === kind) ?? { value: kind, label: String(kind ?? ''), blurb: '', defaultUrl: '', keyless: false, envVar: 'API_KEY' };
}

/** Kinds that speak an OpenAI wire API and so have its options. */
export const isOpenAiKind = (kind) => kind === 'openai' || kind === 'openai-compat';

/** Well-known OpenAI-compatible endpoints. A preset only prefills. */
export const COMPAT_PRESETS = [
  { id: 'openrouter', label: 'OpenRouter', base_url: 'https://openrouter.ai/api/v1', envVar: 'OPENROUTER_API_KEY' },
  { id: 'groq', label: 'Groq', base_url: 'https://api.groq.com/openai/v1', envVar: 'GROQ_API_KEY' },
  { id: 'deepseek', label: 'DeepSeek', base_url: 'https://api.deepseek.com/v1', envVar: 'DEEPSEEK_API_KEY' },
  { id: 'together', label: 'Together', base_url: 'https://api.together.xyz/v1', envVar: 'TOGETHER_API_KEY' },
  { id: 'mistral', label: 'Mistral', base_url: 'https://api.mistral.ai/v1', envVar: 'MISTRAL_API_KEY' },
  { id: 'xai', label: 'xAI', base_url: 'https://api.x.ai/v1', envVar: 'XAI_API_KEY' },
  { id: 'ollama', label: 'Ollama', base_url: 'http://127.0.0.1:11434/v1', local: true },
  { id: 'lmstudio', label: 'LM Studio', base_url: 'http://127.0.0.1:1234/v1', local: true },
];

/** The well-known endpoint a base URL points at (same host), or null. */
export function presetForUrl(baseUrl) {
  const hostOf = (url) => {
    try {
      return new URL(String(url ?? '').trim()).host.toLowerCase();
    } catch {
      return '';
    }
  };
  const host = hostOf(baseUrl);
  return host ? (COMPAT_PRESETS.find((preset) => hostOf(preset.base_url) === host) ?? null) : null;
}

/**
 * The environment variable a provider's key is usually kept in: the
 * well-known endpoint's own (GROQ_API_KEY) for an OpenAI-compatible provider
 * that points at one, else the kind's.
 */
export function envVarFor(kind, baseUrl) {
  const preset = kind === 'openai-compat' ? presetForUrl(baseUrl) : null;
  return preset?.envVar ?? kindInfo(kind).envVar;
}

/** What the empty state and the Add menu offer: `new=<id>` in the URL. */
export const QUICK_STARTS = [
  { id: 'openai', label: 'OpenAI', kind: 'openai' },
  { id: 'anthropic', label: 'Anthropic', kind: 'anthropic' },
  { id: 'gemini', label: 'Google Gemini', kind: 'gemini' },
  ...COMPAT_PRESETS.map((p) => ({ id: p.id, label: p.label, kind: 'openai-compat', preset: p })),
  { id: 'mock', label: 'Mock', kind: 'mock' },
];

export const EFFORT_LEVELS = ['minimal', 'low', 'medium', 'high', 'xhigh', 'max'];

export const NAME_PATTERN = /^[a-z0-9_-]+$/;

// ---------------------------------------------------------------------------
// Secrets as the admin API shows them
// ---------------------------------------------------------------------------

/** `env:NAME` or `${NAME}`: names a variable, is not a secret, is shown as written. */
export function isReference(value) {
  const v = String(value ?? '').trim();
  return v.startsWith('env:') || (v.startsWith('${') && v.endsWith('}'));
}

/** The variable a reference names, or '' when the text is not a reference. */
export function referenceName(value) {
  const v = String(value ?? '').trim();
  if (v.startsWith('env:')) return v.slice(4).trim();
  if (v.startsWith('${') && v.endsWith('}')) return v.slice(2, -1).trim();
  return '';
}

/** A header whose value is a credential, judged by its name (the gateway's rule). */
export function isCredentialHeader(name) {
  const n = String(name ?? '').toLowerCase();
  return ['auth', 'key', 'token', 'secret', 'cookie', 'password', 'credential', 'signature'].some((hint) => n.includes(hint));
}

/**
 * The gateway's wildcard match for `exclude`: case-insensitive, `*` is any
 * run of characters (none included), nothing else is special.
 */
export function wildcardMatch(pattern, text) {
  const source = String(pattern ?? '')
    .trim()
    .split('*')
    .map((part) => part.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'))
    .join('.*');
  try {
    return new RegExp(`^${source}$`, 'iu').test(String(text ?? ''));
  } catch {
    return false;
  }
}

// ---------------------------------------------------------------------------
// Cooldowns in plain words
// ---------------------------------------------------------------------------

/** Failure classes of the scheduler: a noun for badges, a clause for sentences, and what to do. */
const REASONS = {
  rate_limit: { noun: 'Rate limit', clause: 'rate limited', advice: '' },
  quota: { noun: 'Quota', clause: 'out of quota', advice: 'check billing for this account' },
  auth: { noun: 'Rejected', clause: 'rejected by the provider', advice: 'check the key' },
  server: { noun: 'Provider error', clause: 'the provider failed', advice: '' },
  transport: { noun: 'No connection', clause: 'could not connect', advice: 'check the base URL and the network' },
  model_not_found: { noun: 'Model not found', clause: 'model not found upstream', advice: 'check the model id' },
  request: { noun: 'Request refused', clause: 'request refused', advice: '' },
};

export function reasonNoun(reason) {
  return REASONS[reason]?.noun ?? (reason ? String(reason).replace(/_/g, ' ') : 'Failure');
}

const capital = (text) => (text ? text[0].toUpperCase() + text.slice(1) : text);

/** A countdown that ticks in whole seconds: "42s", "29m 41s", "1h 02m". */
export function formatRest(ms) {
  if (typeof ms !== 'number' || !Number.isFinite(ms)) return formatDuration(null);
  const seconds = Math.max(0, Math.ceil(ms / 1000));
  return seconds < 60 ? `${seconds}s` : formatDuration(seconds * 1000);
}

/**
 * Why something rests and for how long, as a sentence:
 * "Rate limited — resting 42s", "Rejected by the provider (401) — resting
 * 29m 41s; check the key".
 *
 * @param {{ reason?: string|null, until?: number|null, status?: number }} rest
 * @param {number} now  epoch ms on the gateway's clock
 */
export function explainCooldown({ reason, until, status = 0 }, now) {
  const known = REASONS[reason];
  const what = known ? known.clause : reason ? String(reason).replace(/_/g, ' ') : 'failed';
  const code = status > 0 ? ` (${status})` : '';
  const left = until != null ? ` — resting ${formatRest(until - now)}` : '';
  const advice = known?.advice ? `; ${known.advice}` : '';
  return `${capital(what)}${code}${left}${advice}`;
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

const TONE = { ready: 'clear', cooling: 'caution', unusable: 'stop', disabled: 'off', idle: 'off', unknown: 'off' };

/**
 * What a credential is doing right now, in the page's words.
 *
 * key: "ready" | "cooling" | "disabled" | "unusable" | "idle" (its provider
 *      is switched off) | "unknown" (the scheduler has not seen it yet)
 * tone, label: for the lamp
 * text: one sentence of explanation, or null
 * until, reason: of the cooldown that makes it rest, else null
 * resting: per-model cooldowns still running: [{ model, until, reason }]
 */
export function credentialState(credential, provider, now) {
  const resting = (credential.model_cooldowns ?? []).filter((m) => m && m.until > now).sort((a, b) => a.until - b.until);
  const statusFor = (reason) => (credential.last_error && credential.last_error.class === reason ? credential.last_error.status : 0);
  const make = (key, label, text, extra = {}) => ({ id: credential.id, key, tone: TONE[key], label, text, until: null, reason: null, resting, ...extra });

  if (credential.disabled || credential.status === 'disabled') {
    return make('disabled', 'Disabled', 'Switched off. It takes no requests until it is enabled.');
  }
  if (credential.usable === false || credential.status === 'unusable') {
    const why = credential.unusable_reason ? `${capital(String(credential.unusable_reason))}.` : 'It cannot be used as configured.';
    return make('unusable', 'Unusable', why);
  }
  if (provider && provider.enabled === false) {
    return make('idle', 'Provider disabled', 'The provider is switched off, so this credential takes no requests.');
  }
  if (credential.cooldown_until != null && credential.cooldown_until > now) {
    const reason = credential.cooldown_reason;
    return make('cooling', 'Cooling down', explainCooldown({ reason, until: credential.cooldown_until, status: statusFor(reason) }, now), {
      until: credential.cooldown_until,
      reason,
    });
  }
  if (credential.status === 'cooling' && resting.length > 0) {
    // Every model on it rests; the credential is back when the first one is.
    return make('cooling', 'Cooling down', `Every model on it is resting. The first is back in ${formatRest(resting[0].until - now)}.`, {
      until: resting[0].until,
      reason: resting[0].reason,
    });
  }
  if (credential.status === 'unknown') {
    return make('unknown', 'Not seen yet', 'The scheduler has not picked this credential up yet.');
  }
  const streak = credential.consecutive_failures ?? 0;
  const notes = [];
  if (resting.length > 0) notes.push(`${plural(resting.length, 'model')} resting on it.`);
  // In rotation, but nothing has worked since the last error: worth a look.
  if (streak > 0) notes.push(streak === 1 ? 'Its last attempt failed.' : `Its last ${streak} attempts failed.`);
  return make('ready', 'Ready', notes.length > 0 ? notes.join(' ') : null, { failing: streak > 0 });
}

/**
 * A provider's state for the list: lamp, words, counts, traffic.
 *
 * rank orders providers by how much attention they need (0 first):
 * 0 down, 1 every credential cooling, 2 degraded, 3 serving with resting
 * models, 4 serving, 5 switched off.
 */
export function providerHealth(provider, now) {
  const credentials = provider.credentials ?? [];
  const states = credentials.map((c) => credentialState(c, provider, now));
  const counts = { ready: 0, cooling: 0, disabled: 0, unusable: 0, idle: 0, unknown: 0 };
  for (const s of states) counts[s.key] += 1;

  let requests = 0;
  let successes = 0;
  let failures = 0;
  let latencySum = 0;
  let latencyWeight = 0;
  let lastUsedAt = null;
  for (const c of credentials) {
    requests += c.requests ?? 0;
    successes += c.successes ?? 0;
    failures += c.failures ?? 0;
    if (typeof c.latency_ms === 'number') {
      const weight = Math.max(1, c.successes ?? 0);
      latencySum += c.latency_ms * weight;
      latencyWeight += weight;
    }
    if (c.last_used_at != null && (lastUsedAt == null || c.last_used_at > lastUsedAt)) lastUsedAt = c.last_used_at;
  }

  const restingModels = new Set();
  for (const s of states) if (s.key === 'ready') for (const m of s.resting) restingModels.add(m.model);

  const total = credentials.length;
  const broken = counts.cooling + counts.unusable;
  let rank;
  let tone;
  let label;
  let detail = null;
  if (provider.enabled === false) {
    [rank, tone, label] = [5, 'off', 'Disabled'];
  } else if (total === 0) {
    [rank, tone, label, detail] = [0, 'stop', 'No credentials', 'add a key'];
  } else if (counts.ready > 0 && broken === 0 && successes === 0 && failures > 0) {
    // In rotation by the scheduler's book, yet nothing sent to it has worked.
    // A credential that has not been tried yet (a key added a moment ago)
    // does not make the provider healthy: it is failing until something
    // succeeds.
    [rank, tone, label, detail] = [2, 'caution', 'Failing', 'no request has succeeded'];
  } else if (counts.ready > 0 && broken === 0) {
    if (restingModels.size > 0) [rank, tone, label, detail] = [3, 'clear', 'Serving', `${plural(restingModels.size, 'model')} resting`];
    else [rank, tone, label] = [4, 'clear', 'Serving'];
  } else if (counts.ready > 0) {
    [rank, tone, label, detail] = [2, 'caution', 'Degraded', `${counts.ready} of ${total} ready`];
  } else if (counts.cooling > 0) {
    const first = states.filter((s) => s.key === 'cooling' && s.until != null).sort((a, b) => a.until - b.until)[0];
    [rank, tone, label, detail] = [1, 'caution', 'Cooling down', first ? `back in ${formatRest(first.until - now)}` : null];
  } else if (counts.unusable > 0) {
    [rank, tone, label] = [0, 'stop', 'No usable credential'];
  } else if (counts.disabled > 0) {
    [rank, tone, label] = [0, 'off', 'Credentials disabled'];
  } else {
    [rank, tone, label] = [3, 'off', 'Starting'];
  }

  return {
    rank,
    tone,
    label,
    detail,
    states,
    counts,
    total,
    requests,
    successes,
    failures,
    failureRatio: requests > 0 ? failures / requests : null,
    latency: latencyWeight > 0 ? latencySum / latencyWeight : null,
    lastUsedAt,
    restingModels: restingModels.size,
  };
}

/** "2 ready · 1 cooling": the lamps of a provider in words. */
export function summarizeCounts(counts, total) {
  if (total === 0) return 'None';
  const parts = [];
  if (counts.ready) parts.push(`${counts.ready} ready`);
  if (counts.cooling) parts.push(`${counts.cooling} cooling`);
  if (counts.unusable) parts.push(`${counts.unusable} unusable`);
  if (counts.disabled) parts.push(`${counts.disabled} disabled`);
  if (counts.unknown) parts.push(`${counts.unknown} not seen yet`);
  if (counts.idle) parts.push(`${counts.idle} idle`);
  return parts.join(' · ');
}

/** True while anything on screen counts down, so the page ticks every second only then. */
export function hasCountdown(providers) {
  return (providers ?? []).some((p) => (p.credentials ?? []).some((c) => c.cooldown_until != null || (c.model_cooldowns ?? []).length > 0));
}

// ---------------------------------------------------------------------------
// Live frames
// ---------------------------------------------------------------------------

const FRAME_NULLABLE = ['cooldown_until', 'cooldown_reason', 'latency_ms', 'last_used_at', 'last_error', 'unusable_reason'];

/**
 * Fold a "credential" live frame into the provider list. The frame carries
 * the runtime part of a credential and omits values it has none for, so
 * those are cleared first. Returns the same array when the credential is not
 * in the list (the caller then refetches).
 */
export function applyCredentialFrame(list, frame) {
  const incoming = frame?.credential;
  if (!Array.isArray(list) || !incoming?.id) return list;
  let hit = false;
  const next = list.map((provider) => {
    if (provider.name !== frame.provider) return provider;
    const at = (provider.credentials ?? []).findIndex((c) => c.id === incoming.id);
    if (at === -1) return provider;
    hit = true;
    const merged = { ...provider.credentials[at], model_cooldowns: [] };
    for (const field of FRAME_NULLABLE) merged[field] = null;
    Object.assign(merged, incoming);
    const credentials = provider.credentials.slice();
    credentials[at] = merged;
    return { ...provider, credentials };
  });
  return hit ? next : list;
}

/** Replace (or append) one provider view in the list, keeping the order. */
export function replaceProvider(list, view, previousName = view?.name) {
  if (!Array.isArray(list) || !view) return list;
  const at = list.findIndex((p) => p.name === previousName);
  if (at === -1) return list.some((p) => p.name === view.name) ? list.map((p) => (p.name === view.name ? view : p)) : [...list, view];
  const next = list.slice();
  next[at] = view;
  return next;
}

// ---------------------------------------------------------------------------
// The editor's draft
// ---------------------------------------------------------------------------

let uidCounter = 0;
const uid = (prefix) => `${prefix}${++uidCounter}`;

/**
 * One credential row of the editor.
 *
 * stored    the key as the gateway showed it: a mask, a reference as
 *           written, or '' when there is none. Sent back untouched unless
 *           the row is being replaced with something typed.
 * entered   what the user typed
 * editing   the input is shown (always, for a new row)
 * reference the row holds an environment reference, shown in clear
 * noKey     the stored key is to be dropped and the credential kept
 *           without one (kinds that can be called without a key)
 * source    "api_keys" | "credentials" | null: where it was configured
 */
export function blankCredential(extra = {}) {
  return {
    uid: uid('c'),
    source: null,
    stored: '',
    entered: '',
    editing: true,
    reference: false,
    noKey: false,
    label: '',
    weight: null,
    priority: null,
    proxy: '',
    disabled: false,
    service_account_file: '',
    open: false,
    ...extra,
  };
}

export function blankModel(extra = {}) {
  return {
    uid: uid('m'),
    id: '',
    alias: '',
    display_name: '',
    context_window: null,
    max_output_tokens: null,
    thinkingMode: 'none',
    levels: [],
    min: null,
    max: null,
    zero_allowed: false,
    dynamic_allowed: false,
    open: false,
    ...extra,
  };
}

/**
 * One custom-header row of the editor.
 *
 * Only the values of credential-like headers come back masked. Such a row
 * holds the mask in `stored` and the name it was stored under in
 * `storedName`: the gateway restores a masked value by header name, so the
 * mask stands for the secret only while the row keeps that name. Every other
 * value is plain text and lives in `entered` from the start.
 */
export function blankHeader(extra = {}) {
  return { uid: uid('h'), name: '', storedName: '', stored: '', entered: '', editing: true, ...extra };
}

/** Whether the row still stands for the value stored under its original name. */
export function headerKeepsStored(row) {
  return row.stored !== '' && row.name.trim().toLowerCase() === row.storedName.toLowerCase();
}

/** What a header row sends as its value. */
export function headerValue(row) {
  if (!headerKeepsStored(row)) return row.entered;
  // Replacing without typing anything keeps the stored value.
  return row.editing ? row.entered || row.stored : row.stored;
}

/** Whether a credential row carries more than a key, and so needs a `credentials` entry. */
export function hasSettings(row) {
  return !!(row.label.trim() || row.weight != null || row.priority != null || row.proxy.trim() || row.disabled || row.service_account_file.trim());
}

/** What a credential row sends as its key. */
export function keyOf(row) {
  if (row.noKey) return '';
  if (!row.editing) return row.stored;
  const typed = row.entered.trim();
  // Replacing without typing anything keeps the stored key; it does not delete it.
  return typed || row.stored;
}

function thinkingMode(thinking) {
  if (thinking == null) return 'none';
  const levels = Array.isArray(thinking.levels) && thinking.levels.length > 0;
  const range = (thinking.min ?? 0) > 0 || (thinking.max ?? 0) > 0;
  if (levels && range) return 'both';
  return range ? 'budget' : 'levels';
}

/** A new provider's draft. `preset` is one of COMPAT_PRESETS. */
export function emptyDraft(kind = 'openai', preset = null) {
  const info = kindInfo(kind);
  return {
    name: '',
    kind,
    enabled: true,
    base_url: preset?.base_url ?? '',
    prefix: '',
    priority: 0,
    proxy: '',
    wire_api: 'auto',
    legacy_max_tokens: null,
    stream_usage: null,
    websocket: false,
    project: '',
    location: '',
    discover: true,
    exclude: [],
    // One empty row to type into where a key is needed: every kind that
    // cannot work without one, and the hosted OpenAI-compatible endpoints.
    credentials: (preset ? preset.local : info.keyless) ? [] : [blankCredential()],
    headers: [],
    models: [],
  };
}

/** The draft of an existing provider, from its `config` (the shape PUT accepts). */
export function draftFromConfig(config) {
  const fromKey = (key, extra) => blankCredential({ stored: key ?? '', editing: !key, reference: isReference(key), ...extra });
  return {
    name: config.name ?? '',
    kind: config.kind,
    enabled: config.enabled !== false,
    base_url: config.base_url ?? '',
    prefix: config.prefix ?? '',
    priority: config.priority ?? 0,
    proxy: config.proxy ?? '',
    wire_api: config.wire_api ?? 'auto',
    legacy_max_tokens: config.legacy_max_tokens ?? null,
    stream_usage: config.stream_usage ?? null,
    websocket: config.websocket === true,
    project: config.project ?? '',
    location: config.location ?? '',
    discover: config.discover !== false,
    exclude: [...(config.exclude ?? [])],
    credentials: [
      ...(config.api_keys ?? []).map((key) => fromKey(key, { source: 'api_keys' })),
      ...(config.credentials ?? []).map((c) =>
        fromKey(c.api_key, {
          source: 'credentials',
          label: c.label ?? '',
          weight: c.weight ?? null,
          priority: c.priority ?? null,
          proxy: c.proxy ?? '',
          disabled: c.disabled === true,
          service_account_file: c.service_account_file ?? '',
        }),
      ),
    ],
    headers: Object.entries(config.headers ?? {}).map(([name, value]) => {
      const text = String(value ?? '');
      return isCredentialHeader(name) && text !== '' ? blankHeader({ name, storedName: name, stored: text, editing: false }) : blankHeader({ name, entered: text });
    }),
    models: (config.models ?? []).map((m) =>
      blankModel({
        id: m.id ?? '',
        alias: m.alias ?? '',
        display_name: m.display_name ?? '',
        context_window: m.context_window ?? null,
        max_output_tokens: m.max_output_tokens ?? null,
        thinkingMode: thinkingMode(m.thinking),
        levels: [...(m.thinking?.levels ?? [])],
        min: m.thinking?.min ? m.thinking.min : null,
        max: m.thinking?.max ? m.thinking.max : null,
        zero_allowed: m.thinking?.zero_allowed === true,
        dynamic_allowed: m.thinking?.dynamic_allowed === true,
      }),
    ),
  };
}

function thinkingOf(row) {
  if (row.thinkingMode === 'none') return undefined;
  const out = { zero_allowed: row.zero_allowed, dynamic_allowed: row.dynamic_allowed };
  if (row.thinkingMode === 'levels' || row.thinkingMode === 'both') {
    // In the vendor's order, whatever order they were ticked in.
    out.levels = EFFORT_LEVELS.filter((level) => row.levels.includes(level));
  }
  if (row.thinkingMode === 'budget' || row.thinkingMode === 'both') {
    out.min = row.min ?? 0;
    out.max = row.max ?? 0;
  }
  return out;
}

/**
 * The provider entry a draft stands for, in the shape POST and PUT accept,
 * and where each row ended up in it.
 *
 * Credentials: the runtime order is `api_keys` first, then `credentials`.
 * Rows keep the order they have on screen: the leading rows that are a bare
 * key go to `api_keys`, and from the first row that needs settings (or was
 * configured under `credentials`) onward everything is a `credentials`
 * entry. A row with neither key nor settings is dropped. Secrets follow the
 * API's mask rule: an untouched row sends back exactly what it was shown.
 *
 * `resetForKind`: options that belong to another kind go back to their
 * defaults (used when the kind was changed in this form).
 *
 * @returns {{ config: object, paths: Record<string, string> }} `paths` maps
 *          a row's uid to its path in the entry ("api_keys[1]",
 *          "credentials[0]", "models[2]"), for placing validation issues.
 */
export function configFromDraft(draft, { resetForKind = false } = {}) {
  const paths = {};
  const openai = isOpenAiKind(draft.kind);
  const keep = (applies, value, fallback) => (applies || !resetForKind ? value : fallback);

  const rows = draft.credentials.filter((row) => keyOf(row) !== '' || hasSettings(row));
  let split = rows.findIndex((row) => hasSettings(row) || row.source === 'credentials');
  if (split === -1) split = rows.length;
  const apiKeys = [];
  const credentials = [];
  rows.forEach((row, index) => {
    const key = keyOf(row);
    if (index < split) {
      paths[row.uid] = `api_keys[${apiKeys.length}]`;
      apiKeys.push(key);
      return;
    }
    const entry = {};
    if (key) entry.api_key = key;
    if (row.label.trim()) entry.label = row.label.trim();
    if (row.disabled) entry.disabled = true;
    if (row.weight != null) entry.weight = row.weight;
    if (row.priority != null) entry.priority = row.priority;
    if (row.proxy.trim()) entry.proxy = row.proxy.trim();
    if (row.service_account_file.trim()) entry.service_account_file = row.service_account_file.trim();
    paths[row.uid] = `credentials[${credentials.length}]`;
    credentials.push(entry);
  });

  const headers = {};
  for (const row of draft.headers) {
    const name = row.name.trim();
    if (!name) continue;
    headers[name] = headerValue(row);
    paths[row.uid] = `headers.${name}`;
  }

  const models = [];
  for (const row of draft.models) {
    const id = row.id.trim();
    const alias = row.alias.trim();
    const display = row.display_name.trim();
    const thinking = thinkingOf(row);
    if (!id && !alias && !display && row.context_window == null && row.max_output_tokens == null && !thinking) continue;
    const entry = { id };
    if (alias) entry.alias = alias;
    if (display) entry.display_name = display;
    if (row.context_window != null) entry.context_window = row.context_window;
    if (row.max_output_tokens != null) entry.max_output_tokens = row.max_output_tokens;
    if (thinking) entry.thinking = thinking;
    paths[row.uid] = `models[${models.length}]`;
    models.push(entry);
  }

  const config = {
    name: draft.name.trim(),
    kind: draft.kind,
    enabled: draft.enabled,
    base_url: draft.base_url.trim(),
    api_keys: apiKeys,
    credentials,
    prefix: draft.prefix.trim(),
    priority: draft.priority ?? 0,
    proxy: draft.proxy.trim(),
    headers,
    models,
    exclude: draft.exclude.map((p) => p.trim()).filter(Boolean),
    discover: draft.discover,
    wire_api: keep(openai, draft.wire_api, 'auto'),
    legacy_max_tokens: keep(openai, draft.legacy_max_tokens, null),
    stream_usage: keep(openai, draft.stream_usage, null),
    websocket: keep(draft.kind === 'openai', draft.websocket, false),
    project: keep(draft.kind === 'vertex', draft.project.trim(), ''),
    location: keep(draft.kind === 'vertex', draft.location.trim(), ''),
  };
  return { config, paths };
}

// ---------------------------------------------------------------------------
// Keyless credentials
// ---------------------------------------------------------------------------
//
// In an update, an empty `credentials[].api_key` means "keep the stored
// key", and the gateway decides which stored entry that is: the one with the
// same label, else the one at the same position. A keyless entry that is
// new, relabelled or moved can therefore be handed the key of an entry that
// was removed in the same save, and nothing in the request can say "no key".
//
// What the gateway never does is hand out a key that another entry of the
// same request claims (by its mask, or by the key written in full). So a
// save with entries at risk goes in two requests: first without them, which
// drops the removed keys from the stored configuration; then whole, when
// every stored key is claimed and there is nothing left to inherit.

const keylessEntry = (entry) => !String(entry?.api_key ?? '').trim();

/**
 * Indexes of `config.credentials` whose empty key the gateway could fill
 * from `stored` (the provider's entry as last read, secrets masked). It errs
 * on the side of naming an entry: the gateway's rule has more exceptions
 * than are followed here, and each of them only ever leaves a field empty.
 */
export function keylessAtRisk(config, stored) {
  const incoming = config?.credentials ?? [];
  const have = stored?.credentials ?? [];
  // How many stored entries carry each key (as masked) beyond those an
  // incoming entry sends back, and so claims.
  const spare = new Map();
  for (const entry of have) if (!keylessEntry(entry)) spare.set(entry.api_key, (spare.get(entry.api_key) ?? 0) + 1);
  for (const entry of incoming) if (!keylessEntry(entry) && spare.has(entry.api_key)) spare.set(entry.api_key, spare.get(entry.api_key) - 1);
  const unclaimed = (entry) => entry != null && !keylessEntry(entry) && (spare.get(entry.api_key) ?? 0) > 0;
  const fileOf = (entry) => String(entry?.service_account_file ?? '').trim();
  const labelOf = (entry) => String(entry?.label ?? '').trim();

  const risky = [];
  incoming.forEach((entry, index) => {
    if (!keylessEntry(entry)) return;
    const file = fileOf(entry);
    const label = labelOf(entry);
    // By identity: the stored entry with the same service-account file, else the same label.
    const byIdentity = file ? have.some((other) => fileOf(other) === file && unclaimed(other)) : label !== '' && have.some((other) => labelOf(other) === label && unclaimed(other));
    // By position: the stored entry at the same index, when it is of the same sort.
    const byPosition = unclaimed(have[index]) && fileOf(have[index]) === file;
    if (byIdentity || byPosition) risky.push(index);
  });
  return risky;
}

/**
 * The first of the two requests: `config` without the keyless credentials
 * at risk, or null when one request is enough. Leaving an entry out moves
 * the ones after it, so the check runs until nothing more is at risk.
 */
export function withoutRiskyKeyless(config, stored) {
  let credentials = config?.credentials ?? [];
  let dropped = 0;
  for (;;) {
    const risky = keylessAtRisk({ credentials }, stored);
    if (risky.length === 0) break;
    credentials = credentials.filter((_, index) => !risky.includes(index));
    dropped += risky.length;
  }
  return dropped > 0 ? { ...config, credentials } : null;
}

/**
 * After a save: indexes of the credentials that were sent without a key and
 * came back with one. `view` is the gateway's answer; its `config` lists the
 * entries in the order they were sent.
 */
export function filledKeyless(config, view) {
  const sent = config?.credentials ?? [];
  const saved = view?.config?.credentials ?? [];
  if (saved.length !== sent.length) return [];
  return sent.map((entry, index) => (keylessEntry(entry) && !keylessEntry(saved[index]) ? index : -1)).filter((index) => index !== -1);
}

/** How a credentials entry is named in a message: its label, its file, else its place. */
export function credentialName(entry, index) {
  return String(entry?.label ?? '').trim() || String(entry?.service_account_file ?? '').trim() || `credential ${index + 1}`;
}

/** What the browser can check before sending: [{ path, message }]. */
export function localIssues(draft, { takenNames = [] } = {}) {
  const issues = [];
  const name = draft.name.trim();
  if (!name) issues.push({ path: 'name', message: 'Give the provider a name.' });
  else if (!NAME_PATTERN.test(name)) issues.push({ path: 'name', message: 'Use lowercase letters, digits, - and _ only.' });
  else if (takenNames.includes(name)) issues.push({ path: 'name', message: `A provider named ${name} already exists.` });
  if (draft.kind === 'openai-compat' && !draft.base_url.trim()) {
    issues.push({ path: 'base_url', message: 'An OpenAI-compatible provider needs its base URL, for example https://openrouter.ai/api/v1.' });
  }
  for (const row of draft.headers) {
    const header = row.name.trim();
    // A masked value was stored under another name and cannot follow the rename.
    if (header && row.stored && !headerKeepsStored(row) && !row.entered) {
      issues.push({ path: `headers.${header}`, message: `Type the value of ${header}: the one stored for ${row.storedName} does not follow a renamed header.` });
    }
  }
  return issues;
}

// ---------------------------------------------------------------------------
// Validation issues from the gateway
// ---------------------------------------------------------------------------

const ownPath = (path) => String(path ?? '').replace(/^providers\[\d+\]\.?/, '');

/**
 * The gateway names a field by its place in the whole configuration
 * ("providers[2].base_url"); the form knows it as "base_url". Returns the
 * error with paths, and the paths quoted in its message, made relative to
 * the provider entry. Not an ApiError any more, but everything FormError and
 * useIssues read is there.
 */
export function rebaseError(error) {
  if (!error) return null;
  return {
    status: error.status,
    code: error.code,
    aborted: error.aborted === true,
    // Set by the editor when the first of two requests went through and the second did not.
    partial: error.partial != null,
    message: String(error.message ?? '').replace(/providers\[\d+\]\./g, ''),
    issues: (error.issues ?? []).map((issue) => ({ path: ownPath(issue.path), message: issue.message })),
  };
}

/** Which form section an issue path belongs to. */
export function sectionOfPath(path) {
  const head = String(path ?? '').split(/[.[]/)[0];
  if (['name', 'kind', 'enabled', 'base_url', 'project', 'location'].includes(head)) return 'basics';
  if (head === 'api_keys' || head === 'credentials') return 'credentials';
  if (['prefix', 'priority', 'proxy', 'wire_api', 'legacy_max_tokens', 'stream_usage', 'websocket', 'headers'].includes(head)) return 'routing';
  if (['models', 'exclude', 'discover'].includes(head)) return 'models';
  return null;
}

/** A name for a new provider that is not taken: "groq", then "groq-2". */
export function freeName(base, takenNames) {
  const clean = String(base ?? '').toLowerCase().replace(/[^a-z0-9_-]+/g, '-').replace(/^-+|-+$/g, '');
  if (!clean) return '';
  if (!takenNames.includes(clean)) return clean;
  for (let n = 2; n < 1000; n += 1) if (!takenNames.includes(`${clean}-${n}`)) return `${clean}-${n}`;
  return clean;
}

// Page modules need a default export (tests/check.mjs); this one's is the
// function the rest of the page is built around.
export default providerHealth;
