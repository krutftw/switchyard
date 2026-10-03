// Overview: live vitals. One number per question, from the "stats" frame the
// gateway pushes every second, with a few minutes of history behind each as a
// sparkline. Without the live connection the same cells are fed by polling.

import { html, useEffect, useState } from '../../../vendor/preact-htm.js';
import { Sparkline, Stat, StatGroup, StatusLamp } from '../../components/index.js';
import { formatCompact, formatDuration, formatNumber, formatPercent, formatTime, formatTokens } from '../../lib/format.js';
import { useIsPhone, useResource } from '../../lib/hooks.js';
import { useLive } from '../../lib/live.js';
import { gauges, markFrame, serverNow, syncClock } from './data.js';
import { errorRateTone, errorRateWords, historySeries, latencyUnknown, pushHistory, vitalsFromPoll, vitalsFromStats } from './model.js';

const POLL_MS = 5_000;
// A frame older than this is not shown as current after coming back to the page.
const FRESH_MS = 5_000;
// Points a sparkline needs before it is drawn (one point is 5 seconds): two
// or three are a dash, not a trend.
const MIN_POINTS = 4;

// Module level: the history survives a visit to another page (with a gap for
// the time the page was not listening).
const history = [];
let lastFrame = null; // { vitals, receivedAt }

function remember(vitals) {
  if (vitals?.at == null) return;
  // No request in the last hour: the percentiles describe nothing.
  const quiet = latencyUnknown(vitals);
  pushHistory(history, {
    at: vitals.at,
    rpm: vitals.rpm,
    tpm: vitals.tpm,
    // Polled error rates cover an hour, not a minute: not the same line.
    errorRate: vitals.errorWindow === 'minute' ? vitals.errorRate : null,
    inFlight: vitals.inFlight,
    p50: quiet ? null : vitals.p50,
    p95: quiet ? null : vitals.p95,
    streams: vitals.streams,
    sockets: vitals.sockets,
  });
}

const LIVE_NOTE = {
  connecting: { tone: 'info', label: 'Connecting to live updates', detail: `refreshing every ${POLL_MS / 1000}s meanwhile` },
  reconnecting: { tone: 'caution', label: 'Live connection lost', detail: `refreshing every ${POLL_MS / 1000}s` },
  offline: { tone: 'stop', label: 'Live connection lost', detail: `refreshing every ${POLL_MS / 1000}s` },
  unavailable: { tone: 'off', label: 'Live updates are off', detail: `refreshing every ${POLL_MS / 1000}s` },
  idle: { tone: 'off', label: 'Live updates are off', detail: `refreshing every ${POLL_MS / 1000}s` },
};

/**
 * status        /status (useResource): gauges and totals for the polling fallback
 * liveStatus    liveState.status
 * down          null, or { at } while the gateway does not answer: the numbers
 *               are then the last ones heard, and say so
 */
export default function Vitals({ status, liveStatus, down = null }) {
  const liveOpen = liveStatus === 'open';
  const phone = useIsPhone();
  const [frame, setFrame] = useState(() => (lastFrame && Date.now() - lastFrame.receivedAt < FRESH_MS ? lastFrame.vitals : null));

  useLive('stats', (data) => {
    const vitals = vitalsFromStats(data);
    if (!vitals) return;
    syncClock(vitals.at);
    markFrame();
    remember(vitals);
    lastFrame = { vitals, receivedAt: Date.now() };
    setFrame(vitals);
  });

  // Without the live connection: rates and percentiles from the usage summary.
  const summary = useResource(liveOpen ? null : ['/usage/summary', { range: '1h' }], { pollMs: POLL_MS });
  const polled = liveOpen ? null : vitalsFromPoll(status.data, summary.data);
  useEffect(() => {
    if (polled && summary.data && !status.error) remember(polled);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [polled?.at, summary.updatedAt]);

  // Live: the last frame. Polling: the polled numbers once they are complete;
  // until then the last frame, rather than a row of dashes.
  const vitals = liveOpen ? frame : summary.data ? polled : (frame ?? polled);
  const loading = !vitals;
  const totals = vitals?.totals;
  const idle = totals != null && totals.requests === 0;
  const now = serverNow();

  // The activity feed compares its rows with this count.
  const inFlight = vitals?.inFlight ?? null;
  useEffect(() => {
    gauges.set({ inFlight: down ? null : inFlight });
  }, [inFlight, Boolean(down)]);

  const spark = (field, label, mode) =>
    html`<${Sparkline} data=${historySeries(history, field, now, mode)} minPoints=${MIN_POINTS} width=${phone ? 120 : 96} label=${`${label} over the last few minutes`} />`;

  const rate = vitals?.errorRate;
  const minuteWindow = vitals?.errorWindow === 'minute';
  const noRecent = minuteWindow && vitals?.rpm === 0;
  const rateText = rate == null || noRecent ? null : formatPercent(rate);
  // The lamp judges the rate; its words say against which line.
  const rateTone = noRecent || down ? null : errorRateTone(rate);
  const failedLastMinute = minuteWindow && rate != null && vitals.rpm > 0 ? Math.round(rate * vitals.rpm) : null;
  let errorHint = null;
  if (loading) errorHint = null;
  else if (noRecent) errorHint = 'No requests in the last minute';
  else if (failedLastMinute != null) errorHint = `${formatNumber(failedLastMinute)} of ${formatNumber(vitals.rpm)} failed, last minute`;
  // Without the live connection the rate comes from the hourly summary.
  else errorHint = minuteWindow ? 'Last minute' : 'Last hour';
  const tokensTotal = totals ? (totals.input_tokens || 0) + (totals.cache_read_tokens || 0) + (totals.cache_write_tokens || 0) + (totals.output_tokens || 0) : null;

  // The percentiles cover the last hour. After an hour without a request the
  // gateway reports 0 for both, made of 0 samples: "nothing to measure", not
  // "0ms". The cell then shows a dash. The count decides, not the totals: a
  // gateway that has just restarted has served nothing since its start and
  // still knows the hour before it.
  const quiet = vitals != null && latencyUnknown(vitals);
  const latencyHint = (words) => (!quiet ? words : idle ? 'No requests yet' : 'No requests in the last hour');
  const latency = (value) => (vitals && !quiet ? formatDuration(value) : null);

  const note = down
    ? { tone: 'stop', label: 'No answer from the gateway', detail: `these are the values of ${formatTime(down.at)}` }
    : liveOpen
      ? null
      : LIVE_NOTE[liveStatus] ?? LIVE_NOTE.idle;

  return html`
    <section class="overview-section" aria-labelledby="overview-vitals-title">
      <div class="overview-section-head">
        <h2 id="overview-vitals-title">${down ? 'Last known' : 'Right now'}</h2>
        ${note
          ? html`<${StatusLamp} tone=${note.tone} label=${note.label} detail=${note.detail} />`
          : html`<span class="faint overview-section-note">Updated every second. Trend lines cover up to the last 5 minutes.</span>`}
      </div>
      <${StatGroup} label="Live vitals" class="overview-vitals" data-stale=${down ? '' : undefined}>
        <${Stat}
          label="Requests per minute"
          value=${vitals ? formatCompact(vitals.rpm) : null}
          hint=${totals ? `${formatCompact(totals.requests)} since start` : null}
          trend=${spark('rpm', 'Requests per minute')}
          loading=${loading}
        />
        <${Stat}
          label="Tokens per minute"
          value=${vitals ? formatTokens(vitals.tpm) : null}
          hint=${tokensTotal != null ? `${formatTokens(tokensTotal)} since start` : null}
          trend=${spark('tpm', 'Tokens per minute')}
          loading=${loading}
        />
        <${Stat}
          label="Error rate"
          value=${rateText ? rateText.replace('%', '') : null}
          unit=${rateText ? '%' : undefined}
          lamp=${rateTone}
          lampLabel=${rateTone ? errorRateWords(rate, minuteWindow ? 'minute' : 'hour') : undefined}
          hint=${errorHint}
          trend=${spark('errorRate', 'Error rate')}
          loading=${loading}
        />
        <${Stat}
          label="In flight"
          value=${vitals ? formatNumber(vitals.inFlight) : null}
          hint=${down ? 'When last heard' : 'Being served now'}
          trend=${spark('inFlight', 'Requests in flight', 'max')}
          loading=${loading}
        />
        <${Stat} label="p50 latency" value=${latency(vitals?.p50)} hint=${latencyHint('Median, last hour')} trend=${spark('p50', 'Median latency')} loading=${loading} />
        <${Stat} label="p95 latency" value=${latency(vitals?.p95)} hint=${latencyHint('Slowest 5%, last hour')} trend=${spark('p95', '95th percentile latency')} loading=${loading} />
        <${Stat}
          label="Active streams"
          value=${vitals ? formatNumber(vitals.streams) : null}
          hint=${down ? 'When last heard' : 'Streaming now'}
          trend=${spark('streams', 'Active streams', 'max')}
          loading=${loading}
        />
        <${Stat}
          label="WebSocket clients"
          value=${vitals ? formatNumber(vitals.sockets) : null}
          hint=${down ? 'When last heard' : 'Connected now'}
          trend=${spark('sockets', 'WebSocket clients', 'max')}
          loading=${loading}
        />
      <//>
    </section>
  `;
}
