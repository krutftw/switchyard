# Switchyard dashboard: guide for page authors

This is the contract between the dashboard foundation (design system and app
shell) and the pages built on it. Read it once, keep `#/_kit` open while you
work, and copy patterns from `js/pages/kitchen-sink.js`.

Contents: [Ground rules](#ground-rules) · [Running it](#running-it) ·
[File layout](#file-layout) · [Adding a page](#adding-a-page) ·
[Data](#data) · [Live updates](#live-updates) · [Forms](#forms-and-api-issues) ·
[Components](#components) · [Charts](#charts) · [Tokens](#tokens) ·
[Motion](#motion) · [Accessibility](#accessibility) · [Copy](#copy) ·
[Do and do not](#do-and-do-not) · [htm and Preact notes](#htm-and-preact-notes)

## Ground rules

- **No build step.** Plain ES modules, hand-written CSS, Preact and htm from
  `vendor/preact-htm.js`. Nothing else at runtime.
- **No network requests except to the gateway.** No CDN, no web fonts, no
  remote images. The binary must work air-gapped. Fonts are in `fonts/`.
- **All URLs relative.** The app is served at `/admin/`; routes live in the
  hash (`#/requests?status=error`). The API base is one constant, `API_BASE`
  in `js/lib/api.js`.
- **Works from 360px to wide desktops, in both themes, from the keyboard.**
  Check every page at phone width and in light and dark before you call it done.
- **The look is a signal box:** graphite panels, hairline rules, mono for
  identifiers and numbers, and four lamp colours that always mean something.
  Do not add colours, gradients, glass or emoji.

## Running it

```
node tools/ui-dev.mjs                              # mock admin API, secret "dev"
node tools/ui-dev.mjs --api http://127.0.0.1:8317  # proxy to a running gateway
node tools/ui-dev.mjs --port 5173 --host 0.0.0.0   # other address
```

Open `http://127.0.0.1:5173/admin/`. Files are served with caching off, so a
reload always shows what is on disk.

The mock answers `/login`, `/status`, `/ws-ticket`, `/ws` (hello, then stats,
request.started, request.finished and log frames every second) and
`/playground`. Every other route is a 404 in the admin error envelope, so
build pages against a real gateway with `--api`. Mock sign-in secrets
`remote`, `disabled` and `locked` produce the 403, 404 and 429 states.

The shapes of `/status` and of the `hello` and `stats` frames are not fixed by
`docs/DESIGN.md`; the mock's are placeholders. Request records follow
`crates/telemetry/src/record.rs`.

Before handing a page over, run the self-check:

```
node ui/tests/check.mjs
```

It parses and imports every module, loads every route, asserts the logic in
`lib/` (formatting, the SSE parser, the API client against a stubbed `fetch`,
the refetch policy of `useResource`), renders components into a small stub
document to check behaviour (`tests/dom.mjs`: keyboard, focus, submit, where
overlays land), and starts the dev server to send it malformed requests
(`tests/dev-server.mjs`). It takes about five seconds.

It needs no browser, so it cannot tell you how the page looks: the stub
document has no CSS and no layout. Look at the page.

When you fix a bug in a component, add the case to `tests/dom.mjs`; for a
pure helper, to `tests/check.mjs`.

## File layout

```
ui/
  index.html            shell document, theme bootstrap, font preloads
  favicon.svg
  package.json          marks the .js files as ES modules for Node and editors; no dependencies
  UI_GUIDE.md           this file
  tests/
    check.mjs           self-check, runs everything: node ui/tests/check.mjs
    dom.mjs             component behaviour, rendered into a stub document
    dom-stub.mjs        that document: nodes, events, focus, selectors; no CSS
    dev-server.mjs      tools/ui-dev.mjs over real sockets
  css/
    tokens.css          every colour, size, radius, shadow, duration (both themes)
    base.css            reset, type defaults, focus ring, utility classes
    components.css      one block per component
    layout.css          shell, sidebar, top bar, phone nav, palette, boot splash
    pages/<name>.css    styles of one page, loaded by that page
  fonts/                Archivo (UI), JetBrains Mono (data), licences
  vendor/               preact-htm.js, LICENSES.txt
  js/
    app.js              boot and auth gate
    routes.js           the route table (edit this to add a page)
    shell/              shell.js (frame, navigation), palette.js (Ctrl/Cmd+K)
    lib/
      api.js            fetch wrapper, ApiError, session, streamSSE
      live.js           live event client, useLive, liveState
      store.js          createStore, useStore
      router.js         useRoute, navigate, href, useQueryParam, setQuery
      hooks.js          useResource, useAsync, useInterval, useNow, useDebounced,
                        useLocalStorage, useMediaQuery, useIsPhone, useHotkey,
                        useSize, usePresence, useModalLayer, useOutsidePointer
      format.js         numbers, tokens, durations, times, bytes, money, percent
      commands.js       useCommands: add entries to the command palette
      theme.js          theme store, setThemePref
      dom.js            cx, loadStyles, copyText, placeFloating, focus helpers
    components/         see "Components"; index.js re-exports all of them
    pages/              one module per route
tools/ui-dev.mjs        dev server
```

## Adding a page

1. Create `js/pages/<name>.js` with a default-exported component.
2. Add an entry to `ROUTES` in `js/routes.js` (path, title, icon, group, load).

That is all: the sidebar, the phone navigation, the command palette and the
document title follow the route table. The ten product pages already have
entries and stub modules; replace the stub.

```js
// js/pages/providers.js
import { html } from '../../vendor/preact-htm.js';
import { Button, ErrorState, Page, Panel, StatusLamp, Table } from '../components/index.js';
import { formatCompact } from '../lib/format.js';
import { useResource } from '../lib/hooks.js';
import { useQueryParam } from '../lib/router.js';

export default function Providers({ route }) {
  const providers = useResource('/providers', { pollMs: 15_000 });
  const [open, setOpen] = useQueryParam('open', '');

  return html`
    <${Page} title="Providers" description="Upstreams the gateway can route to."
             actions=${html`<${Button} variant="primary" icon="plus">Add provider<//>`}>
      <${Panel} flush>
        <${Table}
          rowKey="name"
          rows=${providers.data?.providers}
          loading=${providers.loading}
          error=${providers.error}
          onRetry=${providers.refresh}
          onRowClick=${(p) => setOpen(p.name)}
          selectedKey=${open || null}
          empty=${{ icon: 'providers', title: 'No providers yet', description: 'Add one to start routing requests.' }}
          columns=${[
            { key: 'name', header: 'Provider', primary: true, sortable: true, mono: true },
            { key: 'state', header: 'State', render: (p) => html`<${StatusLamp} tone=${p.tone} label=${p.state} />` },
            { key: 'requests', header: 'Requests', align: 'right', num: true, sortable: true, render: (p) => formatCompact(p.requests) },
          ]}
        />
      <//>
    <//>
  `;
}
```

What a page gets and must do:

- **Props:** `route`, the parsed route `{ path, segments, query }`.
  `route.segments[1]` is the first sub-segment (`#/requests/req_123`).
- **Start with `<Page>`.** It renders the `h1` and sets the tab title.
- **URL state.** Filters, the selected tab, the time range and the open record
  belong in the query (`useQueryParam`), so a link reproduces the view and
  Back closes a drawer. Use `navigate(path, { query })` to change page.
- **Page CSS.** If the shared classes are not enough, add
  `css/pages/<name>.css` and load it at the top of the module, before the
  component is defined: `await loadStyles('pages/<name>.css');`. Prefix its
  classes with the page name. Use tokens, never raw colours or sizes.
- **Palette entries.** `useCommands(() => [...], deps)` adds things worth
  jumping to (a provider, a key) while the page is open.
- **Errors.** A crash in a page is caught by the shell and shown with a retry;
  do not add your own boundary.

## Data

`js/lib/api.js`: every call carries the admin secret and resolves with parsed
JSON, or rejects with an `ApiError`.

```js
import { api, ApiError } from '../lib/api.js';

await api.get('/providers');
await api.get('/requests', { query: { limit: 50, status: 'error' }, signal });
await api.post('/providers', body);
await api.put(`/providers/${name}`, body);
await api.patch('/settings', patch);
await api.del(`/keys/${id}`);
await api.streamSSE('/playground', body, (event) => { /* { event, data, id, json } */ }, signal);
```

`ApiError`: `status` (0 when there is no usable response), `message` (the
gateway's own, when it sent one), `issues` (`[{ path, message }]`, possibly
empty), `retryAfter` (seconds or null), and `code`:

| `code` | Means |
|---|---|
| `http` | the gateway answered with an error status (or with a body that is not JSON) |
| `network` | no connection, or it dropped while the response was arriving |
| `timeout` | no complete answer within the limit (30 s; `{ timeout: ms }` to change) |
| `aborted` | cancelled through the `signal` you passed; `error.aborted` is true, do not show it |
| `invalid` | not sent: the admin secret contains a control character (sign-in only) |

Whatever fails, at whatever stage (sending, waiting, reading the body), the
rejection is an `ApiError`; you never need to handle a raw `TypeError` or
`DOMException`. A 401 from any call ends the session and brings back the
sign-in page; pages do not handle it.

A success without a body (a 204, an empty 200) resolves `null`.

`streamSSE` resolves when the stream ends. When the gateway answers with
plain JSON instead (a non-streaming playground request), the callback is
called once with `{ event: 'response', json }`. An exception thrown by your
callback closes the stream and is rethrown unchanged.

### Reading: `useResource`

```js
const keys = useResource('/keys');
const usage = useResource(['/usage/summary', { range }], { pollMs: 30_000 });
const detail = useResource(id ? `/requests/${id}` : null);   // null = idle
```

Returns `{ data, error, loading, refreshing, updatedAt, refresh(), mutate() }`.

- `loading` is the first load: show skeletons (Table does it for you).
- `refreshing` is a refetch with data on screen: keep the data, dim charts
  with `stale=${res.refreshing}`. Never swap content for a spinner.
- `error` with `data` still set means the refetch failed: keep showing the
  data and say so (a Notice, or a toast). `error` without `data`: `ErrorState`.
- After a write, either `refresh()` or `mutate(updater)` to patch locally.
- Polling pauses while the tab is hidden and refetches when it returns.
- One request is in flight at a time, and only a changed key (or unmount)
  aborts it. A poll tick that finds a request still on its way is skipped,
  so a gateway slower than `pollMs` still gets to answer. `refresh()` while
  a request is in flight lets it finish and sends exactly one more after it
  (the one in flight may predate your write); the promise resolves when
  that follow-up has landed. So `refresh()` is safe to call from every live
  frame: a burst collapses into one request.
- `refresh` takes no arguments: pass it straight to `onClick` or `onRetry`.
- `mutate()` after a write drops a refetch that was already in flight and
  starts it again, so an answer from before the write cannot undo the patch.
- The function form (`useResource((signal) => ...)`) must settle: go through
  `api`, which times out, and pass it the `signal`.

### Writing: `useAsync`

```js
const remove = useAsync((id) => api.del(`/keys/${id}`));
if (await remove.run(key.id)) { toast.success('Key deleted'); keys.refresh(); }
// remove.loading, remove.error, remove.data, remove.reset()
```

`run` never rejects. It resolves with the action's result, or `true` when
the action has none (a 204, an empty body, a function that returns nothing),
and with `undefined` when it failed; the error is then in `.error`. Testing
the result for truth is therefore right for everything the admin API returns.
Only an action that can itself resolve `0`, `''` or `false` needs
`(await x.run()) !== undefined`.

Destructive writes go through `confirm({ ..., action })` or `ConfirmDialog`,
which run the request, show progress and keep the error in the dialog.

## Live updates

`js/lib/live.js` holds one WebSocket for the whole app. It buys a ticket,
connects, reconnects with backoff and re-subscribes; the top bar shows its
state. Pages only listen:

```js
import { useLive, liveState } from '../lib/live.js';

useLive('request.finished', (record) => setRows((rows) => [record, ...rows].slice(0, 200)));
useLive('stats', setStats);
useLive('request.*', onAnyRequestEvent);            // prefix pattern
useLive('log', onLine, { enabled: !paused });        // pause without unmounting
const { status } = useStore(liveState);              // "open", "reconnecting", ...
```

Topics: `hello`, `request.started`, `request.finished`, `log`, `credential`,
`config.reloaded`, `stats`. The client asks the gateway only for topics that
have listeners.

Patterns:

- **Live table:** load a page with `useResource`, prepend frames with
  `useLive`, cap the list, pass `freshKeys` to `Table` so new rows flash once.
  Offer a pause switch; while paused, count what arrived and show
  "12 new requests" as the way back.
- **Live numbers:** render the last `stats` frame; fall back to polling
  `/status` when `liveState.status` is not `open`.
- **After a reconnect** the frames in between are gone. Refetch on the
  transition to `open` (watch `liveState.since`).
- **config.reloaded / credential:** refetch the resource the page shows.

## Forms and API issues

Every control takes `label`, `hint`, `error`, `optional` and reports changes
as `onChange(value)`. The admin API reports validation problems as
`issues: [{ path, message }]`; `useIssues` hands each one to its field and
`FormError` lists the rest.

```js
const [draft, setDraft] = useState(initial);
const set = (field) => (value) => setDraft((d) => ({ ...d, [field]: value }));
const save = useAsync(() => api.put(`/providers/${name}`, draft));
const issues = useIssues(save.error);

html`<${Form} onSubmit=${async () => { if (await save.run()) { toast.success('Provider saved'); onDone(); } }}>
  <${Input} label="Base URL" mono value=${draft.base_url} onChange=${set('base_url')} error=${issues.at('base_url')} />
  <${SecretInput} label="API key" value=${draft.api_key} onChange=${set('api_key')}
                  placeholder="Leave empty to keep the current key" optional />
  <${TagInput} label="Models" value=${draft.models} onChange=${set('models')}
               error=${issues.under('models').map((i) => i.message).join(' ') || undefined} />
  <${FormError} error=${save.error} issues=${issues} title="Could not save the provider" />
  <${FormActions}>
    <${Button} onClick=${onCancel}>Cancel<//>
    <${Button} type="submit" variant="primary" loading=${save.loading}>Save provider<//>
  <//>
<//>`
```

- `save.run()` is truthy when the save went through, whether the gateway
  answered with a body or with a bare 204 (see `useAsync`).
- `issues.at('a.b[0].c')` matches `a.b.0.c` too. Call `at`/`under` in the
  same render function as `FormError`.
- `Form` does not call `onSubmit` while a control shows text it could not
  take as its value: a `NumberInput` holding `5000` where `max` is 1000 keeps
  reporting the last good number, marks itself invalid with the reason, and
  gets the focus when the user presses Enter. What is saved is always what
  is on screen. Nothing to do in the page; do not work around it with your
  own Enter handling.
- Secrets: the API masks them in responses, and a secret field sent back
  empty or still masked means "keep". Start secret inputs empty with a
  placeholder that says so.
- Validate on submit, not on every keystroke. Client-side checks are for
  things the user can fix at once (required, number range); the gateway is
  the authority.
- Keep the form on screen when saving fails. Never clear what was typed.
- Guard unsaved changes when leaving a drawer or page (`confirm`).

## Components

Import from `js/components/index.js`. Each module documents its props above
the component; this is the summary. Shared conventions:

- `class` adds a class to the root. Unknown props go to the main element.
- Tones are lamp names: `clear`, `caution`, `stop`, `info`, `off` (and
  `neutral` where a component has a plain state).
- Sizes are `sm`, `md` (default), `lg`.

### Actions

| Component | Props | Example |
|---|---|---|
| `Button` | `variant` secondary · primary · ghost · danger · danger-quiet; `size`; `icon`, `iconRight`; `loading`; `disabled`; `href`; `block`; `type` | `<${Button} variant="primary" icon="plus" onClick=${add}>Add provider<//>` |
| `IconButton` | `icon`, `label` (required: name and tooltip), `variant` (ghost), `size`, `tooltip`, `tooltipSide` | `<${IconButton} icon="refresh" label="Refresh" onClick=${refresh} />` |
| `CopyButton` | `value` (string or function, may be async), `label`, `size`, `variant`, children for a labelled button | `<${CopyButton} value=${id} label="Copy request id" />` |
| `Spinner` | `size`, `label` | `<${Spinner} label="Loading" />` |
| `Menu` | `items` `[{ label, icon, hint, onSelect, href, danger, disabled, checked } \| { separator } \| { heading }]`, `label`, `icon`, `trigger(props)`, `side`, `align` | `<${Menu} label="Key actions" items=${items} />` |
| `Tooltip` | `content`, `side`, `align`, `delay`, `describe` | `<${Tooltip} content="Clears the cooldown">…<//>` |

One primary button per view. `danger` is the confirming button inside a
dialog; `danger-quiet` is the button that opens it. A tooltip never carries
the only copy of something: touch screens do not show it.

### Status

| Component | Props | Example |
|---|---|---|
| `StatusLamp` | `tone`, `label`, `detail`, `pulse`, `size`, `title` | `<${StatusLamp} tone="caution" label="Cooling down" detail="41s left" />` |
| `Badge` | `tone`, `mono`, `outline`, `lamp` | `<${Badge} mono tone=${toneForStatus(r.status)}>${r.status}<//>` |
| `Kbd` | children | `<${Kbd}>Esc<//>` |
| `toneForStatus(status)` | HTTP status to tone | 2xx clear, 3xx and 429 caution, other 4xx/5xx stop |

A lamp always has words next to it (or a `title` when the column header says
what it is). `pulse` is for things that are live right now, nothing else.

### Surfaces and content

| Component | Props | Example |
|---|---|---|
| `Page` | `title`, `description`, `actions` | see "Adding a page" |
| `Panel` (`Card`) | `title`, `description`, `actions`, `footer`, `flush` | `<${Panel} title="Credentials" flush>…<//>` |
| `Notice` | `tone`, `title`, `icon`, `action`, children | `<${Notice} tone="caution" title="Restart needed">…<//>` |
| `StatGroup` | `label`, children (`Stat`s) | `<${StatGroup} label="Traffic">…<//>` |
| `Stat` | `label`, `value` (formatted), `unit`, `delta` (ratio), `goodWhen` up · down · none, `deltaLabel`, `hint`, `trend`, `lamp`, `loading` | `<${Stat} label="Error rate" value="1.8" unit="%" delta=${0.31} goodWhen="down" deltaLabel="vs previous hour" />` |
| `KeyValue` | `items` `[{ label, value, mono, copy, hidden }]` | `<${KeyValue} items=${[{ label: 'Request id', value: id, mono: true, copy: true }]} />` |
| `CodeBlock` | `value` (string or object), `language` auto · json · text, `title`, `wrap`, `copy`, `maxHeight`, `note`, `actions` | `<${CodeBlock} title="Upstream request" value=${body} />` |
| `Timeline` | `items` `[{ tone, title, badges, time, description }]` | attempts of a request, see the kit |
| `Skeleton` | `width`, `height`, `lines` | `<${Skeleton} lines=${4} />` |
| `EmptyState` | `icon`, `title`, `description`, `action`, `compact` | `<${EmptyState} icon="key" title="No client keys yet" description="…" action=${button} />` |
| `ErrorState` | `error` (ApiError), `title`, `description`, `onRetry`, `retrying`, `compact` | `<${ErrorState} title="Could not load providers" error=${res.error} onRetry=${res.refresh} />` |
| `Pagination` | `page`, `pageSize`, `total`, `onPage`, `noun` | known totals |
| `LoadMore` | `hasMore`, `loading`, `onLoad`, `shown`, `noun` | cursor paging (`before=`) |

Do not nest panels. Inside a panel separate with `<hr>` or space. Stats go
in a `StatGroup`, not in separate cards. An empty state says what will appear
and how to make it appear.

`CodeBlock` and captured bodies: pass the body as the **string** that was
captured. JSON text is re-indented token by token (`formatJson`), never
parsed and re-serialised, so a 64-bit `seed`, `1.0` versus `1`, key order,
duplicate keys and escapes are shown, and copied, exactly as they were on
the wire. An object you pass has already been through `JSON.parse` and can
only be shown as JavaScript sees it. Text that is not valid JSON (a truncated
capture, SSE lines) is shown unchanged.

### Table

`Table` props: `columns`, `rows`, `rowKey`, `sort` + `onSort` (controlled) or
`defaultSort` (local), `onRowClick`, `selectedKey`, `freshKeys`, `loading`,
`error`, `onRetry`, `empty`, `sticky`, `maxHeight`, `dense`, `collapse`,
`caption`.

Column: `key`, `header`, `render(row)`, `align`, `num`, `mono`, `width`,
`sortable`, `sortValue(row)`, `primary`, `hideOnPhone`.

- Numbers: `align: 'right', num: true`. Identifiers: `mono: true`.
- Mark one column `primary`: on phones the row becomes a card with that cell
  as its title, the other cells as label/value lines, and the sortable
  headers as a "Sort by" menu. Use `hideOnPhone` for columns that only repeat
  what the card already says.
- Put the table in `<Panel flush>`. Loading, empty and error states are built
  in; pass `loading`, `error`, `empty`.
- `dense` (32px rows) is for streams: requests, logs.

### Choice

| Component | Props | Example |
|---|---|---|
| `Tabs` | `tabs` `[{ id, label, count, icon, disabled }]`, `value`, `onChange`, `label` | views of one thing; render the selected view yourself |
| `Segmented` | `options` (strings or `{ value, label, icon, title, disabled }`), `value`, `onChange`, `label`, `size` | `<${Segmented} label="Range" value=${range} onChange=${setRange} options=${['1h','24h','7d','30d']} />` |

Both are one tab stop: Tab enters at the selected item, arrows and Home/End
move and select. Disabled items are skipped, so a disabled "Bodies" tab (no
bodies captured) never traps the keyboard. If `value` names no enabled item
(a stale `?tab=` in a link), the first enabled one is the tab stop.

### Form controls

All take `label`, `hint`, `error`, `optional`, `disabled`, and call
`onChange(value)`.

| Component | Extra props |
|---|---|
| `Input` | `value`, `type`, `icon`, `suffix`, `actions`, `mono`, `size`, `autoFocus`, `onEnter`, `inputRef`, `placeholder` |
| `Textarea` | `value`, `rows`, `maxRows`, `mono`, `autoGrow` |
| `Select` | `value`, `options` (strings or `{ value, label, disabled }`), `placeholder`, `size` |
| `NumberInput` | `value` (number or null), `min`, `max`, `step` (a whole step means whole numbers only), `unit`, `placeholder` (say what empty means). `onChange` fires only for values inside the range; other text is marked invalid, blocks `Form` submit, and is clamped on blur |
| `Switch` | `checked` |
| `Checkbox` | `checked`, `indeterminate` |
| `TagInput` | `value` (string[]), `validate(tag)`, `placeholder` |
| `SecretInput` | `value`, `placeholder`, `copy`, `readOnly`, `onReveal` (async, for stored secrets), `autocomplete` |
| `Field` | `label`, `hint`, `error`, `htmlFor`: labels a custom control |
| `Form`, `FormRow`, `FormActions`, `FormError`, `useIssues` | see "Forms and API issues" |

A Switch is a state that applies ("Enabled"); a Checkbox is a choice inside a
form that is saved later. Every control needs a visible label; a placeholder
is an example, not a label.

### Overlays

| Component | Props |
|---|---|
| `Modal` | `open`, `onClose`, `title`, `description`, `size` sm · md · lg, `footer`, `dismissable` |
| `Drawer` | `open`, `onClose`, `title`, `subtitle`, `actions`, `footer`, `width`, `side` right · bottom, `dismissable` |
| `ConfirmDialog` | `open`, `title`, `message`, `confirmLabel`, `cancelLabel`, `danger`, `onConfirm` (may be async), `onClose`, `typeToConfirm` |
| `confirm(options)` | same options plus `action`; resolves `true` or `false` |
| `toast` | `toast.success · info · warning · error(title, { description, action, duration, id })`, `toast.dismiss(id)` |

```js
const ok = await confirm({
  danger: true,
  title: `Delete key ${key.name}?`,
  message: 'Applications using it will get 401 from now on.',
  confirmLabel: 'Delete key',
  action: () => api.del(`/keys/${key.id}`),
});
if (ok) { toast.success('Key deleted'); keys.refresh(); }
```

- Drawer for the detail of a row; keep its id in the URL.
- Modal only when the task must interrupt: a short create form, a
  confirmation. Try inline first.
- `dismissable=${false}` (Modal and Drawer) turns off every way out the
  layer provides: Escape, the scrim and the close button. Set it while a
  save is in flight (`dismissable=${!save.loading}`) and disable your own
  Cancel button the same way.
- Toast for the result of what the user just did. Anything they must act on
  stays on the page: `Notice`, `FormError`, `ErrorState`.
- Toasts are drawn above modals, drawers and their scrims, so the result of
  saving inside a drawer is seen without closing it.
- `Toaster` and `ConfirmHost` are mounted by the shell; do not mount them.

### Track diagram

`TrackDiagram`: `models` `[{ id, label, note }]`, `providers`
`[{ id, label, note, state }]`, `routes` `[{ model, provider, order, state, lit }]`,
`selected`, `onSelect`. The lit rails are the route a request takes now (the
first route in failover order whose state is `clear`). Dashed rails lead to a
failing provider. Below 520px it becomes a list.

## Charts

`Sparkline`, `LineChart`, `AreaChart`, `BarChart`, `BarList`, `LatencyBars`,
`Meter`, `HealthStrip`. Data shape for the axis charts:

```js
html`<${LineChart}
  x=${buckets.map((b) => b.at)}                    // epoch ms, or category labels
  series=${[{ key: 'gpt-4o', label: 'gpt-4o', values: [...] }, ...]}
  yFormat=${formatCompact} valueFormat=${formatNumber}
  stale=${res.refreshing}
  label="Requests per minute by model, last hour" />`
```

Options: `height`, `area`, `stacked`, `yFormat`, `valueFormat`, `xFormat`,
`yMin`, `yMax`, `xLabel`, `legend`, `table`, `emptyText`. A `null` in
`values` breaks the line. `AreaChart` is `LineChart` with `area`; `BarChart`
takes the same props (no `area`, `stacked` or `yMin`: columns always stack
from zero).

| Component | Props | Example |
|---|---|---|
| `Sparkline` | `data` (numbers, oldest first), `width`, `height`, `area`, `min`, `max`, `label` (without it the sparkline is decorative) | `<${Sparkline} data=${rpm} label="Requests per minute, last hour" />` |
| `BarList` | `items` `[{ key, label, value, hint, href, color }]`, `format`, `share`, `rank`, `limit` (folds the rest into "N others"), `sort`, `mono`, `emptyText` | `<${BarList} items=${byModel} format=${formatTokens} share limit=${8} />` |
| `LatencyBars` | `items` `[{ label, value (ms), tone }]`, `max`, `format` | `<${LatencyBars} items=${[{ label: 'p50', value: 420 }, { label: 'p99', value: 3100, tone: 'caution' }]} />` |
| `Meter` | `value`, `max` (default 1), `tone` clear · caution · stop, `text` (default the percentage; `false` hides it), `label` | `<${Meter} value=${468} max=${600} text="468 / 600 rpm" label="Rate limit use" />` |
| `HealthStrip` | `buckets` `[{ ok, failed, label }]` oldest first, `slots` (default 20), `label` | `<${HealthStrip} buckets=${cred.recent} label="cred_7f3a" />` |
| `seriesColor(series, i)`, `foldSeries(series, keep)`, `niceScale(min, max)` | helpers for custom SVG in a page | `foldSeries(series, 5)` keeps the five largest and sums the rest into "Other" |

Pick the form by the question:

| Question | Form |
|---|---|
| One number | `Stat` (with a `Sparkline` if the trend matters) |
| How it changed over time | `LineChart`; parts of a whole over time: `AreaChart stacked` or `BarChart` |
| Which are the biggest | `BarList` (never a donut) |
| How slow, and how bad is the tail | `LatencyBars` |
| How close to a limit | `Meter` |
| Was it healthy recently | `HealthStrip` |

Rules the components hold to, and that you must not work around:

- One y axis per chart. Two measures need two charts.
- Series colours are `--series-1..6`, assigned in the order you pass the
  series. Keep that order stable across refetches and filters so a model
  keeps its colour. More than six: `foldSeries(series)` first.
- Lamp colours mean status. Pass `color: 'var(--lamp-stop)'` only for a
  series that is a status (failed requests).
- Text is never series-coloured; identity comes from the key next to it.
- Every axis chart has a table view and keyboard readout built in. Give it a
  `label` that says what it shows.
- Put the range control in one row above everything it scopes, not inside a
  chart panel.

## Tokens

All in `css/tokens.css`. Use them for every colour, size and duration; a raw
hex or pixel value in page CSS is a bug.

**Surfaces**

| Token | Use |
|---|---|
| `--bg-ground` | the page |
| `--bg-sunken` | sidebar, code, wells |
| `--bg-surface` | panels |
| `--bg-raised` | menus, dialogs, drawers, toasts |
| `--bg-field` | inputs |
| `--bg-hover`, `--bg-active` | hovered, pressed or selected rows and controls |
| `--rule` | hairlines between things |
| `--rule-strong` | outlines of raised layers and secondary buttons |
| `--control-border` | input outlines (3:1 on every surface they sit on) |

**Ink**

| Token | Use |
|---|---|
| `--text` | primary text and values |
| `--text-2` | descriptions, secondary labels |
| `--text-3` | captions, timestamps, placeholders, table headers |
| `--accent-ink` | links, the selected thing |
| `--clear-ink`, `--caution-ink`, `--stop-ink` | status text (errors, deltas) |

All of these pass WCAG AA (4.5:1) on every surface in both themes.

**Lamps** (fills, never text): `--lamp-clear`, `--lamp-caution`, `--lamp-stop`,
`--lamp-info`, `--lamp-off`. Washes for tinted backgrounds: `--wash-clear`,
`--wash-caution`, `--wash-stop`, `--wash-accent`, `--wash-neutral`.

| Colour | Means | Never |
|---|---|---|
| green | clear: healthy, succeeded, serving | "new", "recommended" |
| amber | caution: cooling down, degraded, needs attention | decoration |
| red | stop: failed, blocked, destructive | emphasis |
| blue-white (accent) | selected, in progress, informational, focus, the primary action | a second accent |

**Type.** `--font-sans` (Archivo) for interface text, `--font-mono`
(JetBrains Mono) for identifiers, numbers in columns, code. Sizes:
`--text-2xs` 11 (plate labels, axis ticks) · `--text-xs` 12 (captions,
badges) · `--text-sm` 13 (dense data) · `--text-md` 14 (body, controls) ·
`--text-lg` 16 (panel titles) · `--text-xl` 19 (dialog titles) ·
`--text-2xl` 23 (page titles) · `--text-3xl` 30 (stat readouts).
Classes: `.mono`, `.num` (tabular), `.muted`, `.faint`, `.plate-label`,
`.truncate`, `.sr-only`.

**Space.** 4px grid: `--space-1` 4 · `-2` 8 · `-3` 12 · `-4` 16 · `-5` 20 ·
`-6` 24 · `-8` 32 · `-10` 40 (and `-0h` 2, `-1h` 6 for dense rows). Tight
inside a group, generous between groups. Layout helpers: `.stack`, `.row`
(`--gap` sets the gap), `.grow`, `.grid` (`--min`), `.grid-2`, `.grid-3`,
`.hide-phone`, `.only-phone`.

**Shape.** `--radius-xs` 2 (tags) · `--radius-sm` 4 (controls) ·
`--radius-md` 6 (panels, menus) · `--radius-lg` 10 (dialogs).

**Elevation.** Panels are flat: a hairline, no shadow. `--shadow-2` for menus
and toasts, `--shadow-3` for dialogs and drawers. Depth comes from the
lighter surface first, the shadow second.

**Breakpoint.** Phone layout below 721px (`PHONE_QUERY`, `useIsPhone()`).

## Motion

Tokens: `--dur-press` 110ms, `--dur-fast` 150ms, `--dur-base` 200ms,
`--dur-slow` 300ms, `--dur-exit` 140ms; `--ease-out`, `--ease-in-out`,
`--ease-drawer`.

- Motion shows a state change. If it does not, leave it out.
- Things opened from the keyboard do not animate (palette, keyboard-opened
  menus).
- Enter with `--ease-out`; exit faster than you entered. Never `ease-in`.
- Animate `transform` and `opacity` only. Never `transition: all`.
- Multiply distances by `var(--motion)` (0 under reduced motion) so only the
  fade remains: `transform: translateY(calc(8px * var(--motion)))`.
- Hover effects go inside `@media (hover: hover)`.
- For enter/exit, use `usePresence(open)` and a `data-state` attribute with
  CSS transitions, not keyframes: transitions can be interrupted.

## Accessibility

- Everything works from the keyboard. Custom widgets follow the existing
  ones: arrows inside a group, Tab between groups, Escape closes.
- Do not remove the focus ring. Do not add `outline: none` without a
  replacement.
- Colour is never the only signal: lamps have labels, charts have legends
  and tables, invalid fields have text.
- Icon-only controls use `IconButton` (it requires `label`).
- Live regions: `Notice tone="stop"` and error toasts are announced; do not
  add `role="alert"` to things that are on screen from the start.
- Touch targets are 44px on coarse pointers; the control tokens already grow.
- Test at 360px wide, at 200% zoom, in both themes, with reduced motion.

## Copy

Plain, specific, no marketing. Write what an operator at 3 a.m. needs.

- **Sentence case** everywhere: "Add provider", "API keys", "Rate limit".
- **Buttons are verbs that name the action:** "Delete provider", "Create
  key", "Test connection". Never "OK", "Yes", "Submit".
- **Errors say what happened and what to do:** "Could not reach
  api.openai.com: connection refused. Check the base URL and that the gateway
  can reach the internet." Not "Something went wrong".
- **Confirmations name the thing and the consequence:** title "Delete key
  build-bot?", message "Applications using it will get 401 from now on."
- **Empty states teach:** what will be here, how to get it here.
- **Numbers carry units:** "1.24s", "312ms", "1.2M tokens", "468 / 600 rpm".
  Use `lib/format.js`; a missing value is a dash, never "N/A", "null" or 0.
- **Use the product's words:** provider, credential, client key, model,
  alias, route, attempt, cooldown. One word per thing, the same on every page.
- No exclamation marks, no "please", no "successfully" ("Provider saved"),
  no "simply" or "just", no jokes in errors.
- Identifiers the user might copy (`gpt-4o`, `req_8fK2`, `admin.secret`) are
  set in mono and copied exactly.

## Do and do not

| Do | Do not |
|---|---|
| Keep the previous data on screen while refetching | Replace content with a spinner |
| Put filters, tabs and the open record in the URL | Keep view state only in memory |
| Use `StatusLamp` and `Badge` tones for state | Colour text or borders by hand |
| One primary button per view | Two filled accent buttons side by side |
| `Table` with `primary` and `hideOnPhone` | A table that scrolls sideways on a phone |
| `BarList` for a ranking | A donut or pie |
| `Drawer` for a row's detail | A modal with a table in it |
| `confirm()` before anything destructive | A second "Are you sure?" toast |
| Tokens for every colour and size | Hex values and pixel sizes in page CSS |
| `formatDuration`, `formatTokens`, `formatCurrency` | `toFixed` and string concatenation |
| Inline SVG icons from `icons.js` | Emoji or symbol characters as icons |
| Hairlines and space to separate | Nested panels, coloured side stripes |
| Mask secrets; reveal on request | Secrets in the URL, in logs, or in `localStorage` |

## htm and Preact notes

The vendored bundle is htm 3.1.1 with its bundled Preact 10. Things that bite:

- **Line breaks eat spaces in prose.** htm drops whitespace around a newline
  inside text, so `…secret in⏎<span>file</span>` renders "infile". Keep
  running text with inline elements on one line.
- **`${count && html…}` prints `0`.** Use `count > 0 &&` or a ternary. Props
  that take a slot treat `false` as content: pass `null`.
- **Closing a component:** `<//>`. Spread props: `<${Button} ...${props}>`.
- **ARIA booleans as strings:** `aria-pressed=${on ? 'true' : 'false'}`.
- **Focus events do not bubble:** use `onFocusCapture` / `onBlurCapture` on a
  wrapper; `onFocusIn` does not work.
- **No context across a Portal.** Overlays render in a second root. Share
  state through stores (`createStore`, `useStore`), which is what the app
  does everywhere.
- **No `createPortal`, `Fragment`, `memo` or `forwardRef`** in this bundle. A
  template with several roots returns an array, which is fine. A component
  that needs its DOM node takes an explicit ref prop (`inputRef`).
- **`useSize()` returns a callback ref:** `ref=${ref}`, not `ref.current`.
- **Effects run a frame after the render they belong to.** By then the user
  may have typed again. Do not reconcile a prop with what is in a field from
  an effect (`useEffect(() => setText(String(value)), [value])` eats
  keystrokes); decide during render, as `NumberInput` does, or keep the
  field fully controlled.
- **A floating layer is placed when it opens, not when it mounts.**
  `usePresence` keeps a closing layer mounted for its exit animation, so
  "mounted" does not change when it is reopened within that time. Depend on
  `open` in the effect that measures and positions (see `Menu`).
- **Top-level `await loadStyles(...)`** is allowed in page modules (they are
  loaded with `import()`).
- The inline theme script in `index.html` needs `script-src 'unsafe-inline'`
  or its hash if the admin server ever sets a Content-Security-Policy.
