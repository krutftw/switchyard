// Usage page: the time series (requests with failures, tokens, cost) and the
// latency panel that sits beside them.

import { html, useMemo } from '../../../vendor/preact-htm.js';
import { BarChart, ErrorState, KeyValue, LatencyBars, Panel, Skeleton } from '../../components/index.js';
import { formatCompact, formatCurrency, formatDuration, formatNumber, formatTokens } from '../../lib/format.js';
import { alignedFormats, bucketAxis, bucketWords, errorSeries, groupSeries, stackMax, tokenSeries } from './data.js';

const REQUESTS_HEIGHT = 240;
const ERRORS_HEIGHT = 120;
const SIDE_HEIGHT = 220;

// ---------------------------------------------------------------------------
// Latency
// ---------------------------------------------------------------------------

const WINDOW_WORDS = { 3600000: 'the last hour', 86400000: 'the last 24 hours' };

/**
 * Percentiles of request duration and of time to first byte, drawn on one
 * scale so the two can be compared, with the means of the range below.
 */
export function LatencyPanel({ summary, range, stale }) {
  const latency = summary.latency ?? {};
  const totals = summary.totals ?? {};
  const measured = latency.samples > 0;
  const streamed = latency.ttfb_samples > 0;
  const value = (present, ms) => (present ? ms : null);
  const scale = Math.max(measured ? latency.p99 : 0, streamed ? latency.ttfb_p95 : 0) || undefined;
  // For 7d and 30d the gateway only keeps percentiles of the last 24 hours.
  const shorter = latency.window_ms != null && latency.window_ms < range.ms;
  const window = shorter ? (WINDOW_WORDS[latency.window_ms] ?? `the last ${formatDuration(latency.window_ms)}`) : range.phrase;

  return html`
    <${Panel} title="Latency" description=${`Percentiles over ${window}`}>
      <div class="stack usage-fade" data-stale=${stale ? '' : undefined} style="--gap:var(--space-4)">
        <div class="usage-bars">
          <h3 class="plate-label">Request duration</h3>
          <${LatencyBars}
            max=${scale}
            items=${[
              { label: 'p50', value: value(measured, latency.p50) },
              { label: 'p95', value: value(measured, latency.p95) },
              { label: 'p99', value: value(measured, latency.p99) },
            ]}
          />
        </div>
        <div class="usage-bars">
          <h3 class="plate-label">Time to first byte</h3>
          <${LatencyBars}
            max=${scale}
            items=${[
              { label: 'p50', value: value(streamed, latency.ttfb_p50) },
              { label: 'p95', value: value(streamed, latency.ttfb_p95) },
            ]}
          />
        </div>
        <hr />
        <${KeyValue}
          items=${[
            { label: 'Mean duration', value: totals.requests > 0 ? formatDuration(totals.duration_ms_sum / totals.requests) : null },
            { label: 'Mean time to first byte', value: totals.ttfb_count > 0 ? formatDuration(totals.ttfb_ms_sum / totals.ttfb_count) : null },
            { label: 'Requests measured', value: measured ? formatNumber(latency.samples) : null },
            { label: 'With a first byte', value: streamed ? formatNumber(latency.ttfb_samples) : null },
          ]}
        />
        ${shorter && html`<p class="usage-note">The gateway keeps percentiles for ${window} only. The two means cover ${range.phrase}.</p>`}
      </div>
    <//>
  `;
}

// ---------------------------------------------------------------------------
// Time series
// ---------------------------------------------------------------------------

/**
 * timeseries  the /usage/timeseries response
 * group       the GROUPS entry the response was grouped by
 * slots       colour slots for that group (data.js, assignSlots)
 * rest        what the remainder series stands for: { count, only }
 * range       the RANGES entry
 * showCost    draw the cost chart (a price is configured, or cost was recorded)
 * stale       dim the plots: a refetch is in flight, or these are the
 *             numbers of the previous range while the new ones load
 * side        the panel shown beside the requests chart (latency)
 */
export default function UsageCharts({ timeseries, group, slots, rest, range, showCost, stale, side }) {
  const points = timeseries.points ?? [];
  const words = bucketWords(timeseries.bucket);

  const shaped = useMemo(() => {
    // Day buckets are UTC days and are printed as such; see bucketAxis.
    const axis = bucketAxis(timeseries);
    const requests = groupSeries(points, slots, 'requests', rest);
    const errors = errorSeries(points);
    const tokens = tokenSeries(points);
    const requestsMax = stackMax(requests);
    const errorsMax = stackMax(errors);
    const [requestsAxis, errorsAxis] = alignedFormats([
      { max: requestsMax, height: REQUESTS_HEIGHT, format: formatCompact },
      { max: errorsMax, height: ERRORS_HEIGHT, format: formatCompact },
    ]);
    // Requests, failures and tokens are whole things: their charts are drawn
    // with `integer`, so no axis reads "0.5".
    return {
      axis,
      requests,
      errors,
      requestsAxis,
      errorsAxis,
      tokens,
      cost: showCost ? groupSeries(points, slots, 'cost', rest) : [],
    };
  }, [timeseries, slots, rest, showCost]);

  const by = `by ${group.noun}`;
  const scope = `${words.per}, ${range.phrase}`;

  return html`
    <div class="usage-split">
      <${Panel} title="Requests over time" description=${`Stacked ${by}, ${words.per}`}>
        <div class="stack" style="--gap:var(--space-4)">
          <${BarChart}
            ...${shaped.axis}
            series=${shaped.requests}
            height=${REQUESTS_HEIGHT}
            integer
            yFormat=${shaped.requestsAxis}
            valueFormat=${formatNumber}
            xLabel=${words.column}
            stale=${stale}
            label=${`Requests ${by} ${scope}`}
            emptyText="No requests in this range"
          />
          <div class="usage-sub">
            <h3 class="plate-label">Failed requests</h3>
            <${BarChart}
              ...${shaped.axis}
              series=${shaped.errors}
              height=${ERRORS_HEIGHT}
              integer
              yFormat=${shaped.errorsAxis}
              valueFormat=${formatNumber}
              xLabel=${words.column}
              stale=${stale}
              label=${`Failed requests ${scope}`}
              emptyText="No failed requests in this range"
            />
          </div>
        </div>
      <//>
      ${side}
    </div>
    <div class=${showCost ? 'grid-2' : 'stack'}>
      <${Panel} title="Tokens over time" description=${`Stacked by kind, ${words.per}`}>
        <${BarChart}
          ...${shaped.axis}
          series=${shaped.tokens}
          height=${SIDE_HEIGHT}
          integer
          yFormat=${formatTokens}
          valueFormat=${formatNumber}
          xLabel=${words.column}
          stale=${stale}
          label=${`Tokens by kind ${scope}`}
          emptyText="No tokens counted in this range"
        />
      <//>
      ${showCost &&
      html`
        <${Panel} title="Estimated cost over time" description=${`Stacked ${by}, ${words.per}`}>
          <${BarChart}
            ...${shaped.axis}
            series=${shaped.cost}
            height=${SIDE_HEIGHT}
            yFormat=${formatCurrency}
            valueFormat=${formatCurrency}
            xLabel=${words.column}
            stale=${stale}
            label=${`Estimated cost in US dollars ${by} ${scope}`}
            emptyText="No cost recorded in this range"
          />
        <//>
      `}
    </div>
  `;
}

/** The same frames with skeletons, for the first load. */
export function ChartsSkeleton({ showCost = false, side }) {
  const plot = (height) => html`<${Skeleton} width="100%" height=${`${height}px`} />`;
  return html`
    <div class="usage-split" aria-busy="true">
      <${Panel} title="Requests over time">
        <div class="stack" style="--gap:var(--space-4)">${plot(REQUESTS_HEIGHT + 36)}${plot(ERRORS_HEIGHT)}</div>
      <//>
      ${side}
    </div>
    <div class=${showCost ? 'grid-2' : 'stack'} aria-busy="true">
      <${Panel} title="Tokens over time">${plot(SIDE_HEIGHT + 36)}<//>
      ${showCost && html`<${Panel} title="Estimated cost over time">${plot(SIDE_HEIGHT + 36)}<//>`}
    </div>
  `;
}

/** The latency panel while the summary is loading. */
export function LatencySkeleton() {
  return html`
    <${Panel} title="Latency">
      <div class="stack" style="--gap:var(--space-4)" aria-busy="true">
        <${Skeleton} lines=${3} />
        <${Skeleton} lines=${2} />
        <hr />
        <${Skeleton} lines=${4} />
      </div>
    <//>
  `;
}

/**
 * The summary loaded but the time series did not, and there is no earlier
 * one for this range and group to keep showing: say so where the charts
 * would be.
 */
export function ChartsError({ error, onRetry, retrying, side }) {
  return html`
    <div class="usage-split">
      <${Panel} title="Requests over time" flush>
        <${ErrorState} title="Could not load the time series" error=${error} onRetry=${onRetry} retrying=${retrying} />
      <//>
      ${side}
    </div>
  `;
}
