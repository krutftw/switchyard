// Usage (#/usage): statistics and estimated cost for a range.
//
//   ?range=1h|24h|7d|30d        the window (default 24h)
//   ?group=model|provider|key   what the charts stack by
//   ?sort=cost-desc  ?q=text    order and filter of the "By model" table
//   ?psort= ?pq=  ?ksort= ?kq=  the same for "By provider" and "By client key"
//
// Data: GET /usage/summary and GET /usage/timeseries, read together every
// 30 seconds and on demand; GET /pricing decides whether cost is shown as a
// number or as "not priced". The pieces live in ./usage/: data.js (shaping),
// charts.js, breakdown.js, export.js.

import { html, useCallback, useEffect, useMemo, useRef, useState } from '../../vendor/preact-htm.js';
import { Button, EmptyState, ErrorState, Menu, Notice, Page, Panel, Segmented, Stat, StatGroup, confirm, toast } from '../components/index.js';
import { api } from '../lib/api.js';
import { useCommands } from '../lib/commands.js';
import { loadStyles } from '../lib/dom.js';
import { formatCompact, formatCurrency, formatDate, formatNumber, formatPercent, formatRelativeTime, formatTime, formatTokens, plural, sentence } from '../lib/format.js';
import { useNow, useResource, useSize } from '../lib/hooks.js';
import { liveState, useLive, useLiveGap } from '../lib/live.js';
import { href, useQueryParam, useRoute } from '../lib/router.js';
import { useStore } from '../lib/store.js';
import Breakdown from './usage/breakdown.js';
import UsageCharts, { ChartsError, ChartsSkeleton, LatencyPanel, LatencySkeleton } from './usage/charts.js';
import { DEFAULT_GROUP, DEFAULT_RANGE, GROUPS, KEEP, RANGES, assignSlots, averageRate, breakdownRows, bucketWords, filterRows, groupOf, promptTokens, rangeOf, ratio, viewRows } from './usage/data.js';
import downloadFile, { breakdownCsv, fileName, timeseriesCsv, usageJson } from './usage/export.js';

await loadStyles('pages/usage.css');

const REFRESH_MS = 30_000;

/**
 * Both reads of one refresh. The summary is the page; without it the load
 * fails. The time series may fail on its own, and then the numbers and the
 * tables are still shown and the charts say what happened.
 */
async function loadUsage(range, group, signal) {
  const [summary, timeseries] = await Promise.allSettled([
    api.get('/usage/summary', { query: { range }, signal }),
    api.get('/usage/timeseries', { query: { range, group_by: group }, signal }),
  ]);
  if (summary.status === 'rejected') throw summary.reason;
  return {
    range,
    group,
    summary: summary.value,
    timeseries: timeseries.status === 'fulfilled' ? timeseries.value : null,
    timeseriesError: timeseries.status === 'rejected' ? timeseries.reason : null,
  };
}

/** "2 Oct 23:54": a moment, to the minute. */
const moment = (t) => `${formatDate(t)} ${formatTime(t).slice(0, 5)}`;

// ---------------------------------------------------------------------------
// Header: when the numbers were read, and what has happened since
// ---------------------------------------------------------------------------

/**
 * "Updated 12s ago", ticking. While the live connection is open it also
 * counts the requests that finished since, from the once-a-second stats
 * frame, so it is plain whether a refresh would change anything.
 */
function Freshness({ updatedAt }) {
  const now = useNow(1000);
  const live = useStore(liveState, (s) => s.status) === 'open';
  const latest = useRef(null);
  const base = useRef({ at: null, total: null });
  const [pending, setPending] = useState(0);

  if (base.current.at !== updatedAt) base.current = { at: updatedAt, total: latest.current };
  useEffect(() => setPending(0), [updatedAt]);

  useLive('stats', (stats) => {
    const total = stats?.totals?.requests;
    if (typeof total !== 'number') return;
    latest.current = total;
    // No baseline yet, or the gateway restarted and counts from zero again.
    if (base.current.total == null || total < base.current.total) base.current.total = total;
    setPending(total - base.current.total);
  });

  if (!updatedAt) return null;
  return html`
    <span class="usage-fresh" title=${`Read at ${formatTime(updatedAt)}`}>
      Updated ${formatRelativeTime(updatedAt, now)}${live && pending > 0 && html`<span> · <span class="num">${formatNumber(pending)}</span> ${pending === 1 ? 'request' : 'requests'} since</span>`}
    </span>
  `;
}

// ---------------------------------------------------------------------------
// Headline numbers
// ---------------------------------------------------------------------------

/** Error-rate lamp: green with no failures, amber below 5%, red from there. */
function errorTone(rate) {
  if (rate == null) return undefined;
  if (rate === 0) return 'clear';
  return rate < 0.05 ? 'caution' : 'stop';
}

/** What each lamp says, in words: the lamp's accessible name and its tooltip. */
const ERROR_TONE_WORDS = {
  clear: 'No failed requests',
  caution: 'Under 5% of requests failed',
  stop: '5% or more of requests failed',
};

/**
 * "38.2 per hour on average". Thin traffic moves to a longer unit ("1.0 per
 * day") and, past that, says "Fewer than 0.1 per day": a rate that exists is
 * never printed as 0.0.
 */
function rateWords(requests, range) {
  // Over the length of the range, not `to - from` (see averageRate).
  const rate = averageRate(requests, range.ms, range.rateUnit);
  if (!rate) return null;
  if (rate.below) return `Fewer than ${rate.below} per ${rate.unit} on average`;
  const value = rate.value >= 1_000_000 ? formatCompact(rate.value) : rate.value >= 100 ? formatNumber(Math.round(rate.value)) : formatNumber(rate.value, 1);
  return `${value} per ${rate.unit} on average`;
}

function Headline({ summary, range, priced, hasPrices, loading, stale }) {
  const t = summary?.totals;
  const requests = t?.requests ?? 0;
  const rateHint = t ? rateWords(requests, range) : null;

  const errorRate = t ? ratio(t.errors, requests) : null;
  const errorText = errorRate == null ? null : formatPercent(errorRate);

  const prompt = t ? promptTokens(t) : null;
  let promptHint = null;
  if (t && prompt > 0) {
    const read = t.cache_read_tokens;
    const write = t.cache_write_tokens;
    promptHint = read > 0 || write > 0 ? `${formatTokens(read)} cache read (${formatPercent(read / prompt, 0)}) · ${formatTokens(write)} cache write` : 'No cache reads or writes';
  }

  let outputHint = null;
  if (t && t.output_tokens > 0) {
    outputHint = t.reasoning_tokens > 0 ? `${formatTokens(t.reasoning_tokens)} reasoning (${formatPercent(t.reasoning_tokens / t.output_tokens, 0)})` : 'No reasoning tokens';
  }

  // Without prices a cost of 0 is "not priced": a dash, not $0.00.
  let cost = null;
  let costHint = null;
  if (t && priced) {
    cost = formatCurrency(t.cost);
    costHint = requests > 0 ? `${formatCurrency(t.cost / requests)} per request` : null;
  } else if (t && hasPrices === false) {
    costHint = 'No prices configured';
  }

  return html`
    <${StatGroup} class="usage-stats usage-fade" label=${`Totals for ${range.phrase}`} data-stale=${stale ? '' : undefined}>
      <${Stat} label="Requests" value=${t ? (requests < 1_000_000 ? formatNumber(requests) : formatCompact(requests)) : null} hint=${rateHint} loading=${loading} />
      <${Stat}
        label="Error rate"
        value=${errorText ? errorText.replace('%', '') : null}
        unit=${errorText ? '%' : undefined}
        lamp=${errorTone(errorRate)}
        lampLabel=${ERROR_TONE_WORDS[errorTone(errorRate)]}
        hint=${t && requests > 0 ? `${formatNumber(t.errors)} of ${formatNumber(requests)} failed` : null}
        loading=${loading}
      />
      <${Stat} label="Input tokens" value=${t ? formatTokens(prompt) : null} hint=${promptHint} loading=${loading} />
      <${Stat} label="Output tokens" value=${t ? formatTokens(t.output_tokens) : null} hint=${outputHint} loading=${loading} />
      <${Stat} label="Estimated cost" value=${cost} hint=${costHint} loading=${loading} />
    <//>
  `;
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

export default function Usage() {
  const route = useRoute();
  // Picking a range or a grouping is a step Back returns to.
  const [rangeParam, setRange] = useQueryParam('range', DEFAULT_RANGE, { push: true });
  const [groupParam, setGroup] = useQueryParam('group', DEFAULT_GROUP, { push: true });
  // A stale link (?range=90d) shows the default instead of an empty page.
  const range = rangeOf(rangeParam);
  const group = groupOf(groupParam);

  // Changing the range or the group starts a new load. Until it lands, the
  // previous view stays on screen, dimmed, instead of a page of skeletons
  // (keepPrevious). If the new one cannot be loaded the page says so in place
  // of the numbers: they would be those of another range than the one selected.
  const usage = useResource((signal) => loadUsage(range.value, group.value, signal), { deps: [range.value, group.value], pollMs: REFRESH_MS, keepPrevious: true });
  const pricing = useResource('/pricing');

  const data = usage.isPrevious && usage.error ? null : (usage.data ?? null);
  const updatedAt = data ? usage.updatedAt : null;
  const firstLoad = usage.loading && !data;
  const summary = data?.summary ?? null;
  const shownRange = data ? rangeOf(data.range) : range;
  const chartGroup = data ? groupOf(data.group) : group;
  const rangeChanging = !!data && data.range !== range.value;

  // A refresh whose time series failed (while the summary arrived) keeps the
  // charts it had for this range and group, and says how old they are. Only
  // a range and group that never had a time series show the error instead.
  const lastSeries = useRef(null);
  if (data?.timeseries && lastSeries.current?.timeseries !== data.timeseries) {
    lastSeries.current = { range: data.range, group: data.group, timeseries: data.timeseries, at: updatedAt ?? Date.now() };
  }
  const held = lastSeries.current;
  const heldSeries = data && !data.timeseries && held && held.range === data.range && held.group === data.group ? held : null;
  const timeseries = data?.timeseries ?? heldSeries?.timeseries ?? null;
  const chartsStale = usage.refreshing || !!heldSeries;

  // null while the price table is unknown (loading, or it could not be read).
  const hasPrices = Array.isArray(pricing.data) ? pricing.data.length > 0 : null;
  const priced = hasPrices === true || (summary?.totals?.cost ?? 0) > 0;

  // Colour slots per group, remembered for as long as the page is open, so a
  // model keeps its colour across refreshes, ranges and group switches.
  const memory = useRef(Object.fromEntries(GROUPS.map((g) => [g.value, new Map()])));
  const shaped = useMemo(() => {
    const out = {};
    for (const g of GROUPS) {
      const rows = summary ? breakdownRows(summary, g) : null;
      const names = rows ? rows.map((row) => row.name) : [];
      const left = names.slice(KEEP);
      out[g.value] = {
        rows,
        slots: assignSlots(memory.current[g.value], names.slice(0, KEEP)),
        rest: { count: left.length, only: left.length === 1 ? left[0] : null },
      };
    }
    return out;
  }, [summary]);

  // The controls stick below the top bar on a desk; the tables' column
  // headings stick below the controls, however many lines those take.
  const [controlsRef, controlsSize] = useSize();

  // ---- Refreshing ---------------------------------------------------------

  const [refreshing, setRefreshing] = useState(false);
  const refreshAll = useCallback(async () => {
    setRefreshing(true);
    await Promise.all([usage.refresh(), pricing.refresh()]);
    setRefreshing(false);
  }, [usage.refresh, pricing.refresh]);

  // Prices are configuration: a reload may have added or removed them.
  useLive('config.reloaded', () => {
    pricing.refresh();
    usage.refresh();
  });

  // Coming back from a dropped live connection: the tab may have slept
  // through any number of refreshes.
  useLiveGap(({ reason }) => {
    if (reason === 'reconnect') usage.refresh();
  });

  // ---- Export and clear ---------------------------------------------------

  // Each breakdown is exported as its table lists it: the same filter, the
  // same order (every matching row, not only the first page).
  const listed = (g) => viewRows(shaped[g.value].rows, route.query[g.filterParam], route.query[g.sortParam]);

  const exportFile = (kind, g) => {
    if (!summary) return;
    const at = new Date();
    if (kind === 'breakdown') {
      const rows = listed(g);
      const total = shaped[g.value].rows?.length ?? 0;
      const name = fileName(`by-${g.value}`, summary.range, 'csv', at);
      downloadFile(name, breakdownCsv(rows, g), 'text/csv;charset=utf-8');
      toast.success(`Breakdown by ${g.noun} exported`, { description: rows.length < total ? `${name}: the ${formatNumber(rows.length)} of ${plural(total, g.noun, g.plural)} that match the filter` : name });
    } else if (kind === 'timeseries' && timeseries) {
      const name = fileName(`timeseries-by-${timeseries.group_by}`, summary.range, 'csv', at);
      downloadFile(name, timeseriesCsv(timeseries, { priced }), 'text/csv;charset=utf-8');
      toast.success('Time series exported', { description: name });
    } else if (kind === 'json') {
      const name = fileName('all', summary.range, 'json', at);
      downloadFile(name, usageJson({ summary, timeseries, pricing: pricing.data ?? null, exportedAt: at.getTime() }), 'application/json');
      toast.success('Usage exported', { description: name });
    }
  };

  const clearStatistics = async () => {
    const ok = await confirm({
      danger: true,
      title: 'Clear all usage statistics?',
      message: 'This deletes the request list, every time bucket of every range and the usage files on disk, and resets the usage shown for each client key on the API keys page. Totals since start and captured bodies are kept. It cannot be undone.',
      confirmLabel: 'Clear statistics',
      typeToConfirm: 'clear',
      action: () => api.del('/usage'),
    });
    if (!ok) return;
    lastSeries.current = null;
    toast.success('Usage statistics cleared');
    usage.refresh();
  };

  useCommands(
    () => [
      ...RANGES.map((r) => ({ id: `usage-range-${r.value}`, label: `Usage: show ${r.phrase}`, group: 'Usage', icon: 'clock', keywords: `range ${r.value}`, run: () => setRange(r.value) })),
      ...GROUPS.map((g) => ({ id: `usage-group-${g.value}`, label: `Usage: stack charts by ${g.noun}`, group: 'Usage', icon: 'usage', keywords: 'group breakdown', run: () => setGroup(g.value) })),
    ],
    [setRange, setGroup],
  );

  // ---- Render ---------------------------------------------------------------

  const hasTraffic = (summary?.totals?.requests ?? 0) > 0;
  const charts = shaped[chartGroup.value];

  const latencyPanel = summary ? html`<${LatencyPanel} summary=${summary} range=${shownRange} stale=${rangeChanging} />` : html`<${LatencySkeleton} />`;

  const exportItems = [
    { heading: shownRange.title },
    ...GROUPS.map((g) => {
      const rows = shaped[g.value].rows ?? [];
      const matching = filterRows(rows, route.query[g.filterParam]).length;
      return {
        label: `By ${g.noun} (CSV)`,
        icon: 'table',
        // A filtered table exports its matching rows only: say how many.
        hint: matching < rows.length ? `${formatNumber(matching)} of ${formatNumber(rows.length)}` : undefined,
        disabled: !hasTraffic || matching === 0,
        onSelect: () => exportFile('breakdown', g),
      };
    }),
    { separator: true },
    { label: `Time series by ${chartGroup.noun} (CSV)`, icon: 'chart', disabled: !timeseries, onSelect: () => exportFile('timeseries') },
    { label: 'Everything (JSON)', icon: 'download', disabled: !summary, onSelect: () => exportFile('json') },
  ];

  const actions = html`
    <${Freshness} updatedAt=${updatedAt} />
    <${Button} icon="refresh" loading=${refreshing} onClick=${refreshAll}>Refresh<//>
    <${Menu} label="Export" trigger=${(props) => html`<${Button} icon="download" iconRight="chevron-down" ...${props}>Export<//>`} items=${exportItems} />
    <${Menu} label="More usage actions" items=${[{ label: 'Clear statistics', icon: 'trash', danger: true, onSelect: clearStatistics }]} />
  `;

  return html`
    <${Page} title="Usage" description="Requests, tokens and estimated cost over time." actions=${actions} class="usage">
      <div class="usage-controls" ref=${controlsRef}>
        <${Segmented} label="Time range" value=${range.value} onChange=${setRange} options=${RANGES.map((r) => ({ value: r.value, label: r.label, title: r.title }))} />
        <div class="usage-control">
          <span class="usage-control-label" aria-hidden="true">Charts by</span>
          <${Segmented} label="Stack the charts by" value=${group.value} onChange=${setGroup} options=${GROUPS.map((g) => ({ value: g.value, label: g.label }))} />
        </div>
        ${summary && html`<span class="usage-window">${moment(summary.from)} to ${moment(summary.to)}${timeseries ? `, ${bucketWords(timeseries.bucket).per}` : ''}</span>`}
      </div>

      ${usage.error &&
      data &&
      html`
        <${Notice} tone="caution" title="Could not refresh usage statistics" action=${html`<${Button} size="sm" loading=${refreshing} onClick=${refreshAll}>Try again<//>`}>
          ${sentence(usage.error.message)} The numbers below were read at ${formatTime(updatedAt)}.
        <//>
      `}

      ${usage.error && !data
        ? html`
            <${Panel} flush>
              <${ErrorState} title="Could not load usage statistics" error=${usage.error} onRetry=${refreshAll} retrying=${refreshing} />
            <//>
          `
        : html`
            <${Headline} summary=${summary} range=${shownRange} priced=${priced} hasPrices=${hasPrices} loading=${firstLoad} stale=${rangeChanging} />

            ${hasPrices === false &&
            html`
              <${Notice} title="No prices configured" action=${html`<${Button} size="sm" href=${href('/settings', { tab: 'pricing' })}>Set prices<//>`}>
                ${priced
                  ? 'Cost is estimated from the price table in Settings, under Pricing. That table is empty now, so new requests are recorded without a cost; earlier requests keep theirs.'
                  : 'Cost is estimated from the price table in Settings, under Pricing. Add the prices of your models there and requests are costed from then on.'}
              <//>
            `}

            ${firstLoad && html`<${ChartsSkeleton} side=${latencyPanel} />`}

            ${summary &&
            !hasTraffic &&
            html`
              <${Panel} flush>
                <${EmptyState}
                  icon="usage"
                  title=${`No requests in ${shownRange.phrase}`}
                  description="Requests, tokens and estimated cost are charted here as clients call the gateway."
                  action=${shownRange.value === '30d'
                    ? html`<${Button} variant="primary" icon="playground" href=${href('/playground')}>Send a test request<//>`
                    : html`<${Button} onClick=${() => setRange('30d')}>Show the last 30 days<//>`}
                />
              <//>
            `}

            ${hasTraffic &&
            heldSeries &&
            html`
              <${Notice} tone="caution" title="Could not refresh the charts" action=${html`<${Button} size="sm" loading=${refreshing} onClick=${refreshAll}>Try again<//>`}>
                ${sentence(data.timeseriesError?.message) || 'The time series could not be read.'} The charts show the time series read at ${formatTime(heldSeries.at)}; the totals and the tables are current.
              <//>
            `}

            ${hasTraffic &&
            (timeseries
              ? html`<${UsageCharts}
                  timeseries=${timeseries}
                  group=${chartGroup}
                  slots=${charts.slots}
                  rest=${charts.rest}
                  range=${shownRange}
                  showCost=${priced}
                  stale=${chartsStale}
                  side=${latencyPanel}
                />`
              : html`<${ChartsError} error=${data.timeseriesError} onRetry=${refreshAll} retrying=${refreshing} side=${latencyPanel} />`)}

            ${(firstLoad || hasTraffic) &&
            GROUPS.map(
              (g) => html`<${Breakdown}
                key=${g.value}
                group=${g}
                rows=${shaped[g.value].rows}
                range=${shownRange}
                since=${summary?.from}
                slots=${timeseries && g.value === chartGroup.value ? shaped[g.value].slots : null}
                loading=${firstLoad}
                stale=${rangeChanging}
                stickyTop=${controlsSize.height}
              />`,
            )}
          `}
    <//>
  `;
}
