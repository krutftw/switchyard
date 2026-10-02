// Overview: who is using the gateway. The busiest models and client keys of
// the last 24 hours, from /usage/summary.

import { html } from '../../../vendor/preact-htm.js';
import { BarList, ErrorState, Panel, Skeleton } from '../../components/index.js';
import { formatCompact, formatPercent } from '../../lib/format.js';
import { href } from '../../lib/router.js';
import { described } from './stale.js';

const LIMIT = 6;

// Names the gateway uses for "none": they are not a model or a key to filter by.
const PLACEHOLDERS = new Set(['unknown', 'other']);

function toItems(entries, param) {
  return (entries ?? []).map((entry) => ({
    key: entry.name,
    label: entry.name,
    value: entry.requests,
    hint: entry.errors > 0 ? `${formatPercent(entry.errors / entry.requests, 0)} failed` : undefined,
    href: PLACEHOLDERS.has(entry.name) ? undefined : href('/requests', { [param]: entry.name }),
  }));
}

function TopPanel({ title, summary, items, emptyText, errorTitle }) {
  return html`
    <${Panel} title=${title} description=${described('By requests, last 24 hours', summary)} footer=${html`<span>Select a name to see its requests</span><a href=${href('/usage')}>Usage</a>`}>
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
      />
      <${TopPanel}
        title="Top client keys"
        summary=${summary}
        items=${toItems(data?.by_key, 'key')}
        errorTitle="Could not load the top client keys"
        emptyText="No requests in the last 24 hours"
      />
    </div>
  `;
}
