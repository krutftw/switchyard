// Table: sortable, sticky header, clickable rows, built-in loading / empty /
// error states, and a card layout on phones.
//
//   const columns = [
//     { key: 'name', header: 'Provider', primary: true, sortable: true,
//       render: (p) => html`<span class="mono">${p.name}</span>` },
//     { key: 'state', header: 'State', render: (p) => html`<${StatusLamp} tone=${p.tone} label=${p.state} />` },
//     { key: 'requests', header: 'Requests', align: 'right', num: true, sortable: true,
//       render: (p) => formatCompact(p.requests) },
//   ];
//   html`<${Panel} title="Providers" flush>
//     <${Table} columns=${columns} rows=${data} rowKey="name"
//               loading=${res.loading} error=${res.error} onRetry=${res.refresh}
//               onRowClick=${(p) => navigate('/providers', { query: { open: p.name } })}
//               empty=${{ title: 'No providers yet', description: 'Add one to start routing.' }} />
//   <//>`

import { Component, html, useCallback, useLayoutEffect, useMemo, useRef, useState } from '../../vendor/preact-htm.js';
import { cx } from '../lib/dom.js';
import { Icon } from './icons.js';
import { EmptyState, ErrorState, Skeleton } from './surface.js';

/**
 * Sort rows by a column. Exported so a page that sorts on the server can
 * still reuse the comparison for local data.
 */
export function sortRows(rows, column, dir) {
  if (!column) return rows;
  const value = column.sortValue ?? ((row) => row[column.key]);
  const sign = dir === 'desc' ? -1 : 1;
  return [...rows].sort((a, b) => {
    const va = value(a);
    const vb = value(b);
    // Missing values sink to the bottom in either direction.
    if (va == null && vb == null) return 0;
    if (va == null) return 1;
    if (vb == null) return -1;
    if (typeof va === 'number' && typeof vb === 'number') return (va - vb) * sign;
    return String(va).localeCompare(String(vb), undefined, { numeric: true, sensitivity: 'base' }) * sign;
  });
}

/**
 * The column's name as plain text: the label of its cells on phone cards
 * (data-label) and its entry in the phone sort menu. `label` when given,
 * else `header` when that is text.
 */
function columnLabel(column) {
  if (column.label != null) return String(column.label);
  return typeof column.header === 'string' || typeof column.header === 'number' ? String(column.header) : '';
}

const cellAttrs = (column) => ({
  'data-align': column.align && column.align !== 'left' ? column.align : undefined,
  'data-num': column.num ? '' : undefined,
  'data-mono': column.mono ? '' : undefined,
});

/**
 * One body row. A class with shouldComponentUpdate (the vendored Preact has
 * no memo): the table renders again whenever a row is selected, a live row
 * arrives or the parent ticks, and with a few thousand rows on screen
 * rendering every cell each time is a visible pause. A row renders again
 * only when something it shows has changed:
 *
 *   row        the record, by identity: replace a record to update its row
 *   columns    the columns array, by identity: an array built on every
 *              render keeps all rows up to date (and gives up the saving);
 *              one from useMemo must list everything its `render`
 *              functions read
 *   selected, fresh, clickable
 *   index      only when a column's render takes it as a second argument
 */
class TableRow extends Component {
  shouldComponentUpdate(next) {
    const now = this.props;
    return (
      next.row !== now.row ||
      next.columns !== now.columns ||
      next.selected !== now.selected ||
      next.fresh !== now.fresh ||
      next.clickable !== now.clickable ||
      next.rowKey !== now.rowKey ||
      (next.usesIndex && next.index !== now.index)
    );
  }

  render({ row, index, rowKey, columns, selected, fresh, clickable, onActivate }) {
    return html`
      <tr
        role="row"
        data-row-key=${rowKey == null ? undefined : String(rowKey)}
        data-clickable=${clickable ? '' : undefined}
        data-selected=${selected ? '' : undefined}
        data-fresh=${fresh ? '' : undefined}
        tabindex=${clickable ? 0 : undefined}
        onClick=${clickable
          ? (event) => {
              // Buttons, links and inputs inside a row act on their own.
              if (event.target.closest('a, button, input, select, textarea, label')) return;
              // Dragging to select text is not a click.
              if (String(window.getSelection?.() ?? '').length > 0) return;
              onActivate(row, event);
            }
          : undefined}
        onKeyDown=${clickable
          ? (event) => {
              if (event.target !== event.currentTarget) return;
              if (event.key === 'Enter' || event.key === ' ') {
                event.preventDefault();
                onActivate(row, event);
              }
            }
          : undefined}
      >
        ${columns.map(
          (column) => html`
            <td
              role="cell"
              key=${column.key}
              data-label=${columnLabel(column)}
              data-primary=${column.primary ? '' : undefined}
              data-hide-phone=${column.hideOnPhone ? '' : undefined}
              ...${cellAttrs(column)}
            >
              ${column.render ? column.render(row, index) : row[column.key]}
            </td>
          `,
        )}
      </tr>
    `;
  }
}

/**
 * columns   [{
 *             key         unique id; also the row field read by default
 *             header      column heading: text, markup (an abbreviation with
 *                         its title, a visually hidden "Actions"), or
 *                         nothing
 *             label       the column's name as plain text, for the phone
 *                         card (label of the cell) and the phone sort menu.
 *                         Default: `header` when that is text. Give it when
 *                         the header is markup or empty.
 *             render      (row, index) => cell content; default row[key]
 *             align       "left" | "right" | "center"
 *             num         numeric cell: monospace, tabular figures, no wrap
 *             mono        monospace cell (identifiers)
 *             width       CSS width for the column
 *             sortable    header becomes a sort button
 *             sortValue   (row) => comparable value; default row[key]
 *             primary     on phones this cell becomes the card's title
 *             hideOnPhone drop this cell from the phone card
 *           }]
 * rows      array of records (undefined while loading)
 * rowKey    field name or (row) => key. Each row carries it in the DOM as
 *           data-row-key, so a page can find a row (to move focus to the
 *           neighbour of a deleted one, say) without counting.
 * sort      { key, dir: "asc" | "desc" } for controlled sorting, with onSort;
 *           leave both out and the table sorts locally (defaultSort sets the
 *           initial order). onSort(null) means "back to the default order"
 *           (the phone sort menu always offers it).
 * onRowClick (row) => void; rows become focusable and respond to Enter
 * selectedKey  key of the row shown in a drawer, highlighted
 * freshKeys    Set of keys that arrived live just now; they flash once
 * loading   first load: skeleton rows. With `error` set, a load in flight
 *           is the retry: the error state stays, its button spinning
 * error     ApiError: shown in place of rows when there are none to show
 * errorTitle  heading of that error state ("Could not load the providers");
 *           default "Could not load this"
 * onRetry   retry handler for the error state
 * empty     { title, description, action, icon } for the empty state
 * sticky    keep the header visible while the list scrolls (default true).
 *           A table that scrolls with the page keeps its header under the
 *           top bar; one with `maxHeight`, or one too wide for its box (it
 *           then scrolls sideways in that box), keeps it at the top of its
 *           own box. See --sticky-top in UI_GUIDE.md for a page with a
 *           sticky bar of its own.
 * maxHeight CSS max-height; the table scrolls inside it (implies a scroll box)
 * dense     32px rows for streams
 * collapse  card layout on phones (default true)
 * sortMenu  false leaves out the "Sort by" menu that collapsed tables show
 *           on phones, for a page that has its own order control
 * caption   accessible description of the table
 *
 * Rows are memoised: see TableRow above for what makes a row render again.
 */
export function Table({
  columns,
  rows,
  rowKey = 'id',
  sort,
  onSort,
  defaultSort = null,
  onRowClick,
  selectedKey,
  freshKeys,
  loading = false,
  error = null,
  errorTitle,
  onRetry,
  empty,
  sticky = true,
  maxHeight,
  dense = false,
  collapse = true,
  sortMenu = true,
  caption,
  skeletonRows = 6,
  class: className,
}) {
  const [localSort, setLocalSort] = useState(defaultSort);
  const controlled = typeof onSort === 'function';
  const activeSort = controlled ? sort : localSort;

  const keyOf = typeof rowKey === 'function' ? rowKey : (row) => row[rowKey];
  const sortable = columns.filter((c) => c.sortable);

  const sorted = useMemo(() => {
    if (!rows) return [];
    if (controlled || !activeSort) return rows;
    return sortRows(rows, columns.find((c) => c.key === activeSort.key), activeSort.dir);
  }, [rows, controlled, activeSort?.key, activeSort?.dir, columns]);

  // Does any cell ask for the row's position? Then a row must follow it.
  const usesIndex = useMemo(() => columns.some((c) => typeof c.render === 'function' && c.render.length > 1), [columns]);

  // Rows get one handler for good; it calls whatever onRowClick is now.
  const clickRef = useRef(onRowClick);
  clickRef.current = onRowClick;
  const activate = useCallback((row, event) => clickRef.current?.(row, event), []);
  const clickable = typeof onRowClick === 'function';

  const changeSort = (next) => {
    if (controlled) onSort(next);
    else setLocalSort(next);
  };

  const toggleSort = (column) => {
    // First click sorts numbers high-to-low and text A-to-Z, the order people
    // usually want; the second click reverses.
    const first = column.num || column.align === 'right' ? 'desc' : 'asc';
    if (activeSort?.key !== column.key) changeSort({ key: column.key, dir: first });
    else changeSort({ key: column.key, dir: activeSort.dir === 'asc' ? 'desc' : 'asc' });
  };

  // Where the header sticks. `position: sticky` works against the nearest
  // scrolling box, and a box that scrolls sideways is one: inside it the
  // header can only stick to the box, which is useless when it is the page
  // that scrolls. So a table that fits its box (the usual case) stops being
  // a scroll box (overflow: clip) and its header sticks to the page, under
  // the top bar. One that is too wide keeps scrolling sideways, as before.
  const box = useRef(null);
  const table = useRef(null);
  const pageScrolled = sticky && !maxHeight;
  const [fits, setFits] = useState(false);
  useLayoutEffect(() => {
    const wrapEl = box.current;
    const tableEl = table.current;
    if (!pageScrolled || !wrapEl || !tableEl) return undefined;
    const measure = () => {
      const next = tableEl.offsetWidth <= wrapEl.clientWidth + 1;
      setFits((was) => (was === next ? was : next));
    };
    measure();
    if (typeof ResizeObserver === 'undefined') return undefined;
    const observer = new ResizeObserver(measure);
    observer.observe(wrapEl);
    observer.observe(tableEl);
    return () => observer.disconnect();
  }, [pageScrolled]);

  const hasRows = sorted.length > 0;
  // A retry keeps the error state on screen, its button spinning, until it
  // has an answer: swapped for skeletons, the "Try again" that has the
  // focus would leave the document and drop the keyboard on <body>. When
  // the rows come, ErrorState hands the focus to the first of them.
  const showError = !!error && !hasRows;
  const showSkeleton = loading && !hasRows && !showError;
  const showEmpty = !hasRows && !showSkeleton && !showError;

  return html`
    <div
      ref=${box}
      class=${cx('table-wrap', className)}
      data-collapse=${collapse ? '' : undefined}
      data-scroll=${maxHeight ? '' : undefined}
      data-page-sticky=${pageScrolled && fits ? '' : undefined}
      style=${maxHeight ? `max-height:${maxHeight}` : undefined}
    >
      ${sortMenu &&
      sortable.length > 0 &&
      collapse &&
      hasRows &&
      html`
        <label class="table-sortbar">
          <span class="faint" style="font-size:var(--text-xs)">Sort by</span>
          <span class="input grow" data-size="sm" data-select="">
            <select
              class="input-el"
              value=${activeSort ? `${activeSort.key}:${activeSort.dir}` : ''}
              onChange=${(event) => {
                const [key, dir] = event.target.value.split(':');
                changeSort(key ? { key, dir } : null);
              }}
            >
              <option value="">Default order</option>
              ${sortable.map((c) => {
                const name = columnLabel(c) || c.key;
                return html`
                  <option value=${`${c.key}:asc`}>${name}, ascending</option>
                  <option value=${`${c.key}:desc`}>${name}, descending</option>
                `;
              })}
            </select>
            <${Icon} name="chevron-down" size=${14} />
          </span>
        </label>
      `}
      <table
        ref=${table}
        class="table"
        role="table"
        data-sticky=${sticky ? '' : undefined}
        data-dense=${dense ? '' : undefined}
        data-collapse=${collapse ? '' : undefined}
        aria-busy=${loading ? 'true' : undefined}
      >
        ${caption && html`<caption class="sr-only">${caption}</caption>`}
        <thead role="rowgroup">
          <tr role="row">
            ${columns.map((column) => {
              const active = activeSort?.key === column.key;
              const ariaSort = column.sortable ? (active ? (activeSort.dir === 'asc' ? 'ascending' : 'descending') : 'none') : undefined;
              return html`
                <th
                  key=${column.key}
                  role="columnheader"
                  scope="col"
                  aria-sort=${ariaSort}
                  style=${column.width ? `width:${column.width}` : undefined}
                  ...${cellAttrs(column)}
                >
                  ${column.sortable
                    ? html`
                        <button type="button" class="th-sort" onClick=${() => toggleSort(column)}>
                          <span>${column.header ?? columnLabel(column)}</span>
                          <${Icon} name=${active ? (activeSort.dir === 'asc' ? 'arrow-up' : 'arrow-down') : 'sort'} size=${12} />
                        </button>
                      `
                    : column.header}
                </th>
              `;
            })}
          </tr>
        </thead>
        <tbody role="rowgroup">
          ${showSkeleton &&
          Array.from(
            { length: skeletonRows },
            (_, r) => html`
              <tr role="row" key=${`skeleton-${r}`} aria-hidden="true">
                ${columns.map(
                  (column, c) => html`
                    <td role="cell" key=${column.key} data-label="" data-primary=${column.primary ? '' : undefined} ...${cellAttrs(column)}>
                      <${Skeleton} width=${`${[62, 44, 70, 38, 54][(r + c) % 5]}%`} />
                    </td>
                  `,
                )}
              </tr>
            `,
          )}
          ${(showError || showEmpty) &&
          html`
            <tr role="row">
              <td role="cell" class="table-state-cell" data-label="" colspan=${columns.length} style="padding:0">
                ${showError
                  ? html`<${ErrorState} error=${error} title=${errorTitle} onRetry=${onRetry} retrying=${loading} compact />`
                  : html`<${EmptyState} compact title=${empty?.title ?? 'Nothing to show'} description=${empty?.description} action=${empty?.action} icon=${empty?.icon} />`}
              </td>
            </tr>
          `}
          ${hasRows &&
          sorted.map((row, index) => {
            const key = keyOf(row);
            return html`<${TableRow}
              key=${key}
              row=${row}
              index=${index}
              rowKey=${key}
              columns=${columns}
              usesIndex=${usesIndex}
              selected=${selectedKey != null && key === selectedKey}
              fresh=${!!freshKeys?.has(key)}
              clickable=${clickable}
              onActivate=${activate}
            />`;
          })}
        </tbody>
      </table>
    </div>
  `;
}
