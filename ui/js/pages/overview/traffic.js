// Overview: requests over time, stacked by outcome. The series comes from
// /usage/timeseries and grows live as requests finish.

import { html, useMemo } from '../../../vendor/preact-htm.js';
import { BarChart, ErrorState, Panel, Segmented, Skeleton } from '../../components/index.js';
import { formatNumber, plural } from '../../lib/format.js';
import { useDebounced } from '../../lib/hooks.js';
import { useStore } from '../../lib/store.js';
import { tail } from './data.js';
import { trafficSeries } from './model.js';
import { described } from './stale.js';

const RANGES = [
  { value: '1h', label: '1h', title: 'Last hour, per minute' },
  { value: '24h', label: '24h', title: 'Last 24 hours, per hour' },
];

const COPY = {
  '1h': { per: 'Per minute, last hour', label: 'Requests per minute over the last hour, succeeded and failed', empty: 'No requests in the last hour' },
  '24h': { per: 'Per hour, last 24 hours', label: 'Requests per hour over the last 24 hours, succeeded and failed', empty: 'No requests in the last 24 hours' },
};

/**
 * series   the /usage/timeseries resource for `range` (useResource)
 * range    "1h" | "24h"
 * onRange  (range) => void
 */
export default function Traffic({ series, range, onRange }) {
  const finished = useStore(tail);
  const data = useMemo(() => trafficSeries(series.data, finished), [series.data, finished]);
  const chart = useMemo(
    () => [
      { key: 'ok', label: 'Succeeded', values: data.ok },
      // Failed is a status, so it wears the stop lamp's colour.
      { key: 'failed', label: 'Failed', color: 'var(--lamp-stop)', values: data.failed },
    ],
    [data],
  );
  // A background poll that answers at once should not make the plot blink.
  const slow = useDebounced(series.refreshing, 400);
  const copy = COPY[range] ?? COPY['1h'];
  const summary = series.data ? `${copy.per} · ${plural(data.requests, 'request')}${data.errors > 0 ? `, ${formatNumber(data.errors)} failed` : ''}` : copy.per;

  return html`
    <${Panel}
      class="overview-traffic"
      title="Traffic"
      description=${described(summary, series)}
      actions=${html`<${Segmented} size="sm" label="Time range of the traffic chart" value=${range} onChange=${onRange} options=${RANGES} />`}
    >
      ${series.loading
        ? html`<${Skeleton} height="248px" />`
        : series.error && !series.data
          ? html`<${ErrorState} compact title="Could not load the traffic chart" error=${series.error} onRetry=${series.refresh} retrying=${series.loading} />`
          : html`
              <${BarChart}
                x=${data.x}
                series=${chart}
                height=${220}
                stale=${series.refreshing && slow}
                valueFormat=${formatNumber}
                label=${copy.label}
                emptyText=${copy.empty}
              />
            `}
    <//>
  `;
}
