// Charts: responsive inline SVG, no library.
//
//   Sparkline    a trend with no axes, for Stat and table cells
//   LineChart    one or more series over time; hover crosshair and tooltip
//   AreaChart    LineChart with a wash under each line; `stacked` adds them up
//   BarChart     columns over time or categories, stacked when there are several series
//   BarList      ranked horizontal bars: "top models", "top keys" (use it
//                where a donut would tempt you)
//   LatencyBars  percentiles on one scale: p50 / p90 / p99
//   Meter        a ratio against a limit
//   HealthStrip  recent success/failure in fixed time slots
//
// Rules the components enforce, so pages stay consistent:
//   - one y axis, starting at zero unless yMin says otherwise
//   - series colours come from --series-1..6 in the order given; a seventh
//     series is drawn in the neutral "other" colour (fold the tail into
//     "Other" with foldSeries before it gets there). Lamp colours are for
//     status only: pass color="var(--lamp-stop)" only when the series IS a
//     status (failed requests)
//   - two or more series always get a legend; text is never series-coloured
//   - every time chart has a table twin behind the table toggle, and the same
//     values on keyboard focus (arrow keys move the crosshair)
//   - while refetching, pass stale=${res.refreshing}: the old plot stays,
//     dimmed; nothing jumps

import { html, useMemo, useState } from '../../vendor/preact-htm.js';
import { cx } from '../lib/dom.js';
import { DASH, formatCompact, formatDate, formatDateTime, formatDuration, formatPercent, formatTime, toDate } from '../lib/format.js';
import { useSize } from '../lib/hooks.js';
import { IconButton } from './button.js';

// ---------------------------------------------------------------------------
// Shared maths
// ---------------------------------------------------------------------------

const MAX_SERIES = 6;

/** The colour for the i-th series. Order is identity: do not sort series by value. */
export function seriesColor(series, index) {
  if (series?.color) return series.color;
  return index < MAX_SERIES ? `var(--series-${index + 1})` : 'var(--series-other)';
}

/**
 * Keep the `keep` largest series (by total) and sum the rest into "Other".
 * Order of the kept series is preserved, so colours stay with their series.
 */
export function foldSeries(series, keep = MAX_SERIES - 1, otherLabel = 'Other') {
  if (series.length <= keep + 1) return series;
  const total = (s) => s.values.reduce((sum, v) => sum + (v || 0), 0);
  const ranked = [...series].sort((a, b) => total(b) - total(a));
  const kept = new Set(ranked.slice(0, keep));
  const rest = series.filter((s) => !kept.has(s));
  const length = Math.max(...series.map((s) => s.values.length));
  const other = {
    key: '__other',
    label: otherLabel,
    color: 'var(--series-other)',
    values: Array.from({ length }, (_, i) => rest.reduce((sum, s) => sum + (s.values[i] || 0), 0)),
  };
  return [...series.filter((s) => kept.has(s)), other];
}

/**
 * Round axis bounds and ticks: 0 / 250 / 500 / 750 / 1,000.
 * `integer: true` keeps every tick a whole number, for axes that count
 * things: a maximum of 1 gives 0 / 1, not 0 / 0.5 / 1.
 */
export function niceScale(min, max, target = 4, { integer = false } = {}) {
  if (!(max > min)) max = min + 1;
  const rawStep = (max - min) / target;
  const magnitude = 10 ** Math.floor(Math.log10(rawStep));
  const residual = rawStep / magnitude;
  let step = (residual >= 5 ? 10 : residual >= 2 ? 5 : residual >= 1 ? 2 : 1) * magnitude;
  if (integer) step = Math.max(1, Math.round(step));
  const lo = Math.floor(min / step) * step;
  const hi = Math.ceil(max / step) * step;
  const ticks = [];
  for (let v = lo; v <= hi + step / 2; v += step) ticks.push(Number(v.toPrecision(12)));
  return { min: lo, max: hi, ticks };
}

const isTimeAxis = (x) => x.length > 0 && typeof x[0] === 'number' && x[0] > 1e11;

/**
 * Axis and tooltip formatters for a time axis, picked from its span. With
 * `utc` the clock and the calendar are UTC's, and the tooltip says so: day
 * buckets cut at UTC midnight read as the previous day west of Greenwich
 * when printed in local time.
 */
function timeFormats(x, utc = false) {
  const span = x[x.length - 1] - x[0];
  const DAY = 86_400_000;
  const hm = (t) => formatTime(t, { utc }).slice(0, 5);
  const date = (t) => formatDate(t, new Date(), { utc });
  const zone = utc ? ' UTC' : '';
  if (span <= 36 * 3_600_000) {
    // Within a day and a half: clock times; the tooltip adds the date when the range crosses midnight.
    const dayOf = (t) => (utc ? toDate(t)?.getUTCDate() : toDate(t)?.getDate());
    const crossesDay = dayOf(x[0]) !== dayOf(x[x.length - 1]);
    return { tick: hm, tip: (t) => `${crossesDay ? `${date(t)} ${hm(t)}` : formatTime(t, { utc })}${zone}` };
  }
  if (span <= 14 * DAY) return { tick: date, tip: (t) => `${date(t)} ${hm(t)}${zone}` };
  return { tick: date, tip: (t) => `${date(t)}${zone}` };
}

/** Evenly spread tick indices: first and last always included. */
function pickTicks(n, count) {
  if (n <= 0) return [];
  if (n <= count) return Array.from({ length: n }, (_, i) => i);
  const out = [];
  for (let k = 0; k < count; k += 1) {
    const i = Math.round((k * (n - 1)) / (count - 1));
    if (out[out.length - 1] !== i) out.push(i);
  }
  return out;
}

const round = (v) => Math.round(v * 10) / 10;

// Approximate width of an 11px monospace axis label.
const labelWidth = (text) => String(text).length * 6.7;

// ---------------------------------------------------------------------------
// Hover / keyboard state shared by LineChart and BarChart
// ---------------------------------------------------------------------------

function useChartHover(count, indexAt) {
  const [hover, setHover] = useState(null);
  const clampIndex = (i) => Math.max(0, Math.min(count - 1, i));
  const handlers = {
    onPointerMove: (event) => {
      if (count === 0) return;
      const rect = event.currentTarget.getBoundingClientRect();
      const next = clampIndex(indexAt(event.clientX - rect.left));
      setHover((prev) => (prev === next ? prev : next));
    },
    onPointerDown: (event) => {
      if (count === 0) return;
      const rect = event.currentTarget.getBoundingClientRect();
      setHover(clampIndex(indexAt(event.clientX - rect.left)));
    },
    onPointerLeave: (event) => {
      // Touch keeps the readout until the next tap; a mouse leaving clears it.
      if (event.pointerType !== 'touch') setHover(null);
    },
    onFocus: (event) => {
      if (count > 0 && event.currentTarget.matches(':focus-visible')) setHover((prev) => prev ?? count - 1);
    },
    onBlur: () => setHover(null),
    onKeyDown: (event) => {
      if (count === 0) return;
      let next = null;
      if (event.key === 'ArrowLeft') next = (hover ?? count) - 1;
      else if (event.key === 'ArrowRight') next = (hover ?? -1) + 1;
      else if (event.key === 'Home') next = 0;
      else if (event.key === 'End') next = count - 1;
      else if (event.key === 'Escape' && hover != null) {
        event.stopPropagation();
        setHover(null);
        return;
      }
      if (next == null) return;
      event.preventDefault();
      setHover(clampIndex(next));
    },
  };
  return [hover != null && hover < count ? hover : null, handlers];
}

// ---------------------------------------------------------------------------
// Frame: legend, table toggle, table twin
// ---------------------------------------------------------------------------

function Legend({ series, shape }) {
  return html`
    <ul class="chart-legend">
      ${series.map((s, i) => {
        const name = s.label ?? s.key;
        // A long name is cut with an ellipsis; the full one is in the title.
        return html`<li key=${s.key ?? i} title=${typeof name === 'string' ? name : undefined}>
          <span class="chart-key" data-shape=${shape === 'rect' ? 'rect' : undefined} style=${`--key:${seriesColor(s, i)}`}></span>
          <span class="chart-legend-label">${name}</span>
        </li>`;
      })}
    </ul>
  `;
}

function ChartTable({ x, series, xLabel, xText, valueFormat }) {
  return html`
    <div class="chart-table-wrap">
      <table class="table" data-dense="" data-sticky="">
        <thead>
          <tr>
            <th scope="col">${xLabel}</th>
            ${series.map((s, i) => html`<th scope="col" key=${s.key ?? i} data-align="right">${s.label ?? s.key}</th>`)}
          </tr>
        </thead>
        <tbody>
          ${x.map(
            (value, j) => html`
              <tr key=${j}>
                <td data-mono="">${xText(value)}</td>
                ${series.map((s, i) => html`<td key=${s.key ?? i} data-num="" data-align="right">${s.values[j] == null ? DASH : valueFormat(s.values[j])}</td>`)}
              </tr>
            `,
          )}
        </tbody>
      </table>
    </div>
  `;
}

function ChartFrame({ series, x, shape, legend, table, stale, xLabel, xText, valueFormat, class: className, children }) {
  const [asTable, setAsTable] = useState(false);
  const showLegend = legend && series.length >= 2;
  return html`
    <div class=${cx('chart', className)} data-stale=${stale ? '' : undefined}>
      ${(showLegend || table) &&
      html`
        <div class="chart-top">
          ${showLegend ? html`<${Legend} series=${series} shape=${shape} />` : html`<span></span>`}
          ${table &&
          html`<${IconButton}
            icon=${asTable ? 'chart' : 'table'}
            label=${asTable ? 'Show as chart' : 'Show as table'}
            size="sm"
            aria-pressed=${asTable ? 'true' : 'false'}
            onClick=${() => setAsTable(!asTable)}
          />`}
        </div>
      `}
      ${asTable ? html`<${ChartTable} x=${x} series=${series} xLabel=${xLabel} xText=${xText} valueFormat=${valueFormat} />` : children}
    </div>
  `;
}

/** From this many series on, a tooltip leaves out the ones that are zero in the bucket. */
const TIP_SKIP_ZEROS_FROM = 5;

/**
 * The tooltip's rows for one bucket: one per series. With many series the
 * ones whose value there is zero (or missing) are left out, so the readout
 * of a quiet minute in a seven-series chart is one or two lines, not seven
 * of "0". Returns { rows, skipped }.
 */
function tipRowsFor(series, index, { shape, format }) {
  const all = series.map((s, i) => ({
    key: s.key ?? i,
    shape,
    color: seriesColor(s, i),
    label: s.label ?? s.key,
    raw: s.values[index],
    value: s.values[index] == null ? DASH : format(s.values[index]),
  }));
  if (series.length < TIP_SKIP_ZEROS_FROM) return { rows: all, skipped: 0 };
  const rows = all.filter((row) => row.raw != null && row.raw !== 0);
  return { rows, skipped: all.length - rows.length };
}

function ChartTip({ left, flip, head, rows, total, skipped = 0 }) {
  // translate() keeps pointer tracking off the layout path.
  const style = flip ? `transform:translate(calc(${round(left - 12)}px - 100%), 8px)` : `transform:translate(${round(left + 12)}px, 8px)`;
  return html`
    <div class="chart-tip" style=${style} role="status">
      <div class="chart-tip-head">${head}</div>
      ${rows.length === 0 && skipped > 0 && html`<div class="chart-tip-row chart-tip-none"><span class="chart-tip-label">Nothing in this bucket</span></div>`}
      ${rows.map(
        (row) => html`
          <div class="chart-tip-row" key=${row.key}>
            <span class="chart-key" data-shape=${row.shape === 'rect' ? 'rect' : undefined} style=${`--key:${row.color}`}></span>
            <span class="chart-tip-label">${row.label}</span>
            <span class="chart-tip-value">${row.value}</span>
          </div>
        `,
      )}
      ${total != null &&
      html`<div class="chart-tip-row chart-tip-total">
        <span class="chart-tip-label">Total</span>
        <span class="chart-tip-value">${total}</span>
      </div>`}
    </div>
  `;
}

/** Geometry shared by the two axis charts. */
function useAxes({ x, tops, width, height, yMin, yMax, yFormat, xFormat, tipFormat, utc, integer, band }) {
  return useMemo(() => {
    const n = x.length;
    const time = isTimeAxis(x);
    let dataMax = 0;
    for (const values of tops) for (const v of values) if (v != null && v > dataMax) dataMax = v;
    const scale = niceScale(yMin, yMax ?? (dataMax > 0 ? dataMax : 1), height < 160 ? 2 : 4, { integer });
    if (yMax != null) scale.max = Math.max(scale.max, yMax);
    const fmt = time ? timeFormats(x, utc) : null;
    const tickText = xFormat ?? (fmt ? fmt.tick : (v) => String(v));
    // The x value in full: the tooltip's head and the first column of the table view.
    const tipText = tipFormat ?? (fmt ? fmt.tip : (v) => String(v));
    // (In the table the column heading carries the "UTC".)
    const tableText = tipFormat ?? (time ? (v) => formatDateTime(v, { utc }) : (v) => String(v));

    const left = Math.ceil(Math.max(...scale.ticks.map((t) => labelWidth(yFormat(t)))) + 10);
    const right = 12;
    const top = 8;
    const bottom = 24;
    const innerW = Math.max(0, width - left - right);
    const innerH = Math.max(0, height - top - bottom);

    let xPos;
    if (band) {
      const bandW = n > 0 ? innerW / n : innerW;
      xPos = (j) => left + bandW * (j + 0.5);
    } else if (n <= 1) {
      xPos = () => left + innerW / 2;
    } else if (time) {
      const x0 = x[0];
      const span = x[n - 1] - x0 || 1;
      xPos = (j) => left + ((x[j] - x0) / span) * innerW;
    } else {
      xPos = (j) => left + (j / (n - 1)) * innerW;
    }
    const yPos = (v) => top + innerH - ((v - scale.min) / (scale.max - scale.min || 1)) * innerH;

    const widest = n > 0 ? Math.max(labelWidth(tickText(x[0])), labelWidth(tickText(x[n - 1]))) : 40;
    const tickCount = Math.max(2, Math.min(8, Math.floor(innerW / (widest + 28))));
    const xTicks = pickTicks(n, tickCount);

    return { n, time, scale, left, right, top, bottom, innerW, innerH, xPos, yPos, xTicks, tickText, tipText, tableText };
  }, [x, tops, width, height, yMin, yMax, yFormat, xFormat, tipFormat, utc, integer, band]);
}

function Axes({ axes, width, yFormat, x, band }) {
  const { scale, left, top, innerW, innerH, xPos, yPos, xTicks, tickText, n } = axes;
  return html`
    <g aria-hidden="true">
      ${scale.ticks.map(
        (t) => html`
          <g key=${t}>
            <line class=${t === scale.min ? 'chart-axis' : 'chart-grid'} x1=${left} x2=${left + innerW} y1=${Math.round(yPos(t)) + 0.5} y2=${Math.round(yPos(t)) + 0.5} />
            <text x=${left - 8} y=${yPos(t)} dy="0.32em" text-anchor="end">${yFormat(t)}</text>
          </g>
        `,
      )}
      ${xTicks.map((j, k) => {
        // Edge labels hug the plot edges so they are never clipped.
        const anchor = band || n === 1 ? 'middle' : k === 0 ? 'start' : k === xTicks.length - 1 ? 'end' : 'middle';
        return html`<text key=${j} x=${round(xPos(j))} y=${top + innerH + 16} text-anchor=${anchor}>${tickText(x[j])}</text>`;
      })}
    </g>
  `;
}

// ---------------------------------------------------------------------------
// LineChart / AreaChart
// ---------------------------------------------------------------------------

/**
 * x            epoch-ms timestamps (a time axis) or category labels
 * series       [{ key, label, values, color? }]; values line up with x; null breaks the line
 * height       plot height in px including the x axis (default 220)
 * area         wash under each line
 * stacked      add the series up (implies area); use for parts of a whole
 * yFormat      formats axis ticks (default formatCompact)
 * valueFormat  formats tooltip and table values (default yFormat)
 * xFormat      overrides the axis tick text
 * tipFormat    formats the x value in full, for the tooltip's head and the
 *              first column of the table view (default: the time with as
 *              much of the date as the range needs; the label for categories)
 * utc          a time axis is printed in UTC (ticks, tooltip head, table
 *              view), and the tooltip and the table say "UTC". For buckets
 *              the gateway cuts at UTC midnight.
 * integer      whole-number y ticks, for axes that count things (requests,
 *              errors): a maximum of 1 gives 0 / 1, never 0.5
 * yMin, yMax   axis bounds; yMin defaults to 0
 * xLabel       heading of the x column in the table twin (default "Time")
 * label        accessible summary: say what the chart shows
 * stale        dim the plot while a refetch is in flight
 * legend, table  set false to drop the legend / the table toggle
 * emptyText    shown when there is nothing to plot
 *
 * The tooltip lists every series; from five series on it leaves out those
 * whose value in the bucket is zero.
 */
export function LineChart({
  x = [],
  series = [],
  height = 220,
  area = false,
  stacked = false,
  yFormat = formatCompact,
  valueFormat,
  xFormat,
  tipFormat,
  utc = false,
  integer = false,
  yMin = 0,
  yMax,
  xLabel = 'Time',
  label,
  stale = false,
  legend = true,
  table = true,
  emptyText = 'No data in this range',
  class: className,
}) {
  const [ref, size] = useSize();
  const width = size.width;
  const fmtValue = valueFormat ?? yFormat;

  // For stacking, each layer's top edge is the running sum.
  const layers = useMemo(() => {
    if (!stacked) return series.map((s) => ({ top: s.values, base: null }));
    const running = new Array(x.length).fill(0);
    return series.map((s) => {
      const base = [...running];
      const top = s.values.map((v, j) => {
        running[j] += v || 0;
        return running[j];
      });
      return { top, base };
    });
  }, [series, stacked, x.length]);

  const tops = useMemo(() => layers.map((l) => l.top), [layers]);
  const axes = useAxes({ x, tops, width, height, yMin, yMax, yFormat, xFormat, tipFormat, utc, integer, band: false });
  const { n, left, innerW, innerH, top, xPos, yPos, tipText, scale } = axes;

  const indexAt = (px) => {
    let best = 0;
    let bestDistance = Infinity;
    for (let j = 0; j < n; j += 1) {
      const d = Math.abs(xPos(j) - px);
      if (d < bestDistance) {
        bestDistance = d;
        best = j;
      }
    }
    return best;
  };
  const [hover, handlers] = useChartHover(n, indexAt);

  const hasData = n > 0 && series.some((s) => s.values.some((v) => v != null));

  const paths = useMemo(() => {
    if (!width || !hasData) return [];
    return layers.map((layer) => {
      // Split at nulls so a gap in the data is a gap in the line.
      const runs = [];
      let run = [];
      for (let j = 0; j < n; j += 1) {
        if (layer.top[j] == null) {
          if (run.length) runs.push(run);
          run = [];
        } else run.push(j);
      }
      if (run.length) runs.push(run);
      const line = runs.map((r) => r.map((j, k) => `${k === 0 ? 'M' : 'L'}${round(xPos(j))} ${round(yPos(layer.top[j]))}`).join('')).join('');
      let fill = '';
      if (area || stacked) {
        fill = runs
          .map((r) => {
            const upper = r.map((j, k) => `${k === 0 ? 'M' : 'L'}${round(xPos(j))} ${round(yPos(layer.top[j]))}`).join('');
            const lower = [...r]
              .reverse()
              .map((j) => `L${round(xPos(j))} ${round(yPos(layer.base ? layer.base[j] : scale.min))}`)
              .join('');
            return `${upper}${lower}Z`;
          })
          .join('');
      }
      // A lone point has no line to draw: mark it.
      const dots = runs.filter((r) => r.length === 1).map((r) => r[0]);
      return { line, fill, dots };
    });
  }, [layers, width, height, hasData, axes, area, stacked]);

  const tip = hover == null ? { rows: [], skipped: 0 } : tipRowsFor(series, hover, { shape: stacked ? 'rect' : 'line', format: fmtValue });
  const tipTotal = hover != null && stacked && series.length > 1 ? fmtValue(series.reduce((sum, s) => sum + (s.values[hover] || 0), 0)) : null;

  return html`
    <${ChartFrame}
      series=${series}
      x=${x}
      shape=${stacked ? 'rect' : 'line'}
      legend=${legend}
      table=${table && hasData}
      stale=${stale}
      xLabel=${axes.time && utc && !tipFormat ? `${xLabel} (UTC)` : xLabel}
      xText=${axes.tableText}
      valueFormat=${fmtValue}
      class=${className}
    >
      <div
        ref=${ref}
        class="chart-plot"
        style=${`height:${height}px`}
        tabindex=${hasData ? 0 : undefined}
        role="img"
        aria-label=${label ?? 'Line chart'}
        ...${hasData ? handlers : {}}
      >
        ${!hasData && html`<div class="chart-empty" style=${`height:${height}px`}>${emptyText}</div>`}
        ${hasData &&
        width > 0 &&
        html`
          <svg width=${width} height=${height} viewBox=${`0 0 ${width} ${height}`}>
            <${Axes} axes=${axes} width=${width} yFormat=${yFormat} x=${x} />
            ${paths.map(
              (p, i) => html`
                <g key=${series[i].key ?? i}>
                  ${p.fill && html`<path d=${p.fill} fill=${seriesColor(series[i], i)} opacity=${stacked ? 0.28 : 0.1} />`}
                  <path class="chart-line" d=${p.line} stroke=${seriesColor(series[i], i)} />
                  ${p.dots.map((j) => html`<circle key=${j} class="chart-dot" cx=${round(xPos(j))} cy=${round(yPos(layers[i].top[j]))} r="4" fill=${seriesColor(series[i], i)} />`)}
                </g>
              `,
            )}
            ${hover != null &&
            html`
              <g aria-hidden="true">
                <line class="chart-cross" x1=${Math.round(xPos(hover)) + 0.5} x2=${Math.round(xPos(hover)) + 0.5} y1=${top} y2=${top + innerH} />
                ${layers.map((layer, i) =>
                  layer.top[hover] == null
                    ? null
                    : html`<circle key=${i} class="chart-dot" cx=${round(xPos(hover))} cy=${round(yPos(layer.top[hover]))} r="4" fill=${seriesColor(series[i], i)} />`,
                )}
              </g>
            `}
          </svg>
          ${hover != null &&
          html`<${ChartTip} left=${xPos(hover)} flip=${xPos(hover) > left + innerW * 0.55} head=${tipText(x[hover])} rows=${tip.rows} skipped=${tip.skipped} total=${tipTotal} />`}
        `}
      </div>
    <//>
  `;
}

/** LineChart with the wash on. Pass `stacked` for parts of a whole. */
export function AreaChart(props) {
  return html`<${LineChart} area=${true} ...${props} />`;
}

// ---------------------------------------------------------------------------
// BarChart
// ---------------------------------------------------------------------------

/** A column with only its top corners rounded: square at the baseline. */
function columnPath(x, y, w, h, r) {
  const radius = Math.max(0, Math.min(r, w / 2, h));
  return `M${round(x)} ${round(y + h)}V${round(y + radius)}Q${round(x)} ${round(y)} ${round(x + radius)} ${round(y)}H${round(x + w - radius)}Q${round(x + w)} ${round(y)} ${round(x + w)} ${round(y + radius)}V${round(y + h)}Z`;
}

/**
 * Columns per time bucket or category. Several series stack (first series
 * at the bottom); the tooltip lists each and the total.
 *
 * Props are the same as LineChart (x, series, height, yFormat, valueFormat,
 * xFormat, tipFormat, utc, integer, yMax, xLabel, label, stale, legend,
 * table, emptyText).
 */
export function BarChart({
  x = [],
  series = [],
  height = 220,
  yFormat = formatCompact,
  valueFormat,
  xFormat,
  tipFormat,
  utc = false,
  integer = false,
  yMax,
  xLabel = 'Time',
  label,
  stale = false,
  legend = true,
  table = true,
  emptyText = 'No data in this range',
  class: className,
}) {
  const [ref, size] = useSize();
  const width = size.width;
  const fmtValue = valueFormat ?? yFormat;

  const totals = useMemo(() => x.map((_, j) => series.reduce((sum, s) => sum + (s.values[j] || 0), 0)), [x, series]);
  const tops = useMemo(() => [totals], [totals]);
  const axes = useAxes({ x, tops, width, height, yMin: 0, yMax, yFormat, xFormat, tipFormat, utc, integer, band: true });
  const { n, left, innerW, innerH, top, xPos, yPos, tipText, scale } = axes;

  const bandW = n > 0 ? innerW / n : 0;
  // Bars never fill the slot: the leftover is the breathing room.
  const barW = Math.max(1, Math.min(24, bandW - (bandW > 6 ? 2 : 0.5)));
  const indexAt = (px) => Math.floor((px - left) / (bandW || 1));
  const [hover, handlers] = useChartHover(n, indexAt);

  const hasData = n > 0 && totals.some((t) => t > 0);

  const bars = useMemo(() => {
    if (!width || !hasData) return [];
    const out = [];
    const baseline = yPos(scale.min);
    for (let j = 0; j < n; j += 1) {
      let acc = 0;
      const present = series.map((s, i) => ({ i, v: s.values[j] || 0 })).filter((e) => e.v > 0);
      present.forEach((entry, k) => {
        const y1 = yPos(acc + entry.v);
        const y0 = yPos(acc);
        acc += entry.v;
        const isTop = k === present.length - 1;
        // A 2px gap in the surface colour separates stacked segments.
        const gap = isTop || y0 - y1 < 4 ? 0 : 2;
        const h = Math.max(1, y0 - y1 - gap);
        const xLeft = xPos(j) - barW / 2;
        out.push({
          key: `${j}-${entry.i}`,
          j,
          color: seriesColor(series[entry.i], entry.i),
          d: isTop ? columnPath(xLeft, y1, barW, Math.min(h, baseline - y1), 3) : columnPath(xLeft, y1 + gap, barW, h, 0),
        });
      });
    }
    return out;
  }, [series, x, width, height, hasData, axes, barW]);

  const tip = hover == null ? { rows: [], skipped: 0 } : tipRowsFor(series, hover, { shape: 'rect', format: fmtValue });

  return html`
    <${ChartFrame}
      series=${series}
      x=${x}
      shape="rect"
      legend=${legend}
      table=${table && hasData}
      stale=${stale}
      xLabel=${axes.time && utc && !tipFormat ? `${xLabel} (UTC)` : xLabel}
      xText=${axes.tableText}
      valueFormat=${fmtValue}
      class=${className}
    >
      <div
        ref=${ref}
        class="chart-plot"
        style=${`height:${height}px`}
        tabindex=${hasData ? 0 : undefined}
        role="img"
        aria-label=${label ?? 'Bar chart'}
        ...${hasData ? handlers : {}}
      >
        ${!hasData && html`<div class="chart-empty" style=${`height:${height}px`}>${emptyText}</div>`}
        ${hasData &&
        width > 0 &&
        html`
          <svg width=${width} height=${height} viewBox=${`0 0 ${width} ${height}`}>
            <${Axes} axes=${axes} width=${width} yFormat=${yFormat} x=${x} band=${true} />
            ${hover != null && html`<rect class="chart-band" x=${round(left + bandW * hover)} y=${top} width=${round(bandW)} height=${innerH} />`}
            ${bars.map((b) => html`<path key=${b.key} d=${b.d} fill=${b.color} opacity=${hover != null && hover !== b.j ? 0.55 : 1} />`)}
          </svg>
          ${hover != null &&
          html`<${ChartTip}
            left=${xPos(hover)}
            flip=${xPos(hover) > left + innerW * 0.55}
            head=${tipText(x[hover])}
            rows=${tip.rows}
            skipped=${tip.skipped}
            total=${series.length > 1 ? fmtValue(totals[hover]) : null}
          />`}
        `}
      </div>
    <//>
  `;
}

// ---------------------------------------------------------------------------
// Sparkline
// ---------------------------------------------------------------------------

/** The shortest line worth drawing, in px: under this it reads as a blot, not a trend. */
const SPARK_MIN_RUN = 10;

/**
 * A trend line with no axes. The line is drawn in the quiet ink colour and
 * the latest point is lit in the accent.
 *
 * data    numbers, oldest first (null entries are skipped)
 * width, height   px (default 96 x 28)
 * area    wash under the line (default true)
 * min, max  fix the scale; by default it spans the data, starting at 0
 * minPoints  fewer values than this draw nothing (default 2). Raise it when
 *         a trend of two or three points would say more than is known.
 * label   accessible summary ("Requests per minute, last hour"); without it
 *         the sparkline is decorative and hidden from screen readers
 *
 * With too little to show a trend the sparkline is an empty box of the same
 * size, so the layout holds: fewer than `minPoints` values, or values that
 * sit so close together at one end of the range (the first two readings of
 * an hour) that the line would be a speck with a dot on it.
 */
export function Sparkline({ data = [], width = 96, height = 28, area = true, min, max, minPoints = 2, label, class: className }) {
  const points = data.map((v, i) => [i, v]).filter(([, v]) => v != null && Number.isFinite(v));
  const pad = 3;
  const last = data.length - 1 || 1;
  const px = (i) => pad + (i / last) * (width - pad * 2);
  const drawn = points.length >= 2 ? px(points[points.length - 1][0]) - px(points[0][0]) : 0;
  if (points.length < Math.max(2, minPoints) || drawn < SPARK_MIN_RUN) {
    return html`<svg class=${cx('spark', className)} width=${width} height=${height} data-empty="" aria-hidden="true"></svg>`;
  }
  const lo = min ?? Math.min(0, ...points.map(([, v]) => v));
  const hi = max ?? Math.max(...points.map(([, v]) => v));
  const span = hi - lo || 1;
  const py = (v) => height - pad - ((v - lo) / span) * (height - pad * 2);
  const line = points.map(([i, v], k) => `${k === 0 ? 'M' : 'L'}${round(px(i))} ${round(py(v))}`).join('');
  const [endI, endV] = points[points.length - 1];
  const fill = `${line}L${round(px(endI))} ${height - pad}L${round(px(points[0][0]))} ${height - pad}Z`;
  return html`
    <svg
      class=${cx('spark', className)}
      width=${width}
      height=${height}
      viewBox=${`0 0 ${width} ${height}`}
      role=${label ? 'img' : undefined}
      aria-label=${label}
      aria-hidden=${label ? undefined : 'true'}
    >
      ${area && html`<path class="spark-area" d=${fill} />`}
      <path class="spark-line" d=${line} />
      <circle class="spark-dot" cx=${round(px(endI))} cy=${round(py(endV))} r="3" />
    </svg>
  `;
}

// ---------------------------------------------------------------------------
// BarList
// ---------------------------------------------------------------------------

/**
 * Ranked horizontal bars: the readable alternative to a donut.
 *
 *   html`<${BarList} items=${models.map((m) => ({ key: m.id, label: m.id, value: m.tokens }))}
 *                    format=${formatTokens} share limit=${8} />`
 *
 * items    [{ key, label, value, hint?, href?, color? }], any order
 * format   value formatter (default formatCompact)
 * share    also show each item's share of the total
 * rank     show 1, 2, 3 before the labels
 * limit    keep the top N and fold the rest into one "N others" row
 * sort     false keeps the given order (for ordered categories)
 * mono     labels are identifiers (default true)
 *
 * All bars are one colour: length already encodes the value. Pass item.color
 * only when the colour means something (a lamp colour for failures).
 */
export function BarList({ items = [], format = formatCompact, share = false, rank = false, limit, sort = true, mono = true, emptyText = 'Nothing to rank yet', class: className }) {
  const total = items.reduce((sum, item) => sum + (item.value || 0), 0);
  let rows = sort ? [...items].sort((a, b) => (b.value || 0) - (a.value || 0)) : items;
  if (limit && rows.length > limit) {
    const tail = rows.slice(limit - 1);
    rows = [
      ...rows.slice(0, limit - 1),
      { key: '__others', label: `${tail.length} others`, value: tail.reduce((sum, item) => sum + (item.value || 0), 0), color: 'var(--series-other)', plain: true },
    ];
  }
  const peak = Math.max(0, ...rows.map((r) => r.value || 0));
  if (rows.length === 0) return html`<div class="chart-empty" style="min-height:96px">${emptyText}</div>`;
  return html`
    <ol class=${cx('barlist', className)}>
      ${rows.map((item, index) => {
        const width = peak > 0 ? Math.max(0, ((item.value || 0) / peak) * 100) : 0;
        const body = html`
          <span class="barlist-label">
            ${rank && html`<span class="barlist-rank">${item.plain ? '' : index + 1}</span>`}
            <span class=${cx('barlist-name', mono && !item.plain && 'mono')} title=${typeof item.label === 'string' ? item.label : undefined}>${item.label}</span>
            ${item.hint && html`<span class="barlist-hint">${item.hint}</span>`}
          </span>
          <span class="barlist-value">
            ${format(item.value)}${share && total > 0 && html`<span class="barlist-share">${formatPercent((item.value || 0) / total, 0)}</span>`}
          </span>
          <span class="barlist-track" aria-hidden="true">
            <span class="barlist-fill" style=${`width:${round(width)}%${item.color ? `;--bar:${item.color}` : ''}`}></span>
          </span>
        `;
        return html`<li key=${item.key ?? index}>
          ${item.href ? html`<a class="barlist-row" href=${item.href}>${body}</a>` : html`<div class="barlist-row">${body}</div>`}
        </li>`;
      })}
    </ol>
  `;
}

// ---------------------------------------------------------------------------
// LatencyBars
// ---------------------------------------------------------------------------

/**
 * Percentiles on one scale, so the tail is visible next to the median.
 *
 *   html`<${LatencyBars} items=${[
 *     { label: 'p50', value: stats.p50_ms },
 *     { label: 'p90', value: stats.p90_ms },
 *     { label: 'p99', value: stats.p99_ms },
 *   ]} />`
 *
 * items    [{ label, value (ms), tone? }]; tone "caution" | "stop" marks a
 *          percentile that breaches a threshold you decided on
 * max      scale maximum; default: the largest value
 * format   value formatter (default formatDuration)
 */
export function LatencyBars({ items = [], max, format = formatDuration, class: className }) {
  const peak = max ?? Math.max(0, ...items.map((i) => i.value || 0));
  return html`
    <div class=${cx('latency', className)} role="list">
      ${items.map(
        (item) => html`
          <div style="display:contents" role="listitem" key=${item.label}>
            <span class="latency-label">${item.label}</span>
            <span class="latency-track" aria-hidden="true">
              <span
                class="latency-fill"
                data-tone=${item.tone}
                style=${`width:${peak > 0 && item.value != null ? round(Math.min(100, (item.value / peak) * 100)) : 0}%`}
              ></span>
            </span>
            <span class="latency-value">${format(item.value)}</span>
          </div>
        `,
      )}
    </div>
  `;
}

// ---------------------------------------------------------------------------
// Meter
// ---------------------------------------------------------------------------

/**
 * A ratio against a limit: success rate, rate-limit use, budget.
 *
 * value, max   the fill is value / max (max defaults to 1)
 * tone         "clear" | "caution" | "stop"; default: the accent. The track is
 *              a lighter step of the same colour, so the state reads across
 *              the whole bar
 * text         value text after the bar (default: the percentage); false hides it
 * label        accessible name
 */
export function Meter({ value, max = 1, tone, text, label, class: className }) {
  const ratio = value == null || !max ? 0 : Math.max(0, Math.min(1, value / max));
  const shown = text === false ? null : (text ?? (value == null ? DASH : formatPercent(ratio, 0)));
  return html`
    <div
      class=${cx('meter', className)}
      data-tone=${tone}
      role="meter"
      aria-label=${label}
      aria-valuemin="0"
      aria-valuemax=${max}
      aria-valuenow=${value ?? 0}
      aria-valuetext=${shown ?? undefined}
    >
      <span class="meter-track"><span class="meter-fill" style=${`width:${round(ratio * 100)}%`}></span></span>
      ${shown != null && html`<span class="meter-value">${shown}</span>`}
    </div>
  `;
}

// ---------------------------------------------------------------------------
// HealthStrip
// ---------------------------------------------------------------------------

/**
 * Recent traffic in fixed slots, oldest on the left: each block is green when
 * (nearly) everything succeeded, amber when some failed, red when most did,
 * and unlit when the slot had no traffic. The summary is also given as text
 * for screen readers; show the numbers next to the strip as well.
 *
 * buckets  [{ ok, failed, label? }]; `label` is the slot's time range
 * slots    fixed number of blocks (default 20); missing older slots are unlit
 * label    what this is the health of ("Credential key-1")
 * noun     what the buckets count, in the plural, for the accessible summary
 *          (default "requests"; "upstream attempts" on a provider board)
 */
export function HealthStrip({ buckets = [], slots = 20, label, noun = 'requests', class: className }) {
  const recent = buckets.slice(-slots);
  const padded = [...Array.from({ length: Math.max(0, slots - recent.length) }, () => null), ...recent];
  const ok = recent.reduce((sum, b) => sum + (b.ok || 0), 0);
  const failed = recent.reduce((sum, b) => sum + (b.failed || 0), 0);
  const summary = ok + failed === 0 ? 'no recent traffic' : `${formatPercent(ok / (ok + failed))} of ${ok + failed} recent ${noun} succeeded`;
  return html`
    <span class=${cx('health', className)} role="img" aria-label=${`${label ? `${label}: ` : ''}${summary}`}>
      ${padded.map((bucket, i) => {
        const count = bucket ? (bucket.ok || 0) + (bucket.failed || 0) : 0;
        const ratio = count ? (bucket.ok || 0) / count : null;
        const tone = ratio == null ? undefined : ratio >= 0.98 ? 'clear' : ratio >= 0.8 ? 'caution' : 'stop';
        const title = bucket && count ? `${bucket.label ? `${bucket.label}: ` : ''}${bucket.ok || 0} ok, ${bucket.failed || 0} failed` : bucket?.label ? `${bucket.label}: no traffic` : undefined;
        return html`<span key=${i} class="health-block" data-tone=${tone} title=${title}></span>`;
      })}
    </span>
  `;
}
