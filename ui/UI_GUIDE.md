# Switchyard dashboard: guide for page authors

This is the contract between the dashboard foundation (design system and app
shell) and the pages built on it. Read it once, keep `#/_kit` open while you
work (type the address: the kit is not in the navigation or the command
palette), and copy patterns from `js/pages/kitchen-sink.js`.

Contents: [Ground rules](#ground-rules) · [Running it](#running-it) ·
[File layout](#file-layout) · [Adding a page](#adding-a-page) ·
[Data](#data) · [Live updates](#live-updates) · [Forms](#forms-and-api-issues) ·
[Leaving with unsaved changes](#leaving-with-unsaved-changes) ·
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
- **No inline scripts, no inline event handlers.** The gateway serves the
  dashboard with `Content-Security-Policy: script-src 'self'`
  (`crates/admin/src/assets.rs`): a `<script>` with code in it, an `onclick=`
  attribute or a `javascript:` URL is refused by the browser. Scripts are
  files. Inline `style` attributes are allowed.

## Running it

The gateway is the development server. A debug build serves `ui/` straight
from disk, so the loop is: run it, edit a file under `ui/`, reload.

```
cargo run -p switchyard                # http://127.0.0.1:8317/admin/
cargo run -p switchyard -- --port 9000
```

The first run writes `switchyard.toml` next to where you start it, with a
generated admin secret (the `secret = "…"` line under `[admin]`: sign in with
it) and a client key, and turns the built-in mock provider on. Its models
(`mock-echo`, `mock-lorem`, `mock-think`, `mock-tools`, `mock-slow`,
`mock-error-429`, `mock-error-500`, …) give every page something real to
show without an upstream account: send a few requests at `/v1/chat/completions`
with the client key and the requests, usage, logs and overview pages fill up.

Files are sent with `cache-control: no-cache` and an ETag, so a plain reload
picks up what is on disk. A release build embeds the same files at compile
time; nothing else differs. There is no mock admin API: pages are built and
checked against the real one (`crates/admin/API.md` says what it returns).

Before handing a page over, run the self-check:

```
node ui/tests/check.mjs
```

It parses and imports every module, loads every route, asserts the logic in
`lib/` (formatting, the SSE parser, the API client against a stubbed `fetch`,
the refetch policy of `useResource`), checks `index.html` against the
Content-Security-Policy, and renders components into a small stub document
to check behaviour (`tests/dom.mjs`: keyboard, focus, submit, where overlays
land, the leave guards against a stub history). It takes about twenty
seconds.

It needs no browser, so it cannot tell you how the page looks: the stub
document has no CSS and no layout. Look at the page, in both themes, at
1440px and at 375px, with the browser's console open: it must stay clean.

When you fix a bug in a component, add the case to `tests/dom.mjs`; for a
pure helper, to `tests/check.mjs`. A test that would have passed before the
fix is not a regression test: run it against the old code once.

## File layout

```
ui/
  index.html            shell document, font preloads; loads js/theme-boot.js and js/app.js
  favicon.svg
  package.json          marks the .js files as ES modules for Node and editors; no dependencies
  UI_GUIDE.md           this file
  tests/
    check.mjs           self-check, runs everything: node ui/tests/check.mjs
    dom.mjs             component behaviour, rendered into a stub document
    dom-stub.mjs        that document: nodes, events, focus, selectors; no CSS
  css/
    tokens.css          every colour, size, radius, shadow, duration (both themes)
    base.css            reset, type defaults, focus ring, utility classes
    components.css      one block per component
    layout.css          shell, sidebar, top bar, phone nav, palette, boot splash
    pages/<name>.css    styles of one page, loaded by that page
  fonts/                Archivo (UI), JetBrains Mono (data), licences
  vendor/               preact-htm.js, LICENSES.txt
  js/
    theme-boot.js       classic script: stamps the theme before the first paint
    app.js              boot and auth gate
    routes.js           the route table (edit this to add a page)
    shell/              shell.js (frame, navigation, signOut), palette.js (Ctrl/Cmd+K)
    lib/
      api.js            fetch wrapper, ApiError, session, streamSSE
      live.js           live event client, useLive, useLiveGap, liveState
      store.js          createStore, useStore
      router.js         useRoute, navigate, href, useQueryParam, setQuery;
                        leave guards: useLeaveGuard, registerLeaveGuard, mayLeave
      hooks.js          useResource, useAsync, useInterval, useNow, useDebounced,
                        useLocalStorage, useMediaQuery, useIsPhone, useHotkey,
                        useSize, usePresence, useModalLayer, useOutsidePointer
      format.js         numbers, tokens, durations, countdowns, times, bytes,
                        money, percent, sentence
      commands.js       useCommands: add entries to the command palette
      theme.js          theme store, setThemePref
      dom.js            cx, loadStyles, copyText, placeFloating, focus helpers,
                        the overlay stack (topOverlay, overlayLocked)
    components/         see "Components"; index.js re-exports all of them
    pages/<name>.js     one module per route
    pages/<name>/*.js   that page's own sub-modules, when it needs them
```

## Adding a page

1. Create `js/pages/<name>.js` with a default-exported component.
2. Add an entry to `ROUTES` in `js/routes.js` (path, title, icon, group, load).

That is all: the sidebar, the phone navigation, the command palette and the
document title follow the route table. An entry without `group` stays out of
the navigation; one with `dev: true` (a page for page authors, like the kit)
stays out of the command palette too and is opened by its address.

A page that outgrows one file puts the rest in `js/pages/<name>/` and its
styles in `css/pages/<name>.css`. Only the route module (the file directly in
`js/pages/`) needs a default export; sub-modules export what they like. The
self-check imports all of them.

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
          rows=${providers.data}
          loading=${providers.loading}
          error=${providers.error}
          errorTitle="Could not load the providers"
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

The admin API returns lists as bare arrays (`GET /providers` is
`[{ name, kind, … }, …]`, not `{ providers: […] }`), so `rows` is the
resource's `data` itself.

What a page gets and must do:

- **Props:** `route`, the parsed route `{ path, segments, query }`.
  `route.segments[1]` is the first sub-segment (`#/requests/req_123`).
- **Start with `<Page>`.** It renders the `h1` and sets the tab title.
- **URL state.** Filters, the selected tab, the time range and the open record
  belong in the query (`useQueryParam`), so a link reproduces the view and
  Back closes a drawer. Use `navigate(path, { query })` to change page.
- **Which changes are a step in the history.** A query change replaces the
  current entry by default: right for what is typed (a search field would
  add an entry per keystroke) and for paging. What the user picks from a few
  choices is a step Back returns to: a tab, a time range, a grouping, the
  record a drawer opens. Ask for it where the parameter is declared,
  `useQueryParam('tab', 'general', { push: true })`, or per call,
  `setQuery({ open: id }, { replace: false })`. A pushing setter does nothing
  when given the value already shown, and takes `{ replace: true }` for the
  one call that should not be a step (correcting a stale `?tab=`). The
  setter may be passed straight to `onChange`: the DOM event a control hands
  it as a second argument is not taken for options.
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

**The session and the other tabs.** Each tab holds its own copy of the
secret (in `sessionStorage`, and in memory), including a tab that resumed a
remembered session from `localStorage`: signing in elsewhere without
"Remember" does not take it away. Signing out (`api.logout()`, which the
shell's sign-out calls) forgets the secret in the whole browser: every other
tab of the dashboard hears it (a `BroadcastChannel`, and a `storage` event
for browsers without one), drops its copy, closes its live connection and
shows the sign-in page. The `auth` store then reads
`{ status: 'anonymous', reason: 'signed-out', elsewhere: true }` there, and
`elsewhere: false` in the tab that signed out. Like a 401, a sign-out from
another tab cannot be held back by a leave guard. `api.logout({ allTabs: false })`
drops the secret without telling the other tabs (the boot screen's "Use a
different secret", for a secret it could not even check).

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

Returns `{ data, error, loading, refreshing, isPrevious, updatedAt, refresh(), mutate() }`.

- `loading` is the first load: show skeletons (Table does it for you).
- `refreshing` is a refetch with data on screen: keep the data, dim charts
  with `stale=${res.refreshing}`. Never swap content for a spinner.
- **A new key starts clean.** When the key changes (another id, another
  range), `data` is `undefined` and `loading` is true from the very render
  that has the new key: the hook never hands out one key's data as
  another's, so an effect on `[id, res.data]` can trust the pair.
- **`keepPrevious: true`** keeps the previous key's data on screen instead,
  with `isPrevious` true and `refreshing` true, until the new key's data has
  arrived. Use it where a switch should not collapse the view into
  skeletons: `useResource(['/usage/timeseries', { range }], { keepPrevious: true })`,
  then `stale=${res.refreshing}` on the chart. If the new key fails to load,
  the old data stays, `isPrevious` stays true and `error` is set: say that
  what is shown belongs to the previous selection.
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
useLiveGap(() => requests.refresh());                // frames may have been missed
const { status } = useStore(liveState);              // "open", "reconnecting", ...
```

Topics: `hello`, `request.started`, `request.finished`, `log`, `credential`,
`config.reloaded`, `stats`, `lagged`. The client asks the gateway only for
topics that have listeners. `lagged` (`{ missed }`) is the exception: the
gateway sends it, unasked, to a connection that fell behind, in place of the
events it dropped for it.

**Gaps.** A stream has holes: nothing arrives while the socket is down, and a
`lagged` frame stands for events that were dropped. A view built from frames
must load itself again when that happens. `useLiveGap(handler, { enabled })`
calls `handler({ reason, missed })` with `reason: 'reconnect'` when the
connection is open again after being down (not on the first connection of a
session) and `reason: 'lagged'` for a lagged frame. Outside components:
`live.onGap(fn)` returns the unsubscribe function.

Patterns:

- **Live table:** load a page with `useResource`, prepend frames with
  `useLive`, cap the list, pass `freshKeys` to `Table` so new rows flash once.
  Offer a pause switch; while paused, count what arrived and show
  "12 new requests" as the way back. `useLiveGap(res.refresh)` covers
  reconnects and lag.
- **Live numbers:** render the last `stats` frame; fall back to polling
  `/status` when `liveState.status` is not `open`.
- **config.reloaded / credential:** refetch the resource the page shows.

## Forms and API issues

Every control takes `label`, `hint`, `error`, `warning`, `optional` and
reports changes as `onChange(value)`. The admin API reports validation
problems as `issues: [{ path, message }]`; `useIssues` hands each one to its
field and `FormError` lists the rest.

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
- `error` is for a value that will not be accepted. `warning` is for one that
  will be, with a caveat the user should see ("Keys this short are easy to
  guess"): the caution colour, no `aria-invalid`, and the form still submits.
  It takes the hint's place while it shows; an error takes both.
- `FormError` prints the gateway's message as a sentence (capital first word,
  full stop) before its own "Check the highlighted field." Do the same where
  you put a gateway message in front of other text: `sentence(error.message)`
  from `lib/format.js`.
- A button that is `loading` keeps the keyboard focus (it is `aria-disabled`,
  not `disabled`) and ignores clicks, including the Enter that would submit
  its form again. Nothing to do in the page; do not move focus yourself when
  a save starts or ends.
- Keep the form on screen when saving fails. Never clear what was typed.
- Guard unsaved changes when leaving a drawer or page: the next section.

## Leaving with unsaved changes

A view that would lose something when the route changes asks first.

```js
import { useLeaveGuard } from '../lib/router.js';

const guard = useLeaveGuard(form.dirty, {
  title: 'Discard unsaved changes to the provider?',
  message: 'What you entered in the form has not been saved.',
});
```

While `form.dirty` is true, every way out is caught: `navigate()`,
`setQuery()` and `useQueryParam` setters, links, an address typed by hand,
the command palette, Back and Forward, and the shell's sign-out. The user
gets the confirm dialog ("Keep editing" / "Discard changes"); nothing moves
until they answer, and on "Keep editing" the address and the history are as
they were. Closing or reloading the tab gets the browser's own "Leave site?"
prompt.

Options (the second argument):

| Option | Meaning |
|---|---|
| `title`, `message`, `confirmLabel`, `cancelLabel`, `danger` | the confirm dialog |
| `ask({ to, from, how })` | replaces the dialog: answer `true`, `false` or a promise. Answer `false` after telling the user why (a toast) to refuse outright |
| `matters(to, from, how)` | which changes are guarded. Default: leaving the page (another path) and signing out; a change of the query on the same page passes. A form kept in the query guards itself: `matters: (to, from) => !to \|\| to.path !== from.path \|\| to.query.edit !== from.query.edit` |
| `unload: false` | leave closing and reloading the tab unguarded |

`to` and `from` are parsed routes; `to` is `null` when the view is left
without a route change (sign-out). `how` is `push`, `replace`, `traverse`
(Back, Forward), `signout` or `other`.

The hook returns `{ release() }`. When the user has already chosen to leave
through your own controls (the form's Discard button), call `guard.release()`
right before navigating, or navigate with `{ force: true }`, so they are not
asked twice:

```js
const discard = () => {
  guard.release();
  setQuery({ edit: null });            // or: navigate('/providers', { force: true })
};
```

After the user agrees in the dialog the guard stands down by itself; it
guards again once `dirty` has been false and turns true again.

For leaving that is not a route change (swapping the record a drawer shows in
place), ask before you do it: `if (await mayLeave()) …`. Below the hook,
`registerLeaveGuard(fn, { unload })` registers a plain function
`({ to, from, how }) => boolean | Promise<boolean>` and returns the function
that removes it; guards are asked newest first, and while one question is
open any other attempt to leave is refused without asking again.

What cannot be held back: a session the gateway ends (a 401 from any
request). Keep the draft somewhere that survives it if losing it would hurt.

How it is caught, for when it misbehaves: where the browser has the
Navigation API (`window.navigation`: Chromium browsers, and the newer Safari
and Firefox) a fragment navigation is cancelled in the `navigate` event,
before anything changes.
Otherwise, and for a Back the browser will not let anyone cancel, the router
hears of it in `hashchange`, keeps the route store where it is, writes the
view's address back and makes the step again if the user agrees. A page must
not listen to `hashchange` or `popstate` to do this itself: by the time a
page hears of a navigation, the router has already acted on it.

## Components

Import from `js/components/index.js`. Each module documents its props above
the component; this is the summary. Shared conventions:

- `class` adds a class to the root.
- Props a component does not know (`id`, `data-*`, `aria-*`, handlers) go to
  its main element in the controls and the simple wrappers: `Button`,
  `IconButton`, `CopyButton`, the form controls (to the `<input>`, `<select>`
  or `<button>`), `Form`, `Panel`, `StatGroup`, `StatusLamp` and `Badge`. The
  composite components (`Table`, the charts, `Stat`, `Notice`, the overlays,
  `Menu`, `Tabs`) take only the props listed for them: wrap them in an
  element of your own when you need an attribute on the outside.
- Tones are lamp names: `clear`, `caution`, `stop`, `info`, `off` (and
  `neutral` where a component has a plain state).
- Sizes are `sm`, `md` (default), `lg`.

### Actions

| Component | Props | Example |
|---|---|---|
| `Button` | `variant` secondary · primary · ghost · danger · danger-quiet; `size`; `icon`, `iconRight`; `loading` (spinner; clicks ignored; stays focusable); `disabled`; `href`; `block`; `type` | `<${Button} variant="primary" icon="plus" onClick=${add}>Add provider<//>` |
| `IconButton` | `icon`, `label` (required: name and tooltip), `variant` (ghost), `size`, `tooltip`, `tooltipSide` | `<${IconButton} icon="refresh" label="Refresh" onClick=${refresh} />` |
| `CopyButton` | `value` (string or function, may be async), `label`, `size`, `variant`, children for a labelled button | `<${CopyButton} value=${id} label="Copy request id" />` |
| `Spinner` | `size`, `label` | `<${Spinner} label="Loading" />` |
| `Menu` | `items` `[{ label, icon, hint, onSelect, href, danger, disabled, checked } \| { separator } \| { heading }]`, `label`, `icon`, `trigger(props)`, `side`, `align` | `<${Menu} label="Key actions" items=${items} />` |
| `Tooltip` | `content`, `side`, `align`, `delay`, `describe` | `<${Tooltip} content="Clears the cooldown">…<//>` |

One primary button per view. `danger` is the confirming button inside a
dialog; `danger-quiet` is the button that opens it. A tooltip never carries
the only copy of something: touch screens do not show it.

An open menu or tooltip closes when its anchor scrolls away (the page, or a
box the anchor is in), not when something else on the page scrolls: a log
tail that follows its end does not shut the menus around it.

Icons (`Icon name=…`, all names are on the kit page) include `stop` (a
square, for "stop the stream"), `arrow-left` and `grip` (the handle of a row
that can be dragged). Add a path to `components/icons.js` rather than
drawing an SVG in a page.

### Status

| Component | Props | Example |
|---|---|---|
| `StatusLamp` | `tone`, `label`, `detail`, `pulse`, `size`, `title`; other props (`data-*`, `id`) go to its root element | `<${StatusLamp} tone="caution" label="Cooling down" detail="41s left" />` |
| `Badge` | `tone`, `mono`, `outline`, `lamp`, `title`; other props go to its element | `<${Badge} mono tone=${toneForStatus(r.status)}>${r.status}<//>` |
| `Kbd` | children | `<${Kbd}>Esc<//>` |
| `toneForStatus(status)` | HTTP status to tone | 2xx clear, 3xx and 429 caution, other 4xx/5xx stop |
| `toneWord(tone)` | a tone in a word a person would say | `Healthy`, `Warning`, `Critical`, `In progress`, `Inactive` |

A lamp always has words next to it (or a `title` when the column header says
what it is). A lamp given neither is named `toneWord(tone)` for assistive
technology: a last resort, your own words are better ("Cooling down" says
more than "Warning"). `pulse` is for things that are live right now, nothing
else.

### Surfaces and content

| Component | Props | Example |
|---|---|---|
| `Page` | `title`, `description`, `actions` | see "Adding a page" |
| `Panel` (`Card`) | `title`, `description`, `actions`, `footer`, `flush`; other props go to the `<section>` | `<${Panel} title="Credentials" flush>…<//>` |
| `Notice` | `tone`, `title`, `icon`, `action`, children | `<${Notice} tone="caution" title="Restart needed">…<//>` |
| `StatGroup` | `label`, children (`Stat`s); other props (`data-*`, `id`) go to the group's element | `<${StatGroup} label="Traffic" data-stale=${stale ? '' : undefined}>…<//>` |
| `Stat` | `label`, `value` (formatted), `unit`, `delta` (ratio), `goodWhen` up · down · none, `deltaLabel`, `hint`, `trend`, `lamp`, `lampLabel` (words, or `false`), `loading` | `<${Stat} label="Error rate" value="1.8" unit="%" delta=${0.31} goodWhen="down" deltaLabel="vs previous hour" />` |
| `KeyValue` | `items` `[{ label, value, mono, copy, hidden }]` | `<${KeyValue} items=${[{ label: 'Request id', value: id, mono: true, copy: true }]} />` |
| `CodeBlock` | `value` (string or object), `language` auto · json · text, `title` (text or markup), `label`, `wrap`, `copy`, `maxHeight`, `note`, `actions` | `<${CodeBlock} title="Upstream request" value=${body} />` |
| `Timeline` | `items` `[{ tone, toneLabel, title, badges, time, description }]`; `toneLabel` names the lamp ("Failed"), default `toneWord(tone)` | attempts of a request, see the kit |
| `Skeleton` | `width`, `height`, `lines` | `<${Skeleton} lines=${4} />` |
| `EmptyState` | `icon`, `title`, `description`, `action`, `compact` | `<${EmptyState} icon="key" title="No client keys yet" description="…" action=${button} />` |
| `ErrorState` | `error` (ApiError), `title`, `description`, `onRetry`, `retrying`, `compact` | `<${ErrorState} title="Could not load providers" error=${res.error} onRetry=${res.refresh} />` |
| `Pagination` | `page`, `pageSize`, `total`, `onPage`, `noun` | known totals |
| `LoadMore` | `hasMore`, `loading`, `onLoad`, `shown`, `noun` | cursor paging (`before=`) |

Do not nest panels. Inside a panel separate with `<hr>` or space. Stats go
in a `StatGroup`, not in separate cards. An empty state says what will appear
and how to make it appear.

Names for assistive technology:

- A `Panel` with a `title` is a region named by that title (the heading has
  an id, the section is `aria-labelledby` it). Pass `aria-label` to name it
  differently, or to name a panel that has no title.
- A `Stat` with a `lamp` says the judgement in words as well. Give
  `lampLabel` ("Degraded", "Above the 5% alert line"): it is the lamp's
  accessible name and its tooltip. Without it the lamp is named
  `toneWord(lamp)` ("Warning", "Critical"), whether or not there is a `hint`:
  a hint such as "47 of 126 failed" gives the counts, not the judgement. Pass
  `lampLabel=${false}` only when the value or the hint next to the lamp
  already says the same thing in words ("Degraded"); the lamp is then
  decoration and hidden from assistive technology.
- `CodeBlock` names its scrolling block after a `title` that is text. When
  the title is markup, give `label`.
- On a phone a `Notice` with an `action` puts the action under the text, in
  line with it; nothing to do in the page.

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
`error`, `errorTitle`, `onRetry`, `empty`, `sticky`, `maxHeight`, `dense`,
`collapse`, `sortMenu`, `caption`.

Column: `key`, `header`, `label`, `render(row, index)`, `align`, `num`,
`mono`, `width`, `sortable`, `sortValue(row)`, `primary`, `hideOnPhone`.

- Numbers: `align: 'right', num: true`. Identifiers: `mono: true`.
- `header` is what the column heading shows: text, markup (an abbreviation,
  a visually hidden "Actions"), or nothing. `label` is the column's name as
  plain text, for the phone card (the label of each cell, `data-label`) and
  the phone sort menu; it defaults to a `header` that is text. Give `label`
  whenever the header is markup or empty.
- Mark one column `primary`: on phones the row becomes a card with that cell
  as its title, the other cells as label/value lines, and the sortable
  headers as a "Sort by" menu. Use `hideOnPhone` for columns that only repeat
  what the card already says.
- The "Sort by" menu always offers "Default order". With controlled sorting
  that is `onSort(null)`: go back to the order the page shows by default. A
  page with an order control of its own passes `sortMenu=${false}` (or hides
  the menu in its CSS: `.my-table .table-sortbar { display: none }`).
- Put the table in `<Panel flush>`. Loading, empty and error states are built
  in; pass `loading`, `error`, `empty`, and `errorTitle` to name what could
  not be loaded ("Could not load the providers").
- `dense` (32px rows) is for streams: requests, logs.
- Each row carries its key in the DOM as `data-row-key`
  (`tbody.querySelector('[data-row-key="…"]')`): for moving focus to the
  neighbour of a deleted row, or scrolling a row into view.
- **Sticky headers.** With `sticky` (the default) the header stays in view.
  A table that scrolls with the page keeps it under the top bar; one with
  `maxHeight` keeps it at the top of its own box, and so does one that is too
  wide for its box (it then scrolls sideways in that box, and its header
  leaves with the page, as before). Inside a drawer or a modal the header
  sticks to the top of the scrolling body. A page with a sticky bar of its
  own above its tables sets `--sticky-top` on an element around them, to the
  distance from the top of the window at which headers should stop:
  `--sticky-top: calc(var(--topbar-h) + 48px)`. Without it the header slides
  under the page's bar (the bar is drawn above it). Rows, and the controls in
  them, carry a `scroll-margin-top` worked out from the same variable, so a
  row that takes the keyboard focus above the fold (Shift+Tab up the list)
  stops below the top bar and the header instead of under them. Use
  `--sticky-top` rather than a `top:` rule of your own on the header cells,
  or that margin will not know about your bar.
- **Rows are memoised.** A row renders again only when its record is a
  different object, when `columns` is a different array, when it becomes or
  stops being selected or fresh, or (for a `render` that takes `index`) when
  its position changes. So: update a record by replacing it, not by changing
  it in place; and if you build `columns` with `useMemo`, list everything the
  `render` functions read (a `now` from `useNow`, the open id) in its
  dependencies. A `columns` array built on every render keeps working, and
  renders every row every time, which is fine for short tables. What ticks
  by itself (a relative time) is best a small component of its own in the
  cell.

### Choice

| Component | Props | Example |
|---|---|---|
| `Tabs` | `tabs` `[{ id, label, count, icon, disabled }]`, `value`, `onChange`, `label` | views of one thing; render the selected view yourself |
| `Segmented` | `options` (strings or `{ value, label, icon, title, disabled }`), `value`, `onChange`, `label`, `size` | `<${Segmented} label="Range" value=${range} onChange=${setRange} options=${['1h','24h','7d','30d']} />` |

Both are one tab stop: Tab enters at the selected item, arrows and Home/End
move and select. Disabled items are skipped, so a disabled "Bodies" tab (no
bodies captured) never traps the keyboard. If `value` names no enabled item
(a stale `?tab=` in a link), the first enabled one is the tab stop.

A tab strip wider than its box scrolls sideways. `Tabs` brings the selected
tab into view inside the strip whenever the selection changes (a link, the
palette, a click) without scrolling the page, and an edge of the strip fades
while more tabs are hidden beyond it. Do not call `scrollIntoView` on a tab.

A `Segmented` inside a `Field` keeps its own width. Its `label` is the
group's accessible name; the `Field`'s label is the visible one.

### Form controls

All take `label`, `hint`, `error`, `warning`, `optional`, `disabled`, and
call `onChange(value)`.

| Component | Extra props |
|---|---|
| `Input` | `value`, `type`, `icon`, `suffix`, `actions` (icon buttons are square; a labelled `Button` keeps its width), `clearable`, `onClear`, `clearLabel`, `mono`, `size`, `autoFocus`, `onEnter`, `inputRef`, `placeholder` |
| `Textarea` | `value`, `rows`, `maxRows`, `mono`, `autoGrow` |
| `Select` | `value`, `options` (strings or `{ value, label, disabled }`), `placeholder`, `size` |
| `NumberInput` | `value` (number or null), `min`, `max`, `step` (a whole step means whole numbers only), `unit`, `placeholder` (say what empty means). `onChange` fires only for values inside the range; other text is marked invalid, blocks `Form` submit, and is clamped on blur |
| `Switch` | `checked` |
| `Checkbox` | `checked`, `indeterminate` |
| `TagInput` | `value` (string[]), `validate(tag)`, `placeholder`. Pasting a list adds every entry that is new and empties the field either way |
| `SecretInput` | `value`, `placeholder`, `copy`, `readOnly`, `onReveal` (async, for stored secrets), `autocomplete` |
| `Field` | `label`, `hint`, `error`, `warning`, `htmlFor`: labels a custom control |
| `Form`, `FormRow`, `FormActions`, `FormError`, `useIssues` | see "Forms and API issues" |

A Switch is a state that applies ("Enabled"); a Checkbox is a choice inside a
form that is saved later. Every control needs a visible label; a placeholder
is an example, not a label.

**Search fields and their clear button.** The browser's own clear button on
`type="search"` is hidden (it would sit next to ours, in another style).
`Input` shows one instead, an x at the end of the field, while the field
holds text: by default on every `type="search"` field that passes no
`actions`, so `<${Input} type="search" icon="search" value=${q} onChange=${setQ} />`
needs nothing more. It calls `onChange('')`, then `onClear()` if you gave
one, and puts the focus back in the field. `clearable` asks for it on any
field, or next to your own `actions`; `clearable=${false}` leaves it out;
`clearLabel` renames it (default "Clear"). A search that is applied on Enter
rather than as you type applies the empty search in `onClear`:
`onEnter=${() => apply(text)} onClear=${() => apply('')}`. A field that
passes its own clear action in `actions` keeps it and gets no second one.

### Overlays

| Component | Props |
|---|---|
| `Modal` | `open`, `onClose`, `title`, `description`, `size` sm · md · lg, `footer`, `dismissable`, `returnFocus` |
| `Drawer` | `open`, `onClose`, `title`, `subtitle`, `actions`, `footer`, `width`, `side` right · bottom, `dismissable`, `returnFocus` |
| `ConfirmDialog` | `open`, `title`, `message`, `confirmLabel`, `cancelLabel`, `danger`, `onConfirm` (may be async), `onClose`, `typeToConfirm`, `returnFocus` |
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
- A drawer's `actions` sit on one line with the close button, at their own
  width; a long title (a provider name with no space in it) wraps beside
  them. Keep `actions` to a few icon buttons and at most one labelled button,
  or they leave the title little room on a phone; put the rest in a `Menu`.
- Modal only when the task must interrupt: a short create form, a
  confirmation. Try inline first.
- `dismissable=${false}` (Modal and Drawer) turns off every way out the
  layer provides: Escape, the scrim and the close button. Set it while a
  save is in flight (`dismissable=${!save.loading}`) and disable your own
  Cancel button the same way. The command palette does not open while any
  open layer is not dismissable, a menu open inside such a dialog included
  (`overlayLocked()` in `lib/dom.js` is the question to ask before opening
  something over the page from a shortcut of your own).
- **Focus on closing** goes back to the control that opened the layer. When
  that control is gone by then (the menu button of a row the dialog just
  deleted), can no longer take focus, or sits in a layer that is closing
  with this one (the "Delete" button of the drawer a confirmed delete also
  closes), it goes to `returnFocus` if you gave one (an element, a ref, or a
  function returning an element), else to the nearest thing around the
  opener that can hold focus: its row, its panel, the drawer it was in, the
  page's `<main>`. Never to `<body>`. The same happens when the opener is
  removed shortly after the layer closed (the list refetched). A layer that
  opened while nothing had the focus (Ctrl+K on a page just loaded, a drawer
  opened by a link) has no opener: it hands the focus to `returnFocus`, else
  to the page's `<main>`.
- **A focus you place yourself wins.** A closing layer only hands the focus
  back while it is still inside the layer or nowhere (`<body>`). If the page
  has moved it meanwhile, to the row that took a deleted one's place say, it
  stays there: neither the opener, nor `returnFocus`, nor the ancestors take
  it away, and the fallback waits a moment (80ms) for the page before it
  steps in. So a page may either focus the new place itself right after the
  action (in the same task, a zero timer or the next frame), or pass
  `returnFocus` and do nothing; it need not do both, and need not check
  again later.
- Toast for the result of what the user just did. Anything they must act on
  stays on the page: `Notice`, `FormError`, `ErrorState`.
- Toasts are drawn above modals, drawers and their scrims, so the result of
  saving inside a drawer is seen without closing it. They stand clear of a
  drawer's footer by themselves; a page with a sticky bar of its own in the
  bottom corner lifts them with `--toast-lift` (see Tokens).
- `Toaster` and `ConfirmHost` are mounted by the shell; do not mount them.

### Track diagram

`TrackDiagram`: `models` `[{ id, label, note }]`, `providers`
`[{ id, label, note, state }]`, `routes` `[{ model, provider, order, state, lit }]`,
`selected`, `onSelect`. The lit rails are the route a request takes now (the
first route in failover order whose state is `clear`). Dashed rails lead to a
failing provider. Below 520px it becomes a list.

Each rail is two lines with the surface colour between them, so the diagram
has to know what it sits on. In a panel, a drawer or a modal it does
(`--surface-bg`, see Tokens). On any other background, a well inside a
drawer say, set `--surface-bg` on the diagram's container to that colour.

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
`tipFormat`, `utc`, `integer`, `yMin`, `yMax`, `xLabel`, `legend`, `table`,
`emptyText`. A `null` in `values` breaks the line. `AreaChart` is `LineChart`
with `area`; `BarChart` takes the same props (no `area`, `stacked` or `yMin`:
columns always stack from zero).

- **`utc`**: a time axis is printed in UTC everywhere: the ticks, the
  tooltip's head (which then ends in "UTC") and the table view (its heading
  becomes "Time (UTC)"). Use it for buckets the gateway cuts at UTC midnight
  (`/usage/timeseries` day buckets): printed in local time they read as the
  previous day west of Greenwich.
- **`integer`**: whole-number y ticks, for axes that count things (requests,
  errors, keys). A maximum of 1 gives ticks 0 and 1, never 0.5.
  `niceScale(min, max, target, { integer: true })` does the same for your
  own SVG.
- **`xFormat`** formats the tick text. **`tipFormat`** formats the x value in
  full, for the tooltip's head and the first column of the table view (by
  default the time with as much of the date as the range needs, or the
  category label).
- The tooltip lists every series. From five series on it leaves out the ones
  whose value in that bucket is zero or missing, so a quiet minute reads as
  one or two lines; with nothing left it says "Nothing in this bucket".
- A long series name in the legend is cut with an ellipsis (the full name is
  its `title`). Keep names short all the same: the tooltip has less room.

| Component | Props | Example |
|---|---|---|
| `Sparkline` | `data` (numbers, oldest first), `width`, `height`, `area`, `min`, `max`, `minPoints` (default 2), `label` (without it the sparkline is decorative). With fewer values than `minPoints`, or values bunched at one end so the line would be a speck, it draws an empty box of the same size | `<${Sparkline} data=${rpm} label="Requests per minute, last hour" />` |
| `BarList` | `items` `[{ key, label, value, hint, href, color }]`, `format`, `share`, `rank`, `limit` (folds the rest into "N others"), `sort`, `mono`, `emptyText` | `<${BarList} items=${byModel} format=${formatTokens} share limit=${8} />` |
| `LatencyBars` | `items` `[{ label, value (ms), tone }]`, `max`, `format` | `<${LatencyBars} items=${[{ label: 'p50', value: 420 }, { label: 'p99', value: 3100, tone: 'caution' }]} />` |
| `Meter` | `value`, `max` (default 1), `tone` clear · caution · stop, `text` (default the percentage; `false` hides it), `label` | `<${Meter} value=${468} max=${600} text="468 / 600 rpm" label="Rate limit use" />` |
| `HealthStrip` | `buckets` `[{ ok, failed, label }]` oldest first, `slots` (default 20), `label`, `noun` (what the buckets count, plural, for the spoken summary; default "requests") | `<${HealthStrip} buckets=${p.attempts} label=${p.name} noun="upstream attempts" />` |
| `seriesColor(series, i)`, `foldSeries(series, keep)`, `niceScale(min, max, target, { integer })` | helpers for custom SVG in a page | `foldSeries(series, 5)` keeps the five largest and sums the rest into "Other" |

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

**Variables a page sets.** Three custom properties let a page tell the shared
styles about its surroundings. They are not in `tokens.css` because they have
no value until a page gives one.

| Variable | Set it on | Says |
|---|---|---|
| `--sticky-top` | an element around your tables | where sticky table headers stop, measured from the top of what scrolls. Default: `var(--topbar-h)` on a page, `0px` inside a drawer or modal body. A page with its own sticky bar adds the bar's height: `--sticky-top: calc(var(--topbar-h) + 48px)` |
| `--toast-lift` | `body` or `:root` (the toasts live in `<body>`, outside the page) | how far to raise the toast stack above its corner, for a page with a sticky bar there: `body:has(.my-savebar) { --toast-lift: 64px; }` |
| `--surface-bg` | the container | the colour of the surface things are drawn on, for marks that paint "in the surface colour": the bed between a `TrackDiagram`'s rails, the ring around chart dots. Panels set it to `--bg-surface`, drawers and modals to `--bg-raised`. Set it yourself where the background is anything else (a well: `--surface-bg: var(--bg-sunken)`) |

**Focus ring.** `:focus-visible` draws a 2px outline, 2px out, that follows
the element's own corners. It sets no radius: do not add one to make the ring
fit, give the element the radius it should have.

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
  `usePresence` reaches `"open"` after two animation frames, or after 80ms
  in a tab that gets none (a background tab), so a layer opened there is
  visible when the tab is. Do not wait on `requestAnimationFrame` alone for
  anything that must happen: pair it with a timer the same way.
- A keyframe animation that only draws the eye (the flash of a fresh table
  row) goes inside `@media (prefers-reduced-motion: no-preference)`.

## Accessibility

- Everything works from the keyboard. Custom widgets follow the existing
  ones: arrows inside a group, Tab between groups, Escape closes.
- **Shortcuts** go through `useHotkey(combo, handler, options)`:
  `useHotkey('/', focusSearch)`, `useHotkey('mod+s', save, { enabled: dirty })`.
  One without `mod` is ignored while the user types in a field. While a
  modal layer is open (drawer, modal, menu, the palette) the keyboard belongs
  to that layer and page shortcuts are ignored, so "/" behind a drawer does
  nothing; there is no need for `enabled: !drawerOpen`. A shortcut that is
  part of what a layer shows (Ctrl+S in a drawer's form) passes
  `inLayer: true`: it then fires for keys pressed inside the topmost layer.
- Do not remove the focus ring. Do not add `outline: none` without a
  replacement.
- Focus is never dropped on `<body>`. Overlays hand it back (see Overlays);
  when your own control removes itself (a dismiss button, "Show 12 new"),
  move focus to what took its place. The shell does it for the page: on a
  change of page, and when it replaces the sign-in form after signing in,
  the focus goes to `<main>`. A plain load of a remembered session leaves it
  where the browser put it, so the first Tab reaches "Skip to content".
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
- **Time, by what it is for** (all in `lib/format.js`):

  | Need | Function | Reads |
  |---|---|---|
  | how long something took | `formatDuration(ms)` | `312ms`, `1.24s`, `1m 05s` |
  | a countdown as a clock | `formatCountdown(seconds)` | `0:42`, `29:41` |
  | a countdown in a sentence ("back in …") | `formatCountdownWords(seconds)` | `42s`, `29m 41s`, `1h 02m` |
  | a duration spelled out (a hint under a setting) | `formatDurationWords(ms)` | `30 minutes`, `1 hour 30 minutes` |
  | when, roughly | `formatRelativeTime(at, now)` | `12s ago`, `in 5m` |
  | when, on the clock | `formatTime(at, { ms, utc })`, `formatDateTime(at, { utc })` | `14:03:27`, `2 Oct 14:03:27` |
  | when, exactly (detail views, `title`) | `formatTimestamp(at, { zone, utc })` | `2 Oct 2026 14:03:27.512 UTC+02:00` |

  Countdowns round up, so they never read "0s" while time is left. Take `now`
  from `useNow()`: it is the clock's reading at each tick, in ms, not rounded
  to the second, so `(end - now) / 1000` is the time really left. `useNow(60_000)`
  re-renders once a minute for things that change slowly.
- **A gateway message in front of other text** goes through `sentence()`:
  the gateway writes "the request body is not valid JSON", lower case and
  without a full stop.
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
| `useLeaveGuard(dirty, …)` for unsaved changes | Your own `hashchange`, `popstate` or `beforeunload` listeners |
| `useLiveGap(res.refresh)` after reconnects and lag | Watching `liveState` for transitions by hand |
| `useResource(key, { keepPrevious: true })` across a range switch | Keeping the last data in a ref of your own |
| Scripts in files | Inline `<script>` or `onclick=` (the CSP refuses them) |

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
  that needs its DOM node takes an explicit ref prop (`inputRef`). To keep a
  component from rendering again, write a class with `shouldComponentUpdate`
  (`Component` is exported by the bundle; `TableRow` in `components/table.js`
  is the model).
- **`useSize()` returns a callback ref:** `ref=${ref}`, not `ref.current`.
  The size is 0 on the first render and right from the second, which comes
  before the first paint; it follows the element through a ResizeObserver
  after that.
- **A ref is called before its element is in the document.** This Preact
  applies refs while the tree is still being built, so measuring in a ref
  callback reads 0. Measure in `useLayoutEffect`, which runs once the commit
  is done (`useSize` does both).
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
- **`js/theme-boot.js` is the one classic script.** It is loaded with a plain
  blocking `<script src>` ahead of the stylesheets so the theme is set before
  the first paint, and it is a file because the Content-Security-Policy
  refuses inline scripts. It shares its storage key and its two colours with
  `lib/theme.js`; the self-check holds them together.
