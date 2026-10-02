// Models page, "Models" tab: the client-facing model table with search,
// filters, sorting and paging. Rows come from logic.js (buildRows).

import { html, useMemo, useRef } from '../../../vendor/preact-htm.js';
import { Badge, Button, CopyButton, Icon, Input, Notice, Pagination, Panel, Select, StatusLamp, Table, sortRows } from '../../components/index.js';
import { DASH, formatNumber, formatTokens, plural } from '../../lib/format.js';
import { useHotkey, useNow } from '../../lib/hooks.js';
import { href, setQuery, useQueryParam } from '../../lib/router.js';
import { cooldownReason, timeLeft } from './logic.js';

const PAGE_SIZE = 50;
/** Routes shown in a table cell before the rest fold into "+N more". */
const ROUTES_SHOWN = 3;

const AVAILABILITY_OPTIONS = [
  { value: 'routable', label: 'Routable now' },
  { value: 'cooling', label: 'All cooling' },
  { value: 'unusable', label: 'No usable credentials' },
];

/** "back in 41s", ticking. Its own component so only this text re-renders each second. */
export function BackIn({ until, reason, prefix = 'back in', suffix = '' }) {
  const now = useNow();
  const why = cooldownReason(reason);
  if (!until || until <= now) return why ? html`<span>${why}${suffix}</span>` : null;
  return html`<span>${why ? `${why}, ` : ''}${prefix} <span class="num">${timeLeft(until, now)}</span>${suffix}</span>`;
}

/** Entry kind: model or alias, plus "hidden" when an alias hides it from listings. */
export function KindBadges({ row }) {
  return html`
    <span class="models-kind">
      ${row.isAlias ? html`<${Badge}>Alias<//>` : html`<span class="models-kind-plain">Model</span>`}
      ${row.hidden && html`<${Badge} outline title="Left out of the model lists clients fetch; requests that name it still work"><${Icon} name="eye-off" size=${12} />Hidden<//>`}
    </span>
  `;
}

/** One route in a cell: lamp, provider, the upstream model when it differs, credentials. */
function RouteLine({ route, modelName }) {
  return html`
    <li class="models-route">
      <${StatusLamp} tone=${route.state} title=${route.label} />
      <span class="models-route-text">
        <span class="mono">${route.provider}</span>
        ${route.upstream_model !== modelName &&
        html`<span class="models-route-up"><${Icon} name="arrow-right" size=${12} /><span class="mono">${route.upstream_model}</span></span>`}
      </span>
      <span class="models-route-count num" aria-hidden="true">${route.credentials_available}/${route.credentials_total}</span>
      <span class="sr-only">${route.label}: ${route.credentials_available} of ${plural(route.credentials_total, 'credential')} available</span>
    </li>
  `;
}

function RoutesCell({ row }) {
  if (row.routes.length === 0) {
    return html`<span class="faint">${row.isAlias ? 'No routable target' : 'No route'}</span>`;
  }
  const shown = row.routes.slice(0, ROUTES_SHOWN);
  const more = row.routes.length - shown.length;
  return html`
    <ul class="models-routes">
      ${shown.map((route) => html`<${RouteLine} key=${route.index} route=${route} modelName=${row.name} />`)}
      ${more > 0 && html`<li class="models-route-more faint">and ${plural(more, 'more route')}</li>`}
    </ul>
  `;
}

function NameCell({ row }) {
  return html`
    <div class="models-name">
      <span class="models-name-line">
        <span class="mono models-name-id" title=${row.name}>${row.name}</span>
        <${CopyButton} value=${row.name} label="Copy model name" />
      </span>
      ${row.isAlias
        ? html`<span class="models-name-sub" title=${row.aliasTargets.join(', ')}><${Icon} name="arrow-right" size=${12} /><span class="mono truncate">${row.aliasTargets.join(', ')}</span></span>`
        : row.info.display_name && html`<span class="models-name-sub"><span class="truncate">${row.info.display_name}</span></span>`}
    </div>
  `;
}

/**
 * Why no provider serves a model, and the one thing to do about it: what
 * the table says when it has no model to show. Returns the props of an
 * EmptyState, plus a `tone` for when it is shown as a Notice.
 *
 * providers   GET /providers data, undefined when it is not loaded
 * onRetry     refetch the model table
 */
function whyNoModels(providers, onRetry) {
  const open = html`<${Button} href=${href('/providers')}>Open providers<//>`;
  if (!providers) {
    return { tone: 'caution', icon: 'models', title: 'No models are listed', description: 'No provider serves a model right now.', action: open };
  }
  if (providers.length === 0) {
    return {
      tone: 'info',
      icon: 'providers',
      title: 'No providers yet',
      description: 'Models come from providers. Add one and the names clients can ask for are listed here, with where each is routed.',
      action: html`<${Button} variant="primary" icon="plus" href=${href('/providers')}>Add a provider<//>`,
    };
  }
  const enabled = providers.filter((p) => p.enabled !== false);
  if (enabled.length === 0) {
    return {
      tone: 'caution',
      icon: 'providers',
      title: providers.length === 1 ? 'The only provider is switched off' : 'Every provider is switched off',
      description: 'A provider that is switched off serves no models. Enable one to list its models here.',
      action: open,
    };
  }
  const discovering = enabled.filter((p) => p.discover && (p.models?.length ?? 0) === 0);
  if (discovering.length > 0) {
    const one = discovering.length === 1;
    return {
      tone: 'info',
      icon: 'search',
      title: 'Waiting for model lists',
      description: `The gateway asks ${one ? discovering[0].name : `${discovering.length} providers`} which models ${one ? 'it serves' : 'they serve'}. They appear here as soon as an answer arrives; a provider that cannot be reached lists none.`,
      action: html`<${Button} icon="refresh" onClick=${onRetry}>Check again<//><${Button} variant="ghost" href=${href('/providers')}>Open providers<//>`,
    };
  }
  return {
    tone: 'caution',
    icon: 'models',
    title: 'No models are listed',
    description: 'The providers list no models. Give a provider a model list, or turn on discovery for it.',
    action: open,
  };
}

/**
 * rows        table rows (logic.js buildRows), undefined while loading
 * providers   GET /providers data (for the provider filter and empty states)
 * providersLoading  that list is on its first load: too early to say why
 *             there are no models
 * loading, error, onRetry: of the model table
 * open        name of the model shown in the drawer
 * onOpen      (name) => void
 */
export default function ModelsTab({ rows, providers, providersLoading = false, loading, error, onRetry, open, onOpen }) {
  const [q] = useQueryParam('q', '');
  const [provider] = useQueryParam('provider', '');
  const [avail] = useQueryParam('avail', '');
  const [sortParam] = useQueryParam('sort', 'name:asc');
  const [pageParam] = useQueryParam('page', '1');
  const search = useRef(null);

  // A filter change starts again at the first page.
  const setFilter = (patch) => setQuery({ ...patch, page: null });
  // Not while the drawer is open: the search box is behind it, outside its focus trap.
  useHotkey('/', () => search.current?.focus(), { enabled: !open });

  const columns = useMemo(
    () => [
      { key: 'name', header: 'Model', primary: true, sortable: true, render: (row) => html`<${NameCell} row=${row} />` },
      { key: 'kind', header: 'Kind', sortable: true, sortValue: (row) => `${row.kind}${row.hidden ? ' hidden' : ''}`, render: (row) => html`<${KindBadges} row=${row} />` },
      {
        key: 'availability',
        header: 'Availability',
        sortable: true,
        sortValue: (row) => row.availability.rank,
        render: (row) => html`
          <${StatusLamp}
            tone=${row.availability.tone}
            label=${row.availability.label}
            detail=${row.availability.until ? html`<${BackIn} until=${row.availability.until} />` : row.availability.detail}
          />
        `,
      },
      { key: 'routes', header: 'Routes', sortable: true, sortValue: (row) => row.routes.length, render: (row) => html`<${RoutesCell} row=${row} />` },
      {
        key: 'context',
        header: 'Context',
        align: 'right',
        num: true,
        sortable: true,
        sortValue: (row) => row.info.context_window,
        render: (row) => (row.info.context_window ? html`<span title=${`${formatNumber(row.info.context_window)} tokens`}>${formatTokens(row.info.context_window)}</span>` : DASH),
      },
      {
        key: 'output',
        header: 'Max output',
        align: 'right',
        num: true,
        sortable: true,
        sortValue: (row) => row.info.max_output_tokens,
        render: (row) => (row.info.max_output_tokens ? html`<span title=${`${formatNumber(row.info.max_output_tokens)} tokens`}>${formatTokens(row.info.max_output_tokens)}</span>` : DASH),
      },
      {
        key: 'reasoning',
        header: 'Reasoning',
        sortable: true,
        sortValue: (row) => row.reasoning.rank,
        render: (row) => (row.reasoning.text ? html`<span class=${row.reasoning.rank === 1 ? 'faint' : undefined}>${row.reasoning.text}</span>` : html`<span class="faint" title=${row.reasoning.title}>${DASH}</span>`),
      },
    ],
    [],
  );

  const providerOptions = useMemo(() => {
    const names = new Set((providers ?? []).map((p) => p.name));
    for (const row of rows ?? []) for (const name of row.providerNames) names.add(name);
    // A provider named in a link that no longer exists stays selectable, so the filter can be seen and cleared.
    if (provider) names.add(provider);
    return [...names].sort((a, b) => a.localeCompare(b));
  }, [providers, rows, provider]);

  const filtered = useMemo(() => {
    if (!rows) return undefined;
    const needle = q.trim().toLowerCase();
    return rows.filter((row) => {
      if (needle && !row.search.includes(needle)) return false;
      if (provider && !row.providerNames.includes(provider)) return false;
      if (avail === 'routable' && row.availability.key !== 'routable') return false;
      if (avail === 'cooling' && row.availability.key !== 'cooling') return false;
      if (avail === 'unusable' && row.availability.key !== 'nocreds' && row.availability.key !== 'noroute') return false;
      return true;
    });
  }, [rows, q, provider, avail]);

  const [sortKey, sortDir] = sortParam.split(':');
  const sort = columns.some((c) => c.key === sortKey) ? { key: sortKey, dir: sortDir === 'desc' ? 'desc' : 'asc' } : { key: 'name', dir: 'asc' };
  const sorted = useMemo(() => (filtered ? sortRows(filtered, columns.find((c) => c.key === sort.key), sort.dir) : undefined), [filtered, columns, sort.key, sort.dir]);

  const total = sorted?.length ?? 0;
  const pageCount = Math.max(1, Math.ceil(total / PAGE_SIZE));
  const page = Math.min(Math.max(1, Number.parseInt(pageParam, 10) || 1), pageCount);
  const pageRows = sorted?.slice((page - 1) * PAGE_SIZE, page * PAGE_SIZE);

  const filtering = !!(q.trim() || provider || avail);
  const clear = () => setQuery({ q: null, provider: null, avail: null, page: null });

  // The gateway lists every alias, also one with nothing to route to, so
  // "no models" is decided from what providers serve: a model of the table,
  // or an alias that routes (its targets are served, even when an alias has
  // taken their name). With nothing served the reason is said: in place of
  // the table when it is empty, above it when only dead aliases are left.
  const served = rows ? rows.filter((row) => !row.isAlias || row.routes.length > 0).length : null;
  const noModels = served === 0 && !providersLoading ? whyNoModels(providers, onRetry) : null;
  const onlyAliases = noModels && rows.length > 0;

  let empty;
  if (filtering && rows?.length) {
    empty = {
      icon: 'filter',
      title: onlyAliases ? 'No alias matches these filters' : 'No model matches these filters',
      description: q.trim()
        ? `None of the ${plural(rows.length, onlyAliases ? 'alias' : 'model', onlyAliases ? 'aliases' : undefined)} match. Check the spelling, or clear the filters to see them all.`
        : `None of the ${plural(rows.length, onlyAliases ? 'alias' : 'model', onlyAliases ? 'aliases' : undefined)} is in that state right now. Clear the filters to see them all.`,
      action: html`<${Button} onClick=${clear}>Clear filters<//>`,
    };
  } else {
    empty = noModels ?? whyNoModels(providers, onRetry);
  }

  const count = !rows ? null : filtering ? `${formatNumber(total)} of ${plural(rows.length, 'model')}` : plural(rows.length, 'model');

  return html`
    ${onlyAliases &&
    html`
      <${Notice} tone=${noModels.tone} title=${noModels.title} action=${noModels.action}>
        ${noModels.description} Until then the ${rows.length === 1 ? 'alias below has' : `${rows.length} aliases below have`} nothing to route to.
      <//>
    `}
    <${Panel} flush footer=${total > PAGE_SIZE ? html`<${Pagination} class="grow" page=${page} pageSize=${PAGE_SIZE} total=${total} noun="models" onPage=${(next) => setQuery({ page: next === 1 ? null : String(next) })} />` : null}>
      <div class="models-toolbar" role="search" aria-label="Filter models">
        <${Input}
          class="models-search"
          type="search"
          icon="search"
          value=${q}
          onChange=${(value) => setFilter({ q: value || null })}
          inputRef=${search}
          placeholder="Search names, providers, upstream models"
          aria-label="Search models"
          aria-keyshortcuts="/"
        />
        <${Select}
          class="models-filter"
          value=${provider}
          onChange=${(value) => setFilter({ provider: value || null })}
          placeholder="All providers"
          options=${providerOptions}
          aria-label="Filter by provider"
        />
        <${Select}
          class="models-filter"
          value=${avail}
          onChange=${(value) => setFilter({ avail: value || null })}
          placeholder="Any availability"
          options=${AVAILABILITY_OPTIONS}
          aria-label="Filter by availability"
        />
        <span class="models-toolbar-end">
          ${filtering && html`<${Button} variant="ghost" size="sm" icon="x" onClick=${clear}>Clear<//>`}
          ${count && html`<span class="models-count" role="status">${count}</span>`}
        </span>
      </div>
      <${Table}
        class="models-table"
        columns=${columns}
        rows=${pageRows}
        rowKey="name"
        sort=${sort}
        onSort=${(next) => setQuery({ sort: !next || (next.key === 'name' && next.dir === 'asc') ? null : `${next.key}:${next.dir}`, page: null })}
        loading=${loading || providersLoading}
        error=${error}
        onRetry=${onRetry}
        onRowClick=${(row) => onOpen(row.name)}
        selectedKey=${open || null}
        empty=${empty}
        skeletonRows=${8}
        caption="Client-facing models and where each is routed"
      />
    <//>
  `;
}
