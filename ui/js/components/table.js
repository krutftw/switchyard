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

import { html, useMemo, useState } from '../../vendor/preact-htm.js';
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
 * columns   [{
 *             key         unique id; also the row field read by default
 *             header      column heading (text)
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
 * rowKey    field name or (row) => key
 * sort      { key, dir: "asc" | "desc" } for controlled sorting, with onSort;
 *           leave both out and the table sorts locally (defaultSort sets the
 *           initial order)
 * onRowClick (row) => void; rows become focusable and respond to Enter
 * selectedKey  key of the row shown in a drawer, highlighted
 * freshKeys    Set of keys that arrived live just now; they flash once
 * loading   first load: skeleton rows
 * error     ApiError: shown in place of rows when there are none to show
 * onRetry   retry handler for the error state
 * empty     { title, description, action, icon } for the empty state
 * sticky    keep the header visible while the table scrolls
 * maxHeight CSS max-height; the table scrolls inside it (implies a scroll box)
 * dense     32px rows for streams
 * collapse  card layout on phones (default true)
 * caption   accessible description of the table
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
  onRetry,
  empty,
  sticky = true,
  maxHeight,
  dense = false,
  collapse = true,
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

  const hasRows = sorted.length > 0;
  const showSkeleton = loading && !hasRows;
  const showError = error && !hasRows && !showSkeleton;
  const showEmpty = !hasRows && !showSkeleton && !showError;

  const cellAttrs = (column) => ({
    'data-align': column.align && column.align !== 'left' ? column.align : undefined,
    'data-num': column.num ? '' : undefined,
    'data-mono': column.mono ? '' : undefined,
  });

  return html`
    <div
      class=${cx('table-wrap', className)}
      data-collapse=${collapse ? '' : undefined}
      data-scroll=${maxHeight ? '' : undefined}
      style=${maxHeight ? `max-height:${maxHeight}` : undefined}
    >
      ${sortable.length > 0 &&
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
              ${!activeSort && html`<option value="">Default order</option>`}
              ${sortable.map(
                (c) => html`
                  <option value=${`${c.key}:asc`}>${c.header}, ascending</option>
                  <option value=${`${c.key}:desc`}>${c.header}, descending</option>
                `,
              )}
            </select>
            <${Icon} name="chevron-down" size=${14} />
          </span>
        </label>
      `}
      <table
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
                          <span>${column.header}</span>
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
                  ? html`<${ErrorState} error=${error} onRetry=${onRetry} compact />`
                  : html`<${EmptyState} compact title=${empty?.title ?? 'Nothing to show'} description=${empty?.description} action=${empty?.action} icon=${empty?.icon} />`}
              </td>
            </tr>
          `}
          ${hasRows &&
          sorted.map((row, index) => {
            const key = keyOf(row);
            const clickable = typeof onRowClick === 'function';
            return html`
              <tr
                role="row"
                key=${key}
                data-clickable=${clickable ? '' : undefined}
                data-selected=${selectedKey != null && key === selectedKey ? '' : undefined}
                data-fresh=${freshKeys?.has(key) ? '' : undefined}
                tabindex=${clickable ? 0 : undefined}
                onClick=${clickable
                  ? (event) => {
                      // Buttons, links and inputs inside a row act on their own.
                      if (event.target.closest('a, button, input, select, textarea, label')) return;
                      // Dragging to select text is not a click.
                      if (String(window.getSelection?.() ?? '').length > 0) return;
                      onRowClick(row, event);
                    }
                  : undefined}
                onKeyDown=${clickable
                  ? (event) => {
                      if (event.target !== event.currentTarget) return;
                      if (event.key === 'Enter' || event.key === ' ') {
                        event.preventDefault();
                        onRowClick(row, event);
                      }
                    }
                  : undefined}
              >
                ${columns.map(
                  (column) => html`
                    <td
                      role="cell"
                      key=${column.key}
                      data-label=${column.header ?? ''}
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
          })}
        </tbody>
      </table>
    </div>
  `;
}
