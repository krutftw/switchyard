// Usage page: one breakdown table (by model, by provider or by client key).
// A row opens the Requests page filtered to that row.
//
// The table has eight columns and lives in a column whose width depends on
// the window and on the sidebar, so it picks its layout from the width it
// actually has (not from the viewport):
//
//   full     all eight columns, the share as a bar
//   compact  six columns: errors with their rate, tokens in / out, and the
//            share as a percentage
//   cards    each row a card of labelled values (also the phone layout,
//            which the shared Table switches to by itself)
//
// Its order and filter live in the URL (group.sortParam, group.filterParam),
// so a link reproduces the table and the CSV export can write the same rows.

import { html, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from '../../../vendor/preact-htm.js';
import { Button, Input, Meter, Panel, Table } from '../../components/index.js';
import { DASH, formatCompact, formatCurrency, formatNumber, formatPercent, formatTokens, plural } from '../../lib/format.js';
import { useIsPhone, useSize } from '../../lib/hooks.js';
import { navigate, useQueryParam } from '../../lib/router.js';
import { DEFAULT_SORT, OTHER_COLOR, parseSort, slotColor, viewRows } from './data.js';

/** Rows shown before "Show all": enough for any real fleet's top, short enough to scan. */
const PAGE = 25;
/** The filter box appears once the list is longer than a glance. */
const FILTER_FROM = 9;

// Column widths in px. They are set, not left to the content, so the three
// tables line up under each other and the name column takes what is left.
// (A cell that needs more still gets it: these are preferred widths.)
const WIDTHS = {
  full: { requests: 104, errors: 90, errorRate: 114, tokensIn: 104, tokensOut: 116, cost: 96, share: 166, shareTight: 120 },
  compact: { requests: 102, errors: 108, tokensIn: 154, cost: 92, share: 80 },
};
// The widths of the table's own box from which each layout fits: the sum of
// its columns above, plus a name column that shows an ordinary name on one
// line. From ROOMY_FROM the share column has its full heading and bar.
const ROOMY_FROM = 1080;
const FULL_FROM = 980;
const COMPACT_FROM = 716;
const TIERS = ['full', 'compact', 'cards'];
const px = (value) => `${value}px`;

function tierFor(width, phone) {
  if (phone) return 'phone';
  // Not measured yet (the first render): assume there is room.
  if (!width || width >= FULL_FROM) return 'full';
  return width >= COMPACT_FROM ? 'compact' : 'cards';
}

// Counts and money keep their width bounded, so a very busy gateway does not
// push the table out of its panel: full figures up to six digits, then 1.2M.
const count = (value) => (value < 1_000_000 ? formatNumber(value) : formatCompact(value));
// A row without cost has no price that matches it (or only failures): that
// is "no value", a dash, not $0.00.
const money = (value) => (value > 0 ? (value < 1_000_000 ? formatCurrency(value) : `$${formatCompact(value)}`) : DASH);
const rate = (value) => (value == null ? DASH : formatPercent(value));

/**
 * A long identifier with places to break: after "/", ":", "_", "." and "@"
 * (a hyphen breaks by itself). The name wraps there first and is cut inside
 * a word only when one part alone is longer than the line. <wbr> adds no
 * character, so the name is still selected and copied exactly.
 */
function breakable(name) {
  const parts = (String(name).match(/[^/:_.@]*[/:_.@]?/g) ?? []).filter(Boolean);
  return parts.length === 1 ? name : parts.map((part, i) => (i === 0 ? part : [html`<wbr />`, part]));
}

/**
 * group     GROUPS entry (data.js)
 * rows      its breakdown rows; undefined while loading
 * range     RANGES entry
 * since     start of the summary's window, from the API
 * slots     colour slots of the group when the charts above are stacked by
 *           it: rows with a slot wear their chart colour. null for a table
 *           the charts do not show, which then has no colour keys at all.
 * loading   first load: skeleton rows
 * stale     these are the numbers of the previous range, dimmed while the
 *           new range loads
 * stickyTop px the page's own sticky controls take below the top bar, so the
 *           column headings stop under them
 */
export default function Breakdown({ group, rows, range, since, slots, loading, stale, stickyTop = 0 }) {
  const [sort, setSort] = useQueryParam(group.sortParam, DEFAULT_SORT);
  const [query, setQuery] = useQueryParam(group.filterParam, '');
  const [showAll, setShowAll] = useState(false);
  useEffect(() => setShowAll(false), [range.value]);

  // ---- Layout: by the width the table has ---------------------------------

  const phone = useIsPhone();
  const [sizeRef, size] = useSize();
  const box = useRef(null);
  const setBox = useCallback(
    (el) => {
      box.current = el;
      sizeRef(el);
    },
    [sizeRef],
  );
  // The widths above are estimates. If the table still comes out wider than
  // its box (a larger font, unusually long numbers), step down one layout
  // instead of cutting columns off; a new width starts over.
  const [squeeze, setSqueeze] = useState({ width: 0, steps: 0 });
  const steps = squeeze.width === size.width ? squeeze.steps : 0;
  const base = tierFor(size.width, phone);
  const tier = base === 'phone' ? base : TIERS[Math.min(TIERS.length - 1, TIERS.indexOf(base) + steps)];
  // Only the full table can be short of room for its share column; a card
  // always has the whole label.
  const roomy = tier !== 'full' || !size.width || size.width >= ROOMY_FROM;
  useLayoutEffect(() => {
    const el = box.current;
    if (!el || tier === 'cards' || tier === 'phone') return;
    const table = el.querySelector('table');
    if (table && table.offsetWidth > el.clientWidth + 1) setSqueeze({ width: size.width, steps: steps + 1 });
  });

  // ---- Columns ------------------------------------------------------------

  const activeSort = parseSort(sort);
  const slotOf = useMemo(() => (slots ? new Map(slots.map((name, slot) => [name, slot]).filter(([name]) => name != null)) : null), [slots]);

  const columns = useMemo(() => {
    const name = {
      key: 'name',
      header: group.label,
      primary: true,
      sortable: true,
      render: (row) => {
        const slot = slotOf?.get(row.name);
        return html`
          <span class="usage-name">
            ${slotOf && html`<span class="chart-key" data-shape="rect" style=${`--key:${slot == null ? OTHER_COLOR : slotColor(slot)}`} aria-hidden="true"></span>`}
            <span class="usage-name-body">
              <span class="usage-name-text mono">${row.name ? breakable(row.name) : '(no name)'}</span>
              ${row.note && html`<span class="usage-name-note">${row.note}</span>`}
            </span>
          </span>
        `;
      },
    };
    // Numbers: right-aligned, tabular, sortable, at the layout's set width.
    const w = WIDTHS[tier === 'compact' ? 'compact' : 'full'];
    const num = (key, header, render) => ({ key, header, align: 'right', num: true, sortable: true, width: px(w[key]), render });
    const requests = num('requests', 'Requests', (row) => count(row.requests));
    const cost = num('cost', 'Cost', (row) => money(row.cost));

    if (tier === 'compact') {
      return [
        name,
        requests,
        num('errors', 'Errors', (row) => html`<span class="usage-pair"><span>${count(row.errors)}</span><span class="usage-pair-rate">${rate(row.errorRate)}</span></span>`),
        num('tokensIn', 'Tokens in / out', (row) => html`<span class="usage-pair"><span>${formatTokens(row.tokensIn)}</span><span class="usage-pair-sep">/</span><span class="usage-pair-out">${formatTokens(row.tokensOut)}</span></span>`),
        cost,
        num('share', 'Share', (row) => formatPercent(row.share)),
      ];
    }

    return [
      name,
      requests,
      num('errors', 'Errors', (row) => count(row.errors)),
      num('errorRate', 'Error rate', (row) => rate(row.errorRate)),
      num('tokensIn', 'Tokens in', (row) => formatTokens(row.tokensIn)),
      num('tokensOut', 'Tokens out', (row) => formatTokens(row.tokensOut)),
      cost,
      {
        key: 'share',
        // Just fitting: a shorter heading and bar leave the names their line.
        header: roomy ? 'Share of requests' : 'Share',
        sortable: true,
        width: px(roomy ? w.share : w.shareTight),
        render: (row) => html`<${Meter} class="usage-share" value=${row.share} text=${formatPercent(row.share)} label=${`${row.name}: share of requests`} />`,
      },
    ];
  }, [group.label, slotOf, tier, roomy]);

  // ---- Rows ---------------------------------------------------------------

  const all = rows ?? [];
  const sorted = useMemo(() => viewRows(rows, query, sort), [rows, query, sort]);
  const shown = showAll ? sorted : sorted.slice(0, PAGE);
  const hidden = sorted.length - shown.length;
  const filtering = String(query ?? '').trim() !== '';
  // "Clear filter" sits in the empty state and leaves with it: the focus goes
  // to the filter box instead of nowhere.
  const filterBox = useRef(null);
  const clearFilter = () => {
    setQuery('');
    filterBox.current?.focus();
  };

  // In the compact layout two columns each stand for two sort keys. The
  // heading lights up for either, so an order chosen in the full layout (or
  // in a link) is still marked, and pressing it reverses that order instead
  // of switching to the column's own key.
  const headingSort = tier === 'compact' ? { ...activeSort, key: { errorRate: 'errors', tokensOut: 'tokensIn' }[activeSort.key] ?? activeSort.key } : activeSort;
  const changeSort = (next) => {
    if (!next) return setSort(DEFAULT_SORT);
    const key = next.key === headingSort.key ? activeSort.key : next.key;
    return setSort(`${key}-${next.dir}`);
  };

  // Every row is a filter of the request list, the stand-in names included
  // ("unknown", "anonymous": see GROUPS in data.js).
  const open = (row) => navigate('/requests', { query: { [group.param]: row.name, since } });

  const description = loading && !rows ? null : `${plural(all.length, group.noun, group.plural)} in ${range.phrase}. Select a row to see its requests.`;

  return html`
    <${Panel}
      flush
      title=${`By ${group.noun}`}
      description=${description}
      actions=${all.length >= FILTER_FROM || filtering
        ? html`<${Input} class="usage-filter" size="sm" type="search" icon="search" value=${query} onChange=${setQuery} inputRef=${filterBox} clearLabel="Clear filter" placeholder="Filter by name" aria-label=${`Filter ${group.plural} by name`} />`
        : null}
      footer=${hidden > 0 || (showAll && sorted.length > PAGE)
        ? html`
            <span>Showing <span class="num">${formatNumber(shown.length)}</span> of <span class="num">${formatNumber(sorted.length)}</span> ${group.plural}</span>
            ${hidden > 0
              ? html`<${Button} size="sm" onClick=${() => setShowAll(true)}>Show all ${formatNumber(sorted.length)}<//>`
              : html`<${Button} size="sm" onClick=${() => setShowAll(false)}>Show the first ${PAGE}<//>`}
          `
        : null}
    >
      <div ref=${setBox} class="usage-table usage-fade" data-tier=${tier} data-stale=${stale ? '' : undefined} style=${`--usage-controls-h:${Math.max(0, Math.round(stickyTop))}px`}>
        <${Table}
          columns=${columns}
          rows=${rows ? shown : undefined}
          rowKey="name"
          sort=${headingSort}
          onSort=${changeSort}
          loading=${loading && !rows}
          onRowClick=${open}
          caption=${`Usage by ${group.noun}, ${range.phrase}`}
          empty=${filtering
            ? {
                icon: 'search',
                title: `No ${group.noun} matches "${String(query).trim()}"`,
                description: `${plural(all.length, group.noun, group.plural)} had traffic in ${range.phrase}.`,
                action: html`<${Button} size="sm" onClick=${clearFilter}>Clear filter<//>`,
              }
            : { icon: 'usage', title: `No ${group.plural} with traffic`, description: `Nothing was requested in ${range.phrase}.` }}
        />
      </div>
    <//>
  `;
}
