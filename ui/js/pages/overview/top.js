// Overview: who is using the gateway. The busiest models and client keys of
// the last 24 hours, from /usage/summary.

import { html } from '../../../vendor/preact-htm.js';
import { BarList, ErrorState, Panel, Skeleton } from '../../components/index.js';
import { formatCompact, formatPercent } from '../../lib/format.js';
import { href } from '../../lib/router.js';
import { described } from './stale.js';

const LIMIT = 6;

// A row that stands for several names is not one to filter by. ("unknown",
// the row of requests that had no model, is: GET /requests?model=unknown
// selects them.)
const PLACEHOLDERS = new Set(['other']);

// A row counts the requests under one client-facing model, but the list it
// opens is GET /requests?model=, which also matches the requested and the
// upstream model: requests that another name (lab/mock-echo) sent upstream
// as mock-echo are in the list of mock-echo and not in its count. No filter
// selects the client-facing model alone, so the footer says what the list
// holds.
const MODEL_FOOTNOTE = 'Select a model to see its requests, plus any routed upstream to a model of that name';

function toItems(entries, param) {
  return (entries ?? []).map((entry) => ({
    key: entry.name,
    label: entry.name,
    value: entry.requests,
    hint: entry.errors > 0 ? `${formatPercent(entry.errors / entry.requests, 0)} failed` : undefined,
    href: PLACEHOLDERS.has(entry.name) ? undefined : href('/requests', { [param]: entry.name }),
  }));
}

function TopPanel({ title, summary, items, emptyText, errorTitle, footnote }) {
  return html`
    <${Panel} title=${title} description=${described('By requests, last 24 hours', summary)} footer=${html`<span>${footnote}</span><a href=${href('/usage')}>Usage</a>`}>
      ${summary.loading
        ? html`<${Skeleton} lines=${5} />`
        : summary.error && !summary.data
          ? html`<${ErrorState} compact title=${errorTitle} error=${summary.error} onRetry=${summary.refresh} />`
          : html`<${BarList} items=${items} format=${formatCompact} share limit=${LIMIT} emptyText=${emptyText} />`}
    <//>
  `;
}

/** summary  /usage/summary?range=24h (useResource) */
export default function TopLists({ summary }) {
  const data = summary.data;
  return html`
    <div class="overview-tops">
      <${TopPanel}
        title="Top models"
        summary=${summary}
        items=${toItems(data?.by_model, 'model')}
        errorTitle="Could not load the top models"
        emptyText="No requests in the last 24 hours"
        footnote=${MODEL_FOOTNOTE}
      />
      <${TopPanel}
        title="Top client keys"
        summary=${summary}
        items=${toItems(data?.by_key, 'key')}
        errorTitle="Could not load the top client keys"
        emptyText="No requests in the last 24 hours"
        footnote="Select a key to see its requests"
      />
    </div>
  `;
}
