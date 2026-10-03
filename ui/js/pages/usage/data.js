// Usage page: shaping of /usage/summary and /usage/timeseries for the views.
// Pure functions, no DOM and nothing rendered, so they can be exercised in
// Node.
//
// Vocabulary (crates/admin/API.md, "Totals"): `input_tokens` are the uncached
// prompt tokens; cache reads and writes are counted apart from them.
// `output_tokens` includes `reasoning_tokens`.

import { niceScale } from '../../components/charts.js';
import { sortRows } from '../../components/table.js';
import { formatDate } from '../../lib/format.js';

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

/**
 * The ranges the API accepts, with the words the page uses for each.
 * `rateUnit` is the unit the average request rate is given in when there is
 * enough traffic for it (see averageRate).
 */
export const RANGES = [
  { value: '1h', label: '1h', title: 'Last hour', phrase: 'the last hour', ms: HOUR, rateUnit: 'minute' },
  { value: '24h', label: '24h', title: 'Last 24 hours', phrase: 'the last 24 hours', ms: DAY, rateUnit: 'hour' },
  { value: '7d', label: '7d', title: 'Last 7 days', phrase: 'the last 7 days', ms: 7 * DAY, rateUnit: 'hour' },
  { value: '30d', label: '30d', title: 'Last 30 days', phrase: 'the last 30 days', ms: 30 * DAY, rateUnit: 'day' },
];
export const DEFAULT_RANGE = '24h';

/**
 * The three breakdowns. `field` is the list in the summary, `param` the
 * filter of the Requests page, `special` the names that stand for "none".
 * `sortParam` and `filterParam` are where each table keeps its order and its
 * filter in the URL.
 *
 * The provider `unknown` holds two kinds of request (API.md, "Names that
 * stand for none"): those that failed before routing, and those the router
 * refused because every credential of the model was cooling down. Its note
 * names both: the second kind is often most of the row.
 *
 * The stand-in names are filters too: GET /requests takes `model=unknown`
 * (no model was read), `provider=unknown` (no provider served it),
 * `key=anonymous` and `key=dashboard`, so every row opens the list of the
 * requests it counts.
 */
export const GROUPS = [
  { value: 'model', label: 'Model', noun: 'model', plural: 'models', field: 'by_model', param: 'model', sortParam: 'sort', filterParam: 'q', special: { unknown: 'no model resolved' } },
  { value: 'provider', label: 'Provider', noun: 'provider', plural: 'providers', field: 'by_provider', param: 'provider', sortParam: 'psort', filterParam: 'pq', special: { unknown: 'no provider: failed before routing, or every credential cooling down' } },
  { value: 'key', label: 'Client key', noun: 'client key', plural: 'client keys', field: 'by_key', param: 'key', sortParam: 'ksort', filterParam: 'kq', special: { anonymous: 'no client key', dashboard: 'playground' } },
];
export const DEFAULT_GROUP = 'model';

export const rangeOf = (value) => RANGES.find((r) => r.value === value) ?? RANGES.find((r) => r.value === DEFAULT_RANGE);
export const groupOf = (value) => GROUPS.find((g) => g.value === value) ?? GROUPS.find((g) => g.value === DEFAULT_GROUP);

/** How many series keep a colour of their own; the rest are summed into one. */
export const KEEP = 6;

const BUCKET_WORDS = {
  minute: { per: 'per minute', column: 'Minute starting' },
  hour: { per: 'per hour', column: 'Hour starting' },
  // Day buckets are cut at midnight UTC, whatever the browser's time zone,
  // and are labelled with the UTC date they cover (see bucketAxis).
  day: { per: 'per day (UTC)', column: 'Day (UTC)' },
};
export const bucketWords = (bucket) => BUCKET_WORDS[bucket] ?? { per: 'per bucket', column: 'Time' };

/** "2 Oct UTC": the UTC calendar day a day bucket covers. */
const utcDay = (t) => `${formatDate(t, new Date(), { utc: true })} UTC`;

/**
 * The x axis of the time series, as props for the chart components:
 * { x, utc, tipFormat }.
 *
 * Every bucket goes to the charts as its start in epoch milliseconds. Minute
 * and hour buckets are moments and are printed in local time.
 *
 * A day bucket is a UTC calendar day. Printed as a local moment it reads as
 * the day before for everyone west of UTC ("1 Oct 20:00" in New York is the
 * start of 2 Oct UTC). So the charts print day buckets in UTC (`utc`): "2 Oct"
 * on the axis, where the panel heading already says UTC, and "2 Oct UTC" in
 * the tooltip and the table view, as a day and not as its midnight.
 */
export function bucketAxis(timeseries) {
  const x = (timeseries?.points ?? []).map((p) => p.t);
  return timeseries?.bucket === 'day' ? { x, utc: true, tipFormat: utcDay } : { x, utc: false, tipFormat: undefined };
}

// ---------------------------------------------------------------------------
// Counters
// ---------------------------------------------------------------------------

const n = (value) => (typeof value === 'number' && Number.isFinite(value) ? value : 0);

/** All prompt tokens: uncached input plus cache reads and cache writes. */
export const promptTokens = (t) => n(t?.input_tokens) + n(t?.cache_read_tokens) + n(t?.cache_write_tokens);

/** Output that is not reasoning. Never negative, whatever an upstream reports. */
export const visibleOutput = (t) => Math.max(0, n(t?.output_tokens) - n(t?.reasoning_tokens));

/** A ratio, or null when there is nothing to divide by (shown as a dash). */
export const ratio = (part, whole) => (n(whole) > 0 ? n(part) / n(whole) : null);

const RATE_UNITS = [
  { unit: 'minute', ms: MINUTE },
  { unit: 'hour', ms: HOUR },
  { unit: 'day', ms: DAY },
];

/**
 * The average request rate over a span, in a unit that gives a readable
 * number: { value, unit, below }, or null without requests.
 *
 * `spanMs` is the length of the range (`ms` in RANGES), not `to - from` of
 * the summary: the range is a whole number of buckets ending with the
 * current, partial one, so `to - from` falls short of it by up to a bucket
 * and moves with the clock. Twelve requests in the last hour are "12.0 per
 * hour" on every refresh, not 12.1 or 12.2 depending on the second.
 *
 * It starts at `unit` (the range's own) and moves to a longer one while the
 * rate is under one per unit, but never to a unit longer than the span: one
 * request in the last hour is "1 per hour", not "24 per day". `below` is set
 * when even the longest unit would print as 0.0, so the page can say "fewer
 * than 0.1 per day" for traffic that exists.
 */
export function averageRate(requests, spanMs, unit = 'minute') {
  if (!(n(requests) > 0) || !(n(spanMs) > 0)) return null;
  let at = Math.max(0, RATE_UNITS.findIndex((u) => u.unit === unit));
  const rate = (i) => requests / (spanMs / RATE_UNITS[i].ms);
  while (rate(at) < 1 && at < RATE_UNITS.length - 1 && RATE_UNITS[at + 1].ms <= spanMs) at += 1;
  const value = rate(at);
  return { value, unit: RATE_UNITS[at].unit, below: value < 0.05 ? 0.1 : null };
}

/** Rows of the breakdown table for one group, in the API's order (most requests first). */
export function breakdownRows(summary, group) {
  const list = Array.isArray(summary?.[group.field]) ? summary[group.field] : [];
  const total = list.reduce((sum, row) => sum + n(row.requests), 0);
  return list.map((row) => ({
    name: String(row.name ?? ''),
    note: group.special[row.name] ?? null,
    requests: n(row.requests),
    errors: n(row.errors),
    errorRate: ratio(row.errors, row.requests),
    tokensIn: promptTokens(row),
    tokensOut: n(row.output_tokens),
    cost: n(row.cost),
    share: ratio(row.requests, total) ?? 0,
    raw: row,
  }));
}

/** Case-insensitive substring filter on the name. */
export function filterRows(rows, query) {
  const q = String(query ?? '').trim().toLowerCase();
  return q ? rows.filter((row) => row.name.toLowerCase().includes(q)) : rows;
}

export const DEFAULT_SORT = 'requests-desc';
const SORT_KEYS = ['name', 'requests', 'errors', 'errorRate', 'tokensIn', 'tokensOut', 'cost', 'share'];

/** "cost-desc" -> { key, dir }; anything else falls back to the default. */
export function parseSort(text) {
  const [key, dir] = String(text ?? '').split('-');
  if (SORT_KEYS.includes(key) && (dir === 'asc' || dir === 'desc')) return { key, dir };
  return { key: 'requests', dir: 'desc' };
}

/**
 * The rows of a breakdown as its table lists them: filtered by `query`, in
 * the order of `sort` ("key-dir"). The table and the CSV export both go
 * through here, so the file holds what the table shows.
 */
export function viewRows(rows, query, sort) {
  const { key, dir } = parseSort(sort);
  return sortRows(filterRows(rows ?? [], query), { key }, dir);
}

// ---------------------------------------------------------------------------
// Series colours: a colour belongs to an entity, not to its rank
// ---------------------------------------------------------------------------

/**
 * Give each of the `names` (the busiest first, at most KEEP) one of the KEEP
 * colour slots. A name keeps the slot it had before whenever that slot is
 * free, so a refetch or another range that reorders the ranking does not
 * repaint the series. `memory` (Map name -> slot) is updated in place.
 *
 * Returns an array of KEEP entries: the name in each slot, or null.
 */
export function assignSlots(memory, names, keep = KEEP) {
  const slots = new Array(keep).fill(null);
  const wanted = names.slice(0, keep);
  const waiting = [];
  for (const name of wanted) {
    const before = memory.get(name);
    if (before != null && before < keep && slots[before] == null) slots[before] = name;
    else waiting.push(name);
  }
  for (const name of waiting) {
    const free = slots.indexOf(null);
    if (free === -1) break;
    slots[free] = name;
    // The slot now belongs to this name. Whoever held it before has left the
    // chart, and must not push this one out when it comes back.
    for (const [other, slot] of memory) if (slot === free) memory.delete(other);
    memory.set(name, free);
  }
  return slots;
}

export const slotColor = (slot) => `var(--series-${slot + 1})`;
export const OTHER_COLOR = 'var(--series-other)';

/**
 * Shorten a long identifier for the charts: the legend, the tooltip and the
 * headings of the table view. The start and the end are the parts that tell
 * two model names apart, so both are kept. (The shared legend and tooltip
 * cut a name too, but at its end, and the legend only once the name is wider
 * than the whole chart; the table view does not cut at all.)
 */
export function shortName(name, max = 34) {
  const text = String(name ?? '');
  if (text.length <= max) return text;
  const tail = Math.floor((max - 1) / 3);
  return `${text.slice(0, max - 1 - tail)}…${text.slice(-tail)}`;
}

// ---------------------------------------------------------------------------
// Chart series
// ---------------------------------------------------------------------------

const EPSILON = 1e-9;

/**
 * One stacked series per slot, plus the remainder. `metric` is a field that
 * exists both on a point and on its groups: "requests", "errors", "cost".
 * The remainder is the bucket total minus the kept series, so it also covers
 * what the API itself folded into "other".
 *
 * `rest` describes the remainder: { count, only }: how many entities it
 * holds and, when it is exactly one, that entity's name.
 */
export function groupSeries(points, slots, metric, rest = {}) {
  const series = [];
  slots.forEach((name, slot) => {
    if (name == null) return;
    const values = points.map((p) => n(p.groups?.[name]?.[metric]));
    series.push({ key: name, label: shortName(name), color: slotColor(slot), values });
  });
  const remainder = points.map((p, j) => {
    const left = n(p[metric]) - series.reduce((sum, s) => sum + s.values[j], 0);
    return left > EPSILON ? left : 0;
  });
  if (remainder.some((v) => v > 0)) {
    const label = rest.only ? shortName(rest.only) : rest.count > 1 ? `Other (${rest.count})` : 'Other';
    series.push({ key: '__other', label, color: OTHER_COLOR, values: remainder });
  }
  return series;
}

/** Failed requests per bucket. Red is a status here, so it is a lamp colour. */
export function errorSeries(points) {
  return [{ key: 'errors', label: 'Failed requests', color: 'var(--lamp-stop)', values: points.map((p) => n(p.errors)) }];
}

/**
 * Tokens per bucket by kind. Input and output are always there; cache reads,
 * cache writes and reasoning join only when the range has any, and then the
 * plain series are renamed so the legend says what is left in them.
 *
 * Each kind owns a colour whether or not the others are present, so input
 * stays blue when cache reads appear. The series palette is only validated
 * for neighbours (css/tokens.css), and which kinds touch in the stack depends on
 * which are present, so the slots below were picked with the dataviz
 * validator: every pair that can end up adjacent (input/output,
 * input/cache read, cache read/cache write, cache read/output,
 * cache write/output, output/reasoning) passes its colour-vision and
 * normal-vision checks in both themes. The one pair that does not, input
 * next to cache write, needs a range with cache writes and no cache reads.
 */
const TOKEN_SLOT = { input: 0, cache_read: 1, cache_write: 3, output: 2, reasoning: 5 };

export function tokenSeries(points) {
  const sum = (field) => points.reduce((total, p) => total + n(p[field]), 0);
  const cached = sum('cache_read_tokens') > 0 || sum('cache_write_tokens') > 0;
  const reasoned = sum('reasoning_tokens') > 0;
  const make = (key, label, values) => ({ key, label, color: slotColor(TOKEN_SLOT[key]), values });
  const series = [make('input', cached ? 'Uncached input' : 'Input', points.map((p) => n(p.input_tokens)))];
  if (sum('cache_read_tokens') > 0) series.push(make('cache_read', 'Cache read', points.map((p) => n(p.cache_read_tokens))));
  if (sum('cache_write_tokens') > 0) series.push(make('cache_write', 'Cache write', points.map((p) => n(p.cache_write_tokens))));
  series.push(make('output', reasoned ? 'Visible output' : 'Output', points.map(visibleOutput)));
  if (reasoned) series.push(make('reasoning', 'Reasoning', points.map((p) => n(p.reasoning_tokens))));
  return series;
}

/**
 * Two charts stacked above each other share an x axis only when their plots
 * start at the same place, and the plot's left edge depends on the widest y
 * tick label. Returns y formatters that pad the labels of both charts to the
 * same length (with figure spaces, which SVG keeps).
 *
 * charts: [{ max, height, format }], as the chart components see them. They
 * count whole things and are drawn with `integer`, so the ticks are worked
 * out the same way here.
 */
export function alignedFormats(charts) {
  const widest = Math.max(
    ...charts.map(({ max, height, format }) => {
      const scale = niceScale(0, max > 0 ? max : 1, height < 160 ? 2 : 4, { integer: true });
      return Math.max(...scale.ticks.map((tick) => String(format(tick)).length));
    }),
  );
  return charts.map(({ format }) => (value) => String(format(value)).padStart(widest, ' '));
}

/** The largest stacked total of a set of series (what the y axis must reach). */
export function stackMax(series) {
  const length = Math.max(0, ...series.map((s) => s.values.length));
  let max = 0;
  for (let j = 0; j < length; j += 1) {
    const total = series.reduce((sum, s) => sum + n(s.values[j]), 0);
    if (total > max) max = total;
  }
  return max;
}
