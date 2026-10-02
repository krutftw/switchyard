// Models page, "Catalog" tab: the metadata built into the gateway for
// well-known models (GET /catalog). Read-only.

import { html, useMemo } from '../../../vendor/preact-htm.js';
import { Badge, Button, CopyButton, Input, Notice, Panel, Segmented, Table, sortRows } from '../../components/index.js';
import { DASH, formatDate, formatNumber, formatTokens, plural } from '../../lib/format.js';
import { href, setQuery, useQueryParam } from '../../lib/router.js';
import { reasoningSummary } from './logic.js';

const DEFAULT_SORT = { key: 'id', dir: 'asc' };
const FAMILY_LABEL = { openai: 'OpenAI', anthropic: 'Anthropic', google: 'Google' };
const familyLabel = (family) => FAMILY_LABEL[family] ?? family ?? 'Other';

/**
 * catalog   GET /catalog data (undefined while loading)
 * loading, error, onRetry   of that request
 * served    Map(upstream model id -> [provider names]) from the model table:
 *           which catalog entries this gateway routes to right now
 */
export default function CatalogTab({ catalog, loading, error, onRetry, served }) {
  const [q] = useQueryParam('cq', '');
  const [family] = useQueryParam('family', '');
  const [sortParam] = useQueryParam('csort', '');

  const rows = useMemo(
    () =>
      (catalog ?? []).map((entry) => ({
        ...entry,
        reasoning: reasoningSummary(entry),
        providers: served.get(entry.id) ?? [],
        search: [entry.id, entry.display_name, entry.owned_by, entry.family, ...(entry.kinds ?? [])].filter(Boolean).join('\n').toLowerCase(),
      })),
    [catalog, served],
  );

  const families = useMemo(() => [...new Set(rows.map((r) => r.family).filter(Boolean))], [rows]);

  const filtered = useMemo(() => {
    const needle = q.trim().toLowerCase();
    return rows.filter((row) => (!needle || row.search.includes(needle)) && (!family || row.family === family));
  }, [rows, q, family]);

  const filtering = !!(q.trim() || family);
  const clear = () => setQuery({ cq: null, family: null });

  const columns = useMemo(
    () => [
      {
        key: 'id',
        header: 'Model id',
        primary: true,
        sortable: true,
        render: (row) => html`
          <span class="models-name-line">
            <span class="mono models-name-id" title=${row.id}>${row.id}</span>
            <${CopyButton} value=${row.id} label="Copy model id" />
          </span>
        `,
      },
      { key: 'display_name', header: 'Display name', sortable: true, render: (row) => row.display_name ?? DASH },
      { key: 'family', header: 'Family', sortable: true, render: (row) => familyLabel(row.family) },
      {
        key: 'kinds',
        header: 'Provider kinds',
        hideOnPhone: true,
        sortValue: (row) => (row.kinds ?? []).join(','),
        render: (row) => (row.kinds?.length ? html`<span class="models-kind">${row.kinds.map((kind) => html`<${Badge} key=${kind} mono outline>${kind}<//>`)}</span>` : html`<span class="faint">Every kind of the family</span>`),
      },
      {
        key: 'context_window',
        header: 'Context',
        align: 'right',
        num: true,
        sortable: true,
        render: (row) => (row.context_window ? html`<span title=${`${formatNumber(row.context_window)} tokens`}>${formatTokens(row.context_window)}</span>` : DASH),
      },
      {
        key: 'max_output_tokens',
        header: 'Max output',
        align: 'right',
        num: true,
        sortable: true,
        render: (row) => (row.max_output_tokens ? html`<span title=${`${formatNumber(row.max_output_tokens)} tokens`}>${formatTokens(row.max_output_tokens)}</span>` : DASH),
      },
      {
        key: 'reasoning',
        header: 'Reasoning',
        sortable: true,
        sortValue: (row) => row.reasoning.rank,
        render: (row) => html`<span class=${row.reasoning.rank <= 1 ? 'faint' : undefined}>${row.reasoning.text ?? DASH}</span>`,
      },
      { key: 'created', header: 'Released', sortable: true, align: 'right', num: true, render: (row) => (row.created ? formatDate(row.created) : DASH) },
      {
        key: 'providers',
        header: 'Routed here by',
        sortable: true,
        sortValue: (row) => (row.providers.length ? row.providers.join(',') : null),
        render: (row) => (row.providers.length ? html`<span class="mono">${row.providers.join(', ')}</span>` : html`<span class="faint">${DASH}</span>`),
      },
    ],
    [],
  );

  // The order is part of the view, so it is in the URL like the filters (?csort=context_window:desc).
  const [sortKey, sortDir] = sortParam.split(':');
  const sort = columns.some((c) => c.sortable && c.key === sortKey) ? { key: sortKey, dir: sortDir === 'desc' ? 'desc' : 'asc' } : DEFAULT_SORT;
  const sorted = useMemo(() => sortRows(filtered, columns.find((c) => c.key === sort.key), sort.dir), [filtered, columns, sort.key, sort.dir]);

  const count = !catalog ? null : filtering ? `${formatNumber(filtered.length)} of ${plural(rows.length, 'entry', 'entries')}` : plural(rows.length, 'entry', 'entries');

  return html`
    <${Notice} tone="info" title="Built into the gateway, read-only">
      This is what the gateway knows about well-known models before you configure anything: limits and reasoning support, keyed by the vendor's model id. A model entry in a provider's settings (display name, context window, max output, reasoning) overrides it for that provider. Models that are not listed still work; their requests pass through without being fitted. <a href=${href('/providers')}>Open providers</a>
    <//>
    <${Panel} flush>
      <div class="models-toolbar" role="search" aria-label="Filter the catalog">
        <${Input}
          class="models-search"
          type="search"
          icon="search"
          value=${q}
          onChange=${(value) => setQuery({ cq: value || null })}
          placeholder="Search ids, names, vendors"
          aria-label="Search the catalog"
        />
        ${families.length > 1 &&
        html`
          <${Segmented}
            label="Family"
            size="sm"
            value=${family}
            onChange=${(value) => setQuery({ family: value || null })}
            options=${[{ value: '', label: 'All' }, ...families.map((f) => ({ value: f, label: familyLabel(f) }))]}
          />
        `}
        <span class="models-toolbar-end">
          ${filtering && html`<${Button} variant="ghost" size="sm" icon="x" onClick=${clear}>Clear<//>`}
          ${count && html`<span class="models-count" role="status">${count}</span>`}
        </span>
      </div>
      <${Table}
        class="models-table"
        columns=${columns}
        rows=${catalog ? sorted : undefined}
        rowKey="id"
        sort=${sort}
        onSort=${(next) => setQuery({ csort: !next || (next.key === DEFAULT_SORT.key && next.dir === DEFAULT_SORT.dir) ? null : `${next.key}:${next.dir}` })}
        loading=${loading}
        error=${error}
        onRetry=${onRetry}
        empty=${filtering
          ? {
              icon: 'filter',
              title: 'No catalog entry matches',
              description: 'A model that is not in the catalog can still be served: give it a model entry in the provider’s settings.',
              action: html`<${Button} onClick=${clear}>Clear filters<//>`,
            }
          : { icon: 'models', title: 'The catalog is empty', description: 'This build of the gateway carries no built-in model metadata.' }}
        caption="Built-in model catalog"
      />
    <//>
  `;
}
