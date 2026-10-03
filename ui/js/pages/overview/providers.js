// Overview: the provider health board. One row per provider: its credentials
// as signal lamps with the words that go with them, its recent upstream
// attempts as a health strip, the share of them that succeeded and the mean
// latency. Rows that hurt come first.

import { html, useEffect, useMemo, useRef } from '../../../vendor/preact-htm.js';
import { Button, HealthStrip, Panel, Skeleton, StatusLamp, Table } from '../../components/index.js';
import { DASH, formatDuration, formatNumber, formatPercent, formatTime, plural } from '../../lib/format.js';
import { href, navigate } from '../../lib/router.js';
import { useStore } from '../../lib/store.js';
import { attempts, readAttempts, serverNow, useServerNow } from './data.js';
import { credentialDetail, credentialTitle, nextCooldownEnd, providerAttempts, sortProviders, summarizeProvider } from './model.js';
import { described } from './stale.js';

// More lamps than this in one row stop being readable; the sentence next to
// them still counts every credential.
const MAX_LAMPS = 12;
// Credentials named one by one under a provider before "and N more".
const MAX_NAMED = 3;
// Rows before the board scrolls inside its panel.
const ROWS_BEFORE_SCROLL = 8;
// The strips are recomputed this often, not on every tick of the countdowns.
const STRIP_STEP_MS = 5_000;

const LAMP_ORDER = { stop: 0, caution: 1, clear: 2, off: 3 };

function CredentialLamps({ summary, now }) {
  const { credentials } = summary;
  if (credentials.length === 0) return null;
  // Trouble first, so it is never among the lamps that did not fit.
  const ordered = credentials.length > MAX_LAMPS ? [...credentials].sort((a, b) => LAMP_ORDER[a.tone] - LAMP_ORDER[b.tone]) : credentials;
  const shown = ordered.slice(0, MAX_LAMPS);
  const rest = credentials.length - shown.length;
  // The words beside and below the lamps say the same thing, credential by
  // credential, so the lamps themselves stay out of the accessibility tree.
  // (The credentials of a provider that is switched off all read "disabled":
  // no lamp is lit.)
  return html`
    <span class="overview-lamps" aria-hidden="true">
      ${shown.map((cred) => html`<${StatusLamp} key=${cred.id} tone=${cred.tone} title=${credentialTitle(cred, now)} />`)}
      ${rest > 0 && html`<span class="overview-lamps-more">+${formatNumber(rest)}</span>`}
    </span>
  `;
}

/** Which credential is cooling down or unusable, and why: text, not a tooltip. */
function NamedCredentials({ summary, now }) {
  const named = summary.trouble;
  if (named.length === 0) return null;
  const shown = named.slice(0, MAX_NAMED);
  const rest = named.length - shown.length;
  return html`
    <ul class="overview-cred-list">
      ${shown.map(
        (cred) => html`
          <li key=${cred.id}>
            <span class="overview-cred-name mono" title=${cred.name}>${cred.name}</span>
            <span class="overview-cred-state">${credentialDetail(cred, now)}</span>
          </li>
        `,
      )}
      ${rest > 0 && html`<li class="overview-cred-rest">and ${formatNumber(rest)} more, listed under Providers</li>`}
    </ul>
  `;
}

/**
 * providers  /providers (useResource)
 * recent     /requests?limit=… (useResource): the page of request records the
 *            attempt log was filled from
 * onExpire   called once when a cooldown on the board has run out, so the
 *            page can ask the gateway what the credential's state is now
 */
export default function ProviderBoard({ providers, recent, onExpire }) {
  const now = useServerNow(1000);
  const version = useStore(attempts, (s) => s.version);

  const rows = useMemo(() => sortProviders((providers.data ?? []).map((p) => summarizeProvider(p, now))), [providers.data, now]);
  const step = Math.floor(now / STRIP_STEP_MS);
  // eslint-disable-next-line react-hooks/exhaustive-deps
  const health = useMemo(() => providerAttempts(readAttempts(), now), [version, step]);

  // The gateway announces the start of a cooldown, not its end: when one on
  // the board runs out, ask what the state is now. One timer per end time,
  // left to fire: the board renders every second, and the render that first
  // sees a cooldown as over must not cancel the question about it.
  const nextEnd = nextCooldownEnd(rows);
  const expire = useRef(onExpire);
  expire.current = onExpire;
  const timers = useRef(new Map());
  useEffect(() => {
    if (nextEnd == null || timers.current.has(nextEnd)) return;
    const wait = Math.min(Math.max(0, nextEnd - serverNow()) + 400, 2_000_000_000);
    timers.current.set(
      nextEnd,
      setTimeout(() => {
        timers.current.delete(nextEnd);
        expire.current?.();
      }, wait),
    );
  }, [nextEnd]);
  useEffect(
    () => () => {
      for (const timer of timers.current.values()) clearTimeout(timer);
      timers.current.clear();
    },
    [],
  );

  const totals = rows.reduce(
    (sum, row) => {
      sum.ready += row.counts.ready;
      sum.active += row.counts.total - row.counts.disabled;
      return sum;
    },
    { ready: 0, active: 0 },
  );

  const healthOf = (row) => health.byName.get(row.name) ?? null;
  // The window the attempts cover: the last hour, or less on a gateway so
  // busy that the records loaded do not reach that far back.
  const sinceClock = formatTime(health.since).slice(0, 5);
  const windowWords = health.full ? 'in the last hour' : `since ${sinceClock}`;
  const attemptsFailed = !health.known && Boolean(recent.error);

  const columns = [
    {
      key: 'name',
      header: 'Provider',
      primary: true,
      sortable: true,
      render: (row) => html`
        <span class="overview-provider">
          <span class="overview-provider-name mono" title=${row.name}>${row.name}</span>
          <span class="overview-provider-kind">${row.kind}</span>
        </span>
      `,
    },
    {
      key: 'credentials',
      header: 'Credentials',
      render: (row) => html`
        <div class="overview-cred">
          <${CredentialLamps} summary=${row} now=${now} />
          <span class="overview-cred-text">${row.text}</span>
          <${NamedCredentials} summary=${row} now=${now} />
        </div>
      `,
    },
    {
      key: 'health',
      header: health.full ? 'Attempts, last hour' : `Attempts since ${sinceClock}`,
      render: (row) => {
        if (!health.known) {
          return attemptsFailed ? html`<span class="faint" title="The request records could not be loaded">${DASH}</span>` : html`<${Skeleton} width="124px" height="12px" />`;
        }
        return html`<${HealthStrip} buckets=${healthOf(row)?.buckets ?? []} label=${`${row.name}, ${windowWords}`} noun=${['upstream attempt', 'upstream attempts']} />`;
      },
    },
    {
      key: 'success',
      header: 'Succeeded',
      align: 'right',
      num: true,
      sortable: true,
      sortValue: (row) => healthOf(row)?.success ?? null,
      render: (row) => {
        const entry = healthOf(row);
        if (!entry || entry.success == null) return html`<span class="faint" title=${health.known ? `No upstream attempts ${windowWords}` : undefined}>${DASH}</span>`;
        return html`
          <span class="overview-ratio" title=${`${formatNumber(entry.attempts - entry.failed)} of ${plural(entry.attempts, 'upstream attempt')} succeeded ${windowWords}`}>
            <span>${formatPercent(entry.success)}</span>
            <span class="overview-ratio-count">${formatNumber(entry.attempts - entry.failed)} of ${formatNumber(entry.attempts)}</span>
          </span>
        `;
      },
    },
    {
      key: 'latency',
      header: 'Latency',
      align: 'right',
      num: true,
      sortable: true,
      sortValue: (row) => row.latency,
      render: (row) => (row.latency == null ? html`<span class="faint">${DASH}</span>` : html`<span title="Moving average of the response latency">${formatDuration(row.latency)}</span>`),
    },
  ];

  const summary = providers.data
    ? rows.length === 0
      ? 'Upstreams the gateway routes to'
      : `${plural(rows.length, 'provider')} · ${totals.active === 0 ? 'no active credentials' : `${formatNumber(totals.ready)} of ${plural(totals.active, 'credential')} ready`}`
    : 'Upstreams the gateway routes to';

  return html`
    <${Panel}
      class="overview-board"
      title="Provider health"
      description=${html`${described(summary, providers, recent)}${attemptsFailed &&
      providers.data != null &&
      html`<span class="overview-stale"><${StatusLamp} tone="caution" label="Attempts could not be loaded" /></span>`}`}
      flush
      actions=${html`<${Button} size="sm" href=${href('/providers')} iconRight="arrow-right">Providers<//>`}
    >
      <${Table}
        columns=${columns}
        rows=${providers.data ? rows : undefined}
        rowKey="name"
        loading=${providers.loading}
        error=${providers.data ? null : providers.error}
        errorTitle="Could not load the providers"
        onRetry=${providers.refresh}
        skeletonRows=${3}
        maxHeight=${rows.length > ROWS_BEFORE_SCROLL ? '452px' : undefined}
        onRowClick=${(row) => navigate('/providers', { query: { open: row.name } })}
        caption=${`Providers with the state of their credentials, their upstream attempts ${windowWords} (every call the gateway made to the provider, including those it then failed over from), the share that succeeded, and the latency. Select a row to open the provider.`}
        empty=${{
          icon: 'providers',
          title: 'No providers yet',
          description: 'A provider is an upstream the gateway routes requests to. Add one and it appears here with a lamp for each credential.',
          action: html`<${Button} icon="plus" href=${href('/providers', { new: 1 })}>Add a provider<//>`,
        }}
      />
    <//>
  `;
}
