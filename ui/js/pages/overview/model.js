// Overview: the logic behind the page, free of Preact and the DOM so it can be
// read (and run under Node) on its own.
//
//   vitals      one shape for the "stats" live frame and the polling fallback
//   history     a few minutes of vitals, kept in memory, for the sparklines
//   credentials what a credential's lamp says right now
//   providers   one row of the health board
//   verdict     the one-line answer to "is the gateway healthy?"
//   series      /usage/timeseries extended with requests that finished since
//   attempts    upstream attempts per provider, from request records
//   feed        the live activity list
//   warnings    where a configuration warning is fixed
//   first run   what is still missing, and a curl line that works
//
// All times are the gateway's clock (unix ms). Callers pass `now` already
// corrected for the difference between the two clocks.

import { formatCountdown, formatNumber, formatTime, plural } from '../../lib/format.js';

// ---------------------------------------------------------------------------
// Vitals
// ---------------------------------------------------------------------------

/** The `stats` live frame as the page reads it. */
export function vitalsFromStats(data) {
  if (!data) return null;
  return {
    source: 'live',
    at: data.at,
    rpm: data.rpm,
    tpm: data.tpm,
    errorRate: data.error_rate_1m,
    errorWindow: 'minute',
    p50: data.p50_ms,
    p95: data.p95_ms,
    // How many requests the percentiles were taken over (null: a gateway
    // that does not say).
    samples: data.latency_samples ?? null,
    inFlight: data.in_flight,
    streams: data.active_streams,
    sockets: data.ws_connections,
    totals: data.totals ?? null,
  };
}

/**
 * The same numbers without the live connection: gauges and totals from
 * /status, rates and percentiles from /usage/summary?range=1h. The error rate
 * then covers the last hour, not the last minute; `errorWindow` says which.
 */
export function vitalsFromPoll(status, summary) {
  if (!status) return null;
  const live = status.live ?? {};
  const started = live.started_at ?? status.started_at;
  const uptime = live.uptime_ms ?? status.uptime_ms;
  return {
    source: 'poll',
    at: started != null && uptime != null ? started + uptime : null,
    rpm: summary?.requests_per_minute ?? null,
    tpm: summary?.tokens_per_minute ?? null,
    errorRate: summary?.error_rate ?? null,
    errorWindow: 'hour',
    p50: summary?.latency?.p50 ?? null,
    p95: summary?.latency?.p95 ?? null,
    samples: summary?.latency?.samples ?? null,
    inFlight: live.in_flight ?? null,
    streams: live.active_streams ?? null,
    sockets: live.ws_connections ?? null,
    totals: live.totals ?? null,
  };
}

/**
 * True when the latency percentiles describe nothing: no request finished in
 * the hour they cover. The gateway then reports 0 for both, which is "no
 * data", not "0ms". It says so with a sample count of 0; a gateway that
 * sends no count is taken at its zeros.
 */
export function latencyUnknown(vitals) {
  if (!vitals) return true;
  if (vitals.samples != null) return vitals.samples === 0;
  return !vitals.p50 && !vitals.p95;
}

/** Lamp for an error rate: amber from 5%, red from 25%. */
export function errorRateTone(rate) {
  if (typeof rate !== 'number' || !Number.isFinite(rate)) return null;
  if (rate >= 0.25) return 'stop';
  if (rate >= 0.05) return 'caution';
  return null;
}

/**
 * What that lamp says, in words: its name and its tooltip. `span` is what
 * the rate covers, "minute" or "hour".
 */
export function errorRateWords(rate, span = 'minute') {
  const tone = errorRateTone(rate);
  if (!tone) return null;
  return `${tone === 'stop' ? 'High: 25%' : 'Elevated: 5%'} or more of the requests in the last ${span} failed`;
}

// ---------------------------------------------------------------------------
// History for the sparklines
// ---------------------------------------------------------------------------

/** How much history the sparklines show at most. */
export const HISTORY_MS = 5 * 60_000;
/** One point of a sparkline covers this long. */
export const SLOT_MS = 5_000;
// A page that has just been opened has seconds of history, not minutes. The
// window starts at one minute and grows with the history, so the first line
// is not a speck at the right edge of an empty five minutes.
const MIN_WINDOW_MS = 60_000;

/** Append a sample ({ at, ...numbers }) and forget what is older than the window. */
export function pushHistory(list, sample) {
  if (!sample || typeof sample.at !== 'number') return list;
  const last = list[list.length - 1];
  // A clock that went backwards (the gateway restarted on another machine's
  // time, a laptop woke up): the old samples no longer line up.
  if (last && sample.at < last.at - SLOT_MS) list.length = 0;
  if (last && sample.at === last.at) return list;
  list.push(sample);
  const cutoff = sample.at - HISTORY_MS - SLOT_MS;
  while (list.length > 0 && list[0].at < cutoff) list.shift();
  return list;
}

/**
 * One field of the history as fixed time slots ending at `now`, oldest first.
 * A slot without a sample is null, so a pause in the data is a gap in time,
 * not a compressed line. `mode`: "last" for rolling figures (requests per
 * minute), "max" for gauges that spike between samples (in flight).
 */
export function historySeries(list, field, now, mode = 'last') {
  const end = Math.ceil(now / SLOT_MS) * SLOT_MS;
  const oldest = list.length > 0 ? Math.floor(list[0].at / SLOT_MS) * SLOT_MS : end;
  const span = Math.min(HISTORY_MS, Math.max(MIN_WINDOW_MS, end - oldest));
  const slots = Math.round(span / SLOT_MS);
  const start = end - span;
  const out = new Array(slots).fill(null);
  for (const sample of list) {
    const value = sample[field];
    if (typeof value !== 'number' || !Number.isFinite(value)) continue;
    const slot = Math.floor((sample.at - start) / SLOT_MS);
    if (slot < 0 || slot >= slots) continue;
    out[slot] = mode === 'max' && out[slot] != null ? Math.max(out[slot], value) : value;
  }
  return out;
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

const REASONS = {
  rate_limit: 'rate limited',
  quota: 'quota used up',
  auth: 'authentication failed',
  server: 'upstream error',
  transport: 'connection failed',
  model_not_found: 'model not found',
  request: 'request rejected',
};

/** A cooldown cause in plain words. Unknown causes are shown as sent. */
export function reasonText(reason) {
  if (!reason) return null;
  return REASONS[reason] ?? String(reason).replace(/_/g, ' ');
}

/**
 * What one credential's lamp shows at `now`.
 * kind: "ready" | "cooling" | "disabled" | "unusable" | "unknown".
 * A cooldown that has run out counts as ready even before the gateway says
 * so: it announces the start of a cooldown, not its end.
 *
 * "disabled" is the gateway's word and wins over everything else. `by` says
 * what switched the credential off: "provider" (its provider is disabled, so
 * all of the provider's credentials read disabled), "credential" or
 * "runtime" (its own switch).
 */
export function credentialState(cred, now) {
  const resting = (cred.model_cooldowns ?? []).filter((m) => typeof m.until === 'number' && m.until > now);
  const base = { id: cred.id, name: cred.label || cred.id, until: null, reason: null, resting };
  if (cred.disabled || cred.status === 'disabled') return { ...base, kind: 'disabled', tone: 'off', label: 'Disabled', by: cred.disabled_by ?? null };
  if (cred.usable === false || cred.status === 'unusable') {
    return { ...base, kind: 'unusable', tone: 'stop', label: 'Unusable', reason: cred.unusable_reason || null };
  }
  if (cred.status === 'cooling') {
    // "cooling" is either the whole credential, or every model on it: then it
    // is back when the first model is.
    const until = cred.cooldown_until ?? (resting.length > 0 ? Math.min(...resting.map((m) => m.until)) : null);
    const stillCooling = until == null ? cred.cooldown_until == null && (cred.model_cooldowns ?? []).length === 0 : until > now;
    if (stillCooling) {
      return { ...base, kind: 'cooling', tone: 'caution', label: 'Cooling down', until, reason: reasonText(cred.cooldown_reason ?? resting[0]?.reason) };
    }
    return { ...base, kind: 'ready', tone: 'clear', label: 'Ready' };
  }
  if (cred.status === 'ready') return { ...base, kind: 'ready', tone: 'clear', label: 'Ready' };
  return { ...base, kind: 'unknown', tone: 'off', label: 'Not seen yet' };
}

/** One credential's state in words: "cooling down, back in 0:44 (rate limited)". */
export function credentialDetail(state, now) {
  let text = state.label.toLowerCase();
  if (state.kind === 'disabled' && state.by === 'provider') {
    text = 'provider disabled';
  } else if (state.kind === 'cooling') {
    if (state.until != null) text += `, back in ${formatCountdown((state.until - now) / 1000)}`;
    if (state.reason) text += ` (${state.reason})`;
  } else if (state.kind === 'unusable' && state.reason) {
    text += `: ${state.reason}`;
  } else if (state.kind === 'ready' && state.resting.length > 0) {
    text += `, ${plural(state.resting.length, 'model')} resting`;
  }
  return text;
}

/** The words that go with a credential lamp (its title). */
export function credentialTitle(state, now) {
  return `${state.name}: ${credentialDetail(state, now)}`;
}

/**
 * The credentials of a provider that are worth naming one by one: those
 * cooling down (the one back first comes first), then those that cannot be
 * used. A provider with a single credential says all of it in its sentence.
 */
export function troubledCredentials(credentials) {
  if (credentials.length < 2) return [];
  const cooling = credentials.filter((c) => c.kind === 'cooling').sort((a, b) => (a.until ?? Infinity) - (b.until ?? Infinity));
  return [...cooling, ...credentials.filter((c) => c.kind === 'unusable')];
}

// ---------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------

// Board order. Something that is failing right now (a cooldown) comes before
// something that is configured wrong and has been all along.
function rankOf(tone, counts) {
  const incident = counts.cooling > 0;
  if (tone === 'stop') return incident ? 0 : 2;
  if (tone === 'caution') return incident ? 1 : 3;
  return tone === 'clear' ? 4 : 5;
}

/** One provider of GET /providers as a row of the health board. */
export function summarizeProvider(provider, now) {
  const credentials = (provider.credentials ?? []).map((cred) => credentialState(cred, now));
  const counts = { total: credentials.length, ready: 0, cooling: 0, disabled: 0, unusable: 0, unknown: 0 };
  for (const state of credentials) counts[state.kind] += 1;

  const enabled = provider.enabled !== false;
  let tone;
  let label;
  if (!enabled) [tone, label] = ['off', 'Disabled'];
  else if (counts.total === 0) [tone, label] = ['stop', 'No credentials'];
  else if (counts.ready > 0) [tone, label] = counts.cooling + counts.unusable > 0 ? ['caution', 'Degraded'] : ['clear', 'Serving'];
  else if (counts.cooling + counts.unusable + counts.unknown === 0) [tone, label] = ['off', 'Disabled'];
  else [tone, label] = ['stop', 'Not serving'];

  // Mean response latency, weighted by how much each credential has served.
  let weight = 0;
  let weighted = 0;
  let requests = 0;
  let failures = 0;
  for (const cred of provider.credentials ?? []) {
    requests += cred.requests || 0;
    failures += cred.failures || 0;
    if (typeof cred.latency_ms !== 'number') continue;
    const w = Math.max(1, cred.successes || 0);
    weight += w;
    weighted += cred.latency_ms * w;
  }

  // The cooldown that ends first, and models resting on credentials that
  // otherwise serve.
  const cooling = credentials.filter((c) => c.kind === 'cooling');
  const next = cooling.filter((c) => c.until != null).sort((a, b) => a.until - b.until)[0] ?? cooling[0] ?? null;
  const restingModels = new Set();
  for (const cred of credentials) if (cred.kind === 'ready') for (const m of cred.resting) restingModels.add(m.model);

  // When something on this provider changes by itself (a cooldown ends).
  let nextChange = null;
  for (const cred of credentials) {
    if (cred.kind === 'cooling' && cred.until != null) nextChange = Math.min(nextChange ?? Infinity, cred.until);
    for (const m of cred.resting) nextChange = Math.min(nextChange ?? Infinity, m.until);
  }

  const summary = {
    name: provider.name,
    kind: provider.kind,
    enabled,
    tone,
    label,
    rank: rankOf(tone, counts),
    counts,
    credentials,
    trouble: troubledCredentials(credentials),
    next,
    resting: restingModels.size,
    nextChange,
    latency: weight > 0 ? weighted / weight : null,
    requests,
    failures,
  };
  summary.text = describeProvider(summary, now);
  return summary;
}

/** The sentence next to a provider's lamps. */
export function describeProvider(summary, now) {
  const { counts, credentials, next } = summary;
  if (!summary.enabled) return 'Provider disabled';
  if (counts.total === 0) return 'No credentials';
  const countdown = (state) => (state?.until != null ? formatCountdown((state.until - now) / 1000) : null);
  const parts = [];
  if (counts.total === 1) {
    const only = credentials[0];
    if (only.kind === 'cooling') {
      const left = countdown(only);
      parts.push(`Cooling down${left ? `, back in ${left}` : ''}${only.reason ? ` (${only.reason})` : ''}`);
    } else if (only.kind === 'unusable') {
      parts.push(`Unusable${only.reason ? `: ${only.reason}` : ''}`);
    } else if (only.kind === 'disabled') {
      parts.push('Credential disabled');
    } else {
      parts.push(only.label);
    }
  } else {
    parts.push(`${counts.ready} of ${counts.total} ready`);
    // The counts only: each credential that is cooling down or unusable is
    // named on a line of its own below (see troubledCredentials), with its
    // own countdown and reason.
    if (counts.cooling > 0) parts.push(`${counts.cooling} cooling down`);
    if (counts.unusable > 0) parts.push(`${counts.unusable} unusable`);
    if (counts.disabled > 0) parts.push(`${counts.disabled} disabled`);
    if (counts.unknown > 0) parts.push(`${counts.unknown} not seen yet`);
  }
  if (summary.resting > 0) parts.push(`${plural(summary.resting, 'model')} resting`);
  return parts.join(' · ');
}

/** Board order: what hurts first, then the order of the configuration. */
export function sortProviders(summaries) {
  return summaries.map((s, index) => ({ s, index })).sort((a, b) => a.s.rank - b.s.rank || a.index - b.index).map((entry) => entry.s);
}

/** The earliest moment a cooldown on any provider ends, or null. */
export function nextCooldownEnd(summaries) {
  let next = null;
  for (const s of summaries ?? []) if (s.nextChange != null) next = Math.min(next ?? Infinity, s.nextChange);
  return next;
}

// ---------------------------------------------------------------------------
// Verdict
// ---------------------------------------------------------------------------

/**
 * Is the gateway healthy? From the provider rows. The counts in /status stand
 * in only when the provider list could not be loaded (`providersFailed`), and
 * then without figures: like the rows they leave out the credentials of
 * disabled providers, but they count a credential that is switched off by
 * itself, which the rows do not, and one strip must not show two different
 * numbers. While the provider list is still loading the answer is null (a
 * skeleton).
 *
 * Returns { tone, label, detail, refused }. `refused` is true while the
 * gateway refuses the configuration file on disk (`status.config_rejected`):
 * it keeps serving the last valid configuration, so a healthy gateway is not
 * just "Serving" then but a caution, and the strip says the file was refused
 * and links to the raw file. A worse verdict (degraded, not serving) keeps
 * its own words and tone, with the same note.
 */
export function gatewayVerdict(summaries, status, providersFailed = false) {
  const verdict = credentialVerdict(summaries, status, providersFailed);
  if (!verdict || !status?.config_rejected) return verdict ? { ...verdict, refused: false } : null;
  if (verdict.tone === 'clear') return { ...verdict, tone: 'caution', label: 'Serving the last valid configuration', refused: true };
  return { ...verdict, refused: true };
}

function credentialVerdict(summaries, status, providersFailed) {
  if (!summaries) {
    const counts = status?.counts;
    if (!counts || !providersFailed) return null;
    if (counts.providers === 0) return { tone: 'off', label: 'No providers', detail: 'Nothing to route requests to yet' };
    if (counts.credentials_ready === 0) return { tone: 'stop', label: 'Not serving', detail: 'No credential is ready' };
    if (counts.credentials_ready < counts.credentials) return { tone: 'caution', label: 'Degraded', detail: 'Some credentials are not ready' };
    return { tone: 'clear', label: 'Serving', detail: 'Credentials are ready' };
  }
  if (summaries.length === 0) return { tone: 'off', label: 'No providers', detail: 'Nothing to route requests to yet' };
  // The credentials of a disabled provider all read "disabled": they are out
  // of the figures together with those that are switched off one by one.
  const sum = { total: 0, ready: 0, cooling: 0, unusable: 0, disabled: 0, unknown: 0 };
  for (const s of summaries) for (const key of Object.keys(sum)) sum[key] += s.counts[key];
  const active = sum.total - sum.disabled;
  if (sum.ready === 0) {
    if (sum.cooling > 0) return { tone: 'stop', label: 'Not serving', detail: sum.cooling === 1 ? 'The only usable credential is cooling down' : `All ${formatNumber(sum.cooling)} usable credentials are cooling down` };
    return { tone: 'stop', label: 'Not serving', detail: 'No credential is ready' };
  }
  if (sum.cooling + sum.unusable > 0) {
    const what = [sum.cooling > 0 && `${formatNumber(sum.cooling)} cooling down`, sum.unusable > 0 && `${formatNumber(sum.unusable)} unusable`].filter(Boolean).join(', ');
    return { tone: 'caution', label: 'Degraded', detail: `${formatNumber(sum.ready)} of ${formatNumber(active)} credentials ready, ${what}` };
  }
  return { tone: 'clear', label: 'Serving', detail: active === 1 ? '1 credential ready' : `All ${formatNumber(active)} credentials ready` };
}

// ---------------------------------------------------------------------------
// Time series, extended live
// ---------------------------------------------------------------------------

// A request that finished after the series was computed is added to its
// bucket here, so the chart moves between refetches. `tail` holds those
// requests: [{ at, ok }]. Anything not newer than `series.to` is
// already in the series.
function extend(series, tail, visit) {
  const points = series?.points ?? [];
  if (points.length === 0) return 0;
  const size = series.bucket_ms || 60_000;
  const first = points[0].t;
  let length = points.length;
  const limit = points.length + 180;
  for (const entry of tail ?? []) {
    if (entry.at <= series.to) continue;
    const index = Math.floor((entry.at - first) / size);
    if (index < 0 || index >= limit) continue;
    if (index >= length) length = index + 1;
    visit(index, entry);
  }
  return length;
}

/** Stacked outcome per bucket for the traffic chart. */
export function trafficSeries(series, tail) {
  const points = series?.points ?? [];
  if (points.length === 0) return { x: [], ok: [], failed: [], requests: 0, errors: 0 };
  const size = series.bucket_ms || 60_000;
  const ok = points.map((p) => Math.max(0, (p.requests || 0) - (p.errors || 0)));
  const failed = points.map((p) => p.errors || 0);
  const length = extend(series, tail, (index, entry) => {
    while (ok.length <= index) {
      ok.push(0);
      failed.push(0);
    }
    if (entry.ok) ok[index] += 1;
    else failed[index] += 1;
  });
  while (ok.length < length) {
    ok.push(0);
    failed.push(0);
  }
  // The window keeps its width: a new bucket on the right drops one on the left.
  const drop = ok.length - points.length;
  const x = ok.map((_, i) => points[0].t + i * size).slice(drop);
  const okOut = ok.slice(drop);
  const failedOut = failed.slice(drop);
  const sum = (list) => list.reduce((total, v) => total + v, 0);
  const errors = sum(failedOut);
  return { x, ok: okOut, failed: failedOut, requests: sum(okOut) + errors, errors };
}

/** Forget tail entries every loaded series already contains. */
export function pruneTail(tail, loadedUpTo, cap = 20_000) {
  let out = tail;
  if (loadedUpTo != null) out = out.filter((entry) => entry.at > loadedUpTo);
  if (out.length > cap) out = out.slice(out.length - cap);
  return out.length === tail.length ? tail : out;
}

// ---------------------------------------------------------------------------
// Upstream attempts per provider
// ---------------------------------------------------------------------------

// A request record names one provider: the one of its last attempt. A
// provider that failed and was failed over is not in that figure at all, and
// the provider tried last answers for the whole chain. The board therefore
// counts what a record lists under `attempts`: every upstream call, with the
// provider it went to and how it ended.
//
// The log is filled from GET /requests (a page of the most recent records)
// and from "request.finished" frames. `from` is the start time back to which
// it is known to be complete: -Infinity when the gateway had nothing older,
// else the start of the oldest record loaded. `to` is when it was last known
// to be current, so a later load can tell whether it joins up with what is
// already here or leaves a gap.

/** How far back the board looks. */
export const ATTEMPT_WINDOW_MS = 3_600_000;
const ATTEMPT_CAP = 20_000;
// The upstream refused the request itself (malformed, too large): the
// client's fault, not a sign of the provider's health. The gateway leaves
// these out of a credential's failures as well.
const REQUEST_FAULT = new Set([400, 413, 422]);
// Widths a block of the strip can have. Twenty blocks of the widest are an hour.
const SLOT_STEPS_MS = [1_000, 2_000, 5_000, 10_000, 15_000, 30_000, 60_000, 120_000, 180_000];

export function createAttemptLog() {
  return { byId: new Map(), from: null, to: null };
}

function compactAttempts(record) {
  const at = record?.started_at ?? record?.finished_at;
  if (record?.id == null || typeof at !== 'number') return null;
  const list = [];
  for (const attempt of record.attempts ?? []) {
    if (!attempt?.provider) continue;
    if (attempt.ok !== true && REQUEST_FAULT.has(attempt.status)) continue;
    list.push({ provider: attempt.provider, ok: attempt.ok === true });
  }
  return { at, list };
}

/** Take in a page of GET /requests (newest first). `hasMore`: older records exist. */
export function mergeLoadedAttempts(log, items, hasMore, now) {
  let oldest = Infinity;
  for (const record of items ?? []) {
    const entry = compactAttempts(record);
    if (!entry) continue;
    oldest = Math.min(oldest, entry.at);
    if (entry.list.length > 0) log.byId.set(record.id, entry);
  }
  const reach = hasMore ? Math.min(oldest, now) : -Infinity;
  // Joined up with what was known: the older start stands. A gap: only the
  // new page is known to be complete.
  log.from = log.from != null && log.to != null && reach <= log.to ? Math.min(log.from, reach) : reach;
  log.to = Math.max(log.to ?? -Infinity, now);
  pruneAttempts(log, now);
  return log;
}

/** Take in one finished request from a live frame. */
export function mergeLiveAttempts(log, record, now) {
  const entry = compactAttempts(record);
  if (!entry) return log;
  if (entry.list.length > 0) log.byId.set(record.id, entry);
  if (log.from != null) log.to = Math.max(log.to ?? -Infinity, now);
  return log;
}

/** Forget what is older than the window, and the oldest beyond the cap. */
export function pruneAttempts(log, now) {
  const cutoff = now - ATTEMPT_WINDOW_MS - 180_000;
  for (const [id, entry] of log.byId) if (entry.at < cutoff) log.byId.delete(id);
  if (log.byId.size > ATTEMPT_CAP) {
    const byAge = [...log.byId].sort((a, b) => a[1].at - b[1].at);
    const drop = byAge.length - ATTEMPT_CAP;
    for (let i = 0; i < drop; i += 1) log.byId.delete(byAge[i][0]);
    if (log.from != null) log.from = Math.max(log.from, byAge[drop][1].at);
  }
  return log;
}

/**
 * Per provider: its upstream attempts in "slots" blocks for a HealthStrip,
 * and the totals behind them.
 *
 * known   false until a page of requests has been loaded
 * full    the window is the whole last hour; otherwise the log does not reach
 *         that far back (a busy gateway) and the window starts at "since"
 * since   start of the window (unix ms)
 * byName  Map(provider -> { buckets, attempts, failed, success })
 */
export function providerAttempts(log, now, slots = 20) {
  const result = { known: log.from != null, full: true, since: now - ATTEMPT_WINDOW_MS, byName: new Map() };
  if (!result.known) return result;
  const hourStart = now - ATTEMPT_WINDOW_MS;
  result.full = log.from <= hourStart;
  const from = result.full ? hourStart : log.from;
  result.since = from;
  const span = Math.max(1, now - from);
  const per = SLOT_STEPS_MS.find((step) => step * slots >= span) ?? SLOT_STEPS_MS[SLOT_STEPS_MS.length - 1];
  // Blocks sit on fixed boundaries, so a block does not change what it
  // covers from one second to the next.
  const end = Math.ceil(now / per) * per;
  const first = end - slots * per;
  const firstSlot = Math.max(0, Math.floor((from - first) / per));
  const stamp = per < 60_000 ? (t) => formatTime(t) : (t) => formatTime(t).slice(0, 5);

  const row = (name) => {
    let entry = result.byName.get(name);
    if (!entry) {
      entry = {
        buckets: Array.from({ length: slots - firstSlot }, (_, i) => {
          const start = first + (firstSlot + i) * per;
          return { ok: 0, failed: 0, label: stamp(start) + ' to ' + stamp(start + per) };
        }),
        attempts: 0,
        failed: 0,
        success: null,
      };
      result.byName.set(name, entry);
    }
    return entry;
  };
  for (const entry of log.byId.values()) {
    if (entry.at < from || entry.at >= end) continue;
    const slot = Math.floor((entry.at - first) / per) - firstSlot;
    if (slot < 0) continue;
    for (const attempt of entry.list) {
      const mine = row(attempt.provider);
      mine.attempts += 1;
      if (attempt.ok) mine.buckets[slot].ok += 1;
      else {
        mine.buckets[slot].failed += 1;
        mine.failed += 1;
      }
    }
  }
  for (const entry of result.byName.values()) entry.success = entry.attempts > 0 ? (entry.attempts - entry.failed) / entry.attempts : null;
  return result;
}

// ---------------------------------------------------------------------------
// Activity feed
// ---------------------------------------------------------------------------

/** Prompt, cache and output tokens of a request record. */
export function tokensOf(record) {
  const u = record?.usage;
  if (!u) return null;
  return (u.input_tokens || 0) + (u.cache_read_tokens || 0) + (u.cache_write_tokens || 0) + (u.output_tokens || 0);
}

/** A record that has started and not ended. */
export const isPending = (record) => record.finished_at == null && record.status == null;

/**
 * Apply live frames to the feed. `events`: [{ type: "started" | "finished",
 * data }]. Rows are kept newest first by start time, one per request id, at
 * most `cap`. A "finished" replaces its "started" in place, so a row does not
 * jump when the request ends.
 */
export function applyFeed(rows, events, cap = 15) {
  if (events.length === 0) return rows;
  const byId = new Map(rows.map((row) => [row.id, row]));
  for (const event of events) {
    const record = event.data;
    if (!record || record.id == null) continue;
    if (event.type === 'finished') byId.set(record.id, record);
    else if (!byId.has(record.id)) byId.set(record.id, record);
  }
  return [...byId.values()].sort((a, b) => (b.started_at || 0) - (a.started_at || 0) || (a.id < b.id ? 1 : -1)).slice(0, cap);
}

// ---------------------------------------------------------------------------
// Warnings
// ---------------------------------------------------------------------------

/** The page where a warning from /status is fixed. */
export function warningTarget(text) {
  // The file on disk was refused (config_rejected): it is fixed in the raw file.
  if (/^configuration file:/.test(text)) return { path: '/settings', query: { tab: 'raw' }, label: 'Open the raw file' };
  const provider = /^provider `([^`]+)`/.exec(text);
  if (provider) return { path: '/providers', query: { open: provider[1] }, label: 'Open providers' };
  if (/^alias\b/.test(text)) return { path: '/models', query: { tab: 'aliases' }, label: 'Open aliases' };
  if (/\bclient key\b|auth\.keys/.test(text)) return { path: '/keys', query: undefined, label: 'Open API keys' };
  return { path: '/settings', query: undefined, label: 'Open settings' };
}

// ---------------------------------------------------------------------------
// First run
// ---------------------------------------------------------------------------

/** What a new installation still lacks. `show` is false once nothing is. */
export function firstRun(status, providers) {
  if (!status || !providers) return { show: false };
  const real = providers.filter((p) => p.kind !== 'mock');
  const noProviders = providers.length === 0;
  const mockOnly = !noProviders && real.length === 0;
  const noKey = (status.counts?.client_keys ?? 0) === 0;
  const requests = status.live?.totals?.requests ?? 0;
  return {
    show: noProviders || mockOnly || noKey,
    noProviders,
    mockOnly,
    noKey,
    authRequired: status.auth_required !== false,
    hasTraffic: requests > 0,
    // A model the example can name: the mock's echo when it is there.
    model: pickModel(providers),
  };
}

function pickModel(providers) {
  const names = providers.filter((p) => p.enabled !== false).flatMap((p) => p.models ?? []);
  if (names.includes('mock-echo')) return 'mock-echo';
  return names[0] ?? null;
}

const WILDCARD_HOSTS = new Set(['0.0.0.0', '[::]', '[::0]', '::']);
const LOOPBACK_HOSTS = new Set(['127.0.0.1', 'localhost', '[::1]', '::1']);
const bare = (host) => String(host ?? '').toLowerCase();

/**
 * The base URL clients use, as { base, direct }.
 *
 * `listen` is what the gateway bound to and `tls` whether that listener
 * serves HTTPS (both from /status). When the dashboard was opened on that
 * very socket (`direct`), the listen address is the answer, with the scheme
 * the gateway says it speaks; a wildcard address is not one a client can
 * call, so the host the dashboard was opened on stands in for it.
 *
 * When the page's address is a different one (a TLS-terminating proxy, a
 * port mapping, a tunnel), the listen address is not known to be reachable
 * from where the operator sits. The page's own origin is: it has just served
 * this dashboard from the gateway. A page whose scheme is not the listener's
 * came through such a proxy, whatever its host and port.
 *
 * `tls` is not a boolean for a gateway that does not report it: the page's
 * scheme is then taken for the gateway's.
 */
export function clientBase(listen, pageLocation, tls) {
  const page = pageLocation?.host ? pageLocation : null;
  const origin = page ? `${page.protocol}//${page.host}` : null;
  const parts = /^(.*):(\d+)$/.exec(listen ?? '');
  // No address with a port to go by (the gateway did not report one).
  if (!parts) return { base: origin ?? 'http://127.0.0.1:8317', direct: false };
  if (!page) return { base: `${tls === true ? 'https' : 'http'}://${listen}`, direct: true };
  const [, host, port] = parts;
  const pageTls = page.protocol === 'https:';
  const pagePort = page.port || (pageTls ? '443' : '80');
  const wildcard = WILDCARD_HOSTS.has(bare(host));
  const sameHost = wildcard || bare(host) === bare(page.hostname) || (LOOPBACK_HOSTS.has(bare(host)) && LOOPBACK_HOSTS.has(bare(page.hostname)));
  const listenTls = typeof tls === 'boolean' ? tls : pageTls;
  if (port !== pagePort || !sameHost || listenTls !== pageTls) return { base: origin, direct: false };
  return { base: wildcard ? origin : `${listenTls ? 'https' : 'http'}://${listen}`, direct: true };
}

/** A request for bash/zsh. The key is a variable, never the key. */
export function curlExample({ base, model, authRequired }) {
  const body = JSON.stringify({ model: model ?? 'MODEL', messages: [{ role: 'user', content: 'Say hello' }] });
  const quoted = (value) => `'${String(value).replace(/'/g, "'\\''")}'`;
  const lines = [`curl ${quoted(`${base}/v1/chat/completions`)} \\`];
  if (authRequired) lines.push('  -H "Authorization: Bearer $SWITCHYARD_KEY" \\');
  lines.push('  -H "Content-Type: application/json" \\');
  lines.push(`  -d ${quoted(body)}`);
  return lines.join('\n');
}
