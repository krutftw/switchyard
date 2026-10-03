// Usage page: CSV and JSON exports, built in the browser from the data that
// is on screen. Nothing is sent anywhere; the file is handed to the browser
// as a download.

import { promptTokens, ratio } from './data.js';

// A cell that starts with one of these is run as a formula by spreadsheet
// programs. Model names come from client requests, so they are not trusted:
// such a cell is prefixed with an apostrophe, which spreadsheets show as text.
const FORMULA_START = /^[=+\-@\t\r]/;

/** Text cells are always quoted, including delimiter-like text. Numbers stay numeric. */
export function csvCell(value) {
  if (value == null) return '';
  if (typeof value === 'number') return Number.isFinite(value) ? String(value) : '';
  let text = String(value);
  if (FORMULA_START.test(text)) text = `'${text}`;
  return `"${text.replace(/"/g, '""')}"`;
}

/** Rows (arrays of cells, the header first) to CSV text with CRLF line ends. */
export function toCsv(rows) {
  return `${rows.map((row) => row.map(csvCell).join(',')).join('\r\n')}\r\n`;
}

// Sums of binary floats carry noise in the last digits (0.00010499999999999999).
// Nine decimals of a dollar keep every real digit of a per-token price.
const money = (value) => (typeof value === 'number' && Number.isFinite(value) ? Number(value.toFixed(9)) : null);
const mean = (sum, count) => (count > 0 ? Math.round((sum / count) * 10) / 10 : null);
const share = (value) => (value == null ? null : Number(value.toFixed(6)));

const COUNTER_HEADER = ['requests', 'errors', 'error_rate', 'input_tokens', 'cache_read_tokens', 'cache_write_tokens', 'output_tokens', 'reasoning_tokens', 'prompt_tokens_total', 'cost_usd', 'mean_duration_ms', 'mean_ttfb_ms'];

/** `cost` is the cell to write: a number, or null for "no cost on record". */
function counterCells(t, cost) {
  return [
    t.requests,
    t.errors,
    share(ratio(t.errors, t.requests)),
    t.input_tokens,
    t.cache_read_tokens,
    t.cache_write_tokens,
    t.output_tokens,
    t.reasoning_tokens,
    promptTokens(t),
    cost,
    mean(t.duration_ms_sum, t.requests),
    mean(t.ttfb_ms_sum, t.ttfb_count),
  ];
}

/**
 * A breakdown as CSV: one line per model, provider or client key.
 *
 * `rows` are the rows of the table as it stands (data.js, viewRows), so the
 * file has the table's filter and order, and its numbers: a row the table
 * shows without a cost (no price matched it, or it only failed) has an empty
 * cost cell, not 0; `share_of_requests` is of the whole range, as in the
 * table. There is no totals line: anything that sums a column would count
 * it twice.
 */
export function breakdownCsv(rows, group) {
  const lines = [[group.value, ...COUNTER_HEADER, 'share_of_requests']];
  for (const row of rows) lines.push([row.name, ...counterCells(row.raw, row.cost > 0 ? money(row.cost) : null), share(row.share)]);
  return toCsv(lines);
}

/**
 * The time series as CSV: a row per bucket with its totals, then four
 * columns (requests, errors, tokens, cost) for every series of the group.
 * A bucket without traffic is a row of zeros, as in the API. `priced` false
 * (no price configured and no cost on record, where the page shows a dash
 * and no cost chart) leaves the cost cells empty instead.
 */
export function timeseriesCsv(timeseries, { priced = true } = {}) {
  const names = timeseries.series ?? [];
  const cost = (value) => (priced ? money(value ?? 0) : null);
  const header = ['bucket_start_utc', 'bucket_start_unix_ms', ...COUNTER_HEADER];
  for (const name of names) header.push(`${name} requests`, `${name} errors`, `${name} tokens`, `${name} cost_usd`);
  const rows = [header];
  for (const point of timeseries.points ?? []) {
    const row = [new Date(point.t).toISOString(), point.t, ...counterCells(point, cost(point.cost))];
    for (const name of names) {
      const g = point.groups?.[name];
      row.push(g?.requests ?? 0, g?.errors ?? 0, g?.tokens ?? 0, cost(g?.cost));
    }
    rows.push(row);
  }
  return toCsv(rows);
}

/** Everything the page shows, as the API returned it. */
export function usageJson({ summary, timeseries, pricing, exportedAt = Date.now() }) {
  return `${JSON.stringify({ exported_at: new Date(exportedAt).toISOString(), range: summary.range, group_by: timeseries?.group_by ?? null, pricing: pricing ?? null, summary, timeseries: timeseries ?? null }, null, 2)}\n`;
}

const pad = (value) => String(value).padStart(2, '0');

/** "switchyard-usage-by-model-24h-20261002-2354.csv" (local time). */
export function fileName(kind, range, extension, at = new Date()) {
  const stamp = `${at.getFullYear()}${pad(at.getMonth() + 1)}${pad(at.getDate())}-${pad(at.getHours())}${pad(at.getMinutes())}`;
  return `switchyard-usage-${kind}-${range}-${stamp}.${extension}`;
}

/**
 * Hand `text` to the browser as a file download. CSV gets a byte order mark
 * so spreadsheet programs read it as UTF-8.
 */
export default function downloadFile(name, text, type) {
  const body = type.startsWith('text/csv') ? `﻿${text}` : text;
  const url = URL.createObjectURL(new Blob([body], { type }));
  const link = document.createElement('a');
  link.href = url;
  link.download = name;
  link.hidden = true;
  document.body.appendChild(link);
  link.click();
  link.remove();
  // The download has its own reference by now; a later revoke is safe in every browser.
  setTimeout(() => URL.revokeObjectURL(url), 10_000);
}
