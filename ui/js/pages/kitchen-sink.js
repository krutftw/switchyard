// Component kit (#/_kit): every component and chart with sample data.
// This page is the reference for page authors. When a component changes,
// its example here changes with it; when you need a pattern, copy it from
// here. Props are documented in ui/UI_GUIDE.md and above each component.

import { html, useMemo, useRef, useState } from '../../vendor/preact-htm.js';
import {
  AreaChart,
  Badge,
  BarChart,
  BarList,
  Button,
  Checkbox,
  CodeBlock,
  ConfirmDialog,
  CopyButton,
  Drawer,
  EmptyState,
  ErrorState,
  Field,
  Form,
  FormActions,
  FormError,
  FormRow,
  HealthStrip,
  ICON_NAMES,
  Icon,
  IconButton,
  Input,
  Kbd,
  KeyValue,
  LatencyBars,
  LineChart,
  LoadMore,
  Menu,
  Meter,
  Modal,
  Notice,
  NumberInput,
  Page,
  Pagination,
  Panel,
  SecretInput,
  Segmented,
  Select,
  Skeleton,
  Sparkline,
  Spinner,
  Stat,
  StatGroup,
  StatusLamp,
  Switch,
  Table,
  Tabs,
  TagInput,
  Textarea,
  Timeline,
  Tooltip,
  TrackDiagram,
  confirm,
  sortRows,
  toast,
  toneForStatus,
  useIssues,
} from '../components/index.js';
import { ApiError } from '../lib/api.js';
import { loadStyles } from '../lib/dom.js';
import { DASH, formatCompact, formatCurrency, formatDate, formatDuration, formatNumber, formatPercent, formatRelativeTime, formatTime, formatTokens } from '../lib/format.js';
import { useAsync, useNow } from '../lib/hooks.js';
import { liveState, useLive, useLiveGap } from '../lib/live.js';
import { navigate, useLeaveGuard, useQueryParam } from '../lib/router.js';
import { useStore } from '../lib/store.js';
import { setThemePref, theme } from '../lib/theme.js';

await loadStyles('pages/kitchen-sink.css');

// ---------------------------------------------------------------------------
// Sample data (seeded, so the page looks the same on every load)
// ---------------------------------------------------------------------------

function seeded(seed) {
  let state = seed >>> 0;
  return () => {
    state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
    return state / 4294967296;
  };
}

function makeSeries(seed, count, base, swing) {
  const random = seeded(seed);
  let value = base;
  return Array.from({ length: count }, (_, i) => {
    const wave = Math.sin((i / count) * Math.PI * 2 - 1) * swing * 0.6;
    value = Math.max(0, value * 0.6 + (base + wave + (random() - 0.5) * swing) * 0.4);
    return Math.round(value);
  });
}

const HOUR = 3_600_000;
const END = Math.floor(Date.now() / HOUR) * HOUR;
const X48 = Array.from({ length: 48 }, (_, i) => END - (47 - i) * (HOUR / 2));
const X24 = Array.from({ length: 24 }, (_, i) => END - (23 - i) * HOUR);

const REQUEST_SERIES = [
  { key: 'gpt-4o', label: 'gpt-4o', values: makeSeries(11, 48, 420, 260) },
  { key: 'claude-sonnet-4-5', label: 'claude-sonnet-4-5', values: makeSeries(23, 48, 300, 200) },
  { key: 'gemini-2.5-pro', label: 'gemini-2.5-pro', values: makeSeries(37, 48, 150, 120) },
];

const TOKEN_SERIES = [
  { key: 'input', label: 'Input', values: makeSeries(5, 48, 820_000, 500_000) },
  { key: 'cached', label: 'Cache read', values: makeSeries(6, 48, 310_000, 240_000) },
  { key: 'output', label: 'Output', values: makeSeries(7, 48, 190_000, 140_000) },
];

const OUTCOME_SERIES = [
  { key: 'ok', label: 'Succeeded', values: makeSeries(41, 24, 900, 700) },
  { key: 'failed', label: 'Failed', color: 'var(--lamp-stop)', values: makeSeries(43, 24, 40, 70) },
];

// Day buckets the way the gateway cuts them: at UTC midnight. Small counts,
// so the axis needs whole-number ticks.
const DAY = 24 * HOUR;
const TODAY_UTC = Math.floor(Date.now() / DAY) * DAY;
const X14D = Array.from({ length: 14 }, (_, i) => TODAY_UTC - (13 - i) * DAY);
const FAILOVER_SERIES = [{ key: 'failovers', label: 'Failovers', values: [0, 1, 0, 0, 2, 1, 0, 0, 0, 3, 1, 0, 1, 0] }];

const TOP_MODELS = [
  { key: 'gpt-4o', label: 'gpt-4o', value: 18_420_000, hint: 'openai-main' },
  { key: 'claude-sonnet-4-5', label: 'claude-sonnet-4-5', value: 12_870_000, hint: 'anthropic' },
  { key: 'gemini-2.5-pro', label: 'gemini-2.5-pro', value: 6_310_000, hint: 'google' },
  { key: 'gpt-4o-mini', label: 'gpt-4o-mini', value: 3_950_000, hint: 'openai-main' },
  { key: 'claude-haiku-4-5', label: 'claude-haiku-4-5', value: 1_240_000, hint: 'anthropic' },
  { key: 'text-embedding-3-small', label: 'text-embedding-3-small', value: 410_000, hint: 'openai-main' },
  { key: 'o3-mini', label: 'o3-mini', value: 96_000, hint: 'azure-east' },
];

const SAMPLE_ROWS = [
  { id: 'req_8fK2mQx1', at: Date.now() - 4_000, key: 'build-bot', model: 'gpt-4o', provider: 'openai-main', protocol: 'openai-chat', status: 200, duration: 1840, tokens: 3120, cost: 0.0214 },
  { id: 'req_Lw03bZtp', at: Date.now() - 19_000, key: 'support-app', model: 'claude-sonnet-4-5', provider: 'anthropic', protocol: 'anthropic', status: 200, duration: 5230, tokens: 9480, cost: 0.0712 },
  { id: 'req_p9QaD5vN', at: Date.now() - 46_000, key: 'notebooks', model: 'gemini-2.5-pro', provider: 'google', protocol: 'gemini', status: 429, duration: 312, tokens: 0, cost: null },
  { id: 'req_Zc71hYue', at: Date.now() - 95_000, key: 'build-bot', model: 'fast', provider: 'openai-main', protocol: 'openai-responses', status: 200, duration: 624, tokens: 811, cost: 0.00042 },
  { id: 'req_m2XrT8sG', at: Date.now() - 240_000, key: 'support-app', model: 'gpt-4o', provider: 'azure-east', protocol: 'openai-chat', status: 502, duration: 12_400, tokens: 0, cost: null },
  { id: 'req_Ah4kV0dJ', at: Date.now() - 610_000, key: 'notebooks', model: 'claude-sonnet-4-5', provider: 'anthropic', protocol: 'anthropic', status: 200, duration: 2970, tokens: 14_250, cost: 0.1043 },
];

const SAMPLE_CREDENTIALS = [
  { id: 'openai-main:1', provider: 'openai-main', name: 'key 1', tone: 'clear', state: 'Ready', rpm: 412, priority: 0 },
  { id: 'openai-main:2', provider: 'openai-main', name: 'key 2', tone: 'caution', state: 'Cooling down', detail: '41s left', rpm: 0, priority: 0 },
  { id: 'azure-east:1', provider: 'azure-east', name: 'eastus', tone: 'clear', state: 'Ready', rpm: 96, priority: 1 },
  { id: 'google:1', provider: 'google', name: 'service account', tone: 'stop', state: 'Auth failed', rpm: 0, priority: 2 },
];

// The orders the credentials example offers through its own control.
const CREDENTIAL_ORDERS = [
  { value: 'priority', label: 'Priority', dir: 'asc' },
  { value: 'rpm', label: 'Busiest', dir: 'desc' },
  { value: 'name', label: 'Name', dir: 'asc' },
];

const SAMPLE_BODY = {
  model: 'gpt-4o',
  stream: true,
  temperature: 0.2,
  messages: [
    { role: 'system', content: 'You are a terse assistant.' },
    { role: 'user', content: 'Summarise the incident report in three bullet points.' },
  ],
  tools: [{ type: 'function', function: { name: 'lookup_ticket', parameters: { type: 'object', properties: { id: { type: 'string' } }, required: ['id'] } } }],
  metadata: { trace: null, retries: 0, cached: false },
};

// A body as it is captured: one line of text, with a seed too large for a
// JavaScript number and a temperature written as 1.0.
const SAMPLE_WIRE =
  '{"model":"gpt-4o","stream":true,"temperature":1.0,"seed":12345678901234567890,"messages":[{"role":"system","content":"You are a terse assistant."},{"role":"user","content":"Summarise the incident report in three bullet points."}],"tools":[],"metadata":{"trace":null,"retries":0,"cached":false}}';

const HEALTH = Array.from({ length: 20 }, (_, i) => {
  const random = seeded(100 + i);
  const total = i === 3 || i === 4 ? 0 : Math.round(20 + random() * 60);
  const failed = i === 12 ? Math.round(total * 0.6) : i === 13 ? Math.round(total * 0.1) : 0;
  return { ok: total - failed, failed, label: `${formatTime(END - (19 - i) * 600_000).slice(0, 5)}` };
});

const TRACK = {
  models: [
    { id: 'gpt-4o', label: 'gpt-4o' },
    { id: 'fast', label: 'fast', note: 'alias of gpt-4o-mini' },
    { id: 'claude-sonnet-4-5', label: 'claude-sonnet-4-5' },
    { id: 'gemini-2.5-pro', label: 'gemini-2.5-pro' },
  ],
  providers: [
    { id: 'openai-main', label: 'openai-main', state: 'clear', note: '2 credentials' },
    { id: 'azure-east', label: 'azure-east', state: 'caution', note: 'cooling down, 41s left' },
    { id: 'anthropic', label: 'anthropic', state: 'clear', note: '1 credential' },
    { id: 'google', label: 'google', state: 'stop', note: 'auth failed' },
    { id: 'openrouter', label: 'openrouter', state: 'off', note: 'disabled' },
  ],
  routes: [
    { model: 'gpt-4o', provider: 'openai-main', order: 1 },
    { model: 'gpt-4o', provider: 'azure-east', order: 2 },
    { model: 'gpt-4o', provider: 'openrouter', order: 3 },
    { model: 'fast', provider: 'azure-east', order: 1 },
    { model: 'fast', provider: 'openai-main', order: 2 },
    { model: 'claude-sonnet-4-5', provider: 'anthropic', order: 1 },
    { model: 'claude-sonnet-4-5', provider: 'openrouter', order: 2 },
    { model: 'gemini-2.5-pro', provider: 'google', order: 1 },
  ],
};

// ---------------------------------------------------------------------------
// Page furniture
// ---------------------------------------------------------------------------

const SECTIONS = [
  ['tokens', 'Tokens'],
  ['buttons', 'Buttons'],
  ['status', 'Status'],
  ['stats', 'Stats'],
  ['table', 'Table'],
  ['choice', 'Tabs and segmented'],
  ['forms', 'Forms'],
  ['overlays', 'Overlays'],
  ['content', 'Code and details'],
  ['states', 'States'],
  ['charts', 'Charts'],
  ['track', 'Track diagram'],
  ['icons', 'Icons'],
  ['live', 'Live'],
];

function Section({ id, title, description, children }) {
  return html`
    <section class="kit-section" id=${`kit-${id}`} aria-labelledby=${`kit-${id}-title`}>
      <div class="kit-section-head">
        <h2 id=${`kit-${id}-title`}>${title}</h2>
        ${description && html`<p class="muted">${description}</p>`}
      </div>
      ${children}
    </section>
  `;
}

function Example({ label, children, wide = false }) {
  return html`
    <div class="kit-example" data-wide=${wide ? '' : undefined}>
      <div class="plate-label">${label}</div>
      <div class="kit-example-body">${children}</div>
    </div>
  `;
}

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

const SURFACES = ['bg-sunken', 'bg-ground', 'bg-surface', 'bg-raised', 'bg-hover', 'bg-active'];
const INKS = ['text', 'text-2', 'text-3', 'accent-ink', 'clear-ink', 'caution-ink', 'stop-ink'];
const LAMPS = ['lamp-clear', 'lamp-caution', 'lamp-stop', 'lamp-info', 'lamp-off'];
const SERIES = ['series-1', 'series-2', 'series-3', 'series-4', 'series-5', 'series-6', 'series-other'];

function ThemeSample({ name }) {
  return html`
    <div class="kit-theme" data-theme=${name}>
      <div class="row row-between">
        <strong>${name === 'dark' ? 'Dark' : 'Light'}</strong>
        <${StatusLamp} tone="clear" label="Serving" pulse />
      </div>
      <div class="kit-swatches">
        ${SURFACES.map((token) => html`<span class="kit-swatch" key=${token} style=${`background:var(--${token})`} title=${`--${token}`}></span>`)}
      </div>
      <div class="kit-inks">
        ${INKS.map((token) => html`<span key=${token} style=${`color:var(--${token})`}>--${token}</span>`)}
      </div>
      <div class="kit-swatches">
        ${LAMPS.map((token) => html`<span class="kit-swatch kit-swatch-round" key=${token} style=${`background:var(--${token})`} title=${`--${token}`}></span>`)}
        <span class="kit-swatch-gap"></span>
        ${SERIES.map((token) => html`<span class="kit-swatch" key=${token} style=${`background:var(--${token})`} title=${`--${token}`}></span>`)}
      </div>
      <div class="row row-wrap">
        <${Badge} tone="clear" lamp>Healthy<//>
        <${Badge} tone="caution" lamp>Cooling<//>
        <${Badge} tone="stop" lamp>Failed<//>
        <${Badge} tone="info">Active<//>
        <${Badge} mono>openai-chat<//>
      </div>
      <div class="row row-wrap">
        <${Button} variant="primary" size="sm">Primary<//>
        <${Button} size="sm">Secondary<//>
        <${Button} variant="ghost" size="sm">Ghost<//>
        <${Button} variant="danger-quiet" size="sm">Delete<//>
      </div>
      <div class="panel"><div class="panel-body"><${Sparkline} data=${REQUEST_SERIES[0].values.slice(-24)} width=${200} height=${32} /></div></div>
    </div>
  `;
}

function TokensSection() {
  const { pref } = useStore(theme);
  return html`
    <${Section} id="tokens" title="Tokens" description="Both themes side by side. Colour is semantic: green clear, amber caution, red stop, the blue-white accent for selected and in progress. The six series colours are for charts only.">
      <div class="row row-wrap">
        <${Segmented}
          label="Theme of this page"
          value=${pref}
          onChange=${setThemePref}
          options=${[
            { value: 'system', label: 'System', icon: 'monitor' },
            { value: 'light', label: 'Light', icon: 'sun' },
            { value: 'dark', label: 'Dark', icon: 'moon' },
          ]}
        />
        <span class="faint">The choice is stored on this device.</span>
      </div>
      <div class="grid-2">
        <${ThemeSample} name="dark" />
        <${ThemeSample} name="light" />
      </div>
      <${Panel} title="Type">
        <div class="stack" style="--gap:var(--space-3)">
          <h1>Page title, 23px</h1>
          <h2>Panel title, 16px</h2>
          <p>Body text, 14px. Plain, specific sentences. <a href="#/_kit">A link</a> looks like this.</p>
          <p class="muted">Secondary text for descriptions and help.</p>
          <p class="faint">Tertiary text for captions and timestamps.</p>
          <p><span class="mono">claude-sonnet-4-5</span> identifiers in mono, <span class="num">1,284,019</span> numbers in tabular mono.</p>
          <span class="plate-label">Plate label for table headers and group captions</span>
        </div>
      <//>
    <//>
  `;
}

function ButtonsSection() {
  const [loading, setLoading] = useState(false);
  const run = () => {
    setLoading(true);
    setTimeout(() => setLoading(false), 1600);
  };
  return html`
    <${Section} id="buttons" title="Buttons" description="One primary button per view. Labels are verbs that name the action.">
      <div class="kit-grid">
        <${Example} label="Variants">
          <${Button} variant="primary" icon="plus">Add provider<//>
          <${Button}>Test connection<//>
          <${Button} variant="ghost">Cancel<//>
          <${Button} variant="danger">Delete provider<//>
          <${Button} variant="danger-quiet" icon="trash">Delete<//>
        <//>
        <${Example} label="Sizes">
          <${Button} size="sm">Small<//>
          <${Button}>Medium<//>
          <${Button} size="lg">Large<//>
        <//>
        <${Example} label="Loading and disabled">
          <${Button} variant="primary" loading=${loading} onClick=${run}>Save changes<//>
          <${Button} loading>Saving<//>
          <${Button} disabled>Disabled<//>
          <${Button} variant="primary" disabled>Disabled<//>
        <//>
        <${Example} label="Icons, links, copy">
          <${Button} icon="refresh">Refresh<//>
          <${Button} iconRight="external" href="#/_kit">Open docs<//>
          <${IconButton} icon="refresh" label="Refresh" />
          <${IconButton} icon="edit" label="Edit" variant="secondary" />
          <${IconButton} icon="trash" label="Delete" size="sm" />
          <${CopyButton} value="sy-example-key" label="Copy key" />
          <${CopyButton} value="curl -s http://127.0.0.1:8317/v1/models" variant="secondary">Copy curl<//>
          <${Spinner} label="Loading" />
        <//>
      </div>
    <//>
  `;
}

function StatusSection() {
  return html`
    <${Section} id="status" title="Status" description="A lamp always comes with words. Unlit lamps are hollow, so off does not depend on colour.">
      <div class="kit-grid">
        <${Example} label="StatusLamp">
          <${StatusLamp} tone="clear" label="Serving" />
          <${StatusLamp} tone="caution" label="Cooling down" detail="41s left" />
          <${StatusLamp} tone="stop" label="Auth failed" />
          <${StatusLamp} tone="info" label="Streaming" pulse />
          <${StatusLamp} tone="off" label="Disabled" />
        <//>
        <${Example} label="Badge">
          <${Badge}>Neutral<//>
          <${Badge} tone="clear">Passthrough<//>
          <${Badge} tone="caution">Rate limited<//>
          <${Badge} tone="stop">Error<//>
          <${Badge} tone="info">Translated<//>
          <${Badge} outline>3 attempts<//>
        <//>
        <${Example} label="Mono badges, HTTP status">
          <${Badge} mono>openai-responses<//>
          <${Badge} mono>anthropic<//>
          ${[200, 429, 502].map((code) => html`<${Badge} key=${code} mono tone=${toneForStatus(code)}>${code}<//>`)}
        <//>
        <${Example} label="Lamp in a badge, key caps">
          <${Badge} tone="clear" lamp>Healthy<//>
          <${Badge} tone="caution" lamp>Degraded<//>
          <${Badge} tone="off" lamp>Unknown<//>
          <span class="row" style="--gap:var(--space-1)"><${Kbd}>Ctrl<//><${Kbd}>K<//></span>
        <//>
      </div>
    <//>
  `;
}

function StatsSection() {
  const spark = REQUEST_SERIES[0].values.slice(-24);
  return html`
    <${Section} id="stats" title="Stats" description="Headline numbers share one instrument panel. The delta wears a lamp colour only when its direction is good or bad news. A lamp before the label says its judgement in words too: lampLabel is its name and its tooltip.">
      <${StatGroup} label="Sample statistics">
        <${Stat} label="Requests, last hour" value=${formatCompact(12_840)} delta=${0.124} deltaLabel="vs previous hour" trend=${html`<${Sparkline} data=${spark} label="Requests per half hour" />`} />
        <${Stat} label="Error rate" value="1.8" unit="%" delta=${0.31} goodWhen="down" deltaLabel="vs previous hour" lamp="caution" lampLabel="Above the 1% alert line" />
        <${Stat} label="Median latency" value=${formatDuration(1240)} delta=${-0.08} goodWhen="down" deltaLabel="vs yesterday" />
        <${Stat} label="Tokens today" value=${formatTokens(48_200_000)} hint=${`${formatCurrency(132.4)} estimated`} />
        <${Stat} label="Still loading" loading />
      <//>
    <//>
  `;
}

function TableSection() {
  const [mode, setMode] = useState('data');
  const [open, setOpen] = useQueryParam('request', '');
  const [page, setPage] = useState(1);
  const now = useNow(5000);
  const record = SAMPLE_ROWS.find((row) => row.id === open);
  const toPage = (next) => {
    setPage(next);
    // On the first and the last page the button that was pressed disables
    // itself, and a disabled button drops the focus on <body>: the other
    // button takes it.
    setTimeout(() => {
      const active = document.activeElement;
      if (active === document.body || active?.disabled) document.querySelector('#kit-table .pager button:not(:disabled)')?.focus();
    }, 0);
  };

  const columns = [
    { key: 'at', header: 'Time', sortable: true, width: '96px', render: (r) => html`<span class="faint">${formatRelativeTime(r.at, now)}</span>` },
    { key: 'model', header: 'Model', primary: true, sortable: true, render: (r) => html`<span class="mono">${r.model}</span>` },
    { key: 'key', header: 'Client key', sortable: true },
    { key: 'provider', header: 'Provider', sortable: true, hideOnPhone: true, render: (r) => html`<span class="mono">${r.provider}</span>` },
    { key: 'status', header: 'Status', sortable: true, render: (r) => html`<${Badge} mono tone=${toneForStatus(r.status)}>${r.status}<//>` },
    { key: 'duration', header: 'Duration', align: 'right', num: true, sortable: true, render: (r) => formatDuration(r.duration) },
    { key: 'tokens', header: 'Tokens', align: 'right', num: true, sortable: true, render: (r) => formatTokens(r.tokens) },
    { key: 'cost', header: 'Cost', align: 'right', num: true, sortable: true, render: (r) => formatCurrency(r.cost) },
  ];

  return html`
    <${Section} id="table" title="Table" description="Sort by any header. Rows open a drawer. On a phone each row becomes a card and the headers become a sort menu.">
      <${Panel}
        title="Recent requests"
        description="Sample rows"
        flush
        actions=${html`<${Segmented} size="sm" label="Table state" value=${mode} onChange=${setMode} options=${[
          { value: 'data', label: 'Data' },
          { value: 'loading', label: 'Loading' },
          { value: 'empty', label: 'Empty' },
          { value: 'error', label: 'Error' },
        ]} />`}
        footer=${mode === 'data' ? html`<${Pagination} class="grow" page=${page} pageSize=${6} total=${42} noun="requests" onPage=${toPage} />` : null}
      >
        <${Table}
          columns=${columns}
          rows=${mode === 'data' ? SAMPLE_ROWS : []}
          loading=${mode === 'loading'}
          error=${mode === 'error' ? new ApiError(502, 'The gateway could not read the request log.') : null}
          errorTitle="Could not load the requests"
          onRetry=${() => {
            setMode('data');
            // "Try again" goes away with the error state. Focus is never left
            // on <body>: it moves to what took the button's place.
            setTimeout(() => document.querySelector('#kit-table tbody tr[data-row-key]')?.focus(), 0);
          }}
          empty=${{ icon: 'requests', title: 'No requests yet', description: 'Requests appear here as soon as a client calls the gateway.' }}
          defaultSort=${{ key: 'at', dir: 'desc' }}
          onRowClick=${(row) => setOpen(row.id)}
          selectedKey=${open || null}
          caption="Recent requests"
        />
      <//>

      <${CredentialsExample} />

      <${Drawer}
        open=${!!record}
        onClose=${() => setOpen('')}
        title="Request"
        subtitle=${record?.id}
        actions=${record && html`<${CopyButton} value=${record.id} label="Copy request id" />`}
        footer=${html`<${Button} onClick=${() => setOpen('')}>Close<//>`}
      >
        ${record &&
        html`
          <div class="stack" style="--gap:var(--space-5)">
            <${KeyValue}
              items=${[
                { label: 'Request id', value: record.id, mono: true, copy: true },
                { label: 'Model', value: record.model, mono: true },
                { label: 'Provider', value: record.provider, mono: true },
                { label: 'Client key', value: record.key },
                { label: 'Status', value: html`<${Badge} mono tone=${toneForStatus(record.status)}>${record.status}<//>` },
                { label: 'Duration', value: formatDuration(record.duration) },
                { label: 'Cost', value: record.cost == null ? null : formatCurrency(record.cost) },
              ]}
            />
            <div class="stack" style="--gap:var(--space-3)">
              <h3>Attempts</h3>
              <${AttemptsTimeline} />
            </div>
            <${CodeBlock} title="Client request" value=${SAMPLE_BODY} />
          </div>
        `}
      <//>
    <//>
  `;
}

// A table whose page orders it with a control of its own: sorting is
// controlled (sort + onSort), and sortMenu=${false} leaves out the "Sort by"
// menu the phone layout would add, which would be a second control for the
// same thing. Two headers are not plain text (an abbreviation, a hidden
// "Actions"), so those columns say their name in `label`: it is what the
// phone card prints before each value.
function CredentialsExample() {
  const [sort, setSort] = useState({ key: 'priority', dir: 'asc' });
  const columns = [
    {
      key: 'name',
      header: 'Credential',
      primary: true,
      sortable: true,
      sortValue: (c) => `${c.provider} ${c.name}`,
      render: (c) => html`<span class="mono">${c.provider}</span> <span class="faint">${c.name}</span>`,
    },
    { key: 'state', header: 'State', render: (c) => html`<${StatusLamp} tone=${c.tone} label=${c.state} detail=${c.detail} />` },
    { key: 'rpm', header: html`<abbr title="Requests per minute">RPM</abbr>`, label: 'Requests per minute', align: 'right', num: true, sortable: true, render: (c) => formatNumber(c.rpm) },
    { key: 'priority', header: 'Priority', align: 'right', num: true, sortable: true },
    {
      key: 'actions',
      header: html`<span class="sr-only">Actions</span>`,
      label: 'Actions',
      align: 'right',
      render: (c) => html`<${IconButton} icon="refresh" size="sm" label=${`Reset the cooldown of ${c.provider} ${c.name}`} onClick=${() => toast.success('Cooldown cleared', { description: `${c.provider} ${c.name}` })} />`,
    },
  ];
  // With sort and onSort the table shows the order it is told about and
  // leaves the sorting to the page (which may do it on the server). Here it
  // is local, with the comparison the table itself uses.
  const rows = sortRows(SAMPLE_CREDENTIALS, columns.find((column) => column.key === sort.key), sort.dir);
  return html`
    <${Panel}
      title="Credentials"
      description="Ordered by the page: no sort menu, labelled columns"
      flush
      actions=${html`<${Segmented}
        size="sm"
        label="Order"
        value=${sort.key}
        onChange=${(key) => setSort({ key, dir: CREDENTIAL_ORDERS.find((order) => order.value === key).dir })}
        options=${CREDENTIAL_ORDERS}
      />`}
    >
      <${Table}
        columns=${columns}
        rows=${rows}
        sort=${sort}
        onSort=${(next) => setSort(next ?? { key: 'priority', dir: 'asc' })}
        sortMenu=${false}
        caption="Credentials"
      />
    <//>
  `;
}

function AttemptsTimeline() {
  return html`
    <${Timeline}
      items=${[
        {
          tone: 'stop',
          toneLabel: 'Rate limited',
          title: html`<span class="mono">azure-east</span> <span class="faint">eastus</span>`,
          badges: html`<${Badge} mono tone="caution">429<//>`,
          time: formatDuration(312),
          description: 'Rate limit reached. The credential cools down for 30s.',
        },
        {
          tone: 'stop',
          toneLabel: 'Failed',
          title: html`<span class="mono">openai-main</span> <span class="faint">key 2</span>`,
          badges: html`<${Badge} mono tone="stop">502<//>`,
          time: formatDuration(1480),
          description: 'Upstream connect error: connection reset.',
        },
        {
          tone: 'clear',
          toneLabel: 'Succeeded',
          title: html`<span class="mono">openai-main</span> <span class="faint">key 1</span>`,
          badges: html`<${Badge} mono tone="clear">200<//> <${Badge} tone="info">Translated<//>`,
          time: formatDuration(1840),
          description: 'First byte after 412ms. 3,120 tokens.',
        },
      ]}
    />
  `;
}

function ChoiceSection() {
  const [tab, setTab] = useState('summary');
  const [range, setRange] = useState('24h');
  const [view, setView] = useState('chart');
  const [part, setPart] = useState('client-request');
  const [capture, setCapture] = useState('all');
  return html`
    <${Section} id="choice" title="Tabs and segmented" description="Tabs switch between views of one thing. A segmented control picks a value that changes what the view shows.">
      <div class="kit-grid">
        <${Example} label="Tabs" wide>
          <div class="stack grow" style="--gap:var(--space-3)">
            <${Tabs}
              label="Request detail"
              value=${tab}
              onChange=${setTab}
              tabs=${[
                { id: 'summary', label: 'Summary' },
                { id: 'attempts', label: 'Attempts', count: 3 },
                { id: 'bodies', label: 'Bodies', icon: 'logs' },
                { id: 'raw', label: 'Raw events', disabled: true },
              ]}
            />
            <p class="muted">Showing: <span class="mono">${tab}</span>. Arrow keys move between tabs.</p>
          </div>
        <//>
        <${Example} label="Segmented">
          <${Segmented} label="Time range" value=${range} onChange=${setRange} options=${['1h', '24h', '7d', '30d']} />
          <${Segmented}
            label="View"
            size="sm"
            value=${view}
            onChange=${setView}
            options=${[
              { value: 'chart', label: 'Chart', icon: 'chart' },
              { value: 'table', label: 'Table', icon: 'table' },
            ]}
          />
        <//>
        <${Example} label="Tabs wider than their box">
          <div class="stack grow" style="--gap:var(--space-3)">
            <${Tabs}
              label="Parts of a request"
              value=${part}
              onChange=${setPart}
              tabs=${[
                { id: 'client-request', label: 'Client request' },
                { id: 'upstream-request', label: 'Upstream request' },
                { id: 'upstream-response', label: 'Upstream response' },
                { id: 'client-response', label: 'Client response' },
                { id: 'stream-events', label: 'Stream events', count: 128 },
                { id: 'headers', label: 'Headers' },
                { id: 'timing', label: 'Timing' },
              ]}
            />
            <p class="muted">The strip scrolls sideways. An edge fades while tabs are hidden beyond it, and the selected tab is brought into view: press End.</p>
          </div>
        <//>
        <${Example} label="Segmented in a field, with a warning">
          <${Field}
            label="Capture bodies"
            hint="Which requests keep their bodies for the request inspector."
            warning=${capture === 'all' ? 'Every request and response body is kept on disk, prompts included.' : undefined}
          >
            <${Segmented}
              label="Capture bodies"
              value=${capture}
              onChange=${setCapture}
              options=${[
                { value: 'off', label: 'Off' },
                { value: 'errors', label: 'Errors only' },
                { value: 'all', label: 'All requests' },
              ]}
            />
          <//>
        <//>
      </div>
    <//>
  `;
}

// The clear button of a search field. It is built in: a type="search" Input
// that brings no `actions` shows an x while it holds text.
function SearchExample() {
  const [text, setText] = useState('gpt-4o');
  const [applied, setApplied] = useState('gpt-4o');
  const [plain, setPlain] = useState('status:429');
  const [label, setLabel] = useState('eastus');
  return html`
    <${Example} label="Search and the clear button">
      <div class="stack grow" style="--gap:var(--space-3)">
        <${Input}
          label="Applied on Enter"
          type="search"
          icon="search"
          value=${text}
          onChange=${setText}
          onEnter=${() => setApplied(text.trim())}
          onClear=${() => setApplied('')}
          clearLabel="Clear search"
          placeholder="Model, key or request id"
          hint=${applied ? html`Showing requests that match <span class="mono">${applied}</span>. Clearing applies the empty search at once: onClear.` : 'Showing every request. Type and press Enter.'}
        />
        <${Input}
          label="Without the button"
          type="search"
          icon="filter"
          value=${plain}
          onChange=${setPlain}
          clearable=${false}
          hint="clearable is false: for a field that should never be emptied in one press."
        />
        <${Input} label="Any field can have one" value=${label} onChange=${setLabel} clearable hint="clearable on a plain text field." />
      </div>
    <//>
  `;
}

// useLeaveGuard in the smallest form that needs it. While the draft differs
// from what was saved, every way off this page asks first: links, the
// palette, Back, a typed address, sign-out, and (the browser's own prompt)
// closing or reloading the tab. Changes of the query on this page pass.
function LeaveGuardExample() {
  const [saved, setSaved] = useState('Build bot');
  const [draft, setDraft] = useState(saved);
  const dirty = draft !== saved;
  const field = useRef(null);
  const guard = useLeaveGuard(dirty, {
    title: 'Discard the unsaved display name?',
    message: 'What you typed in the leave guard example has not been saved.',
  });
  const save = useAsync(async () => {
    await new Promise((resolve) => setTimeout(resolve, 600));
    setSaved(draft);
    toast.success('Display name saved');
  });
  return html`
    <${Panel} title="Unsaved changes" description="useLeaveGuard: change the name, then try to leave this page">
      <${Form} onSubmit=${() => save.run()}>
        <${Input}
          label="Display name"
          value=${draft}
          onChange=${setDraft}
          inputRef=${field}
          hint=${dirty ? 'Not saved. Open another page, press Back or sign out: the dashboard asks before anything is lost.' : 'Saved. Leaving this page asks nothing.'}
        />
        <${FormActions}>
          <${Button}
            variant="ghost"
            iconRight="arrow-right"
            onClick=${() => {
              // The user has chosen to leave through the form's own button:
              // stand the guard down so they are not asked a second time.
              guard.release();
              navigate('/overview');
            }}
          >
            Discard and open Overview
          <//>
          <${Button}
            disabled=${!dirty || save.loading}
            onClick=${() => {
              // This button is disabled once there is nothing to discard, and
              // a disabled button cannot hold the focus: hand it to the field.
              field.current?.focus();
              setDraft(saved);
            }}
          >
            Discard
          <//>
          <${Button} type="submit" variant="primary" loading=${save.loading}>Save name<//>
        <//>
      <//>
    <//>
  `;
}

function FormsSection() {
  const [draft, setDraft] = useState({
    name: 'openai-main',
    base_url: 'api.openai.com/v1',
    kind: 'openai',
    strategy: '',
    weight: 1,
    rpm: null,
    models: ['gpt-4o*', 'o3-*', 'text-embedding-3-small'],
    notes: '',
    key: '',
    enabled: true,
    discover: true,
    capture: false,
  });
  const set = (field) => (value) => setDraft((d) => ({ ...d, [field]: value }));
  const [proxy, setProxy] = useState('http://proxy.internal:3128');
  const [strategy, setStrategy] = useState('fill-first');

  // A fake save that answers the way the admin API does. Whatever the status
  // (400 for a body of the wrong shape, 409 for a conflict, 422 for values
  // that do not validate), `issues[].path` is relative to the body that was
  // sent: "name", "base_url", "models[1]", "headers.x-team". They go to
  // useIssues as they come; there is no prefix to strip. The messages are
  // the gateway's own: lower case, no full stop.
  const save = useAsync(async () => {
    await new Promise((resolve) => setTimeout(resolve, 700));
    const name = draft.name.trim();
    if (TRACK.providers.some((provider) => provider.id === name && name !== 'openai-main')) {
      throw new ApiError(409, `a provider named \`${name}\` already exists`, {
        issues: [{ path: 'name', message: 'is the name of another provider' }],
      });
    }
    const found = [];
    if (!name) found.push({ path: 'name', message: 'must not be empty' });
    if (!/^https?:\/\//.test(draft.base_url)) {
      found.push({ path: 'base_url', message: 'must start with http:// or https://' });
      if (draft.models.length > 1) found.push({ path: 'models[1]', message: 'matches no model the upstream lists' });
      found.push({ path: 'headers.x-team', message: 'needs a value' });
    }
    if (found.length > 0) {
      const shown = found.slice(0, 3).map((issue) => `${issue.path}: ${issue.message}`).join('; ');
      throw new ApiError(422, `the configuration is not valid: ${shown}${found.length > 3 ? ` (and ${found.length - 3} more)` : ''}`, { issues: found });
    }
    toast.success('Provider saved');
    return true;
  });
  const issues = useIssues(save.error);
  const modelIssues = issues.under('models');

  return html`
    <${Section} id="forms" title="Forms" description="Every control draws its own label, hint, warning and error. Press Save with the base URL as it is to see the issues of a 422 land on their fields; the ones without a field are listed in the form error. Rename the provider to azure-east for a 409.">
      <${Panel} title="Edit provider" description="Sample form">
        <${Form} onSubmit=${() => save.run()}>
          <${FormRow}>
            <${Input} label="Name" value=${draft.name} onChange=${set('name')} mono error=${issues.at('name')} hint="Used in routes and logs. Letters, digits and dashes." />
            <${Select}
              label="Kind"
              value=${draft.kind}
              onChange=${set('kind')}
              options=${[
                { value: 'openai', label: 'OpenAI' },
                { value: 'anthropic', label: 'Anthropic' },
                { value: 'gemini', label: 'Google Gemini' },
                { value: 'openai-compat', label: 'OpenAI-compatible' },
              ]}
            />
          <//>
          <${Input} label="Base URL" icon="link" value=${draft.base_url} onChange=${set('base_url')} mono error=${issues.at('base_url')} placeholder="https://api.example.com/v1" />
          <${SecretInput} label="API key" value=${draft.key} onChange=${set('key')} placeholder="Leave empty to keep the current key" hint="Stored in switchyard.toml. Shown masked after saving." optional />
          <${FormRow}>
            <${NumberInput}
              label="Weight"
              value=${draft.weight}
              onChange=${set('weight')}
              min=${0}
              max=${1000}
              hint="Share of traffic within a priority tier."
              warning=${draft.weight === 0 ? 'With weight 0 it is only used when the others in its tier are cooling down.' : undefined}
            />
            <${NumberInput} label="Rate limit" value=${draft.rpm} onChange=${set('rpm')} min=${1} step=${10} unit="rpm" placeholder="No limit" optional />
            <${Select} label="Strategy" value=${draft.strategy} onChange=${set('strategy')} placeholder="Use the global strategy" options=${['round-robin', 'fill-first', 'weighted', 'least-latency']} />
          <//>
          <${TagInput}
            label="Model patterns"
            value=${draft.models}
            onChange=${set('models')}
            placeholder="gpt-4o*, o3-*"
            hint="Enter or comma adds a pattern. * matches any run of characters."
            validate=${(tag) => (/\s/.test(tag) ? 'Patterns cannot contain spaces.' : null)}
            error=${modelIssues.length ? modelIssues.map((issue) => issue.message).join(' ') : undefined}
          />
          <${Textarea} label="Notes" value=${draft.notes} onChange=${set('notes')} rows=${3} autoGrow optional placeholder="Who owns this account, where the invoice goes" />
          <div class="stack" style="--gap:var(--space-3)">
            <${Switch} label="Enabled" hint="Disabled providers are skipped by the router." checked=${draft.enabled} onChange=${set('enabled')} />
            <${Switch} label="Discover models" hint="Ask the upstream for its model list instead of listing models by hand." checked=${draft.discover} onChange=${set('discover')} />
            <${Checkbox} label="Capture request bodies for this provider" hint="Bodies are truncated and secrets are redacted." checked=${draft.capture} onChange=${set('capture')} />
          </div>
          <${FormError} error=${save.error} issues=${issues} title="Could not save the provider" />
          <${FormActions}>
            <${Button} onClick=${() => save.reset()}>Discard<//>
            <${Button} type="submit" variant="primary" loading=${save.loading}>Save provider<//>
          <//>
        <//>
      <//>

      <div class="kit-grid">
        <${Example} label="Stored secret: reveal and copy fetch it on demand" wide>
          <div class="grow">
            <${SecretInput}
              label="Client key"
              value="sy-a81F…c2e0"
              readOnly
              copy
              onReveal=${() => new Promise((resolve) => setTimeout(() => resolve('sy-a81F93b07d2e4c5fa6Qk7Zm2Lx9c2e0'), 500))}
            />
          </div>
        <//>
        <${Example} label="Sizes, search, disabled" wide>
          <div class="stack grow" style="--gap:var(--space-2)">
            <${Input} size="sm" icon="search" type="search" placeholder="Filter by model, key or request id" aria-label="Filter" />
            <${Input} placeholder="Medium" aria-label="Medium" suffix="ms" />
            <${Input} size="lg" placeholder="Large" aria-label="Large" />
            <${Input} value="Read only while the config reloads" disabled aria-label="Disabled" />
          </div>
        <//>
        <${SearchExample} />
        <${Example} label="Warning: the value is allowed, with a caveat">
          <div class="stack grow" style="--gap:var(--space-3)">
            <${Input}
              label="Proxy"
              mono
              value=${proxy}
              onChange=${setProxy}
              optional
              placeholder="https://proxy.example.com:3128"
              hint="Upstream requests of this provider go through it."
              warning=${/^http:\/\//i.test(proxy.trim()) ? 'Requests to an http:// proxy are not encrypted on the way to it.' : undefined}
            />
            <${Select}
              label="Strategy"
              value=${strategy}
              onChange=${setStrategy}
              options=${['round-robin', 'fill-first', 'weighted', 'least-latency']}
              hint="How the router picks among credentials of one priority."
              warning=${strategy === 'fill-first' ? 'Fill-first sends everything to the first credential until it is rate limited.' : undefined}
            />
          </div>
        <//>
      </div>

      <${LeaveGuardExample} />
    <//>
  `;
}

function OverlaysSection() {
  const [modal, setModal] = useState(false);
  const [drawer, setDrawer] = useState(false);
  const [sheet, setSheet] = useState(false);
  const [dialog, setDialog] = useState(false);
  const [name, setName] = useState('');
  const [limit, setLimit] = useState(null);
  const [sort, setSort] = useState('recent');
  // A slow fake request, so there is time to see the modal locked while it
  // runs: dismissable=${!create.loading} turns off Escape, the scrim and the
  // close button, and the command palette stays shut.
  const create = useAsync(async () => {
    await new Promise((resolve) => setTimeout(resolve, 2500));
    setModal(false);
    toast.success(`Key ${name.trim()} created`);
    setName('');
    setLimit(null);
  });
  return html`
    <${Section} id="overlays" title="Overlays" description="All of them trap focus, close on Escape and give focus back to the control that opened them. A layer that is saving cannot be dismissed: create a key in the modal and try Escape, the scrim or Ctrl+K while it saves.">
      <div class="kit-grid">
        <${Example} label="Modal (locked while it saves), drawer, sheet">
          <${Button} onClick=${() => setModal(true)}>Open modal<//>
          <${Button} onClick=${() => setDrawer(true)}>Open drawer<//>
          <${Button} onClick=${() => setSheet(true)}>Open bottom sheet<//>
        <//>
        <${Example} label="Confirmation">
          <${Button} variant="danger-quiet" icon="trash" onClick=${() => setDialog(true)}>Delete provider<//>
          <${Button}
            onClick=${async () => {
              const ok = await confirm({
                danger: true,
                title: 'Clear all usage statistics?',
                message: 'Request history and token counts are deleted from disk. This cannot be undone.',
                confirmLabel: 'Clear statistics',
                typeToConfirm: 'clear',
                action: () => new Promise((resolve) => setTimeout(resolve, 600)),
              });
              if (ok) toast.success('Usage statistics cleared');
            }}
          >
            confirm() with typed check
          <//>
        <//>
        <${Example} label="Toasts">
          <${Button} onClick=${() => toast.success('Provider saved')}>Success<//>
          <${Button} onClick=${() => toast.info('Config reloaded from disk')}>Info<//>
          <${Button} onClick=${() => toast.warning('Restart needed', { description: 'The listener address changed.' })}>Warning<//>
          <${Button} onClick=${() => toast.error('Could not delete the key', { description: 'The gateway answered 409: the key is in use by a running request.' })}>Error<//>
          <${Button} onClick=${() => toast.success('Key revoked', { action: { label: 'Undo', onClick: () => toast.info('Key restored') } })}>With action<//>
        <//>
        <${Example} label="Tooltip and menu">
          <${Tooltip} content="Clears the cooldown so the credential is tried again at once.">
            <${Button} icon="refresh">Reset cooldown<//>
          <//>
          <${Tooltip} content="Shown on the right" side="right"><${Badge} outline>Hover or focus<//><//>
          <${Menu}
            label="Provider actions"
            items=${[
              { label: 'Test connection', icon: 'zap', onSelect: () => toast.success('Reachable in 212ms') },
              { label: 'Discover models', icon: 'search', onSelect: () => toast.info('42 models found') },
              { label: 'Edit', icon: 'edit', hint: 'E', onSelect: () => setModal(true) },
              { separator: true },
              { label: 'Disable', icon: 'pause', disabled: true },
              { label: 'Delete provider', icon: 'trash', danger: true, onSelect: () => setDialog(true) },
            ]}
          />
          <${Menu}
            label="Sort order"
            align="start"
            trigger=${(props) => html`<${Button} iconRight="chevron-down" ...${props}>Sort: ${sort}<//>`}
            items=${[
              { heading: 'Sort by' },
              ...['recent', 'name', 'priority'].map((value) => ({ label: value[0].toUpperCase() + value.slice(1), checked: sort === value, onSelect: () => setSort(value) })),
            ]}
          />
        <//>
      </div>

      <${Modal}
        open=${modal}
        onClose=${() => setModal(false)}
        title="Create client key"
        description="The full key is shown once, after it is created."
        dismissable=${!create.loading}
        footer=${html`
          <${Button} disabled=${create.loading} onClick=${() => setModal(false)}>Cancel<//>
          <${Button} type="submit" form="kit-key-form" variant="primary" disabled=${!name.trim()} loading=${create.loading}>Create key<//>
        `}
      >
        <${Form} id="kit-key-form" onSubmit=${() => name.trim() && create.run()}>
          <${Input} label="Name" value=${name} onChange=${setName} autoFocus hint="Who or what will use this key." placeholder="build-bot" />
          <${NumberInput} label="Rate limit" value=${limit} onChange=${setLimit} min=${1} unit="rpm" placeholder="No limit" optional />
        <//>
      <//>

      <${Drawer} open=${drawer} onClose=${() => setDrawer(false)} title="Credential" subtitle="cred_7f3a" footer=${html`<${Button} onClick=${() => setDrawer(false)}>Close<//>`}>
        <div class="stack">
          <${KeyValue}
            items=${[
              { label: 'Provider', value: 'openai-main', mono: true },
              { label: 'State', value: html`<${StatusLamp} tone="caution" label="Cooling down" detail="41s left" />` },
              { label: 'Last error', value: '429 Rate limit reached for requests' },
              { label: 'Recent traffic', value: html`<${HealthStrip} buckets=${HEALTH} label="cred_7f3a" />` },
            ]}
          />
          <${Button} icon="refresh" onClick=${() => toast.success('Cooldown cleared')}>Reset cooldown<//>
        </div>
      <//>

      <${Drawer} open=${sheet} onClose=${() => setSheet(false)} side="bottom" title="Bottom sheet">
        <p class="muted">The phone navigation uses this. It is a Drawer with side="bottom".</p>
      <//>

      <${ConfirmDialog}
        open=${dialog}
        danger
        title="Delete provider openai-main?"
        message="Requests for its 14 models will fail over to other providers, or be rejected when there is none."
        confirmLabel="Delete provider"
        onConfirm=${() => new Promise((resolve) => setTimeout(resolve, 700))}
        onClose=${(reason) => {
          setDialog(false);
          if (reason === 'confirmed') toast.success('Provider deleted');
        }}
      />
    <//>
  `;
}

function ContentSection() {
  return html`
    <${Section} id="content" title="Code and details">
      <div class="grid-2">
        <${CodeBlock}
          title="Client request"
          value=${SAMPLE_WIRE}
          note="A captured body is passed as the string it arrived as. It is re-indented, never re-serialised: 1.0 and the 20-digit seed are shown, and copied, as they were sent."
        />
        <div class="stack">
          <${CodeBlock} language="text" label="Example request with curl" value=${'curl -s http://127.0.0.1:8317/v1/chat/completions \\\n  -H "Authorization: Bearer $SWITCHYARD_KEY" \\\n  -d \'{"model":"gpt-4o","messages":[{"role":"user","content":"Hi"}]}\''} />
          <${Panel} title="Key and value">
            <${KeyValue}
              items=${[
                { label: 'Config file', value: '/etc/switchyard/switchyard.toml', mono: true, copy: true },
                { label: 'Listening on', value: '127.0.0.1:8317', mono: true },
                { label: 'Started', value: formatRelativeTime(Date.now() - 3 * 86_400_000) },
                { label: 'TLS', value: null },
              ]}
            />
          <//>
        </div>
      </div>
    <//>
  `;
}

function StatesSection() {
  const [shown, setShown] = useState(40);
  const [loading, setLoading] = useState(false);
  const paging = useRef(null);
  return html`
    <${Section} id="states" title="States" description="Loading, empty and failed are designed states, not afterthoughts. Empty states say what will appear and how to make it appear.">
      <div class="grid-2">
        <${Panel} title="First load">
          <div class="stack">
            <${Skeleton} width="40%" height="20px" />
            <${Skeleton} lines=${4} />
          </div>
        <//>
        <${Panel} title="Notices">
          <div class="stack" style="--gap:var(--space-2)">
            <${Notice} tone="info" title="Config reloaded">The file changed on disk at ${formatTime(Date.now() - 65_000)} and was applied.<//>
            <${Notice} tone="caution" title="Restart needed" action=${html`<${Button} size="sm">See what changed<//>`}>The listener address changed. It takes effect after a restart.<//>
            <${Notice} tone="stop" title="Config file rejected">Line 42: unknown strategy "fastest". The previous config is still in use.<//>
            <${Notice} tone="clear" title="Connection works">openai-main answered in 212ms.<//>
          </div>
        <//>
        <${Panel} flush>
          <${EmptyState}
            icon="key"
            title="No client keys yet"
            description="Create a key and give it to an application that should use this gateway."
            action=${html`<${Button} variant="primary" icon="plus">Create key<//>`}
          />
        <//>
        <${Panel} flush>
          <${ErrorState} title="Could not load providers" error=${new ApiError(503, 'The gateway is not ready: the config is still loading.')} onRetry=${() => toast.info('Retrying')} />
        <//>
      </div>
      <${Panel} title="Paging">
        <div class="stack">
          <${Pagination} page=${2} pageSize=${25} total=${312} noun="keys" onPage=${() => {}} />
          <hr />
          <div ref=${paging} tabindex="-1">
            <${LoadMore}
              hasMore=${shown < 100}
              loading=${loading}
              shown=${shown}
              noun="requests"
              onLoad=${() => {
                setLoading(true);
                setTimeout(() => {
                  setShown((n) => n + 30);
                  setLoading(false);
                  // With the last batch the button gives way to "All 100
                  // requests shown". If it had the focus, the focus would be
                  // left on <body>: hand it to the line that took its place.
                  setTimeout(() => {
                    if (document.activeElement === document.body) paging.current?.focus();
                  }, 0);
                }, 600);
              }}
            />
          </div>
        </div>
      <//>
    <//>
  `;
}

// A tipFormat: the x value in full, for the tooltip's head and the first
// column of the table view. Here the hour a bar stands for, from and to.
const hourBucket = (at) => `${formatDate(at)} ${formatTime(at).slice(0, 5)} to ${formatTime(at + HOUR).slice(0, 5)}`;

function ChartsSection() {
  const [stale, setStale] = useState(false);
  return html`
    <${Section} id="charts" title="Charts" description="Hover, touch or focus a chart and use the arrow keys to read values. The table button shows the same numbers as a table.">
      <div class="row">
        <${Switch} label="Simulate a refetch" hint="The old plot stays, dimmed. Nothing jumps." checked=${stale} onChange=${setStale} />
      </div>
      <div class="grid-2">
        <${Panel} title="Requests by model" description="LineChart, three series">
          <${LineChart} x=${X48} series=${REQUEST_SERIES} stale=${stale} label="Requests per half hour by model, last 24 hours" valueFormat=${formatNumber} />
        <//>
        <${Panel} title="Tokens by type" description="AreaChart, stacked">
          <${AreaChart} stacked x=${X48} series=${TOKEN_SERIES} stale=${stale} yFormat=${formatTokens} label="Tokens per half hour by type, last 24 hours" />
        <//>
        <${Panel} title="Outcome per hour" description="BarChart, stacked. Red is a status here, so it may be a lamp colour. tipFormat names the whole bucket in the tooltip and the table.">
          <${BarChart}
            x=${X24}
            series=${OUTCOME_SERIES}
            stale=${stale}
            label="Succeeded and failed requests per hour"
            valueFormat=${formatNumber}
            tipFormat=${hourBucket}
            xLabel="Hour"
          />
        <//>
        <${Panel} title="Failovers per day" description="BarChart with utc and integer: the gateway cuts day buckets at UTC midnight, and an axis that counts things has whole-number ticks.">
          <${BarChart} utc integer x=${X14D} series=${FAILOVER_SERIES} stale=${stale} label="Requests that failed over to another provider, per UTC day, last 14 days" valueFormat=${formatNumber} />
        <//>
        <${Panel} title="Top models by tokens" description="BarList, with share">
          <${BarList} items=${TOP_MODELS} format=${formatTokens} share rank limit=${6} />
        <//>
        <${Panel} title="Latency" description="LatencyBars: percentiles on one scale">
          <div class="stack">
            <${LatencyBars}
              items=${[
                { label: 'p50', value: 1240 },
                { label: 'p90', value: 3900 },
                { label: 'p99', value: 11_800, tone: 'caution' },
              ]}
            />
            <hr />
            <div class="kit-inline">
              <span class="faint">Time to first byte</span>
              <${LatencyBars} items=${[{ label: 'p50', value: 412 }, { label: 'p90', value: 980 }, { label: 'p99', value: 2300 }]} />
            </div>
          </div>
        <//>
        <${Panel} title="Small marks" description="Meter, HealthStrip (its noun says what the buckets count), Sparkline (below minPoints it draws an empty box of the same size)">
          <div class="stack">
            <div class="kit-inline"><span class="faint">Success rate</span><${Meter} value=${0.982} tone="clear" label="Success rate" text=${formatPercent(0.982)} /></div>
            <div class="kit-inline"><span class="faint">Rate limit used</span><${Meter} value=${468} max=${600} tone="caution" label="Rate limit used" text="468 / 600 rpm" /></div>
            <div class="kit-inline"><span class="faint">Budget</span><${Meter} value=${0.34} label="Budget used" /></div>
            <div class="kit-inline"><span class="faint">Last 200 minutes</span><${HealthStrip} buckets=${HEALTH} label="openai-main key 1" noun="upstream attempts" /></div>
            <div class="kit-inline"><span class="faint">Requests</span><${Sparkline} data=${REQUEST_SERIES[1].values.slice(-24)} label="Requests, last 12 hours" /></div>
            <div class="kit-inline"><span class="faint">Three readings</span><${Sparkline} data=${[4, 9, 7]} label="Requests, first three minutes" /></div>
            <div class="kit-inline"><span class="faint">Same, minPoints 6</span><${Sparkline} data=${[4, 9, 7]} minPoints=${6} label="Requests, first three minutes" /></div>
          </div>
        <//>
        <${Panel} title="No data" description="What a chart shows before there is anything to plot">
          <${LineChart} x=${[]} series=${[]} height=${160} />
        <//>
      </div>
    <//>
  `;
}

function TrackSection() {
  return html`
    <${Section} id="track" title="Track diagram" description="Routes from client-facing models to providers. The lit rails are the route a request takes right now; hover or select a model to see its failover order. Dashed rails lead to a provider that is failing.">
      <${Panel} title="Routes">
        <${TrackDiagram} models=${TRACK.models} providers=${TRACK.providers} routes=${TRACK.routes} />
      <//>
    <//>
  `;
}

function IconsSection() {
  return html`
    <${Section} id="icons" title="Icons" description="24px grid, 1.75 stroke. Add new ones to components/icons.js; do not use emoji or symbol characters.">
      <ul class="kit-icons">
        ${ICON_NAMES.map(
          (name) => html`
            <li key=${name}>
              <${Icon} name=${name} size=${20} />
              <span class="mono">${name}</span>
            </li>
          `,
        )}
      </ul>
    <//>
  `;
}

function LiveSection() {
  const state = useStore(liveState);
  const [stats, setStats] = useState(null);
  const [events, setEvents] = useState([]);
  const [gap, setGap] = useState(null);
  const counts = useRef({});
  const [, bump] = useState(0);

  useLive('stats', setStats);
  useLive('*', (data, frame) => {
    counts.current[frame.type] = (counts.current[frame.type] ?? 0) + 1;
    if (frame.type === 'request.finished') {
      // A request that never reached a provider (no such model, every
      // credential cooling down) has no provider, and may have no model.
      setEvents((list) => [{ id: data.id, model: data.requested_model, provider: data.provider, status: data.status, duration: data.duration_ms }, ...list].slice(0, 6));
    }
    bump((n) => n + 1);
  });
  // A stream has holes: after a reconnect, and when the gateway says this
  // connection lagged. A page that keeps a list from frames loads it again
  // here (useLiveGap(res.refresh)); this one has nothing to load, so it says so.
  useLiveGap(({ reason, missed }) => setGap({ reason, missed, at: Date.now() }));

  // p50_ms is made of latency_samples requests of the last hour: with none,
  // the 0 it carries means "no data", not "instant".
  const hasLatency = stats != null && stats.latency_samples > 0;
  // error_rate_1m is the failed share of the requests of the last minute (rpm).
  const quiet = stats != null && stats.rpm === 0;
  const errorRate = stats && !quiet ? stats.error_rate_1m : null;
  const errorTone = errorRate == null ? undefined : errorRate >= 0.05 ? 'stop' : errorRate > 0 ? 'caution' : 'clear';
  const errorWords = { stop: '5% or more of the requests of the last minute failed', caution: 'Some requests of the last minute failed', clear: 'No request failed in the last minute' };
  const freshKeys = useMemo(() => new Set(events.map((event) => event.id)), [events]);
  // No frames arrive while the socket is down: the numbers are the last ones heard.
  const stale = stats != null && state.status !== 'open';
  return html`
    <${Section} id="live" title="Live" description="Frames from the admin WebSocket, as the gateway sends them: a stats frame once a second, and one frame for every request that starts or finishes, log line, credential failure and configuration reload. Send a request through the gateway to see the table fill.">
      <div class="grid-2">
        <${Panel} title="Connection">
          <${KeyValue}
            items=${[
              { label: 'State', value: html`<${StatusLamp} tone=${state.status === 'open' ? 'clear' : state.status === 'idle' || state.status === 'unavailable' ? 'off' : 'caution'} label=${state.status} pulse=${state.status === 'open'} />` },
              { label: 'Since', value: formatTime(state.since) },
              { label: 'Gateway', value: state.hello?.version, mono: true },
              { label: 'Gateway started', value: state.hello?.started_at ? formatRelativeTime(state.hello.started_at) : null },
              { label: 'Frames seen', value: Object.entries(counts.current).map(([type, n]) => `${type} ${n}`).join(', ') || null, mono: true },
              { label: 'Last gap', value: gap ? `${gap.reason === 'lagged' ? `Lagged, ${gap.missed == null ? 'some' : formatNumber(gap.missed)} events missed` : 'Reconnected'} at ${formatTime(gap.at)}` : 'None since this page opened' },
            ]}
          />
        <//>
        <${StatGroup} label="Live statistics" class="kit-live-stats" data-stale=${stale ? '' : undefined}>
          <${Stat} label="In flight" value=${stats ? formatNumber(stats.in_flight) : null} hint=${stale ? 'When last heard' : 'Being served now'} loading=${!stats} />
          <${Stat} label="Requests per minute" value=${stats ? formatNumber(stats.rpm) : null} hint="Finished in the last 60 seconds" loading=${!stats} />
          <${Stat}
            label="Error rate"
            value=${errorRate == null ? null : formatPercent(errorRate).replace('%', '')}
            unit=${errorRate == null ? undefined : '%'}
            lamp=${errorTone}
            lampLabel=${errorTone ? errorWords[errorTone] : undefined}
            hint=${quiet ? 'No requests in the last minute' : 'Of the last minute'}
            loading=${!stats}
          />
          <${Stat}
            label="Median latency"
            value=${hasLatency ? formatDuration(stats.p50_ms) : null}
            hint=${hasLatency ? `p95 ${formatDuration(stats.p95_ms)}, last hour` : 'No request finished in the last hour'}
            loading=${!stats}
          />
        <//>
      </div>
      <${Panel} title="Last finished requests" flush>
        <${Table}
          dense
          rows=${events}
          freshKeys=${freshKeys}
          columns=${[
            { key: 'id', header: 'Request', mono: true, primary: true },
            { key: 'model', header: 'Model', mono: true, render: (r) => r.model ?? DASH },
            { key: 'provider', header: 'Provider', mono: true, render: (r) => r.provider ?? DASH },
            { key: 'status', header: 'Status', render: (r) => html`<${Badge} mono tone=${toneForStatus(r.status)}>${r.status}<//>` },
            { key: 'duration', header: 'Duration', align: 'right', num: true, render: (r) => formatDuration(r.duration) },
          ]}
          empty=${{ icon: 'plug', title: 'Waiting for traffic', description: 'Finished requests appear here as they arrive over the live connection.' }}
        />
      <//>
    <//>
  `;
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

export default function KitchenSink() {
  const jump = (id) => document.getElementById(`kit-${id}`)?.scrollIntoView({ behavior: 'smooth', block: 'start' });
  const nav = useMemo(
    () => html`
      <nav class="kit-nav" aria-label="Sections">
        ${SECTIONS.map(([id, label]) => html`<button type="button" key=${id} class="kit-nav-item" onClick=${() => jump(id)}>${label}</button>`)}
      </nav>
    `,
    [],
  );
  return html`
    <${Page} title="Component kit" description="Every component with sample data. Copy patterns from here; props are in ui/UI_GUIDE.md." class="kit">
      ${nav}
      <${TokensSection} />
      <${ButtonsSection} />
      <${StatusSection} />
      <${StatsSection} />
      <${TableSection} />
      <${ChoiceSection} />
      <${FormsSection} />
      <${OverlaysSection} />
      <${ContentSection} />
      <${StatesSection} />
      <${ChartsSection} />
      <${TrackSection} />
      <${IconsSection} />
      <${LiveSection} />
    <//>
  `;
}
