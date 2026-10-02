// Formatting for everything the dashboard prints. All functions are pure,
// accept null / undefined / NaN and return an em dash for "no value", so a
// table cell never shows "NaN" or "undefined".

export const DASH = '—';

const isNum = (n) => typeof n === 'number' && Number.isFinite(n);

const intFmt = new Intl.NumberFormat('en-US', { maximumFractionDigits: 0 });

/** 1234567 -> "1,234,567". `decimals` fixes the fraction digits. */
export function formatNumber(n, decimals) {
  if (!isNum(n)) return DASH;
  if (decimals == null) return Number.isInteger(n) ? intFmt.format(n) : trimFixed(n, 2, true);
  return new Intl.NumberFormat('en-US', { minimumFractionDigits: decimals, maximumFractionDigits: decimals }).format(n);
}

function trimFixed(n, digits, group = false) {
  const s = new Intl.NumberFormat('en-US', { maximumFractionDigits: digits, useGrouping: group }).format(n);
  return s;
}

/**
 * Compact count: 950 -> "950", 1234 -> "1.2K", 1250000 -> "1.3M", 2.5e9 -> "2.5B".
 * Used for tokens and request counts. One decimal below 100 of a unit, none above.
 */
export function formatCompact(n) {
  if (!isNum(n)) return DASH;
  const sign = n < 0 ? '-' : '';
  const abs = Math.abs(n);
  if (abs < 1000) return sign + trimFixed(abs, abs < 10 && !Number.isInteger(abs) ? 1 : 0);
  const units = ['K', 'M', 'B', 'T'];
  let value = abs;
  let unit = -1;
  while (value >= 1000 && unit < units.length - 1) {
    value /= 1000;
    unit += 1;
  }
  // 999.95K would round to "1000K": carry into the next unit instead.
  let text = value >= 100 ? value.toFixed(0) : value.toFixed(1);
  if (Number(text) >= 1000 && unit < units.length - 1) {
    value /= 1000;
    unit += 1;
    text = value.toFixed(1);
  }
  return sign + text.replace(/\.0$/, '') + units[unit];
}

/** Token counts: same as formatCompact; named for call-site clarity. */
export const formatTokens = formatCompact;

/**
 * Milliseconds to a short duration:
 *   0.4 -> "<1ms", 312 -> "312ms", 1240 -> "1.24s", 12400 -> "12.4s",
 *   65000 -> "1m 05s", 3720000 -> "1h 02m", 90000000 -> "1d 1h".
 */
export function formatDuration(ms) {
  if (!isNum(ms)) return DASH;
  if (ms < 0) ms = 0;
  if (ms > 0 && ms < 1) return '<1ms';
  if (ms < 1000) return `${Math.round(ms)}ms`;
  if (ms < 10_000) return `${(ms / 1000).toFixed(2)}s`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  const totalSeconds = Math.round(ms / 1000);
  const pad = (v) => String(v).padStart(2, '0');
  if (totalSeconds < 3600) return `${Math.floor(totalSeconds / 60)}m ${pad(totalSeconds % 60)}s`;
  const totalMinutes = Math.round(totalSeconds / 60);
  if (totalMinutes < 1440) return `${Math.floor(totalMinutes / 60)}h ${pad(totalMinutes % 60)}m`;
  const totalHours = Math.round(totalMinutes / 60);
  return `${Math.floor(totalHours / 24)}d ${totalHours % 24}h`;
}

/** Seconds to a countdown clock: 59 -> "0:59", 1781 -> "29:41", 3700 -> "1:01:40". */
export function formatCountdown(seconds) {
  if (!isNum(seconds)) return DASH;
  const s = Math.max(0, Math.ceil(seconds));
  const pad = (v) => String(v).padStart(2, '0');
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return h > 0 ? `${h}:${pad(m)}:${pad(s % 60)}` : `${m}:${pad(s % 60)}`;
}

/** Accepts a Date, epoch milliseconds, epoch seconds or an ISO string. */
export function toDate(value) {
  if (value == null || value === '') return null;
  if (value instanceof Date) return Number.isNaN(value.getTime()) ? null : value;
  if (typeof value === 'number') {
    if (!Number.isFinite(value)) return null;
    // Epoch seconds are below 1e11 until the year 5138.
    return new Date(value < 1e11 ? value * 1000 : value);
  }
  const d = new Date(value);
  return Number.isNaN(d.getTime()) ? null : d;
}

/**
 * "just now", "12s ago", "5m ago", "3h ago", "2d ago", then a date.
 * Future times read "in 5m". `now` is injectable for tests and for the
 * shared one-second clock (see useNow in hooks.js).
 */
export function formatRelativeTime(value, now = Date.now()) {
  const d = toDate(value);
  if (!d) return DASH;
  const diff = now - d.getTime();
  const abs = Math.abs(diff);
  const wrap = (text) => (diff < 0 ? `in ${text}` : `${text} ago`);
  if (abs < 5000) return 'just now';
  if (abs < 60_000) return wrap(`${Math.floor(abs / 1000)}s`);
  if (abs < 3_600_000) return wrap(`${Math.floor(abs / 60_000)}m`);
  if (abs < 86_400_000) return wrap(`${Math.floor(abs / 3_600_000)}h`);
  if (abs < 7 * 86_400_000) return wrap(`${Math.floor(abs / 86_400_000)}d`);
  return formatDate(d);
}

const pad2 = (v) => String(v).padStart(2, '0');

/** "14:03:27" in local time; with `ms`, "14:03:27.512". */
export function formatTime(value, { ms = false } = {}) {
  const d = toDate(value);
  if (!d) return DASH;
  const base = `${pad2(d.getHours())}:${pad2(d.getMinutes())}:${pad2(d.getSeconds())}`;
  return ms ? `${base}.${String(d.getMilliseconds()).padStart(3, '0')}` : base;
}

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

/** "2 Oct 2026"; the year is dropped when it is the current one. */
export function formatDate(value, now = new Date()) {
  const d = toDate(value);
  if (!d) return DASH;
  const base = `${d.getDate()} ${MONTHS[d.getMonth()]}`;
  return d.getFullYear() === now.getFullYear() ? base : `${base} ${d.getFullYear()}`;
}

/** "2 Oct 14:03:27" (local time). */
export function formatDateTime(value) {
  const d = toDate(value);
  if (!d) return DASH;
  return `${formatDate(d)} ${formatTime(d)}`;
}

/** 0 -> "0 B", 1536 -> "1.5 KB", 5242880 -> "5 MB". Binary units, decimal labels. */
export function formatBytes(n) {
  if (!isNum(n)) return DASH;
  const abs = Math.abs(n);
  if (abs < 1024) return `${Math.round(n)} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let value = abs;
  let unit = -1;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const text = value >= 100 ? value.toFixed(0) : value.toFixed(1).replace(/\.0$/, '');
  return `${n < 0 ? '-' : ''}${text} ${units[unit]}`;
}

/**
 * US dollars. LLM costs are often fractions of a cent, so small amounts keep
 * enough digits to be non-zero: 0.00042 -> "$0.00042", 0.0421 -> "$0.042",
 * 1.5 -> "$1.50", 1204.5 -> "$1,205".
 */
export function formatCurrency(n, currency = 'USD') {
  if (!isNum(n)) return DASH;
  const symbol = currency === 'USD' ? '$' : `${currency} `;
  const sign = n < 0 ? '-' : '';
  const abs = Math.abs(n);
  if (abs === 0) return `${symbol}0.00`;
  if (abs < 0.000001) return `${sign}<${symbol}0.000001`;
  if (abs < 0.01) return `${sign}${symbol}${Number(abs.toPrecision(2)).toFixed(6).replace(/0+$/, '')}`;
  if (abs < 1) return `${sign}${symbol}${abs.toFixed(3).replace(/0$/, '')}`;
  if (abs < 1000) return `${sign}${symbol}${abs.toFixed(2)}`;
  return `${sign}${symbol}${intFmt.format(Math.round(abs))}`;
}

/**
 * A ratio (0..1) as a percentage: 0.9934 -> "99.3%". Never rounds a non-zero
 * value down to "0%" or an incomplete one up to "100%".
 */
export function formatPercent(ratio, decimals = 1) {
  if (!isNum(ratio)) return DASH;
  const pct = ratio * 100;
  let text = pct.toFixed(decimals);
  if (pct > 0 && Number(text) === 0) return `<${(1 / 10 ** decimals).toFixed(decimals)}%`;
  if (pct < 100 && Number(text) === 100) text = (100 - 1 / 10 ** decimals).toFixed(decimals);
  return `${text.replace(/\.0+$/, '')}%`;
}

/** A signed change for Stat deltas: 0.124 -> "+12.4%", -0.03 -> "-3%". */
export function formatDelta(ratio, decimals = 1) {
  if (!isNum(ratio)) return DASH;
  const text = formatPercent(Math.abs(ratio), decimals);
  if (ratio === 0) return text;
  return `${ratio > 0 ? '+' : '−'}${text}`;
}

/** "1 request", "2 requests". Pass the plural when it is irregular. */
export function plural(n, one, many = `${one}s`) {
  return `${formatNumber(n)} ${n === 1 ? one : many}`;
}

/** Mask a secret for display when the server did not: first 4 and last 4. */
export function maskSecret(value) {
  if (!value) return '';
  const s = String(value);
  if (s.length <= 8) return '•'.repeat(Math.max(4, s.length));
  return `${s.slice(0, 4)}••••${s.slice(-4)}`;
}
