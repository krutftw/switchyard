// Models page: the logic that needs no DOM. Everything here is a pure
// function of what the admin API returned (GET /models, /providers,
// /aliases, /status), so it can be reasoned about, and tested, on its own.
//
// The reasoning helpers mirror crates/core/src/reasoning.rs
// (parse_model_suffix, normalize_depth): the page explains what the gateway
// will do with `model(high)`, so it has to reach the same answer.

import { formatDuration, formatNumber, plural } from '../../lib/format.js';

// ---------------------------------------------------------------------------
// Reasoning
// ---------------------------------------------------------------------------

/** Effort levels, least to most. */
export const EFFORTS = ['minimal', 'low', 'medium', 'high', 'xhigh', 'max'];

/** The token budget conventionally equivalent to a level. */
const EFFORT_BUDGET = { minimal: 512, low: 1024, medium: 8192, high: 24576, xhigh: 32768, max: 128000 };

/** A budget's nearest level. Never "max": a budget cannot say "everything". */
export function budgetToEffort(budget) {
  if (budget <= 512) return 'minimal';
  if (budget <= 1024) return 'low';
  if (budget <= 8192) return 'medium';
  if (budget <= 24576) return 'high';
  return 'xhigh';
}

const hasRange = (t) => (t?.min ?? 0) > 0 || (t?.max ?? 0) > 0;
const hasLevels = (t) => (t?.levels?.length ?? 0) > 0;

/** The text between the parentheses, as a depth; null when it is not one. */
function depthFromRaw(raw) {
  const text = String(raw).trim().toLowerCase();
  if (text === 'none' || text === 'off' || text === 'disabled') return { mode: 'off' };
  if (text === 'auto' || text === 'dynamic') return { mode: 'auto' };
  if (EFFORTS.includes(text)) return { mode: 'level', value: text };
  if (/^[+-]?\d+$/.test(text)) {
    const n = Number(text);
    if (n === 0) return { mode: 'off' };
    if (n === -1) return { mode: 'auto' };
    if (n > 0) return { mode: 'budget', value: n };
  }
  return null;
}

/**
 * Split `name(suffix)` the way the gateway does: the suffix is the text
 * between the last "(" and a ")" that ends the name.
 *
 *   parseSuffix('gpt-5(high)')  -> { base: 'gpt-5', raw: 'high', depth: { mode: 'level', value: 'high' } }
 *   parseSuffix('gpt-5(ultra)') -> { base: 'gpt-5', raw: 'ultra', depth: null }   (removed, then ignored)
 *   parseSuffix('gpt-5')        -> { base: 'gpt-5', raw: null, depth: null }
 */
export function parseSuffix(name) {
  const text = String(name ?? '');
  const none = { base: text, raw: null, depth: null };
  if (!text.endsWith(')')) return none;
  const open = text.lastIndexOf('(');
  if (open <= 0) return none;
  const raw = text.slice(open + 1, -1);
  return { base: text.slice(0, open), raw, depth: depthFromRaw(raw) };
}

function clampBudget(t, budget) {
  if (!hasRange(t)) return budget;
  let b = budget;
  if (t.min > 0 && b < t.min) b = t.min;
  if (t.max > 0 && b > t.max) b = t.max;
  return b;
}

function clampLevel(t, level) {
  const levels = t.levels ?? [];
  if (levels.length === 0 || levels.includes(level)) return level;
  // "Top effort" is xhigh for some vendors and max for others.
  const preferred = level === 'xhigh' ? ['max', 'high'] : level === 'max' ? ['xhigh', 'high'] : [];
  for (const p of preferred) if (levels.includes(p)) return p;
  const rank = (e) => EFFORTS.indexOf(e);
  let best = levels[0];
  let bestDist = Infinity;
  for (const candidate of levels) {
    const dist = Math.abs(rank(candidate) - rank(level));
    if (dist < bestDist || (dist === bestDist && rank(candidate) < rank(best))) {
      best = candidate;
      bestDist = dist;
    }
  }
  return best;
}

/** What the gateway sends upstream when a request asks `t`'s model for `depth`. */
export function fitDepth(depth, t) {
  const range = hasRange(t);
  const levels = hasLevels(t);
  let d = depth;
  if (d.mode === 'level' && range && !levels) d = { mode: 'budget', value: EFFORT_BUDGET[d.value] };
  else if (d.mode === 'budget' && levels && !range) d = { mode: 'level', value: budgetToEffort(d.value) };

  if (d.mode === 'auto' && !t.dynamic_allowed) {
    if (levels && !range) d = { mode: 'level', value: 'medium' };
    else {
      const mid = Math.floor(((t.min ?? 0) + (t.max ?? 0)) / 2);
      if (mid > 0) d = { mode: 'budget', value: mid };
      else if (t.zero_allowed) d = { mode: 'off' };
      else d = { mode: 'budget', value: t.min ?? 0 };
    }
  }

  if (d.mode === 'level' && levels) d = { mode: 'level', value: clampLevel(t, d.value) };
  else if (d.mode === 'budget') d = { mode: 'budget', value: clampBudget(t, d.value) };

  if (d.mode === 'off' && !t.zero_allowed) {
    if (levels) d = { mode: 'level', value: t.levels[0] };
    else if ((t.min ?? 0) > 0) d = { mode: 'budget', value: t.min };
  }
  return d;
}

function depthPhrase(d) {
  if (d.mode === 'off') return 'reasoning off';
  if (d.mode === 'auto') return 'a depth the provider picks';
  if (d.mode === 'level') return `the ${d.value} level`;
  return `a budget of ${formatNumber(d.value)} tokens`;
}

/**
 * One line for the table: "Levels low–high", "Budget 1,024–128,000", both,
 * or "None". `info` is a ModelInfo (GET /models, GET /catalog).
 * Returns { text, rank } where rank orders the column (0 unknown, 1 none,
 * 2 levels, 3 budget, 4 both).
 */
export function reasoningSummary(info) {
  const t = info?.thinking;
  if (!t) {
    // known: false means the gateway has no metadata and passes reasoning
    // settings through untouched, which is not the same as "does not reason".
    return info?.known === false ? { text: null, rank: 0, title: 'No metadata: reasoning settings are passed through as sent' } : { text: 'None', rank: 1 };
  }
  const parts = [];
  if (hasLevels(t)) {
    const first = t.levels[0];
    const last = t.levels[t.levels.length - 1];
    parts.push(first === last ? `Level ${first}` : `Levels ${first}–${last}`);
  }
  if (hasRange(t)) {
    const low = formatNumber(t.min ?? 0);
    const high = formatNumber(t.max ?? 0);
    parts.push(`${parts.length ? 'budget' : 'Budget'} ${low}–${high}`);
  }
  if (parts.length === 0) parts.push('Supported');
  return { text: parts.join(', '), rank: (hasLevels(t) ? 2 : 0) + (hasRange(t) ? 3 : 0) || 2 };
}

/** The longer wording for the detail drawer. */
export function reasoningDetail(info) {
  const t = info?.thinking;
  if (!t) return null;
  const parts = [];
  if (hasLevels(t)) parts.push(`Levels: ${t.levels.join(', ')}`);
  if (hasRange(t)) parts.push(`Budget: ${formatNumber(t.min ?? 0)} to ${formatNumber(t.max ?? 0)} tokens`);
  parts.push(t.zero_allowed ? 'Can be turned off' : 'Cannot be turned off');
  parts.push(t.dynamic_allowed ? 'Provider can pick the depth' : 'Needs an explicit depth');
  return parts;
}

/**
 * The four suffix forms, written for one model: what to type and what the
 * gateway then sends. `name` is the client-facing name.
 */
export function suffixExamples(name, t) {
  const levels = hasLevels(t);
  const range = hasRange(t);
  const out = [];

  const level = levels ? (t.levels.includes('high') ? 'high' : t.levels[t.levels.length - 1]) : 'high';
  const fittedLevel = fitDepth({ mode: 'level', value: level }, t);
  out.push({
    model: `${name}(${level})`,
    title: 'A level',
    text:
      fittedLevel.mode === 'level'
        ? `Reasons at ${depthPhrase(fittedLevel)}.${levels ? ` This model takes ${t.levels.join(', ')}; other levels move to the nearest of these.` : ''}`
        : `This model takes token budgets, so ${level} is sent as ${depthPhrase(fittedLevel)}.`,
  });

  const budget = clampBudget(t, 8192);
  const fittedBudget = fitDepth({ mode: 'budget', value: budget }, t);
  out.push({
    model: `${name}(${budget})`,
    title: 'A token budget',
    text:
      fittedBudget.mode === 'budget'
        ? `Reasons with ${depthPhrase(fittedBudget)}.${range ? ` Budgets outside ${formatNumber(t.min ?? 0)} to ${formatNumber(t.max ?? 0)} move to the nearest end.` : ''}`
        : `This model takes levels, so ${formatNumber(budget)} tokens is sent as ${depthPhrase(fittedBudget)}.`,
  });

  const fittedOff = fitDepth({ mode: 'off' }, t);
  out.push({
    model: `${name}(none)`,
    title: 'Off',
    text: fittedOff.mode === 'off' ? 'Turns reasoning off for the request.' : `This model cannot stop reasoning, so the gateway sends its lowest setting, ${depthPhrase(fittedOff)}. Anthropic upstreams are still told to disable it.`,
  });

  const fittedAuto = fitDepth({ mode: 'auto' }, t);
  out.push({
    model: `${name}(auto)`,
    title: 'Automatic',
    text: fittedAuto.mode === 'auto' ? 'Lets the provider decide how much to reason.' : `This model needs an explicit depth, so the gateway sends ${depthPhrase(fittedAuto)}.`,
  });
  return out;
}

/** Suffixes worth offering in the alias target picker for a reasoning model. */
export function suffixChoices(t) {
  if (!t) return [];
  const choices = [];
  if (hasLevels(t)) choices.push(...t.levels);
  else choices.push('low', 'medium', 'high');
  if (hasRange(t)) choices.push(String(clampBudget(t, 8192)));
  choices.push('none', 'auto');
  return choices;
}

// ---------------------------------------------------------------------------
// Availability
// ---------------------------------------------------------------------------

const COOLDOWN_REASON = {
  rate_limit: 'rate limited',
  quota: 'quota used up',
  auth: 'authentication failed',
  server: 'upstream error',
  transport: 'connection failed',
  model_not_found: 'model not found upstream',
  request: 'request rejected',
};

/** "connection failed" for "transport"; the raw word for a newer gateway's reasons. */
export const cooldownReason = (reason) => (reason ? (COOLDOWN_REASON[reason] ?? String(reason).replace(/_/g, ' ')) : null);

/** "41s", "2m 05s": the time left until `until`, never negative. */
export function timeLeft(until, now) {
  if (!until) return null;
  // Whole seconds: a countdown that reads "51.0s" ticks in the wrong place.
  const seconds = Math.max(1, Math.ceil((until - now) / 1000));
  return seconds < 60 ? `${seconds}s` : formatDuration(seconds * 1000);
}

/**
 * The state of one route (provider + upstream model) of a model entry.
 * `provider` is that provider's view from GET /providers, or undefined when
 * the provider list could not be loaded.
 *
 * Returns { state, label, until, reason, why, said, unknown } where state is
 * a lamp tone:
 *   clear    at least one credential can serve the model now
 *   caution  credentials exist and are resting (cooling down), or the route
 *            cannot serve and the reason is not known (`unknown: true`)
 *   stop     no credential can ever serve it as configured
 *   off      the provider or every credential is switched off
 * `why` is the page's own sentence for the cause; `said` is the gateway's
 * wording (a credential's unusable_reason), kept apart so the page never
 * presents its own words as the gateway's.
 */
export function routeState(route, provider) {
  const base = { until: null, reason: null, why: null, said: null, unknown: false };
  if (route.credentials_available > 0) return { ...base, state: 'clear', label: 'Serving' };
  // GET /models says this much on its own, whatever became of GET /providers.
  if (route.credentials_total === 0) return { ...base, state: 'stop', label: 'No credentials', why: 'The provider has no credential.' };
  if (!provider) return { ...base, state: 'caution', label: 'Not available', unknown: true };
  if (provider.enabled === false) return { ...base, state: 'off', label: 'Provider disabled' };

  const credentials = provider.credentials ?? [];
  if (credentials.length === 0) return { ...base, state: 'stop', label: 'No credentials', why: 'The provider has no credential.' };
  const enabled = credentials.filter((c) => !c.disabled && c.status !== 'disabled');
  if (enabled.length === 0) return { ...base, state: 'off', label: 'Credentials disabled', why: 'Every credential of the provider is switched off.' };
  const usable = enabled.filter((c) => c.usable !== false && c.status !== 'unusable');
  if (usable.length === 0) {
    return { ...base, state: 'stop', label: 'No usable credential', said: enabled.find((c) => c.unusable_reason)?.unusable_reason ?? null };
  }

  // Resting. A credential is back when both its own cooldown and this
  // model's cooldown on it have ended; the route is back with the first one.
  let until = null;
  let reason = null;
  for (const c of usable) {
    const model = (c.model_cooldowns ?? []).find((m) => m.model === route.upstream_model);
    const whole = c.cooldown_until ?? 0;
    const perModel = model?.until ?? 0;
    const back = Math.max(whole, perModel);
    if (back > 0 && (until === null || back < until)) {
      until = back;
      reason = perModel >= whole ? model?.reason : c.cooldown_reason;
    }
  }
  return { ...base, state: 'caution', label: 'Cooling down', until, reason: reason ?? null };
}

const AVAILABILITY_RANK = { routable: 0, cooling: 1, unknown: 2, nocreds: 3, noroute: 4 };

/**
 * Build the table rows: each model entry with its routes' states and one
 * overall availability.
 *
 *   availability.key   routable | cooling | unknown | nocreds | noroute
 *
 * `unknown` is a model that cannot be served right now while the provider
 * details that would say why (cooling down, switched off) are not loaded.
 *
 * `providers` is the array from GET /providers (may be undefined).
 */
export function buildRows(models, providers) {
  const byName = new Map((providers ?? []).map((p) => [p.name, p]));
  return (models ?? []).map((entry) => {
    const routes = (entry.routes ?? []).map((route, index) => ({ ...route, index, ...routeState(route, byName.get(route.provider)) }));
    const serving = routes.filter((r) => r.state === 'clear');
    const cooling = routes.filter((r) => r.state === 'caution' && !r.unknown);
    const unknown = routes.filter((r) => r.unknown);
    let availability;
    if (routes.length === 0) {
      availability = { key: 'noroute', tone: 'off', label: 'No route', until: null };
    } else if (serving.length > 0) {
      availability = { key: 'routable', tone: 'clear', label: 'Routable', detail: serving.length < routes.length ? `${serving.length} of ${plural(routes.length, 'route')}` : null, until: null };
    } else if (cooling.length > 0) {
      const untils = cooling.map((r) => r.until).filter(Boolean);
      availability = { key: 'cooling', tone: 'caution', label: routes.length > 1 ? 'All cooling' : 'Cooling down', until: untils.length ? Math.min(...untils) : null };
    } else if (unknown.length > 0) {
      availability = { key: 'unknown', tone: 'caution', label: 'Not available', until: null };
    } else {
      const allOff = routes.every((r) => r.state === 'off');
      availability = { key: 'nocreds', tone: allOff ? 'off' : 'stop', label: allOff ? 'Switched off' : 'No credentials', until: null };
    }
    availability.rank = AVAILABILITY_RANK[availability.key];
    const isAlias = Array.isArray(entry.alias_targets);
    return {
      name: entry.name,
      info: entry.info ?? {},
      hidden: !!entry.hidden,
      isAlias,
      kind: isAlias ? 'alias' : 'model',
      aliasTargets: entry.alias_targets ?? null,
      routes,
      availability,
      providerNames: [...new Set(routes.map((r) => r.provider))],
      reasoning: reasoningSummary(entry.info),
      // One lower-cased haystack for the search box.
      search: [entry.name, entry.info?.display_name, entry.info?.owned_by, ...(entry.alias_targets ?? []), ...routes.flatMap((r) => [r.provider, r.upstream_model])]
        .filter(Boolean)
        .join('\n')
        .toLowerCase(),
    };
  });
}

/** The soonest moment a cooldown on any credential ends after `now`, or null. */
export function nextCooldownEnd(providers, now) {
  let next = null;
  for (const p of providers ?? []) {
    for (const c of p.credentials ?? []) {
      const ends = [c.cooldown_until, ...(c.model_cooldowns ?? []).map((m) => m.until)];
      for (const at of ends) if (at && at > now && (next === null || at < next)) next = at;
    }
  }
  return next;
}

/**
 * The priority the gateway tries a route at. It tiers by credential, not by
 * provider: each credential has an effective priority (its own, else the
 * provider's), and of the credentials that can serve a model only the
 * highest tier is used. So a route counts at the highest priority among the
 * credentials that can serve it now; when none can, among those that could
 * once they are back.
 *
 * `provider` is the provider's view from GET /providers (may be undefined).
 */
export function routePriority(route, provider, now = Date.now()) {
  if (!provider) return 0;
  const fallback = provider.priority ?? 0;
  const usable = (provider.credentials ?? []).filter((c) => !c.disabled && c.status !== 'disabled' && c.usable !== false && c.status !== 'unusable');
  if (usable.length === 0) return fallback;
  const ready = usable.filter((c) => !((c.cooldown_until ?? 0) > now) && !(c.model_cooldowns ?? []).some((m) => m.model === route.upstream_model && (m.until ?? 0) > now));
  return Math.max(...(ready.length ? ready : usable).map((c) => c.priority ?? fallback));
}

/** Map(route index -> routePriority) for every route of a row. */
export function routePriorities(row, providers, now = Date.now()) {
  const byName = new Map((providers ?? []).map((p) => [p.name, p]));
  return new Map(row.routes.map((r) => [r.index, routePriority(r, byName.get(r.provider), now)]));
}

/**
 * Which routes carry traffic right now. Credentials of a higher priority are
 * tried first and share the load by the routing strategy; an alias tries its
 * targets in order. So: of the routes that can serve, those of the first
 * alias target that has any, and among them the highest priority.
 *
 * `priorities` is Map(route index -> priority) from routePriorities.
 */
export function litRoutes(row, priorities, targetOf) {
  let candidates = row.routes.filter((r) => r.state === 'clear');
  if (candidates.length === 0) return new Set();
  if (row.isAlias && targetOf) {
    const firstTarget = Math.min(...candidates.map((r) => targetOf.get(r.index) ?? 0));
    candidates = candidates.filter((r) => (targetOf.get(r.index) ?? 0) === firstTarget);
  }
  const of = (r) => priorities?.get(r.index) ?? 0;
  const top = Math.max(...candidates.map(of));
  return new Set(candidates.filter((r) => of(r) === top).map((r) => r.index));
}

/**
 * `name` without the version at its end, or null when it carries none:
 * `-YYYY-MM-DD`, `-YYYYMMDD`, `-NNN` or `-latest`. Mirrors
 * strip_version_suffix in crates/scheduler/src/catalog.rs.
 */
export function stripVersionSuffix(name) {
  const text = String(name ?? '');
  for (const pattern of [/^(.+)-\d{4}-\d{2}-\d{2}$/, /^(.+)-\d{8}$/, /^(.+)-\d{3}$/, /^(.+)-latest$/i]) {
    const hit = pattern.exec(text);
    if (hit) return hit[1];
  }
  return null;
}

/** Lower-cased names, and the newest dated snapshot of each undated name. */
function indexNames(entries) {
  const lower = new Map();
  const undated = new Map();
  for (const [name, value, isModel] of entries) {
    const key = name.toLowerCase();
    // An alias beats a model whose name differs only in case.
    if (!lower.has(key) || !isModel) lower.set(key, value);
    if (!isModel) continue; // only models have snapshots; an alias is not one
    const base = stripVersionSuffix(name)?.toLowerCase();
    if (base && (!undated.has(base) || undated.get(base).key < key)) undated.set(base, { key, value });
  }
  return { lower, undated };
}

/** The gateway's name fallbacks over an index: ignoring case, newest snapshot, `-latest`. */
function findFloating(index, name) {
  const lower = name.toLowerCase();
  const direct = (key) => index.lower.get(key) ?? index.undated.get(key)?.value;
  const found = direct(lower);
  if (found !== undefined) return found;
  return lower.length > 7 && lower.endsWith('-latest') ? direct(lower.slice(0, -7)) : undefined;
}

const rowIndexes = new WeakMap();
const nameIndexes = new WeakMap();

/**
 * Find a model entry the way the gateway does (find_name in
 * crates/scheduler/src/registry.rs): the exact name, then ignoring case,
 * then as a floating name: an undated name finds its newest dated snapshot
 * (`claude-sonnet-4-5` finds `claude-sonnet-4-5-20250929`), and
 * `name-latest` finds `name` or its newest snapshot.
 */
export function lookup(rowsByName, name) {
  const text = String(name ?? '').trim();
  if (!text) return null;
  if (rowsByName.has(text)) return rowsByName.get(text);
  let index = rowIndexes.get(rowsByName);
  if (!index) {
    index = indexNames([...rowsByName].map(([key, row]) => [key, row, !row.isAlias]));
    rowIndexes.set(rowsByName, index);
  }
  return findFloating(index, text) ?? null;
}

/** Whether a Set of lower-cased model names holds `name`, with the same fallbacks as lookup. */
export function servesName(names, name) {
  const text = String(name ?? '').trim();
  if (!text || !names) return false;
  if (names.has(text.toLowerCase())) return true;
  let index = nameIndexes.get(names);
  if (!index) {
    index = indexNames([...names].map((key) => [key, true, true]));
    nameIndexes.set(names, index);
  }
  return findFloating(index, text) === true;
}

/**
 * For an alias row: which target each of its routes comes from.
 * Returns Map(route index -> target index). Routes that cannot be attributed
 * are left out.
 */
export function routesByTarget(row, rowsByName) {
  const map = new Map();
  if (!row.isAlias) return map;
  const claimed = new Set();
  let self = -1;
  row.aliasTargets.forEach((target, targetIndex) => {
    const entry = lookup(rowsByName, parseSuffix(target).base) ?? lookup(rowsByName, target);
    if (!entry) return;
    if (entry.name === row.name) {
      // `gpt-5 -> gpt-5(high)`: the target is the provider's model behind the
      // alias, which the table no longer lists under its own name.
      if (self === -1) self = targetIndex;
      return;
    }
    for (const theirs of entry.routes) {
      const mine = row.routes.find((r) => !claimed.has(r.index) && r.provider === theirs.provider && r.upstream_model === theirs.upstream_model);
      if (mine) {
        claimed.add(mine.index);
        map.set(mine.index, targetIndex);
      }
    }
  });
  // What no other target accounts for comes from that model.
  if (self !== -1) for (const route of row.routes) if (!claimed.has(route.index)) map.set(route.index, self);
  return map;
}

/**
 * True when a provider's own model stands behind an alias of the same name
 * (`gpt-5 -> gpt-5(high)` pins a depth on the real gpt-5). The gateway does
 * not say so anywhere, so it is read off the routes: the alias targets its
 * own name and has routes that no other target explains.
 */
export function pinsOwnModel(row, rowsByName) {
  if (!row?.isAlias || row.routes.length === 0) return false;
  const lower = row.name.toLowerCase();
  const self = row.aliasTargets.findIndex((target) => parseSuffix(target).base.trim().toLowerCase() === lower);
  if (self === -1) return false;
  const targets = routesByTarget(row, rowsByName);
  return row.routes.some((route) => targets.get(route.index) === self);
}

// ---------------------------------------------------------------------------
// Calling a model
// ---------------------------------------------------------------------------

const WILDCARD_HOSTS = new Set(['0.0.0.0', '::', '[::]', '']);

/**
 * The base URL to put in copy-ready commands: the address the gateway
 * listens on (GET /status `listen`). A wildcard bind address cannot be
 * called, so the host the dashboard was opened on stands in for it. When
 * the dashboard is on the listener itself the scheme is the page's;
 * otherwise something sits in between and plain http to the listener is the
 * only thing known.
 *
 * `loc` is window.location (protocol, hostname, port, origin).
 */
export function gatewayBase(listen, loc) {
  const match = /^(.*):(\d+)$/.exec(listen ?? '');
  if (!match) return loc?.origin ?? 'http://127.0.0.1:8317';
  let host = match[1];
  const port = match[2];
  if (WILDCARD_HOSTS.has(host)) host = loc?.hostname?.includes(':') ? `[${loc.hostname}]` : (loc?.hostname ?? '127.0.0.1');
  const pagePort = loc?.port || (loc?.protocol === 'https:' ? '443' : '80');
  const scheme = loc && pagePort === port ? loc.protocol : 'http:';
  return `${scheme}//${host}:${port}`;
}

/** Quote for a POSIX shell: wrap in single quotes, escape the ones inside. */
const sh = (text) => `'${String(text).replace(/'/g, `'\\''`)}'`;

export const PROTOCOLS = [
  { id: 'chat', label: 'Chat Completions', short: 'Chat' },
  { id: 'responses', label: 'Responses', short: 'Responses' },
  { id: 'messages', label: 'Messages', short: 'Messages' },
  { id: 'gemini', label: 'Gemini', short: 'Gemini' },
];

/** The environment variable the commands read the client key from. */
export const KEY_PLACEHOLDER = '$SWITCHYARD_KEY';

/**
 * A curl command that calls `model` in one protocol.
 *
 *   curlFor('chat', { base, model: 'gpt-4o', stream: false, auth: true })
 *
 * The key is never written into the command: it is read from
 * $SWITCHYARD_KEY, so the command can be pasted into a ticket.
 */
export function curlFor(protocol, { base, model, stream = false, auth = true }) {
  const lines = [];
  const header = (name, value) => lines.push(`  -H "${name}: ${value}"`);
  let url;
  let body;
  const hello = 'Say hello in one sentence.';
  switch (protocol) {
    case 'responses':
      url = `${base}/v1/responses`;
      if (auth) header('Authorization', `Bearer ${KEY_PLACEHOLDER}`);
      body = { model, input: hello, ...(stream ? { stream: true } : {}) };
      break;
    case 'messages':
      url = `${base}/v1/messages`;
      if (auth) header('x-api-key', KEY_PLACEHOLDER);
      header('anthropic-version', '2023-06-01');
      body = { model, max_tokens: 1024, messages: [{ role: 'user', content: hello }], ...(stream ? { stream: true } : {}) };
      break;
    case 'gemini': {
      // The model is part of the path; "/" in a prefixed name stays a "/".
      const path = encodeURIComponent(model).replace(/%2F/gi, '/');
      url = `${base}/v1beta/models/${path}:${stream ? 'streamGenerateContent?alt=sse' : 'generateContent'}`;
      if (auth) header('x-goog-api-key', KEY_PLACEHOLDER);
      body = { contents: [{ role: 'user', parts: [{ text: hello }] }] };
      break;
    }
    default:
      url = `${base}/v1/chat/completions`;
      if (auth) header('Authorization', `Bearer ${KEY_PLACEHOLDER}`);
      body = { model, messages: [{ role: 'user', content: hello }], ...(stream ? { stream: true } : {}) };
  }
  header('Content-Type', 'application/json');
  return [`curl ${stream ? '-N ' : ''}${sh(url)} \\`, ...lines.map((line) => `${line} \\`), `  -d ${sh(JSON.stringify(body))}`].join('\n');
}

// ---------------------------------------------------------------------------
// Alias drafts
// ---------------------------------------------------------------------------

let draftSeq = 0;
const nextKey = (prefix) => `${prefix}${(draftSeq += 1)}`;

/** A target row of the editor: a stable id so reordering keeps focus and state. */
export const newTarget = (value = '') => ({ id: nextKey('t'), value });

/** An alias row of the editor. `origin` is its saved name, null when new. */
export const newAlias = () => ({ key: nextKey('a'), origin: null, name: '', hide_targets: false, targets: [newTarget()] });

/**
 * The saved list (GET /aliases) as editor rows. Rows and targets that were
 * already in the editor (`previous`) keep their keys, so a field the user is
 * in stays mounted, and focused, when the list is taken over after a save.
 */
export function draftFromServer(list, previous = null) {
  const pool = [...(previous ?? [])];
  const takeRow = (name) => {
    const wanted = String(name ?? '').trim();
    let at = pool.findIndex((row) => row.origin === name);
    if (at === -1) at = pool.findIndex((row) => row.name.trim() === wanted);
    return at === -1 ? null : pool.splice(at, 1)[0];
  };
  return (list ?? []).map((alias) => {
    const old = takeRow(alias.name);
    const ids = [...(old?.targets ?? [])];
    return {
      key: old?.key ?? nextKey('a'),
      origin: alias.name,
      name: alias.name,
      hide_targets: !!alias.hide_targets,
      targets: (alias.targets ?? []).map((value) => {
        const at = ids.findIndex((t) => t.value.trim() === String(value).trim());
        return at === -1 ? newTarget(value) : { id: ids.splice(at, 1)[0].id, value };
      }),
    };
  });
}

/** Editor rows as the body of PUT /aliases. Blank target rows are left out. */
export function draftToBody(rows) {
  return rows.map((row) => ({
    name: row.name.trim(),
    targets: row.targets.map((t) => t.value.trim()).filter(Boolean),
    hide_targets: !!row.hide_targets,
  }));
}

/**
 * Same aliases, same order, same targets: nothing to save. Both sides are
 * read the way the gateway reads them (names and targets trimmed, blank
 * targets dropped): it stores what it was sent, so a list written by hand
 * may carry padding that the editor would otherwise report as a change.
 */
export function sameAliases(body, saved) {
  const norm = (list) =>
    JSON.stringify(
      (list ?? []).map((a) => ({
        name: String(a.name ?? '').trim(),
        targets: (a.targets ?? []).map((t) => String(t).trim()).filter(Boolean),
        hide_targets: !!a.hide_targets,
      })),
    );
  return norm(body) === norm(saved);
}

/**
 * What is worth saying about a draft alias before it is saved. These are
 * warnings, not errors: the gateway accepts all of them (and reports the
 * real errors itself, see the 422 mapping in aliases.js). They follow what
 * the gateway does with the list (build_aliases and resolve in
 * crates/scheduler/src/registry.rs).
 *
 * rows        every draft row
 * rowsByName  Map(name -> table row) of the saved model table
 * realNames   Set of client-facing names providers serve, lower-cased: the
 *             names an alias can shadow
 * ready       false while the model table is not loaded: nothing can be
 *             said about what a name matches, so only the checks that need
 *             the draft alone are made
 *
 * Returns { name: string | null, targets: Map(target id -> string) }.
 */
export function aliasWarnings(row, rows, rowsByName, realNames, { ready = true } = {}) {
  const name = row.name.trim();
  const lower = name.toLowerCase();
  const out = { name: null, targets: new Map() };

  const others = rows.filter((other) => other !== row);
  const draftNames = new Set(others.map((other) => other.name.trim().toLowerCase()).filter(Boolean));
  // What a name matches once this draft is saved: an alias of the draft, a
  // model of the table, or a model a provider serves behind an alias. A
  // saved alias that is renamed or removed in this draft no longer counts.
  const find = (text) => {
    const entry = lookup(rowsByName, text);
    if (entry && !entry.isAlias) return { entry };
    if (draftNames.has(String(text).trim().toLowerCase())) return { entry: null };
    if (servesName(realNames, text)) return { entry: null };
    return null;
  };
  // `gpt-5 -> gpt-5(high)`: the alias pins a depth on the model of its own name.
  const pinsSelf = (value) => {
    const parsed = parseSuffix(value);
    return parsed.raw !== null && parsed.base.trim().toLowerCase() === lower;
  };
  const ownModel = !!name && realNames.has(lower);
  // The pin also reaches a model the name floats to (its newest snapshot).
  const pinnable = !!name && servesName(realNames, name);

  if (name) {
    const parsedName = parseSuffix(name);
    if (draftNames.has(lower)) {
      out.name = 'Another alias already has this name. The gateway refuses duplicates.';
    } else if (ready && ownModel) {
      out.name = row.targets.some((t) => pinsSelf(t.value.trim()))
        ? `A provider serves a model named ${name}. Requests for ${name} come to this alias first, which passes them on to that model with the depth set here.`
        : `A provider already serves a model named ${name}. The alias takes its place: requests for ${name} go to these targets instead.`;
    } else if (ready && parsedName.raw !== null && find(parsedName.base)) {
      // The gateway tries the name without the parentheses first.
      out.name = `Clients cannot reach this alias: ${name} is read as ${parsedName.base.trim()} with a reasoning suffix.`;
    }
  }

  for (const target of row.targets) {
    const value = target.value.trim();
    if (!value) continue;
    // The one case the gateway refuses outright (422).
    if (name && value.toLowerCase() === lower) {
      out.targets.set(target.id, 'An alias cannot target itself.');
      continue;
    }
    if (!ready) continue;
    const { base, raw, depth } = parseSuffix(value);
    if (name && pinsSelf(value)) {
      if (!pinnable) out.targets.set(target.id, `No provider serves a model named ${base.trim()}, so this target leads back to the alias itself. The gateway ignores it.`);
      else if (depth === null) out.targets.set(target.id, `(${raw}) is not a reasoning depth, so it is ignored. Use a level such as (high), a token budget such as (8192), (none) or (auto).`);
      continue;
    }
    // The name without its suffix; failing that, the parentheses may be part of a model id.
    const found = find(base) ?? (raw !== null ? find(value) : null);
    if (!found) {
      out.targets.set(target.id, `No model is named ${raw !== null ? base.trim() : value}. The gateway skips this target.`);
      continue;
    }
    if (raw === null || !find(base)) continue;
    if (depth === null) {
      out.targets.set(target.id, `(${raw}) is not a reasoning depth, so it is ignored. Use a level such as (high), a token budget such as (8192), (none) or (auto).`);
    } else if (found.entry && found.entry.info?.known !== false && !found.entry.info?.thinking) {
      out.targets.set(target.id, `${found.entry.name} does not reason, so the suffix has no effect.`);
    }
  }
  return out;
}

export default buildRows;
