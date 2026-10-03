// Component behaviour, rendered into the stub document (dom-stub.mjs).
// Run by check.mjs, or alone: node ui/tests/dom.mjs
//
// Each block guards a behaviour that once went wrong and that no pure
// helper can show: a hook's timing, where an overlay lands in the document,
// what a key press or a submit does. Looks are not covered: use the kit page.
import assert from 'node:assert/strict';
import { installDom, installFrameClock } from './dom-stub.mjs';

// Modules first: they are written to load without a document.
installFrameClock();
const { html, render, useRef, useState } = await import('../vendor/preact-htm.js');
const { useAsync, useHotkey, useNow, useResource, useSize } = await import('../js/lib/hooks.js');
const { overlayLocked, topOverlay } = await import('../js/lib/dom.js');
const { formatCountdown } = await import('../js/lib/format.js');
const { ApiError, api, auth, watchOtherTabs } = await import('../js/lib/api.js');
const { live, liveState, useLive, useLiveGap, TOPICS } = await import('../js/lib/live.js');
const { Button, IconButton } = await import('../js/components/button.js');
const { Tabs, Segmented } = await import('../js/components/nav.js');
const { Field, Form, FormError, Input, NumberInput, TagInput, useIssues } = await import('../js/components/form.js');
const { Menu } = await import('../js/components/menu.js');
const { ConfirmDialog, ConfirmHost, Drawer, Modal, confirm } = await import('../js/components/overlay.js');
const { toast, Toaster } = await import('../js/components/toast.js');
const { Table } = await import('../js/components/table.js');
const { Panel, Stat, StatGroup, Timeline } = await import('../js/components/surface.js');
const { Badge, StatusLamp, toneWord } = await import('../js/components/status.js');
const { CodeBlock } = await import('../js/components/code.js');
const { BarChart, HealthStrip, LineChart, Sparkline } = await import('../js/components/charts.js');
const { CommandPalette } = await import('../js/shell/palette.js');
const { LoadMore } = await import('../js/components/surface.js');

const { document, window, dispatch, sleep, text, until } = installDom();

/** Render into a fresh <div id="app"> in the body, as index.html has it. */
function mount(vnode) {
  const root = document.createElement('div');
  root.setAttribute('id', 'app');
  document.body.appendChild(root);
  render(vnode, root);
  return {
    root,
    async unmount() {
      render(null, root);
      root.remove();
      await sleep(10);
    },
  };
}

// ---- useAsync: success is never mistaken for failure ---------------------
{
  let action;
  let probe;
  function Probe() {
    probe = useAsync((...args) => action(...args));
    return null;
  }
  const view = mount(html`<${Probe} />`);

  action = async () => null; // api.put() against a route that answers 204
  assert.equal(await probe.run(), true, 'a save that returns no body succeeded');
  await until(() => probe.loading === false && probe.data === null, 'data is the raw result');
  action = async () => {}; // an action written as a block, returning nothing
  assert.equal(await probe.run(), true);
  action = async (id) => ({ id });
  assert.deepEqual(await probe.run('key_1'), { id: 'key_1' }, 'a result is passed through');
  action = async () => {
    throw new Error('refused');
  };
  assert.equal(await probe.run(), undefined, 'undefined is the one failure signal');
  await until(() => probe.error?.message === 'refused', 'the error is in state');
  action = async () => ({ ok: true });
  assert.ok(await probe.run());
  await until(() => probe.error === null, 'a later success clears the error');
  await view.unmount();
}

// ---- useResource: a response slower than the poll interval still lands ----
{
  const seen = { calls: 0, aborted: 0 };
  const slow = (signal) =>
    new Promise((resolve, reject) => {
      const n = ++seen.calls;
      const timer = setTimeout(() => resolve({ n }), 60);
      signal.addEventListener('abort', () => {
        clearTimeout(timer);
        seen.aborted += 1;
        reject(new Error('aborted'));
      });
    });
  let res;
  function Probe() {
    res = useResource(slow, { pollMs: 15 });
    return null;
  }
  const view = mount(html`<${Probe} />`);
  assert.equal(res.loading, true);
  await until(() => res.data !== undefined, 'the first load to land despite polling', 1500);
  assert.equal(seen.aborted, 0, 'polling aborted the request in flight');
  assert.equal(res.loading, false);
  assert.equal(res.error, null);
  const first = res.data.n;
  await until(() => res.data.n > first, 'the poll to refetch');
  assert.equal(seen.aborted, 0);

  // refresh() is handed straight to onClick/onRetry: an event argument must
  // not change what it does, and it resolves when fresh data is in.
  const callsBefore = seen.calls;
  await res.refresh({ type: 'click' });
  assert.ok(seen.calls > callsBefore);
  assert.equal(seen.aborted, 0);

  await view.unmount();
  const callsAtUnmount = seen.calls;
  await sleep(80);
  assert.equal(seen.calls, callsAtUnmount, 'polling stops on unmount');
}

// ---- useResource: a response that predates mutate() cannot undo it --------
{
  const seen = { calls: 0, aborted: 0 };
  const source = (signal) =>
    new Promise((resolve, reject) => {
      const n = ++seen.calls;
      const timer = setTimeout(() => resolve({ from: `request ${n}` }), 40);
      signal.addEventListener('abort', () => {
        clearTimeout(timer);
        seen.aborted += 1;
        reject(new Error('aborted'));
      });
    });
  let res;
  const shown = [];
  function Probe() {
    res = useResource(source);
    if (res.data && shown.at(-1) !== res.data.from) shown.push(res.data.from);
    return null;
  }
  const view = mount(html`<${Probe} />`);
  await until(() => res.data, 'the first load');
  res.refresh(); // request 2 leaves, then the user changes something
  await sleep(10);
  res.mutate({ from: 'local patch' });
  await until(() => res.data.from === 'request 3', 'a fresh request after the patch');
  assert.deepEqual(shown, ['request 1', 'local patch', 'request 3'], 'the stale answer of request 2 was never shown');
  assert.equal(seen.aborted, 1);
  res.mutate((data) => ({ ...data, from: 'idle patch' })); // nothing in flight: no request
  await sleep(60);
  assert.equal(seen.calls, 3);
  assert.equal(res.data.from, 'idle patch');
  await view.unmount();
}

// ---- Toaster: outside #app, so overlays cannot cover it -------------------
{
  const view = mount(html`
    <${Toaster} />
    <${Drawer} open=${true} onClose=${() => {}} title="Request">detail<//>
  `);
  const id = toast.error('Could not save', { duration: 0 });
  const item = await until(() => document.querySelector('.toast'), 'the toast to render');
  const list = document.querySelector('.toasts');
  assert.ok(list.contains(item));
  assert.equal(view.root.contains(list), false, 'the toast list must not be inside #app (an isolated stacking context)');
  assert.equal(list.parentNode.parentNode, document.body, 'the toast list is portalled to <body>');
  const drawer = document.querySelector('.drawer');
  assert.equal(drawer.parentNode.parentNode, document.body, 'overlays are siblings of the toast host, ranked by z-index alone');
  assert.match(text(item), /Could not save/);
  toast.dismiss(id);
  await until(() => !document.querySelector('.toast'), 'the toast to leave');
  await view.unmount();
  assert.equal(document.querySelector('.toasts'), null, 'the portal is removed with the shell');
}

// ---- Drawer and Modal: dismissable=false closes every built-in way out ----
{
  const closed = [];
  function Host({ dismissable }) {
    return html`
      <${Drawer} open=${true} dismissable=${dismissable} onClose=${(how) => closed.push(`drawer:${how}`)} title="Edit provider">form<//>
      <${Modal} open=${true} dismissable=${dismissable} onClose=${(how) => closed.push(`modal:${how}`)} title="Create key">form<//>
    `;
  }
  const view = mount(html`<${Host} dismissable=${false} />`);
  const drawerClose = await until(() => document.querySelector('.drawer [aria-label="Close"]'), 'the drawer');
  const modalClose = document.querySelector('.modal [aria-label="Close"]');
  assert.ok(drawerClose.hasAttribute('disabled'), 'the drawer close button is disabled while not dismissable');
  assert.ok(modalClose.hasAttribute('disabled'));
  drawerClose.click();
  for (const scrim of document.querySelectorAll('.scrim')) scrim.click();
  dispatch(document.body, 'keydown', { key: 'Escape' });
  assert.deepEqual(closed, []);

  render(html`<${Host} dismissable=${true} />`, view.root);
  await until(() => !document.querySelector('.drawer [aria-label="Close"]').hasAttribute('disabled'), 'the close button to come back');
  document.querySelector('.drawer [aria-label="Close"]').click();
  assert.deepEqual(closed, ['drawer:button']);
  await view.unmount();
}

// ---- Drawer: a long title wraps beside its buttons, never over them -------
// The stub has no layout, so this holds the structure and the rules that
// give it: a header of a long provider name once squeezed "Actions" and
// "Close" to 18px each, on top of one another.
{
  const view = mount(html`
    <${Drawer} open=${true} onClose=${() => {}} title="a-very-long-provider-name-for-layout-testing-purposes-1234567890"
      actions=${html`<${Menu} label="Actions for the provider" items=${[{ label: 'Delete', onSelect: () => {} }]} />`}>body<//>
  `);
  const head = await until(() => document.querySelector('.drawer .overlay-head'), 'the drawer');
  const group = head.querySelector('.overlay-head-actions');
  assert.ok(group, 'the actions and the close button sit in .overlay-head-actions');
  const labels = group.querySelectorAll('button').map((b) => b.getAttribute('aria-label'));
  assert.equal(labels.length, 2, labels.join(' | '));
  assert.match(labels[0], /^Actions for the provider/);
  assert.equal(labels[1], 'Close');
  await view.unmount();
  const { readFileSync } = await import('node:fs');
  const css = readFileSync(new URL('../css/components.css', import.meta.url), 'utf8').replace(/\/\*[\s\S]*?\*\//g, '');
  const rule = (selector) => new RegExp(`(^|\\n)${selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}\\s*\\{([^}]*)\\}`).exec(css)?.[2] ?? '';
  assert.match(rule('.overlay-head-actions'), /flex:\s*none/, 'the buttons keep their width');
  assert.match(rule('.overlay-head-text'), /flex:\s*1 1 auto/, 'the title takes what is left');
  assert.match(rule('.overlay-head-text'), /min-width:\s*0/);
  assert.match(rule('.overlay-title'), /overflow-wrap:\s*anywhere/, 'and wraps even without a space to break at');
  assert.match(rule('.icon-btn'), /flex-shrink:\s*0/, 'an icon button is never squeezed by a flex row');
}

// ---- Tabs and Segmented: arrows never strand on a disabled item ----------
{
  const tabs = [
    { id: 'summary', label: 'Summary' },
    { id: 'attempts', label: 'Attempts', count: 3 },
    { id: 'bodies', label: 'Bodies' },
    { id: 'raw', label: 'Raw events', disabled: true },
  ];
  let selected;
  let range;
  function Host({ initial }) {
    const [tab, setTab] = useState(initial);
    const [value, setValue] = useState('24h');
    selected = tab;
    range = value;
    return html`
      <${Tabs} label="Request detail" tabs=${tabs} value=${tab} onChange=${setTab} />
      <${Segmented} label="Range" value=${value} onChange=${setValue} options=${['1h', { value: '6h', label: '6h', disabled: true }, '24h']} />
    `;
  }
  const view = mount(html`<${Host} initial="bodies" />`);
  const tab = (i) => document.querySelectorAll('[role="tab"]')[i];
  const stops = () => document.querySelectorAll('[role="tab"]').map((el) => el.getAttribute('tabindex'));
  assert.deepEqual(stops(), ['-1', '-1', '0', '-1']);

  tab(2).focus();
  let event = dispatch(tab(2), 'keydown', { key: 'ArrowRight' });
  assert.equal(event.defaultPrevented, true);
  await until(() => selected === 'summary', 'ArrowRight to wrap past the disabled last tab');
  assert.equal(document.activeElement, tab(0));
  dispatch(tab(0), 'keydown', { key: 'ArrowLeft' });
  await until(() => selected === 'bodies', 'ArrowLeft to wrap to the last enabled tab');
  assert.equal(document.activeElement, tab(2));
  dispatch(tab(2), 'keydown', { key: 'Home' });
  await until(() => selected === 'summary', 'Home');
  dispatch(tab(0), 'keydown', { key: 'End' });
  await until(() => selected === 'bodies', 'End to stop at the last enabled tab');
  assert.equal(dispatch(tab(2), 'keydown', { key: 'a' }).defaultPrevented, false, 'other keys are left alone');

  const option = (i) => document.querySelectorAll('[role="radio"]')[i];
  assert.ok(option(1).hasAttribute('disabled'));
  option(2).focus();
  dispatch(option(2), 'keydown', { key: 'ArrowLeft' });
  await until(() => range === '1h', 'Segmented to skip its disabled option');
  assert.equal(document.activeElement, option(0));
  await view.unmount();

  // A value that names no enabled tab (a stale ?tab= in the URL) still
  // leaves the strip reachable with Tab.
  const stale = mount(html`<${Host} initial="raw" />`);
  assert.deepEqual(stops(), ['0', '-1', '-1', '-1']);
  await stale.unmount();
}

// ---- NumberInput in a Form: what is saved is what is on screen -----------
{
  let weight;
  let setWeight;
  const saved = [];
  function Host() {
    const [value, setValue] = useState(5);
    weight = value;
    setWeight = setValue;
    return html`
      <${Form} onSubmit=${() => saved.push(weight)}>
        <${NumberInput} label="Weight" value=${value} onChange=${setValue} min=${0} max=${1000} />
      <//>
    `;
  }
  const view = mount(html`<${Host} />`);
  const input = document.querySelector('input');
  const form = document.querySelector('form');
  const type = (value) => {
    input.value = value;
    dispatch(input, 'input');
  };
  input.focus();
  type('50');
  await until(() => weight === 50, 'in-range text to be reported as typed');
  type('500');
  await until(() => weight === 500);
  assert.equal(input.hasAttribute('data-entry-invalid'), false);

  type('5000');
  await until(() => input.getAttribute('aria-invalid') === 'true', 'out-of-range text to be marked invalid');
  assert.equal(weight, 500, 'out-of-range text is not reported');
  assert.equal(input.value, '5000', 'and is not rewritten under the cursor');
  assert.match(text(document.querySelector('.field-error')), /Enter a number from 0 to 1000\./);
  assert.equal(input.getAttribute('aria-describedby'), document.querySelector('.field-error').getAttribute('id'));

  // Enter submits the form without a blur: it must not save the stale 500.
  document.activeElement = document.body;
  const submit = dispatch(form, 'submit');
  assert.equal(submit.defaultPrevented, true);
  assert.deepEqual(saved, [], 'the form refused to submit a value that is not on screen');
  assert.equal(document.activeElement, input, 'focus goes to the field that needs fixing');

  type('1.5');
  await until(() => /whole number/.test(text(document.querySelector('.field-error'))), 'a fraction to be refused where steps are whole');
  type('abc');
  await until(() => /Enter a number\./.test(text(document.querySelector('.field-error'))));
  dispatch(form, 'submit');
  assert.deepEqual(saved, []);

  type('700');
  await until(() => weight === 700 && !input.hasAttribute('data-entry-invalid'), 'a good value to clear the mark');
  assert.equal(document.querySelector('.field-error'), null);
  dispatch(form, 'submit');
  assert.deepEqual(saved, [700]);

  // Leaving the field settles it: clamped, shown, reported.
  type('5000');
  await until(() => input.hasAttribute('data-entry-invalid'));
  input.blur();
  await until(() => weight === 1000, 'blur to clamp');
  await until(() => input.value === '1000' && !input.hasAttribute('data-entry-invalid'));
  dispatch(form, 'submit');
  assert.deepEqual(saved, [700, 1000]);

  // Typing faster than the parent echoes: "400" has been reported and
  // "4000" is on screen before the field hears back. Its own report coming
  // back must not put the text back to "400".
  input.focus();
  type('40');
  type('400');
  type('4000');
  await sleep(80);
  assert.equal(weight, 400);
  assert.equal(input.value, '4000', 'the echo of an earlier keystroke rewrote the text');
  assert.ok(input.hasAttribute('data-entry-invalid'));

  // A change from outside (a reset, a reload) does replace the text, even
  // text that is invalid.
  setWeight(12);
  await until(() => input.value === '12' && !input.hasAttribute('data-entry-invalid'), 'an outside change to show');
  type('13');
  await until(() => weight === 13);
  setWeight(13); // no change: nothing to do
  setWeight(5);
  await until(() => input.value === '5');
  await view.unmount();
}

// ---- Menu: reopened while it fades out, it is placed and visible ----------
{
  const picked = [];
  const view = mount(html`
    <${Menu} label="Theme" items=${[{ label: 'Light', onSelect: () => picked.push('light') }, { label: 'Dark', onSelect: () => picked.push('dark') }]} />
  `);
  const trigger = document.querySelector('button');
  const menu = () => document.querySelector('.menu');
  const shown = () => menu()?.getAttribute('data-state') === 'open' && /top:\d+px;left:\d+px/.test(menu().style.cssText) && !/hidden/.test(menu().style.cssText);

  trigger.click();
  await until(shown, 'the menu to open');
  // Close, then reopen inside the 140 ms exit animation. On a machine busy
  // enough to stall past the whole exit, the menu is gone before we get
  // there: open it again and have another go.
  let reopenedWhileFading = false;
  for (let attempt = 0; attempt < 5 && !reopenedWhileFading; attempt += 1) {
    trigger.click(); // close
    await sleep(15);
    if (!menu()) {
      trigger.click();
      await until(shown, 'the menu to open again');
      continue;
    }
    assert.equal(menu().getAttribute('data-state'), 'closed');
    assert.match(menu().style.cssText, /top:\d+px/, 'it fades out where it stood');
    trigger.click(); // reopen
    await until(() => trigger.getAttribute('aria-expanded') === 'true');
    await until(shown, 'a menu reopened during its exit to be placed and visible');
    reopenedWhileFading = true;
  }
  assert.ok(reopenedWhileFading, 'never managed to reopen the menu while it was fading out');
  await sleep(200); // past the old exit timer: it must not unmount the reopened menu
  assert.ok(shown());

  // Selecting closes it; a second click on the fading item does nothing.
  const item = document.querySelector('.menu-item');
  item.click();
  await sleep(20);
  item.click();
  assert.deepEqual(picked, ['light']);
  await until(() => !menu(), 'the menu to unmount after its exit');
  trigger.click();
  await until(shown, 'the menu to open again from scratch');
  await view.unmount();
}

// ---- useResource: a new key never comes with the previous key's data ------
{
  const realFetch = globalThis.fetch;
  let failNext = false;
  globalThis.fetch = (address, init) =>
    new Promise((resolve, reject) => {
      const fail = failNext;
      failNext = false;
      const timer = setTimeout(() => resolve(fail ? new Response('{"error":{"message":"no such request"}}', { status: 404 }) : new Response(JSON.stringify({ id: String(address).split('/').pop() }), { status: 200 })), 30);
      init.signal.addEventListener('abort', () => {
        clearTimeout(timer);
        reject(Object.assign(new Error('aborted'), { name: 'AbortError' }));
      });
    });
  try {
    const seen = [];
    let res;
    function Probe({ id, keep }) {
      res = useResource(`/requests/${id}`, { keepPrevious: keep });
      seen.push({ key: id, data: res.data?.id, loading: res.loading, refreshing: res.refreshing, isPrevious: res.isPrevious });
      return null;
    }
    const view = mount(html`<${Probe} id="req_a" keep=${false} />`);
    await until(() => res.data?.id === 'req_a', 'the first record');
    assert.equal(res.isPrevious, false);

    // The key changes. The render that has the new key must not carry the
    // old record: an effect on [id, data] would act on the wrong pair.
    seen.length = 0;
    render(html`<${Probe} id="req_b" keep=${false} />`, view.root);
    assert.deepEqual(seen[0], { key: 'req_b', data: undefined, loading: true, refreshing: false, isPrevious: false }, 'the first render with a new key is a loading one');
    await until(() => res.data?.id === 'req_b', 'the second record');
    assert.deepEqual(seen.filter((s) => s.data !== undefined && s.data !== s.key), [], 'no render paired a key with another key\'s data');

    // keepPrevious: the old record stays on screen, flagged, until the new one is in.
    seen.length = 0;
    render(html`<${Probe} id="req_c" keep=${true} />`, view.root);
    assert.deepEqual(seen[0], { key: 'req_c', data: 'req_b', loading: false, refreshing: true, isPrevious: true }, 'the previous data is kept and says that it is the previous');
    await until(() => res.data?.id === 'req_c', 'the third record');
    assert.equal(res.isPrevious, false);
    assert.equal(res.refreshing, false);
    for (const s of seen) assert.ok(s.data === 'req_c' ? !s.isPrevious : s.data === 'req_b' && s.isPrevious && !s.loading, `kept data is always flagged: ${JSON.stringify(s)}`);

    // The new key fails to load: the kept data stays, still flagged, with the error.
    failNext = true;
    render(html`<${Probe} id="req_gone" keep=${true} />`, view.root);
    await until(() => res.error, 'the failure');
    assert.equal(res.error.status, 404);
    assert.equal(res.data.id, 'req_c');
    assert.equal(res.isPrevious, true, 'data that belongs to another key stays flagged when the new key fails');
    assert.equal(res.refreshing, false);

    // Going idle (a closing drawer passes null) keeps what was shown, as before.
    function Idle({ id }) {
      res = useResource(id ? `/requests/${id}` : null);
      return null;
    }
    render(html`<${Idle} id="req_d" />`, view.root);
    await until(() => res.data?.id === 'req_d');
    render(html`<${Idle} id=${null} />`, view.root);
    await sleep(20);
    assert.equal(res.data.id, 'req_d', 'a source that becomes null keeps its last data');
    assert.equal(res.loading, false);
    await view.unmount();
  } finally {
    globalThis.fetch = realFetch;
  }
}

// ---- useNow: the time, not the time rounded down to the step --------------
{
  // Away from a second boundary, where rounding down would not show.
  while (Date.now() % 1000 < 300 || Date.now() % 1000 > 800) await sleep(20);
  let now;
  const renders = [];
  const cooldownEnds = Date.now() + 6000;
  function Probe() {
    now = useNow();
    renders.push(now);
    return null;
  }
  const view = mount(html`<${Probe} />`);
  assert.ok(Math.abs(Date.now() - renders[0]) < 100, `useNow() is the time now, got one ${Date.now() - renders[0]} ms old`);
  assert.equal(formatCountdown((cooldownEnds - renders[0]) / 1000), '0:06', 'a 6 second cooldown reads 0:06, not 0:07');
  const first = now;
  await until(() => now > first + 500, 'the next tick', 2500);
  assert.ok(Math.abs(Date.now() - now) < 200, 'each tick hands out the clock\'s reading at that tick');
  assert.ok(renders.every((value, i) => i === 0 || value >= renders[i - 1]), 'time does not go backwards between renders');
  // A slow step re-renders once per step, and still gives the real time.
  let slow;
  let slowRenders = 0;
  function Slow() {
    slow = useNow(60_000);
    slowRenders += 1;
    return null;
  }
  const other = mount(html`<${Slow} />`);
  await sleep(1300);
  assert.ok(slowRenders <= 3, `a 60 s step does not render every second (${slowRenders} renders)`);
  assert.notEqual(slow % 60_000, 0, 'and its value is not rounded to the step');
  await other.unmount();
  await view.unmount();
}

// ---- useSize: measured again once the element is in the document ----------
{
  // No ResizeObserver here, like a tab in the background that is never
  // painted: the size has to be right without one.
  assert.equal(typeof ResizeObserver, 'undefined');
  let size;
  function Probe() {
    const [ref, measured] = useSize();
    size = measured;
    // Nested: the ref is called while <section> is still being built, before
    // it is attached, and measures 0 then.
    return html`<section><div class="plot" ref=${ref}></div></section>`;
  }
  const view = mount(html`<${Probe} />`);
  await until(() => size.width === 10 && size.height === 10, 'the size of an element measured after it was attached');
  await view.unmount();

  // A chart draws only when it knows its width: it must get there too.
  const chart = mount(html`<${LineChart} x=${['a', 'b', 'c']} series=${[{ key: 's', label: 'S', values: [1, 2, 3] }]} label="Probe" />`);
  await until(() => document.querySelector('.chart-plot svg'), 'the chart to draw without a ResizeObserver');
  await chart.unmount();
}

// ---- usePresence: a layer opens without animation frames ------------------
{
  const realFrame = globalThis.requestAnimationFrame;
  globalThis.requestAnimationFrame = () => 0; // a background tab gets none
  try {
    const view = mount(html`<${Drawer} open=${false} onClose=${() => {}} title="Request">detail<//>`);
    render(html`<${Drawer} open=${true} onClose=${() => {}} title="Request">detail<//>`, view.root);
    const drawer = await until(() => document.querySelector('.drawer'), 'the drawer to mount');
    assert.equal(drawer.getAttribute('data-state'), 'closed', 'it mounts closed, to animate in');
    await until(() => drawer.getAttribute('data-state') === 'open', 'a drawer opened without frames to reach data-state="open"', 1500);
    await view.unmount();
  } finally {
    globalThis.requestAnimationFrame = realFrame;
    // Preact fell back to its 100 ms timer for effects meanwhile: let the
    // last of those run before the next block relies on prompt effects.
    await sleep(150);
  }
}

// ---- useHotkey: while a layer is open the keyboard belongs to it ----------
{
  const fired = [];
  function Inside() {
    useHotkey('mod+s', () => fired.push('save'), { inLayer: true });
    return html`<button id="inside">Save</button>`;
  }
  function Host({ open, dismissable = true }) {
    useHotkey('/', () => fired.push('search'));
    useHotkey('mod+b', () => fired.push('rail'));
    return html`
      <button id="outside">Add</button>
      <${Drawer} open=${open} dismissable=${dismissable} onClose=${() => {}} title="Edit provider"><${Inside} /><//>
    `;
  }
  const view = mount(html`<${Host} open=${false} />`);
  assert.equal(topOverlay(), null);
  // The shortcuts are bound in an effect, a frame or so after the render.
  await until(() => {
    fired.length = 0;
    dispatch(document.body, 'keydown', { key: '/' });
    return fired.length === 1;
  }, 'the page shortcut to be bound');
  dispatch(document.body, 'keydown', { key: 'b', ctrlKey: true });
  assert.deepEqual(fired, ['search', 'rail'], 'with no layer open shortcuts work');

  render(html`<${Host} open=${true} />`, view.root);
  const inside = await until(() => document.querySelector('#inside'), 'the drawer');
  await until(() => topOverlay() !== null, 'the drawer on the overlay stack');
  assert.equal(topOverlay().element, document.querySelector('.drawer'));
  assert.equal(topOverlay().dismissable, true);
  fired.length = 0;
  inside.focus();
  assert.equal(dispatch(inside, 'keydown', { key: '/' }).defaultPrevented, false, 'a page shortcut is not even consumed');
  dispatch(inside, 'keydown', { key: 'b', ctrlKey: true });
  assert.deepEqual(fired, [], 'page shortcuts do nothing while a drawer is open, though focus is inside it');
  dispatch(inside, 'keydown', { key: 's', ctrlKey: true });
  assert.deepEqual(fired, ['save'], 'a shortcut of the layer itself fires for keys pressed inside it');
  dispatch(document.querySelector('#outside'), 'keydown', { key: 's', ctrlKey: true });
  assert.deepEqual(fired, ['save'], 'and not for keys pressed outside the top layer');

  // The stack says whether the top layer may be covered (the shell asks before Ctrl+K).
  render(html`<${Host} open=${true} dismissable=${false} />`, view.root);
  await until(() => topOverlay()?.dismissable === false, 'the layer to report that it is not dismissable');
  render(html`<${Host} open=${false} />`, view.root);
  await until(() => topOverlay() === null, 'the stack to empty');
  fired.length = 0;
  dispatch(document.body, 'keydown', { key: '/' });
  assert.deepEqual(fired, ['search']);
  await view.unmount();
}

// ---- Modal layers: focus has somewhere to go when the opener is gone ------
{
  function Host({ open, opener = 'button', returnFocus }) {
    return html`
      <main id="main" tabindex="-1">
        <div id="row" tabindex="0">
          ${opener === 'button' && html`<button id="opener">Delete key</button>`}
          ${opener === 'disabled' && html`<button id="opener" disabled>Delete key</button>`}
        </div>
        <button id="add">Create key</button>
      </main>
      <${Modal} open=${open} onClose=${() => {}} title="Delete key build-bot?" returnFocus=${returnFocus}>Applications using it get 401.<//>
    `;
  }
  const inDialog = () => document.querySelector('.modal')?.contains(document.activeElement);

  // The usual case is unchanged: back to the control that opened the dialog.
  const view = mount(html`<${Host} open=${false} />`);
  document.querySelector('#opener').focus();
  render(html`<${Host} open=${true} />`, view.root);
  await until(inDialog, 'focus to move into the dialog');
  render(html`<${Host} open=${false} />`, view.root);
  await until(() => document.activeElement === document.querySelector('#opener'), 'focus to return to the opener');

  // What the dialog confirmed removed the opener (a deleted row's button).
  render(html`<${Host} open=${true} />`, view.root);
  await until(inDialog);
  render(html`<${Host} open=${false} opener="none" />`, view.root);
  await until(() => document.activeElement === document.querySelector('#row'), 'focus to land on the nearest focusable ancestor of the removed opener');
  assert.notEqual(document.activeElement, document.body);

  // The opener is still there but can no longer take focus.
  render(html`<${Host} open=${false} opener="button" />`, view.root);
  await sleep(10);
  document.querySelector('#opener').focus();
  render(html`<${Host} open=${true} />`, view.root);
  await until(inDialog);
  render(html`<${Host} open=${false} opener="disabled" />`, view.root);
  await until(() => document.activeElement === document.querySelector('#row'), 'a disabled opener to pass focus on');

  // A fallback the caller names wins over the ancestors.
  render(html`<${Host} open=${false} opener="button" />`, view.root);
  await sleep(10);
  document.querySelector('#opener').focus();
  const fallback = () => document.querySelector('#add');
  render(html`<${Host} open=${true} returnFocus=${fallback} />`, view.root);
  await until(inDialog);
  render(html`<${Host} open=${false} opener="none" returnFocus=${fallback} />`, view.root);
  await until(() => document.activeElement === document.querySelector('#add'), 'focus to go to the returnFocus target');

  // A focus the page placed while the dialog was closing is the page's
  // choice. Three pages move the keyboard to the neighbour of the row a
  // dialog just deleted, and afterwards only look for a focus that is lost:
  // a fallback that lands on <main> after them would be taken for theirs.
  const where = () => {
    const at = document.activeElement;
    return at === document.body ? '<body>' : `<${at.localName}${at.id ? ` id="${at.id}"` : ''}>`;
  };
  const placedByPage = async (closed, what) => {
    render(html`<${Host} open=${false} opener="button" />`, view.root);
    await sleep(120);
    document.querySelector('#opener').focus();
    render(html`<${Host} open=${true} />`, view.root);
    await until(inDialog);
    await sleep(120); // the layer's own late focus attempts are over
    render(closed, view.root);
    const neighbour = document.querySelector('#add');
    neighbour.focus();
    await sleep(300); // the clean-up, and the moment it waits before falling back
    assert.ok(document.activeElement === neighbour, `${what}; it is on ${where()}`);
  };
  await placedByPage(html`<${Host} open=${false} opener="none" />`, 'with the opener gone, the focus the page placed on the neighbour must survive the dialog closing');
  await placedByPage(html`<${Host} open=${false} opener="none" returnFocus=${() => document.querySelector('#row')} />`, 'returnFocus is a fallback too: it does not override a focus the page placed');
  await placedByPage(html`<${Host} open=${false} opener="button" />`, 'nor does the opener take back a focus that was put elsewhere on purpose');

  // A dialog told to close in the very frame it opened in is still waiting
  // for its clean-up when its second focus attempt (the next frame) comes
  // round: that attempt must not pull the focus back into the dialog.
  {
    render(html`<${Host} open=${false} opener="button" />`, view.root);
    await sleep(120);
    document.querySelector('#opener').focus();
    const realFrame = globalThis.requestAnimationFrame;
    const frames = [];
    globalThis.requestAnimationFrame = (fn) => {
      frames.push(fn);
      return 0;
    };
    try {
      render(html`<${Host} open=${true} />`, view.root);
      await until(inDialog); // effects run on Preact's own timer when no frame comes
      render(html`<${Host} open=${false} opener="none" />`, view.root);
      const neighbour = document.querySelector('#add');
      neighbour.focus();
      for (const frame of frames.splice(0)) frame(Date.now());
      assert.ok(document.activeElement === neighbour, `a closing dialog's pending focus attempt took the focus back; it is on ${where()}`);
    } finally {
      globalThis.requestAnimationFrame = realFrame;
    }
    await sleep(300);
    assert.ok(document.activeElement === document.querySelector('#add'), `and it stays where the page put it; it is on ${where()}`);
  }

  // The opener took the focus back and is removed a moment later (the list
  // refetched once the dialog had closed): the browser drops the focus on
  // <body> without a word. The layer watches for that for a short while.
  const reopenAndCancel = async () => {
    render(html`<${Host} open=${false} opener="button" />`, view.root);
    await sleep(120);
    document.querySelector('#opener').focus();
    render(html`<${Host} open=${true} />`, view.root);
    await until(inDialog);
    await sleep(120);
    render(html`<${Host} open=${false} opener="button" />`, view.root);
    await until(() => document.activeElement === document.querySelector('#opener'), 'the opener to take the focus back');
    await sleep(150);
  };
  await reopenAndCancel();
  render(html`<${Host} open=${false} opener="none" />`, view.root);
  assert.ok(document.activeElement === document.body, 'the removed opener left the focus on <body>');
  await until(() => document.activeElement === document.querySelector('#row'), 'the focus to be picked up after the opener was removed');
  // The same, on a page that places the focus itself when it removes the
  // row, the way the pages do it: a moment later, and only if it is lost.
  await reopenAndCancel();
  render(html`<${Host} open=${false} opener="none" />`, view.root);
  setTimeout(() => {
    if (document.activeElement === document.body) document.querySelector('#add').focus();
  }, 0);
  await sleep(300);
  assert.ok(document.activeElement === document.querySelector('#add'), `the watch does not get ahead of a page that focuses the neighbour itself; focus is on ${where()}`);

  // Nothing had the focus when the layer opened (Ctrl+K on a page just
  // loaded, a drawer opened from a link): it does not leave it on <body>
  // either, but on the page's main region, or on returnFocus when given.
  const fromNowhere = async (closed) => {
    render(html`<${Host} open=${false} opener="button" />`, view.root);
    await sleep(120);
    document.activeElement.blur();
    assert.ok(document.activeElement === document.body);
    render(html`<${Host} open=${true} />`, view.root);
    await until(inDialog);
    await sleep(120);
    render(closed, view.root);
  };
  await fromNowhere(html`<${Host} open=${false} opener="button" />`);
  await until(() => document.activeElement === document.querySelector('#main'), 'a layer opened with nothing focused to hand the focus to <main>, not <body>');
  await fromNowhere(html`<${Host} open=${false} opener="button" returnFocus=${() => document.querySelector('#add')} />`);
  await until(() => document.activeElement === document.querySelector('#add'), 'and to returnFocus when there is one');
  await fromNowhere(html`<${Host} open=${false} opener="button" />`);
  setTimeout(() => document.querySelector('#row').focus(), 0);
  await sleep(300);
  assert.ok(document.activeElement === document.querySelector('#row'), `a focus the page places itself still wins; it is on ${where()}`);
  await view.unmount();
}

// ---- Modal layers: an opener inside a layer that closes with the dialog ---
{
  // A row opens a drawer, "Delete" in the drawer opens a dialog, confirming
  // closes both. The dialog's opener is still in the document while the
  // drawer fades out: focus sent there would fall to <body> a moment later.
  function Host({ drawer, dialog }) {
    return html`
      <main id="main" tabindex="-1"><button id="row">build-bot</button></main>
      <${Drawer} open=${drawer} onClose=${() => {}} title="build-bot"><button id="delete">Delete key</button><//>
      <${Modal} open=${dialog} onClose=${() => {}} title="Delete key build-bot?"><button id="confirm">Delete key</button><//>
    `;
  }
  const view = mount(html`<${Host} drawer=${false} dialog=${false} />`);
  document.querySelector('#row').focus();
  render(html`<${Host} drawer=${true} dialog=${false} />`, view.root);
  await until(() => document.querySelector('.drawer')?.contains(document.activeElement), 'focus in the drawer');
  await sleep(120);
  document.querySelector('#delete').focus();
  render(html`<${Host} drawer=${true} dialog=${true} />`, view.root);
  await until(() => document.querySelector('.modal')?.contains(document.activeElement), 'focus in the dialog');
  await sleep(120);
  render(html`<${Host} drawer=${false} dialog=${false} />`, view.root);
  await until(() => document.activeElement === document.querySelector('#row'), 'focus to go past the closing drawer to the row that opened it');
  // Straight there, not by way of the dying button and <body>: the drawer is
  // still fading out when the row has the focus.
  assert.ok(document.querySelector('.drawer'), 'the focus reached the row while the drawer was still closing');
  await sleep(400); // both layers have left the document
  assert.ok(document.activeElement === document.querySelector('#row'), 'and it is still on the row when the drawer has gone');
  await view.unmount();
}

// ---- Modal layers: a delete confirmed from a menu in a drawer it closes ---
{
  // A row opens a drawer; the drawer's menu asks to delete the row's thing;
  // confirming closes the dialog and the drawer and removes the row. Every
  // opener on the way is gone or going: the menu (closed long ago), its
  // button (in the closing drawer), the row. The focus must end on the
  // page's <main>, or on the row the page names, never on <body>.
  const where = () => {
    const at = document.activeElement;
    return at === document.body ? '<body>' : `<${at.localName}${at.id ? ` id="${at.id}"` : ''}>`;
  };
  async function deleteFromDrawerMenu({ fromRow, named }) {
    function Host() {
      const [rows, setRows] = useState(['alpha', 'beta']);
      const [open, setOpen] = useState(fromRow ? null : 'alpha');
      const del = async (name) => {
        const ok = await confirm({ danger: true, title: `Delete ${name}?`, confirmLabel: 'Delete', action: async () => {}, returnFocus: named ? () => document.querySelector('#row-beta') : undefined });
        if (!ok) return;
        setOpen(null);
        setRows((list) => list.filter((r) => r !== name));
      };
      return html`
        <main id="main" tabindex="-1">${rows.map((r) => html`<div key=${r} id=${`row-${r}`} tabindex="0" onClick=${() => setOpen(r)}>${r}</div>`)}</main>
        <${Drawer} open=${open != null} onClose=${() => setOpen(null)} title=${open ?? ''}>
          <${Menu} label="Actions" items=${[{ label: 'Delete', onSelect: () => del(open) }]} />
        <//>
        <${ConfirmHost} />
      `;
    }
    document.activeElement.blur();
    const view = mount(html`<${Host} />`);
    if (fromRow) {
      const row = document.querySelector('#row-alpha');
      row.focus();
      row.click();
    }
    await until(() => document.querySelector('.drawer')?.contains(document.activeElement), 'focus in the drawer');
    await sleep(120);
    const trigger = document.querySelector('.drawer [aria-label="Actions"]');
    trigger.focus();
    trigger.click();
    const item = await until(() => document.querySelector('.menu [role="menuitem"]'), 'the menu');
    await sleep(60);
    item.click();
    await until(() => document.querySelector('.modal')?.contains(document.activeElement), 'focus in the dialog');
    await sleep(500); // the menu has long gone
    const yes = [...document.querySelectorAll('.modal button')].find((b) => text(b) === 'Delete');
    yes.focus();
    yes.click();
    const want = named ? '#row-beta' : '#main';
    await until(() => document.activeElement === document.querySelector(want), `the focus to reach ${want}`, 1500);
    await sleep(500); // every layer has left the document
    assert.ok(document.activeElement === document.querySelector(want), `and to stay there; it is on ${where()}`);
    await view.unmount();
  }
  await deleteFromDrawerMenu({ fromRow: true, named: false });
  await deleteFromDrawerMenu({ fromRow: true, named: true });
  await deleteFromDrawerMenu({ fromRow: false, named: false });
  await deleteFromDrawerMenu({ fromRow: false, named: true });
}

// ---- Controls that remove themselves hand the focus on --------------------
{
  const { ErrorState, Notice, Pagination } = await import('../js/components/surface.js');
  const where = () => {
    const at = document.activeElement;
    return at === document.body ? '<body>' : `<${at.localName}${at.id ? ` id="${at.id}"` : ''}${at.className ? ` class="${at.className}"` : ''}>`;
  };

  // ErrorState: a retry that works swaps it for the content, whose first
  // control gets the focus (the content is put in before the error state
  // leaves, which must not pass for what stood before it).
  {
    let setOk;
    function Host() {
      const [ok, set] = useState(false);
      setOk = set;
      return html`<main id="main" tabindex="-1"><button id="before">Filters</button>${ok ? html`<div class="content"><button id="first">Copy</button></div>` : html`<${ErrorState} error=${new ApiError(503, 'the gateway is not ready')} onRetry=${() => {}} />`}<button id="after">Next</button></main>`;
    }
    const view = mount(html`<${Host} />`);
    assert.equal(text(document.querySelector('.empty-desc')), 'The gateway is not ready.', 'the message is printed as a sentence');
    const retry = document.querySelector('.empty-action button');
    retry.focus();
    setOk(true);
    await until(() => document.activeElement === document.querySelector('#first'), "the focus to go to what took the error state's place");
    // A page that places the focus itself meanwhile is left alone.
    setOk(false);
    await until(() => document.querySelector('.empty-action button'));
    document.querySelector('.empty-action button').focus();
    setOk(true);
    setTimeout(() => document.querySelector('#before').focus(), 10);
    await sleep(200);
    assert.ok(document.activeElement === document.querySelector('#before'), `a focus the page placed wins; it is on ${where()}`);
    await view.unmount();
  }

  // Table: the retry keeps the error state (its button spinning, still
  // focused) until the rows are in; then the first row takes the focus.
  {
    let set;
    function Host() {
      const [state, setState] = useState({ error: new ApiError(503, 'down'), loading: false, rows: undefined });
      set = setState;
      return html`<main id="main" tabindex="-1"><${Table} columns=${[{ key: 'name', header: 'Name', primary: true }]} rows=${state.rows} loading=${state.loading} error=${state.error} onRetry=${() => {}} onRowClick=${() => {}} /></main>`;
    }
    const view = mount(html`<${Host} />`);
    const retry = await until(() => document.querySelector('.table-state-cell button'), 'the error state');
    retry.focus();
    set((s) => ({ ...s, loading: true }));
    await until(() => retry.getAttribute('aria-busy') === 'true', 'the retry button to spin');
    assert.ok(document.activeElement === retry, `the error state stays while retrying, with the focus; it is on ${where()}`);
    assert.equal(document.querySelectorAll('tbody tr[aria-hidden]').length, 0, 'no skeleton rows in its place');
    set({ error: null, loading: false, rows: [{ id: 'a', name: 'alpha' }, { id: 'b', name: 'beta' }] });
    await until(() => document.activeElement === document.querySelector('tbody tr[data-row-key="a"]'), `the first row to take the focus`);
    await view.unmount();
  }

  // Notice: an action that makes its notice go hands the focus to what
  // follows the notice.
  {
    let setShown;
    function Host() {
      const [shown, set] = useState(true);
      setShown = set;
      return html`<main id="main" tabindex="-1">${shown && html`<${Notice} tone="caution" title="Could not refresh" action=${html`<button id="again">Try again</button>`}>The rows are old.<//>`}<section><button id="live">Live</button></section></main>`;
    }
    const view = mount(html`<${Host} />`);
    document.querySelector('#again').focus();
    setShown(false);
    await until(() => document.activeElement === document.querySelector('#live'), `the focus to go to what follows the notice`);
    await view.unmount();
  }

  // LoadMore: when the last batch is in, the end-of-list line takes the focus.
  {
    let setMore;
    function Host() {
      const [more, set] = useState(true);
      setMore = set;
      return html`<main id="main" tabindex="-1"><${LoadMore} hasMore=${more} onLoad=${() => {}} shown=${120} noun="requests" /></main>`;
    }
    const view = mount(html`<${Host} />`);
    document.querySelector('.loadmore button').focus();
    setMore(false);
    await until(() => document.activeElement === document.querySelector('.loadmore-end'), `the end-of-list line to take the focus`);
    await view.unmount();
  }

  // Pagination: Next onto the last page keeps the focus (aria-disabled, not disabled).
  {
    const pages = [];
    function Host() {
      const [page, setPage] = useState(1);
      return html`<${Pagination} page=${page} pageSize=${10} total=${25} onPage=${(p) => (pages.push(p), setPage(p))} />`;
    }
    const view = mount(html`<${Host} />`);
    const byLabel = (label) => document.querySelectorAll('button').find((b) => b.getAttribute('aria-label') === label);
    const prev = byLabel('Previous page');
    const next = byLabel('Next page');
    assert.equal(prev.hasAttribute('disabled'), false, 'Previous on the first page is not `disabled`');
    assert.equal(prev.getAttribute('aria-disabled'), 'true');
    next.focus();
    next.click();
    await until(() => pages.length === 1);
    next.click();
    await until(() => next.getAttribute('aria-disabled') === 'true', 'Next to turn off on the last page');
    assert.equal(next.hasAttribute('disabled'), false, 'a disabled button would drop the focus on <body>');
    assert.ok(document.activeElement === next, `Next keeps the focus; it is on ${where()}`);
    next.click();
    await sleep(20);
    assert.deepEqual(pages, [2, 3], 'clicks on an end are ignored');
    await view.unmount();
  }

  // Toasts: closing the one that has the focus hands it to the next toast,
  // and the last one gives it back to where it was before.
  {
    const view = mount(html`<main id="main" tabindex="-1"><button id="before">Save</button></main><${Toaster} />`);
    await sleep(20); // the toaster starts listening for where the focus is
    document.querySelector('#before').focus();
    toast.info('One', { duration: 0 });
    toast.success('Two', { duration: 0, action: { label: 'Undo', onClick: () => {} } });
    await until(() => document.querySelectorAll('.toast').length === 2, 'two toasts');
    const dismiss = () => document.querySelectorAll('.toast [aria-label="Dismiss"]');
    dismiss()[0].focus();
    dismiss()[0].click();
    const undo = [...document.querySelectorAll('.toast button')].find((b) => text(b) === 'Undo');
    await until(() => document.activeElement === undo, `the next toast to take the focus`);
    dispatch(undo, 'keydown', { key: 'Escape' });
    await until(() => document.activeElement === document.querySelector('#before'), `the focus to go back to where it was before the toasts`);
    await until(() => !document.querySelector('.toast'), 'the toasts to leave');
    assert.ok(document.activeElement === document.querySelector('#before'));
    await view.unmount();
  }
}

// ---- Button: an aria-disabled mode that keeps the focus -------------------
{
  const clicks = [];
  function Host({ off }) {
    return html`<form onSubmit=${(event) => (event.preventDefault(), clicks.push('submit'))}><${Button} id="save" type="submit" aria-disabled=${off ? 'true' : undefined} onClick=${() => clicks.push('click')}>Save changes<//></form>`;
  }
  const view = mount(html`<${Host} off=${true} />`);
  const save = document.querySelector('#save');
  save.focus();
  const press = dispatch(save, 'click', { detail: 0 });
  assert.equal(press.defaultPrevented, true, 'the click (and so the submit it would cause) is cancelled');
  assert.deepEqual(clicks, [], 'onClick is not called');
  assert.equal(save.hasAttribute('disabled'), false);
  assert.equal(save.getAttribute('aria-disabled'), 'true');
  assert.ok(document.activeElement === save, 'it keeps the focus');
  render(html`<${Host} off=${false} />`, view.root);
  save.click();
  assert.deepEqual(clicks, ['click']);
  assert.equal(save.hasAttribute('aria-disabled'), false);
  await view.unmount();
}

// ---- Forms: no rebuild when a message appears, no clamp-and-submit --------
{
  // A control without a label that gains a warning with the first character
  // typed keeps its element, the focus and the text.
  function Grows() {
    const [v, setV] = useState('');
    const [n, setN] = useState(null);
    return html`
      <${Input} id="grows" value=${v} onChange=${setV} warning=${v ? 'Keys this short are easy to guess.' : undefined} />
      <${NumberInput} id="count" value=${n} onChange=${setN} min=${0} hint=${n != null ? 'per minute' : undefined} />
    `;
  }
  const view = mount(html`<${Grows} />`);
  const field = document.querySelector('#grows');
  field.focus();
  field.value = 'a';
  dispatch(field, 'input');
  await until(() => document.querySelector('.field-warning'), 'the warning');
  assert.ok(document.querySelector('#grows') === field, 'the input was rebuilt when the warning appeared');
  assert.ok(document.activeElement === field, 'and the focus was lost with it');
  assert.equal(field.value, 'a');
  assert.equal(field.getAttribute('aria-describedby'), document.querySelector('.field-warning').getAttribute('id'));
  const count = document.querySelector('#count');
  count.focus();
  count.value = '5';
  dispatch(count, 'input');
  await until(() => document.querySelector('.field-hint'), 'the hint');
  assert.ok(document.querySelector('#count') === count && document.activeElement === count, 'a NumberInput is not rebuilt either');
  assert.ok(field.closest('.field') && !field.closest('.field').hasAttribute('data-bare'), 'with a message the field is a box');
  field.value = '';
  dispatch(field, 'input');
  await until(() => field.closest('.field').hasAttribute('data-bare'), 'without one it is bare (display: contents)');
  assert.ok(document.querySelector('#grows') === field);
  await view.unmount();

  // Typing 0 in a min-1 field and pressing Save: the blur that the press
  // causes does not clamp to 1 and let the same click save it.
  const saved = [];
  function Weight() {
    const [v, setV] = useState(5);
    return html`<${Form} onSubmit=${() => saved.push(v)}>
      <${NumberInput} id="weight" label="Weight" value=${v} onChange=${setV} min=${1} max=${100} />
      <${Button} id="save" type="submit" variant="primary">Save<//>
    <//>`;
  }
  const form = mount(html`<${Weight} />`);
  const weight = document.querySelector('#weight');
  const save = document.querySelector('#save');
  weight.focus();
  weight.value = '0';
  dispatch(weight, 'input');
  await until(() => weight.hasAttribute('data-entry-invalid'), 'the field to be marked');
  dispatch(save, 'pointerdown');
  save.focus(); // the press moves the focus: the field blurs
  await sleep(20);
  assert.equal(weight.value, '0', 'the press of Save clamped the field');
  assert.ok(weight.hasAttribute('data-entry-invalid'), 'the field keeps its mark');
  dispatch(document.querySelector('form'), 'submit');
  assert.deepEqual(saved, [], 'the form refused the submit');
  assert.ok(document.activeElement === weight, 'and gave the field the focus');
  // Leaving it any other way still settles it.
  dispatch(document.body, 'pointerdown');
  await sleep(5);
  weight.blur();
  await until(() => weight.value === '1', 'an ordinary blur to clamp');
  dispatch(document.querySelector('form'), 'submit');
  assert.deepEqual(saved, [1]);
  await form.unmount();
}

// ---- Forms: FormError lists what is not already said ----------------------
{
  // A 422 message quotes its first three issues; the list shows the rest.
  const issues = [
    { path: 'providers[0].name', message: 'is required' },
    { path: 'server.port', message: 'must be between 1 and 65535' },
    { path: 'routing.max_attempts', message: 'must be 1 or more' },
    { path: 'admin.secret', message: 'is too short' },
  ];
  const error = new ApiError(422, 'the configuration is not valid: providers[0].name: is required; server.port: must be between 1 and 65535; routing.max_attempts: must be 1 or more (and 1 more)', { issues });
  const view = mount(html`<${FormError} error=${error} />`);
  const items = () => document.querySelectorAll('.issue-list li').map((li) => text(li));
  assert.deepEqual(items(), ['admin.secretis too short'], 'only the issue the message does not quote is listed');
  await view.unmount();

  // A 409 for a broken file quotes the file's issue: nothing to list.
  const disk = new ApiError(409, 'the configuration file on disk is not valid, so the change was not saved; fix or restore the file. What is wrong with the file: line 3, column 9: invalid number', { issues: [{ path: 'line 3, column 9', message: 'invalid number' }] });
  const conflict = mount(html`<${FormError} error=${disk} />`);
  assert.equal(document.querySelector('.issue-list'), null);
  await conflict.unmount();

  // Claims last one render: a field that stops asking for its issue gives it back.
  const taken = new ApiError(409, 'a key named `build-bot` already exists', { issues: [{ path: 'name', message: 'is already used by another key' }] });
  let setEdited;
  function Host() {
    const [edited, set] = useState(false);
    setEdited = set;
    const found = useIssues(taken);
    return html`<${Input} label="Name" value="" error=${edited ? undefined : found.at('name')} /><${FormError} error=${taken} issues=${found} />`;
  }
  const claims = mount(html`<${Host} />`);
  assert.match(text(document.querySelector('.notice-text')), /Check the highlighted field\./);
  assert.equal(document.querySelector('.issue-list'), null);
  setEdited(true);
  await until(() => document.querySelector('.issue-list'), 'the issue to be listed once its field no longer shows it');
  assert.doesNotMatch(text(document.querySelector('.notice-text')), /highlighted/, 'nothing is highlighted any more');
  assert.deepEqual(document.querySelectorAll('.issue-list li').map((li) => text(li)), ['nameis already used by another key']);
  await claims.unmount();

  // The error notice of a dialog: any Error, as a sentence.
  const plain = mount(html`<${FormError} error=${new Error('the key is in use by a running request')} title="That did not work" />`);
  assert.equal(text(document.querySelector('.notice-text')), 'The key is in use by a running request.');
  await plain.unmount();

  // ConfirmDialog uses the same issue rendering, not a message-only notice.
  const dialog = mount(html`<${ConfirmDialog} open title="Remove entry?" confirmLabel="Remove"
    onConfirm=${async () => { throw new ApiError(422, 'entry cannot be removed', { issues: [{ path: 'entry', message: 'is in use' }] }); }} />`);
  const remove = await until(() => [...document.querySelectorAll('.modal button')].find((b) => text(b) === 'Remove'));
  await sleep(20); // the dialog's opening effect clears any previous error
  remove.click();
  await until(() => document.querySelector('.modal .issue-list'), 'the confirmation error issues');
  assert.match(text(document.querySelector('.modal .notice-text')), /^Entry cannot be removed\./);
  assert.equal(text(document.querySelector('.modal .issue-list')), 'entryis in use');
  await dialog.unmount();
}

// ---- Command palette: a command runs after focus is back -----------------
{
  let setOpen;
  const ran = [];
  const commands = [
    { id: 'focus', label: 'Jump to the search field', group: 'Actions', run: () => (ran.push('focus'), document.querySelector('#search').focus()) },
    { id: 'other', label: 'Something else', group: 'Actions', run: () => ran.push('other') },
  ];
  function Host() {
    const [open, set] = useState(false);
    setOpen = set;
    return html`
      <button id="trigger">Jump to…</button>
      <input id="search" />
      <${CommandPalette} open=${open} onClose=${() => set(false)} commands=${commands} />
    `;
  }
  const view = mount(html`<${Host} />`);
  document.querySelector('#trigger').focus();
  setOpen(true);
  const field = await until(() => document.querySelector('.palette input'), 'the palette');
  await until(() => document.activeElement === field, 'focus in the palette');
  dispatch(field, 'keydown', { key: 'Enter' });
  await until(() => ran.length === 1 && !document.querySelector('.palette'), 'the command to run and the palette to close');
  await sleep(30);
  assert.equal(document.activeElement, document.querySelector('#search'), 'the focus a command set must survive the palette giving focus back to where it was');

  // Closing without choosing runs nothing, now or at the next close.
  document.querySelector('#trigger').focus();
  setOpen(true);
  const again = await until(() => document.querySelector('.palette input'));
  await until(() => document.activeElement === again);
  dispatch(again, 'keydown', { key: 'Escape' });
  await until(() => !document.querySelector('.palette'), 'Escape to close the palette');
  await sleep(30);
  assert.deepEqual(ran, ['focus']);
  assert.equal(document.activeElement, document.querySelector('#trigger'), 'and focus returns to where it was');
  await view.unmount();
}

// ---- Button: loading keeps the keyboard focus ------------------------------
{
  const clicks = [];
  function Host({ loading }) {
    return html`
      <${Button} id="save" type="submit" variant="primary" loading=${loading} onClick=${() => clicks.push('save')}>Save provider<//>
      <${Button} id="off" disabled onClick=${() => clicks.push('off')}>Disabled<//>
      <${IconButton} id="reveal" icon="eye" label="Show" loading=${loading} tooltip=${false} onClick=${() => clicks.push('reveal')} />
    `;
  }
  const view = mount(html`<${Host} loading=${false} />`);
  const save = document.querySelector('#save');
  save.focus();
  save.click();
  assert.deepEqual(clicks, ['save']);
  render(html`<${Host} loading=${true} />`, view.root);
  assert.equal(save.hasAttribute('disabled'), false, 'a loading button is not `disabled`: that drops the focus on <body>');
  assert.equal(save.getAttribute('aria-disabled'), 'true');
  assert.equal(save.getAttribute('aria-busy'), 'true');
  assert.equal(document.activeElement, save, 'the button keeps the focus while it works');
  const press = dispatch(save, 'click', { detail: 0 });
  assert.equal(press.defaultPrevented, true, 'a click on a loading submit button does not submit the form again');
  dispatch(document.querySelector('#reveal'), 'click');
  assert.deepEqual(clicks, ['save'], 'clicks are ignored while loading');
  assert.ok(document.querySelector('#off').hasAttribute('disabled'), 'a disabled button is still really disabled');
  render(html`<${Host} loading=${false} />`, view.root);
  assert.equal(save.hasAttribute('aria-disabled'), false);
  save.click();
  assert.deepEqual(clicks, ['save', 'save']);
  await view.unmount();
}

// ---- Table: labels, the sort menu, row keys, and rows that stay put -------
{
  const drawn = new Map();
  const count = (row) => drawn.set(row.id, (drawn.get(row.id) ?? 0) + 1);
  const columnsOf = () => [
    { key: 'name', header: html`<abbr title="Provider">Prov.</abbr>`, label: 'Provider', primary: true, sortable: true, render: (row) => (count(row), row.name) },
    { key: 'requests', header: 'Requests', sortable: true, num: true, align: 'right' },
    { key: 'actions', header: html`<span class="sr-only">Actions</span>`, label: 'Actions', render: () => 'menu' },
    { key: 'bare', header: html`<span>no label</span>`, render: () => 'x' },
  ];
  const rowsOf = () => [
    { id: 'a', name: 'alpha', requests: 3 },
    { id: 'b', name: 'beta', requests: 9 },
    { id: 'c', name: 'gamma', requests: 1 },
  ];
  let state;
  const clicked = [];
  function Host() {
    const [value, set] = useState({ columns: columnsOf(), rows: rowsOf(), selected: null, fresh: null, tick: 0, sortMenu: true, error: null });
    state = { value, set: (patch) => set((was) => ({ ...was, ...patch })) };
    // A new handler on every render, as pages write it.
    return html`<${Table}
      columns=${value.columns}
      rows=${value.error ? [] : value.rows}
      error=${value.error}
      errorTitle="Could not load the providers"
      selectedKey=${value.selected}
      freshKeys=${value.fresh}
      sortMenu=${value.sortMenu}
      onRowClick=${(row) => clicked.push(`${row.id}@${value.tick}`)}
    />`;
  }
  const view = mount(html`<${Host} />`);
  const bodyRows = () => document.querySelectorAll('tbody tr');
  const names = () => bodyRows().map((tr) => text(tr.querySelector('td')));
  await until(() => bodyRows().length === 3);

  // Headers may be markup; the plain name comes from `label`.
  const cells = bodyRows()[0].querySelectorAll('td');
  assert.deepEqual(cells.map((td) => td.getAttribute('data-label')), ['Provider', 'Requests', 'Actions', ''], 'data-label is the label, or a text header, and never "[object Object]"');
  assert.equal(text(document.querySelector('th abbr')), 'Prov.', 'the markup header is rendered as markup');
  assert.deepEqual(bodyRows().map((tr) => tr.getAttribute('data-row-key')), ['a', 'b', 'c'], 'each row carries its key');

  // The phone sort menu names columns by label and always offers the default order.
  const select = document.querySelector('.table-sortbar select');
  const options = () => select.querySelectorAll('option').map((o) => `${o.value}|${text(o)}`);
  assert.deepEqual(options(), ['|Default order', 'name:asc|Provider, ascending', 'name:desc|Provider, descending', 'requests:asc|Requests, ascending', 'requests:desc|Requests, descending']);
  select.value = 'requests:desc';
  dispatch(select, 'change');
  await until(() => names().join() === 'beta,alpha,gamma', 'the rows to sort');
  assert.equal(options()[0], '|Default order', 'the default order is still on offer once a sort is chosen');
  select.value = '';
  dispatch(select, 'change');
  await until(() => names().join() === 'alpha,beta,gamma', 'the default order to come back');

  // Rows are memoised: selecting one renders that one, not all of them.
  const before = () => new Map(drawn);
  let was = before();
  const redrawn = () => [...drawn].filter(([id, n]) => n !== was.get(id)).map(([id]) => id).sort();
  state.set({ selected: 'b' });
  await until(() => bodyRows()[1].hasAttribute('data-selected'), 'the selected row to be marked');
  assert.deepEqual(redrawn(), ['b'], 'only the newly selected row rendered');
  was = before();
  state.set({ selected: 'c' });
  await until(() => bodyRows()[2].hasAttribute('data-selected'));
  assert.equal(bodyRows()[1].hasAttribute('data-selected'), false);
  assert.deepEqual(redrawn(), ['b', 'c'], 'the row that lost the mark and the row that gained it');
  was = before();
  state.set({ fresh: new Set(['a']) });
  await until(() => bodyRows()[0].hasAttribute('data-fresh'), 'the fresh row to be marked');
  assert.deepEqual(redrawn(), ['a']);
  was = before();
  state.set({ tick: 1 });
  await sleep(20);
  assert.deepEqual(redrawn(), [], 'a render of the parent that changes nothing for the rows renders none');
  bodyRows()[0].click();
  assert.deepEqual(clicked, ['a@1'], 'and a row still calls the handler of the latest render');
  // A replaced record renders its row; so do new columns, for every row.
  state.set({ rows: state.value.rows.map((row) => (row.id === 'b' ? { ...row, name: 'beta-2' } : row)) });
  await until(() => names()[1] === 'beta-2', 'the replaced record to show');
  assert.deepEqual(redrawn(), ['b']);
  was = before();
  state.set({ columns: columnsOf() });
  await until(() => redrawn().length === 3, 'new columns to render every row');
  // A cell that takes the row's position follows it when rows move.
  was = before();
  const positions = [];
  state.set({ columns: [{ key: 'name', header: 'Provider', render: (row, index) => (positions.push(`${row.id}:${index}`), `${index + 1}. ${row.name}`) }] });
  await until(() => names()[0] === '1. alpha');
  state.set({ rows: [{ id: 'new', name: 'first' }, ...state.value.rows] });
  await until(() => names().join('|') === '1. first|2. alpha|3. beta-2|4. gamma', 'positions to follow a row put in front');

  // sortMenu=${false} leaves the menu out; the error state can be named.
  state.set({ columns: columnsOf(), sortMenu: false });
  await until(() => !document.querySelector('.table-sortbar'), 'the sort menu to go');
  state.set({ error: new ApiError(503, 'the gateway is not ready') });
  await until(() => document.querySelector('.table-state-cell h3'), 'the error state');
  assert.equal(text(document.querySelector('.table-state-cell h3')), 'Could not load the providers');
  await view.unmount();
}

// ---- Forms: FormError as sentences, TagInput paste, caution messages ------
{
  // The gateway's message has no capital and no full stop; a sentence follows it.
  const error = new ApiError(422, 'the provider could not be saved', { issues: [{ path: 'base_url', message: 'must not be empty' }, { path: 'models[0]', message: 'unknown model' }] });
  function ErrorHost() {
    const issues = useIssues(error);
    issues.at('base_url');
    return html`<${FormError} error=${error} issues=${issues} title="Could not save the provider" />`;
  }
  const view = mount(html`<${ErrorHost} />`);
  assert.match(text(document.querySelector('.notice-text')), /^The provider could not be saved\. Check the highlighted field\./);
  await view.unmount();
  const plain = mount(html`<${FormError} error=${new ApiError(409, 'A provider named `openai` already exists.')} />`);
  assert.equal(text(document.querySelector('.notice-text')), 'A provider named `openai` already exists.', 'a message that is a sentence already is left alone');
  await plain.unmount();

  // TagInput: pasting entries that are all in the list already leaves nothing behind.
  const changes = [];
  function Tags() {
    const [value, setValue] = useState(['gpt-4o', 'gpt-4o-mini']);
    return html`<${TagInput} label="Models" value=${value} onChange=${(next) => (changes.push(next), setValue(next))} />`;
  }
  const tags = mount(html`<${Tags} />`);
  const field = document.querySelector('input');
  field.value = 'gpt-4o, GPT-4o-mini';
  dispatch(field, 'input');
  await sleep(10);
  assert.deepEqual(changes, [], 'nothing was added');
  assert.equal(field.value, '', 'and the pasted text does not stay in the field');
  field.value = 'gpt-4o, o3';
  dispatch(field, 'input');
  await until(() => changes.length === 1);
  assert.deepEqual(changes[0], ['gpt-4o', 'gpt-4o-mini', 'o3']);
  assert.equal(field.value, '');
  await tags.unmount();

  // A caution about a value that is allowed: its own line, tied to the control.
  function Caution({ error: failure }) {
    return html`
      <${Input} id="key" label="Key" value="abc" hint="Leave empty to have one generated." warning="Keys this short are easy to guess." error=${failure} />
      <${Field} label="Strategy" warning="Round robin ignores the weights below."><div class="custom"></div><//>
    `;
  }
  const caution = mount(html`<${Caution} />`);
  const key = document.querySelector('#key');
  const warning = document.querySelector('.field-warning');
  assert.match(text(warning), /Keys this short/);
  assert.equal(key.getAttribute('aria-describedby'), warning.getAttribute('id'), 'the control is described by the warning');
  assert.equal(key.hasAttribute('aria-invalid'), false, 'a warning does not make the value invalid');
  assert.ok(key.parentNode.hasAttribute('data-warning'));
  assert.equal(document.querySelectorAll('.field-hint').length, 0, 'the warning takes the hint\'s place');
  assert.equal(document.querySelectorAll('.field-warning').length, 2, 'Field takes a warning for a custom control too');
  render(html`<${Caution} error="Enter a key." />`, caution.root);
  assert.equal(key.getAttribute('aria-invalid'), 'true');
  assert.match(text(document.querySelector('.field-error')), /Enter a key\./);
  assert.equal(key.parentNode.hasAttribute('data-warning'), false, 'an error takes the place of the warning');
  await caution.unmount();
}

// ---- Input: a search field can be cleared without the browser's own x -----
{
  // base.css hides the native clear button of type="search" (it doubled the
  // one two pages had added). The fields that relied on it get one from the
  // component, unless the page brings its own actions.
  const seen = [];
  let cleared = 0;
  function Host({ disabled = false }) {
    const [q, setQ] = useState('');
    const [own, setOwn] = useState('abc');
    return html`
      <${Input} id="plain" type="search" value=${q} onChange=${(value) => (seen.push(value), setQ(value))} onClear=${() => (cleared += 1)} disabled=${disabled} aria-label="Search keys" />
      <${Input} id="own" type="search" value=${own} onChange=${setOwn} aria-label="Search logs" actions=${own ? html`<${IconButton} icon="x" label="Clear search" onClick=${() => setOwn('')} />` : null} />
      <${Input} id="text" value="abc" onChange=${() => {}} aria-label="Name" />
      <${Input} id="asked" value="abc" onChange=${() => {}} clearable clearLabel="Clear the name" aria-label="Name" />
      <${Input} id="refused" type="search" value="abc" onChange=${() => {}} clearable=${false} aria-label="Filter" />
      <${Input} id="loose" type="search" aria-label="Filter" />
    `;
  }
  const view = mount(html`<${Host} />`);
  const buttons = (id) => [...document.querySelector(`#${id}`).parentNode.querySelectorAll('button')].map((button) => button.getAttribute('aria-label'));
  assert.deepEqual(buttons('plain'), [], 'nothing to clear in an empty field');
  assert.deepEqual(buttons('own'), ['Clear search'], 'a page that brings its own action gets no second one');
  assert.deepEqual(buttons('text'), [], 'only search fields have it by default');
  assert.deepEqual(buttons('asked'), ['Clear the name'], 'clearable asks for it on any field');
  assert.deepEqual(buttons('refused'), [], 'and clearable=false leaves it out');

  const plain = document.querySelector('#plain');
  plain.value = 'build';
  dispatch(plain, 'input');
  await until(() => buttons('plain').length === 1, 'the clear button to appear once there is text');
  assert.deepEqual(buttons('plain'), ['Clear']);
  const clear = plain.parentNode.querySelector('button');
  assert.equal(dispatch(clear, 'mousedown').defaultPrevented, true, 'pressing the button does not take the focus out of the field');
  clear.click();
  await until(() => buttons('plain').length === 0, 'the button to go with the text');
  assert.equal(seen[seen.length - 1], '', 'onChange got the empty value');
  assert.equal(plain.value, '');
  assert.equal(cleared, 1, 'onClear ran once');
  assert.ok(document.activeElement === plain, 'the focus is back in the field');

  plain.value = 'build';
  dispatch(plain, 'input');
  await until(() => buttons('plain').length === 1);
  render(html`<${Host} disabled />`, view.root);
  assert.deepEqual(buttons('plain'), [], 'a disabled field offers nothing');

  // A field nobody controls keeps what was typed, and can be cleared too.
  const loose = document.querySelector('#loose');
  loose.value = 'gpt';
  dispatch(loose, 'input');
  await until(() => buttons('loose').length === 1, 'an uncontrolled search field to offer the button');
  assert.equal(loose.value, 'gpt', 'the typed text survives the render');
  loose.parentNode.querySelector('button').click();
  await until(() => buttons('loose').length === 0);
  assert.equal(loose.value, '');
  await view.unmount();
}

// ---- Tabs: the selected tab is brought into view inside the strip ---------
{
  const tabs = ['general', 'routing', 'streaming', 'logging', 'payload', 'pricing'].map((id) => ({ id, label: id }));
  const view = mount(html`<${Tabs} label="Settings" tabs=${tabs} value="general" onChange=${() => {}} />`);
  const strip = document.querySelector('[role="tablist"]');
  // A 200px strip holding six 100px tabs.
  strip.clientWidth = 200;
  strip.scrollWidth = 600;
  strip.getBoundingClientRect = () => ({ left: 0, right: 200, width: 200 });
  let scrolledIntoView = 0;
  strip.querySelectorAll('[role="tab"]').forEach((tab, i) => {
    tab.getBoundingClientRect = () => ({ left: i * 100 - strip.scrollLeft, right: i * 100 + 100 - strip.scrollLeft, width: 100 });
    tab.scrollIntoView = () => (scrolledIntoView += 1);
  });
  let pageScrolls = 0;
  window.scrollTo = () => (pageScrolls += 1);

  render(html`<${Tabs} label="Settings" tabs=${tabs} value="payload" onChange=${() => {}} />`, view.root);
  // "payload" spans 400..500: its right edge comes to the strip's, less the fade.
  assert.equal(strip.scrollLeft, 328, 'the strip scrolled to show the selected tab');
  assert.ok(strip.hasAttribute('data-more-start') && strip.hasAttribute('data-more-end'), 'both edges fade: tabs are hidden on either side');
  render(html`<${Tabs} label="Settings" tabs=${tabs} value="pricing" onChange=${() => {}} />`, view.root);
  strip.scrollLeft = Math.min(strip.scrollLeft, 400); // a browser clamps to the end
  dispatch(strip, 'scroll', { bubbles: false });
  assert.equal(strip.hasAttribute('data-more-end'), false, 'no fade at the end once the last tab is in view');
  assert.ok(strip.hasAttribute('data-more-start'));
  render(html`<${Tabs} label="Settings" tabs=${tabs} value="general" onChange=${() => {}} />`, view.root);
  assert.ok(strip.scrollLeft <= 0, 'and back to the start for the first tab');
  strip.scrollLeft = 0;
  dispatch(strip, 'scroll', { bubbles: false });
  assert.equal(strip.hasAttribute('data-more-start'), false);
  assert.ok(strip.hasAttribute('data-more-end'));
  assert.equal(scrolledIntoView, 0, 'scrollIntoView is not used: it scrolls the page as well');
  assert.equal(pageScrolls, 0, 'the page was not scrolled');
  window.scrollTo = () => {};
  await view.unmount();
}

// ---- Menu: only scrolling that moves its anchor closes it -----------------
{
  const view = mount(html`
    <div id="list"><${Menu} label="Row actions" items=${[{ label: 'Edit', onSelect: () => {} }]} /></div>
    <div id="log-tail"></div>
  `);
  const trigger = document.querySelector('button');
  const expanded = () => trigger.getAttribute('aria-expanded') === 'true';
  trigger.click();
  await until(() => expanded() && document.querySelector('.menu'), 'the menu to open');
  await sleep(20);
  dispatch(document.querySelector('#log-tail'), 'scroll', { bubbles: false });
  await sleep(20);
  assert.ok(expanded(), 'a list scrolling elsewhere on the page does not close the menu');
  dispatch(document.querySelector('.menu'), 'scroll', { bubbles: false });
  await sleep(20);
  assert.ok(expanded(), 'nor does scrolling the menu itself');
  dispatch(document.querySelector('#list'), 'scroll', { bubbles: false });
  await until(() => !expanded(), 'scrolling a box that holds the trigger to close the menu');
  await until(() => !document.querySelector('.menu'));
  trigger.click();
  await until(expanded);
  await sleep(20);
  dispatch(document, 'scroll', { bubbles: false });
  await until(() => !expanded(), 'scrolling the page to close the menu');
  await view.unmount();
}

// ---- Surfaces and code: names for assistive technology -------------------
{
  const view = mount(html`
    <${CodeBlock} class="a" title="Upstream request" value="{}" />
    <${CodeBlock} class="b" title=${html`<span>Body <b>2 KB</b></span>`} value="{}" />
    <${CodeBlock} class="c" title=${html`<span>Body</span>`} label="Response body" value="{}" />
    <${CodeBlock} class="d" value="{}" />
    <${StatGroup} label="Traffic" data-stale="" id="vitals">
      <${Stat} class="s1" label="Gateway" value="Degraded" lamp="caution" lampLabel="Degraded" />
      <${Stat} class="s2" label="Error rate" value="44%" lamp="stop" hint="44 of 100 failed" />
      <${Stat} class="s3" label="State" value="OK" lamp="clear" />
      <${Stat} class="s4" label="Gateway" value="Degraded" lamp="caution" lampLabel=${false} hint="2 of 5 providers are down" />
    <//>
    <${Timeline} items=${[{ tone: 'stop', title: 'openai · key 1' }, { tone: 'clear', toneLabel: 'Answered', title: 'openai · key 2' }]} />
    <${Panel} class="p1" title="Credentials">rows<//>
    <${Panel} class="p2" title="Credentials" aria-label="Credentials of openai">rows<//>
    <${Panel} class="p3">no title<//>
  `);
  const label = (selector) => document.querySelector(selector).getAttribute('aria-label');
  assert.equal(label('.a pre'), 'Upstream request');
  assert.equal(label('.b pre'), 'Code', 'a title that is markup is not stringified into the name');
  assert.equal(label('.c pre'), 'Response body');
  assert.equal(label('.d pre'), 'Code');

  const group = document.querySelector('.stat-group');
  assert.ok(group.hasAttribute('data-stale'), 'StatGroup passes unknown props to its element');
  assert.equal(group.getAttribute('id'), 'vitals');
  assert.equal(group.getAttribute('aria-label'), 'Traffic');
  assert.equal(label('.s1 .lamp'), 'Degraded', 'lampLabel names the lamp');
  // A hint gives counts, the lamp gives the judgement: the lamp keeps a name
  // and a tooltip, in a word a person would say, not the tone's own name.
  const judged = document.querySelector('.s2 .lamp');
  assert.equal(judged.getAttribute('role'), 'img', 'a hint does not hide the lamp');
  assert.equal(judged.hasAttribute('aria-hidden'), false);
  assert.equal(judged.getAttribute('aria-label'), 'Critical');
  assert.equal(judged.getAttribute('title'), 'Critical');
  assert.equal(label('.s3 .lamp'), 'Healthy', 'without lampLabel the lamp is named by a plain word for its tone');
  const decoration = document.querySelector('.s4 .lamp');
  assert.equal(decoration.getAttribute('aria-hidden'), 'true', 'lampLabel=false: the page says the words are next to it, the lamp is decoration');
  assert.equal(decoration.hasAttribute('aria-label'), false);
  assert.equal(decoration.hasAttribute('role'), false);
  assert.equal(toneWord('caution'), 'Warning');
  assert.equal(toneWord('no-such-tone'), toneWord('off'));
  const nodes = document.querySelectorAll('.timeline-node .lamp');
  assert.equal(nodes[0].getAttribute('aria-label'), 'Critical', 'a timeline lamp is named in a word too');
  assert.equal(nodes[1].getAttribute('aria-label'), 'Answered', 'or by the item\'s toneLabel');

  // StatusLamp and Badge pass what they do not know to their root element,
  // as the guide says the simple components do.
  const marks = mount(html`
    <${StatusLamp} tone="clear" title="Serving" class="bare" data-tip="2 of 2 credentials ready" id="lamp-1" />
    <${StatusLamp} tone="caution" label="Cooling down" class="worded" data-tip="41s left" />
    <${Badge} tone="stop" mono class="code" data-tip="Upstream answered 502" aria-describedby="why">502<//>
  `);
  const bare = document.querySelector('#lamp-1');
  assert.ok(bare.matches('.lamp.bare'), 'a lamp without a label is its own root: class and id land on it');
  assert.equal(bare.getAttribute('data-tip'), '2 of 2 credentials ready');
  assert.equal(bare.getAttribute('aria-label'), 'Serving');
  const worded = document.querySelector('.status.worded');
  assert.equal(worded.getAttribute('data-tip'), '41s left', 'with a label the wrapper is the root');
  assert.equal(worded.querySelector('.lamp').hasAttribute('data-tip'), false);
  const code = document.querySelector('.badge.code');
  assert.equal(code.getAttribute('data-tip'), 'Upstream answered 502');
  assert.equal(code.getAttribute('aria-describedby'), 'why');
  assert.equal(text(code), '502');
  await marks.unmount();

  const titled = document.querySelector('.p1');
  assert.equal(titled.getAttribute('aria-labelledby'), titled.querySelector('h2').getAttribute('id'), 'a panel is labelled by its title');
  assert.ok(titled.querySelector('h2').getAttribute('id'));
  assert.equal(document.querySelector('.p2').hasAttribute('aria-labelledby'), false, 'a name the page gave is kept');
  assert.equal(document.querySelector('.p2').getAttribute('aria-label'), 'Credentials of openai');
  assert.equal(document.querySelector('.p3').hasAttribute('aria-labelledby'), false);
  await view.unmount();
}

// ---- Charts: UTC, whole-number ticks, quiet tooltips, long legends --------
{
  const DAY = 86_400_000;
  const days = Array.from({ length: 30 }, (_, i) => Date.UTC(2026, 8, 3) + i * DAY); // 3 Sep .. 2 Oct, cut at UTC midnight
  const seven = ['gpt-4o', 'gpt-4o-mini', 'o3', 'claude', 'gemini', 'mock-echo', 'an-extremely-long-model-name-that-goes-on-and-on'].map((key, s) => ({
    key,
    label: key,
    values: days.map((_, j) => (j === days.length - 1 ? (s === 1 ? 1 : s === 3 ? 0 : s === 5 ? null : 0) : s)),
  }));
  const ticksOf = (root) => root.querySelectorAll('svg text').map((node) => text(node));

  const view = mount(html`
    <div class="utc"><${BarChart} utc integer x=${days} series=${seven} label="Requests per day" /></div>
    <div class="one"><${LineChart} integer x=${['a', 'b', 'c']} series=${[{ key: 's', label: 'Errors', values: [0, 1, 0] }]} label="Errors" /></div>
    <div class="half"><${LineChart} x=${['a', 'b', 'c']} series=${[{ key: 's', label: 'Rate', values: [0, 1, 0] }]} label="Rate" /></div>
    <div class="custom"><${LineChart} x=${['a', 'b']} tipFormat=${(v) => `Bucket ${v}`} xLabel="Bucket" series=${[{ key: 's', label: 'S', values: [1, 2] }]} label="Custom" /></div>
  `);
  const utc = await until(() => document.querySelector('.utc .chart-plot svg') && document.querySelector('.utc'), 'the charts to draw');

  // A UTC day bucket keeps its date whatever the time zone of the machine.
  const ticks = ticksOf(utc);
  assert.ok(ticks.includes('3 Sep') && ticks.includes('2 Oct'), `UTC dates on the axis, got ${ticks.join(' | ')}`);
  // The readout: keyboard focus shows the last bucket.
  const plot = utc.querySelector('.chart-plot');
  plot.focus();
  const tip = await until(() => utc.querySelector('.chart-tip'), 'the tooltip');
  assert.equal(text(tip.querySelector('.chart-tip-head')), '2 Oct UTC', 'the tooltip says the date is UTC');
  const tipLabels = tip.querySelectorAll('.chart-tip-row').map((row) => text(row.querySelector('.chart-tip-label')));
  assert.deepEqual(tipLabels, ['gpt-4o-mini', 'Total'], 'with seven series, the ones at zero (or with no value) are left out of the tooltip');
  // The table view carries the same dates, and says UTC once, in the heading.
  utc.querySelectorAll('button').find((button) => button.getAttribute('aria-label') === 'Show as table').click();
  await until(() => utc.querySelector('.chart-table-wrap'), 'the table view');
  assert.equal(text(utc.querySelector('.chart-table-wrap th')), 'Time (UTC)');
  const lastRow = utc.querySelectorAll('.chart-table-wrap tbody tr').at(-1);
  assert.equal(text(lastRow.querySelector('td')), '2 Oct 00:00:00');
  // A long series name is cut by CSS: it needs its own element, and the full name stays reachable.
  const long = utc.querySelectorAll('.chart-legend li').at(-1);
  assert.equal(text(long.querySelector('.chart-legend-label')), 'an-extremely-long-model-name-that-goes-on-and-on');
  assert.equal(long.getAttribute('title'), 'an-extremely-long-model-name-that-goes-on-and-on');

  // Whole-number ticks: a maximum of 1 has no 0.5.
  assert.ok(!ticksOf(document.querySelector('.one')).includes('0.5'), 'an axis that counts has no 0.5 tick');
  assert.ok(ticksOf(document.querySelector('.one')).includes('1'));
  assert.ok(ticksOf(document.querySelector('.half')).includes('0.5'), 'without `integer` the scale is as it was');

  // tipFormat: the tooltip head and the table's first column.
  const custom = document.querySelector('.custom');
  custom.querySelector('.chart-plot').focus();
  await until(() => custom.querySelector('.chart-tip'));
  assert.equal(text(custom.querySelector('.chart-tip-head')), 'Bucket b');
  // Few series: every one is listed, zero or not.
  const one = document.querySelector('.one');
  one.querySelector('.chart-plot').focus();
  await until(() => one.querySelector('.chart-tip'));
  assert.equal(one.querySelectorAll('.chart-tip-row').length, 1);
  await view.unmount();

  const small = mount(html`
    <span class="h1"><${HealthStrip} label="openai" noun="upstream attempts" buckets=${[{ ok: 13, failed: 5 }]} /></span>
    <span class="h2"><${HealthStrip} label="openai" buckets=${[{ ok: 9, failed: 1 }]} /></span>
    <span class="sp1"><${Sparkline} data=${[1, 4, 2, 6]} /></span>
    <span class="sp2"><${Sparkline} data=${[...Array.from({ length: 58 }, () => null), 3, 4]} /></span>
    <span class="sp3"><${Sparkline} data=${[1, 4, 2]} minPoints=${4} /></span>
    <span class="sp4"><${Sparkline} data=${[5]} /></span>
    <span class="sp5"><${Sparkline} data=${[2, 3]} /></span>
  `);
  assert.equal(document.querySelector('.h1 .health').getAttribute('aria-label'), 'openai: 72.2% of 18 recent upstream attempts succeeded');
  assert.equal(document.querySelector('.h2 .health').getAttribute('aria-label'), 'openai: 90% of 10 recent requests succeeded');
  const marks = (selector) => document.querySelector(selector).querySelectorAll('path, circle').length;
  assert.ok(marks('.sp1') > 0, 'a real trend is drawn');
  assert.equal(marks('.sp2'), 0, 'two readings at the end of an hour are a speck, not a trend: nothing is drawn');
  assert.equal(marks('.sp3'), 0, 'fewer points than minPoints draw nothing');
  assert.equal(marks('.sp4'), 0);
  assert.ok(marks('.sp5') > 0, 'two points that span the range are a line');
  assert.equal(document.querySelector('.sp2 svg').getAttribute('width'), '96', 'the empty sparkline keeps its box');
  await small.unmount();
}

// ---- Counts of one, full names, and code tools that never cover code -------
{
  const view = mount(html`
    <span class="h1"><${HealthStrip} label="openai" noun=${['upstream attempt', 'upstream attempts']} buckets=${[{ ok: 0, failed: 1 }]} /></span>
    <span class="h2"><${HealthStrip} label="openai" noun="upstream attempts" buckets=${[{ ok: 1, failed: 0 }]} /></span>
    <span class="h3"><${HealthStrip} buckets=${[{ ok: 1, failed: 0 }]} /></span>
    <span class="h4"><${HealthStrip} noun=${['upstream attempt', 'upstream attempts']} buckets=${[{ ok: 1, failed: 1 }]} /></span>
    <div class="lm1"><${LoadMore} hasMore=${false} shown=${1} noun="requests" /></div>
    <div class="lm2"><${LoadMore} hasMore=${false} shown=${1} noun=${['matching request', 'matching requests']} /></div>
    <div class="lm3"><${LoadMore} hasMore=${false} shown=${12} noun="requests" /></div>
    <div class="lm4"><${LoadMore} hasMore=${true} shown=${12} noun=${['request', 'requests']} onLoad=${() => {}} /></div>
    <div class="lm5"><${LoadMore} hasMore=${false} /></div>
    <div class="cb1"><${CodeBlock} language="text" value=${'curl -s http://127.0.0.1:8317/v1/chat/completions -H "Authorization: Bearer $SWITCHYARD_KEY"'} /></div>
    <div class="cb2"><${CodeBlock} title="Body" value="{}" /></div>
    <div class="titled"><${LineChart}
      x=${['a', 'b']}
      series=${[
        { key: 'a', label: 'gpt-4o…', title: 'openai/gpt-4o-2024-08-06-with-a-long-suffix', values: [1, 2] },
        { key: 'b', label: 'claude', values: [2, 1] },
      ]}
      label="Requests by model" /></div>
  `);
  const name = (selector) => document.querySelector(`${selector} .health`).getAttribute('aria-label');
  assert.equal(name('.h1'), 'openai: 0% of 1 recent upstream attempt succeeded', 'one attempt is one attempt');
  assert.equal(name('.h2'), 'openai: 100% of 1 recent upstream attempt succeeded', 'a plural string noun still reads singular for one');
  assert.equal(name('.h3'), '100% of 1 recent request succeeded');
  assert.equal(name('.h4'), '50% of 2 recent upstream attempts succeeded');

  assert.equal(text(document.querySelector('.lm1 .loadmore')), 'The only request is shown');
  assert.equal(text(document.querySelector('.lm2 .loadmore')), 'The only matching request is shown');
  assert.equal(text(document.querySelector('.lm3 .loadmore')), 'All 12 requests shown');
  assert.equal(text(document.querySelector('.lm4 button')), 'Load older requests', 'a noun pair: the button counts many');
  assert.equal(text(document.querySelector('.lm5 .loadmore')), 'No more rows');

  // Without a title the tools still sit in a bar of their own, above the code.
  for (const selector of ['.cb1', '.cb2']) {
    const block = document.querySelector(`${selector} .code`);
    const bar = block.querySelector('.code-bar');
    assert.ok(bar && bar.querySelector('.code-tools'), `${selector}: the tools are in the bar`);
    assert.equal(block.children[0], bar, 'the bar comes first');
    assert.equal(block.querySelector('pre .code-tools'), null, 'nothing of the tools is inside the code');
  }
  assert.ok(document.querySelector('.cb1 .code-bar').hasAttribute('data-untitled'));
  assert.equal(document.querySelector('.cb1 .code-title'), null);
  assert.equal(document.querySelector('.cb1 pre').getAttribute('aria-label'), 'Code');

  // series.title: the full name of a shortened label, on the legend entry and the table heading.
  const chart = await until(() => document.querySelector('.titled .chart-legend') && document.querySelector('.titled'), 'the chart');
  const legend = chart.querySelectorAll('.chart-legend li');
  assert.equal(legend[0].getAttribute('title'), 'openai/gpt-4o-2024-08-06-with-a-long-suffix');
  assert.equal(text(legend[0]), 'gpt-4o…');
  assert.equal(legend[1].getAttribute('title'), 'claude', 'without a title, the label');
  chart.querySelectorAll('button').find((button) => button.getAttribute('aria-label') === 'Show as table').click();
  const heads = await until(() => chart.querySelector('.chart-table-wrap') && chart.querySelectorAll('.chart-table-wrap th'), 'the table view');
  assert.equal(heads[1].getAttribute('title'), 'openai/gpt-4o-2024-08-06-with-a-long-suffix');
  assert.equal(text(heads[1].querySelector('.chart-table-head')), 'gpt-4o…', 'the heading is cut by CSS: it needs its own element');
  await view.unmount();
}

// ---- Live: `lagged` and reconnects are announced as gaps ------------------
{
  assert.ok(TOPICS.includes('lagged'), 'lagged is a documented topic');
  class FakeSocket {
    constructor(address) {
      this.url = address;
      this.readyState = 0;
      this.sent = [];
      FakeSocket.all.push(this);
    }
    send(message) {
      this.sent.push(JSON.parse(message));
    }
    close() {
      this.readyState = 3;
    }
    accept() {
      this.readyState = 1;
      this.onopen?.();
    }
    frame(frame) {
      this.onmessage?.({ data: JSON.stringify(frame) });
    }
    drop() {
      this.readyState = 3;
      this.onclose?.();
    }
  }
  FakeSocket.all = [];
  const realFetch = globalThis.fetch;
  globalThis.fetch = async () => new Response('{"ticket":"t-1"}', { status: 200 });
  Object.defineProperty(globalThis, 'WebSocket', { value: FakeSocket, configurable: true, writable: true });
  Object.defineProperty(globalThis, 'location', { value: { protocol: 'http:', host: 'localhost', hash: '', href: 'http://localhost/admin/' }, configurable: true, writable: true });
  try {
    const gaps = [];
    const lagged = [];
    function Probe({ paused }) {
      useLiveGap((gap) => gaps.push(gap), { enabled: !paused });
      useLive('lagged', (data) => lagged.push(data));
      useLive('stats', () => {});
      return null;
    }
    const view = mount(html`<${Probe} />`);
    await sleep(120); // the hooks subscribe in effects
    live.start();
    const first = await until(() => FakeSocket.all[0], 'the live socket');
    first.accept();
    await until(() => liveState.get().status === 'open', 'the connection to be open');
    assert.deepEqual(gaps, [], 'the first connection of a session is not a gap');
    const subscription = first.sent.find((message) => message.type === 'subscribe');
    assert.ok(subscription.topics.includes('stats') && subscription.topics.includes('hello'));
    assert.ok(!subscription.topics.includes('lagged'), 'lagged is never subscribed to: the gateway sends it regardless');

    first.frame({ type: 'lagged', data: { missed: 7 } });
    assert.deepEqual(gaps, [{ reason: 'lagged', missed: 7 }], 'a lagged frame is a gap');
    assert.deepEqual(lagged, [{ missed: 7 }], 'and still reaches useLive("lagged")');

    first.drop();
    const second = await until(() => FakeSocket.all[1], 'the reconnect', 3000);
    assert.equal(gaps.length, 1, 'nothing is announced until the connection is back');
    second.accept();
    assert.deepEqual(gaps[1], { reason: 'reconnect', missed: null }, 'a reconnect is a gap');

    // The hook lets go in an effect, a frame or so after the render: frames
    // keep coming until one of them is no longer announced.
    render(html`<${Probe} paused=${true} />`, view.root);
    await until(() => {
      const before = gaps.length;
      second.frame({ type: 'lagged', data: { missed: 1 } });
      return gaps.length === before;
    }, 'enabled: false to pause the hook');
    const paused = gaps.length;
    second.frame({ type: 'lagged', data: { missed: 1 } });
    assert.equal(gaps.length, paused, 'and it stays paused');
    await view.unmount();
    live.stop();
    assert.equal(liveState.get().status, 'idle');
  } finally {
    globalThis.fetch = realFetch;
    auth.set({ status: 'anonymous', reason: null });
  }
}

// ---- Live: a page hears that the connection went down -----------------------
{
  class FakeSocket {
    constructor(address) {
      this.url = address;
      this.readyState = 0;
      FakeSocket.all.push(this);
    }
    send() {}
    close() {
      this.readyState = 3;
    }
    accept() {
      this.readyState = 1;
      this.onopen?.();
    }
    drop() {
      this.readyState = 3;
      this.onclose?.();
    }
  }
  FakeSocket.all = [];
  const realFetch = globalThis.fetch;
  globalThis.fetch = async () => new Response('{"ticket":"t-2"}', { status: 200 });
  const realSocket = globalThis.WebSocket;
  Object.defineProperty(globalThis, 'WebSocket', { value: FakeSocket, configurable: true, writable: true });
  try {
    const events = [];
    function Probe({ paused }) {
      useLiveGap((gap) => events.push(`gap:${gap.reason}`), { enabled: !paused, onDown: (down) => events.push(`down:${down.reason}:${liveState.get().status}`) });
      return null;
    }
    const view = mount(html`<${Probe} />`);
    const direct = [];
    const offDown = live.onDown((down) => direct.push(down));
    await sleep(120);
    live.start();
    const first = await until(() => FakeSocket.all[0], 'the live socket');
    // A first connection that fails before it opened is no "down": nobody relied on it.
    first.drop();
    const second = await until(() => FakeSocket.all[1], 'a retry', 3000);
    assert.deepEqual(events, [], 'a socket that never opened does not go down');
    second.accept();
    await until(() => liveState.get().status === 'open');
    assert.deepEqual(events, [], 'nor is the first open connection of the session a gap');

    second.drop();
    assert.deepEqual(events, ['down:down:reconnecting'], 'the open connection lost: onDown, once, with the state already reconnecting');
    assert.deepEqual(direct, [{ reason: 'down' }], 'live.onDown hears it too');
    const third = await until(() => FakeSocket.all[2], 'the reconnect', 3000);
    third.drop();
    const fourth = await until(() => FakeSocket.all[3], 'another retry', 5000);
    assert.equal(events.length, 1, 'failed retries are the same outage: no second down');
    fourth.accept();
    assert.deepEqual(events, ['down:down:reconnecting', 'gap:reconnect'], 'and the reconnect gap closes the pair');

    // Signing out closes the socket: that is not the connection going down.
    offDown();
    live.stop();
    assert.equal(events.length, 2, 'stop() announces nothing');
    assert.equal(direct.length, 1, 'an unsubscribed listener hears nothing more');
    await view.unmount();
  } finally {
    live.stop();
    globalThis.fetch = realFetch;
    Object.defineProperty(globalThis, 'WebSocket', { value: realSocket, configurable: true, writable: true });
    auth.set({ status: 'anonymous', reason: null });
  }
}

// ---- Session: a resumed tab keeps its secret; signing out reaches every tab --
{
  const TOKEN = 'sy.admin.token';
  watchOtherTabs(); // at load in a browser; this module was loaded before the window
  const realFetch = globalThis.fetch;
  const sent = [];
  let pendingLogin = null;
  let loginWaiting = false;
  // The live client asks for tickets on its own once signed in: only /status counts.
  const lastStatus = () => sent.filter((s) => s.url.endsWith('/status')).at(-1)?.auth;
  globalThis.fetch = async (url, init) => {
    sent.push({ url: String(url), auth: new Headers(init.headers).get('authorization') });
    if (pendingLogin && String(url).endsWith('/login')) {
      loginWaiting = true;
      await pendingLogin;
    }
    return new Response('{}', { status: 200 });
  };
  // Another tab of the same browser.
  const other = new BroadcastChannel('sy.admin.session');
  const heard = [];
  other.onmessage = (event) => heard.push(event.data);
  try {
    // A tab opened on a remembered session reads the secret from localStorage.
    localStorage.setItem(TOKEN, 'remembered-secret');
    sessionStorage.removeItem(TOKEN);
    auth.set({ status: 'unknown', reason: null });
    assert.equal(await api.resume(), true);
    assert.equal(sessionStorage.getItem(TOKEN), 'remembered-secret', 'the resumed tab keeps a copy of its own');
    // Another tab signs in without "Remember", which removes the stored copy.
    localStorage.removeItem(TOKEN);
    await api.get('/status');
    assert.equal(lastStatus(), 'Bearer remembered-secret', 'requests still carry the secret');
    assert.equal(auth.get().status, 'authenticated');

    // Another tab signs out: this one does too, and says where it happened.
    other.postMessage({ type: 'signed-out' });
    await until(() => auth.get().status === 'anonymous', 'a sign-out in another tab to reach this one');
    assert.deepEqual(auth.get(), { status: 'anonymous', reason: 'signed-out', elsewhere: true });
    assert.equal(sessionStorage.getItem(TOKEN), null, 'and drops its copy of the secret');
    await api.get('/status');
    assert.equal(lastStatus(), null, 'nothing is sent with the secret afterwards');

    // Browsers without BroadcastChannel hear it through the storage event.
    await api.login('second-secret');
    assert.equal(auth.get().elsewhere, false);
    dispatch(window, 'storage', { key: 'sy.admin.signedOutAt', newValue: String(Date.now()), bubbles: false });
    assert.deepEqual(auth.get(), { status: 'anonymous', reason: 'signed-out', elsewhere: true });
    dispatch(window, 'storage', { key: 'sy.admin.signedOutAt', newValue: null, bubbles: false });
    dispatch(window, 'storage', { key: TOKEN, newValue: null, bubbles: false });

    // Signing out here tells the other tabs, both ways.
    await api.login('third-secret');
    const written = [];
    const realSet = localStorage.setItem;
    localStorage.setItem = (key, value) => {
      written.push(key);
      realSet(key, value);
    };
    try {
      api.logout();
    } finally {
      localStorage.setItem = realSet;
    }
    assert.deepEqual(auth.get(), { status: 'anonymous', reason: 'signed-out', elsewhere: false });
    await until(() => heard.length === 1, 'the other tab to hear the sign-out');
    assert.deepEqual(heard[0], { type: 'signed-out' });
    assert.ok(written.includes('sy.admin.signedOutAt'), 'the storage key for browsers without BroadcastChannel');
    assert.equal(localStorage.getItem('sy.admin.signedOutAt'), null, 'and it is not left behind');
    // Dropping a stored secret that could not be checked concerns this tab only.
    await api.login('fourth-secret');
    api.logout({ allTabs: false });
    await sleep(30);
    assert.equal(heard.length, 1, 'logout({ allTabs: false }) tells nobody');

    // A sign-out heard while the stored secret is being checked wins.
    localStorage.setItem(TOKEN, 'remembered-secret');
    auth.set({ status: 'unknown', reason: null, elsewhere: false });
    let releaseLogin;
    pendingLogin = new Promise((resolve) => { releaseLogin = resolve; });
    const resumed = api.resume();
    await until(() => loginWaiting, 'the resume login check to wait for its answer');
    other.postMessage({ type: 'signed-out' });
    await until(() => auth.get().status === 'anonymous' && auth.get().elsewhere, 'the sign-out to arrive before the pending check');
    releaseLogin();
    assert.equal(await resumed, false);
    assert.equal(auth.get().status, 'anonymous', 'the check that landed afterwards does not sign the tab back in');
    assert.equal(sessionStorage.getItem(TOKEN), null);
  } finally {
    pendingLogin = null;
    other.close();
    globalThis.fetch = realFetch;
    api.logout({ allTabs: false });
    auth.set({ status: 'anonymous', reason: null, elsewhere: false });
  }
}

// ---- Router: leave guards -------------------------------------------------
//
// Against a stub of location and history: entries, pushState, replaceState,
// back, and `hashchange` after the fact. There is no Navigation API here, so
// this is the path a browser without it takes (and the one a traversal the
// browser will not let anyone cancel takes everywhere): the address has
// already changed when the router hears of it. The cancel-before-it-happens
// path is exercised in a real browser.
{
  const entries = [{ hash: '#/keys', state: null }];
  let at = 0;
  const hashOf = (address) => {
    const s = String(address);
    return s.includes('#') ? s.slice(s.indexOf('#')) : '';
  };
  const changed = () => setTimeout(() => dispatch(window, 'hashchange'), 0);
  const location = {
    protocol: 'http:',
    host: 'localhost',
    pathname: '/admin/',
    get hash() {
      return entries[at].hash;
    },
    set hash(value) {
      const hash = value.startsWith('#') ? value : `#${value}`;
      if (hash === entries[at].hash) return;
      entries.splice(at + 1);
      entries.push({ hash, state: null });
      at += 1;
      changed();
    },
    get href() {
      return `http://localhost/admin/${entries[at].hash}`;
    },
  };
  const history = {
    get state() {
      return entries[at].state;
    },
    pushState(state, _title, address) {
      entries.splice(at + 1);
      entries.push({ hash: hashOf(address), state });
      at += 1;
    },
    replaceState(state, _title, address) {
      entries[at] = { hash: address == null ? entries[at].hash : hashOf(address), state };
    },
    back() {
      setTimeout(() => {
        if (at === 0) return;
        const from = entries[at].hash;
        at -= 1;
        if (entries[at].hash !== from) dispatch(window, 'hashchange');
      }, 0);
    },
  };
  Object.defineProperty(globalThis, 'location', { value: location, configurable: true, writable: true });
  Object.defineProperty(globalThis, 'history', { value: history, configurable: true, writable: true });

  // A copy of the router that grew up with a window: it listens to hashchange.
  const router = await import('../js/lib/router.js?with-window');
  const { navigate, setQuery, routeStore, registerLeaveGuard, useLeaveGuard, mayLeave } = router;
  const shown = () => `${routeStore.get().path}${Object.keys(routeStore.get().query).length ? `?${new URLSearchParams(routeStore.get().query)}` : ''}`;
  assert.equal(shown(), '/keys');

  // Without a guard everything goes straight through.
  navigate('/models');
  await until(() => shown() === '/models', 'a plain navigation');
  setQuery({ q: 'gpt' });
  assert.equal(shown(), '/models?q=gpt');
  assert.equal(location.hash, '#/models?q=gpt');
  assert.equal(await mayLeave(), true);

  // A guard that has to ask: nothing moves until it has answered.
  const asked = [];
  let answer;
  const ask = (about) => {
    asked.push(`${about.how}:${about.to ? about.to.path : 'nowhere'}<-${about.from.path}`);
    return new Promise((resolve) => (answer = resolve));
  };
  let off = registerLeaveGuard(ask);
  navigate('/providers');
  await sleep(20);
  assert.equal(shown(), '/models?q=gpt', 'the route waits for the guard');
  assert.equal(location.hash, '#/models?q=gpt', 'and so does the address');
  navigate('/logs');
  await sleep(10);
  assert.deepEqual(asked, ['push:/providers<-/models'], 'a second attempt while the question is open does not ask again');
  answer(false);
  await sleep(20);
  assert.equal(shown(), '/models?q=gpt', '"keep editing" stays');
  assert.equal(entries.length, 2, 'and leaves the history alone');
  navigate('/providers');
  await sleep(10);
  answer(true);
  await until(() => shown() === '/providers', 'the navigation to go ahead once the guard agrees');
  assert.equal(asked.length, 2, 'the redone navigation is not asked about a second time');

  // A query change on the same page is put to the guard too (it decides).
  setQuery({ tab: 'pricing' });
  await sleep(10);
  assert.equal(asked.at(-1), 'replace:/providers<-/providers');
  assert.equal(shown(), '/providers');
  answer(true);
  await until(() => shown() === '/providers?tab=pricing');
  // force: the view has asked already.
  navigate('/usage', { force: true });
  await until(() => shown() === '/usage', 'a forced navigation to skip the guards');
  assert.equal(asked.length, 3);

  // Back: the address has changed before anyone could object. The route
  // does not follow, the address is put back, and the step is redone on a yes.
  assert.deepEqual(entries.map((e) => e.hash), ['#/keys', '#/models?q=gpt', '#/providers?tab=pricing', '#/usage']);
  history.back();
  await until(() => asked.length === 4, 'Back to reach the guard');
  assert.equal(asked.at(-1), 'traverse:/providers<-/usage');
  assert.equal(shown(), '/usage', 'the page stays while the question is open');
  assert.equal(location.hash, '#/usage', 'and the address is the page\'s again');
  answer(false);
  await sleep(20);
  assert.equal(shown(), '/usage');
  assert.equal(at, 3, 'the history is where it was');
  history.back();
  await until(() => asked.length === 5);
  answer(true);
  await until(() => shown() === '/providers?tab=pricing', 'Back to happen once the guard agrees');
  assert.equal(location.hash, '#/providers?tab=pricing');

  // Signing out asks too (the shell calls mayLeave before api.logout()).
  const leaving = mayLeave(null, 'signout');
  await sleep(10);
  assert.equal(asked.at(-1), 'signout:nowhere<-/providers');
  answer(false);
  assert.equal(await leaving, false);

  // Guards answer synchronously when they can; nothing is cancelled and redone then.
  off();
  let allow = false;
  const calls = [];
  off = registerLeaveGuard(({ to }) => (calls.push(to?.path), allow));
  navigate('/logs');
  assert.equal(shown(), '/providers?tab=pricing', 'a flat no');
  allow = true;
  navigate('/logs');
  await until(() => shown() === '/logs');
  // A guard that throws does not trap the user on the page.
  off();
  off = registerLeaveGuard(() => {
    throw new Error('guard bug');
  });
  const realError = console.error;
  console.error = () => {};
  navigate('/about');
  await until(() => shown() === '/about', 'a broken guard to let the navigation through');
  console.error = realError;
  off();

  // Closing or reloading the tab: only while a guard says `unload`.
  assert.equal(dispatch(window, 'beforeunload').defaultPrevented, false);
  off = registerLeaveGuard(() => true, { unload: true });
  assert.equal(dispatch(window, 'beforeunload').defaultPrevented, true, 'the browser\'s own prompt is asked for');
  off();
  assert.equal(dispatch(window, 'beforeunload').defaultPrevented, false);

  // useLeaveGuard: the hook, with the shell's confirm dialog.
  let guard;
  function Editor({ dirty }) {
    guard = useLeaveGuard(dirty, { title: 'Discard unsaved changes to the provider?' });
    return html`<button id="discard">Discard</button>`;
  }
  const view = mount(html`<${Editor} dirty=${false} /><${ConfirmHost} />`);
  await sleep(20);
  navigate('/keys');
  await until(() => shown() === '/keys', 'a clean form to let go');
  render(html`<${Editor} dirty=${true} /><${ConfirmHost} />`, view.root);
  await sleep(20);
  setQuery({ q: 'ci' });
  assert.equal(shown(), '/keys?q=ci', 'a filter on the same page is not leaving it');
  assert.equal(document.querySelector('.modal'), null);
  assert.equal(dispatch(window, 'beforeunload').defaultPrevented, true, 'a dirty form guards the tab as well');
  navigate('/models');
  const dialog = await until(() => document.querySelector('.modal'), 'the discard question');
  assert.match(text(dialog), /Discard unsaved changes to the provider\?/);
  assert.equal(shown(), '/keys?q=ci');
  const buttons = () => document.querySelectorAll('.modal .overlay-foot button');
  assert.deepEqual(buttons().map((b) => text(b)), ['Keep editing', 'Discard changes']);
  buttons()[0].click();
  await until(() => !document.querySelector('.modal'), 'the dialog to close', 1500);
  assert.equal(shown(), '/keys?q=ci', '"Keep editing" stays on the page');
  navigate('/models');
  await until(() => document.querySelector('.modal'));
  buttons()[1].click();
  await until(() => shown() === '/models', '"Discard changes" leaves');
  assert.equal(dispatch(window, 'beforeunload').defaultPrevented, false, 'a guard the user has released no longer holds the tab');
  // release(): the view's own Discard button has asked already.
  render(html`<${Editor} dirty=${false} /><${ConfirmHost} />`, view.root);
  await sleep(20);
  render(html`<${Editor} dirty=${true} /><${ConfirmHost} />`, view.root);
  await sleep(20);
  await until(() => !document.querySelector('.modal'), 'the earlier dialog to be gone', 1500);
  guard.release();
  navigate('/usage');
  await until(() => shown() === '/usage', 'a released guard to let go without asking');
  assert.equal(document.querySelector('.modal'), null);
  await view.unmount();
  navigate('/keys');
  await until(() => shown() === '/keys', 'no guard is left behind by an unmounted view');

  // useQueryParam: what is typed replaces the entry, what is picked with
  // { push: true } is a step Back can return to (Settings tabs, Usage ranges).
  {
    const { useQueryParam } = router;
    let setTab;
    let setQ;
    function Probe() {
      [, setTab] = useQueryParam('tab', 'general', { push: true });
      [, setQ] = useQueryParam('q', '');
      return null;
    }
    const probe = mount(html`<${Probe} />`);
    await sleep(20);
    const start = at;
    setTab('routing');
    await until(() => shown() === '/keys?tab=routing', 'a picked tab');
    setTab('pricing');
    await until(() => shown() === '/keys?tab=pricing');
    assert.equal(at, start + 2, 'each pick is a history entry');
    setTab('pricing');
    await sleep(20);
    assert.equal(at, start + 2, 'picking what is shown is not a step');
    setQ('g');
    setQ('gp');
    await until(() => shown() === '/keys?tab=pricing&q=gp', 'typing');
    assert.equal(at, start + 2, 'typing replaces the entry');
    // A form control hands the setter its DOM event as a second argument:
    // that is not an options object.
    class InputEvent {
      constructor() {
        this.replace = false;
        this.target = {};
      }
    }
    setQ('gpt', new InputEvent());
    await until(() => shown() === '/keys?tab=pricing&q=gpt');
    assert.equal(at, start + 2, 'an event passed along does not turn typing into steps');
    setTab('general', new InputEvent());
    await until(() => shown() === '/keys?q=gpt', 'the default value leaves the address');
    assert.equal(at, start + 3);
    history.back();
    await until(() => shown() === '/keys?tab=pricing&q=gpt', 'Back to return to the previous tab');
    history.back();
    await until(() => shown() === '/keys?tab=routing', 'and to the one before');
    setTab('raw', { replace: true });
    await until(() => shown() === '/keys?tab=raw');
    assert.equal(at, start + 1, 'a call may still ask to replace');
    await probe.unmount();
  }
}

// ---- Table: where the header sticks ---------------------------------------
{
  const columns = [{ key: 'name', header: 'Name', primary: true }];
  const rows = [{ id: 'a', name: 'alpha' }];
  const view = mount(html`
    <div class="fits"><${Table} columns=${columns} rows=${rows} /></div>
    <div class="own-box"><${Table} columns=${columns} rows=${rows} maxHeight="200px" /></div>
    <div class="not-sticky"><${Table} columns=${columns} rows=${rows} sticky=${false} /></div>
  `);
  const wrap = (name) => document.querySelector(`.${name} .table-wrap`);
  await until(() => wrap('fits').hasAttribute('data-page-sticky'), 'a table that fits its box to be marked as scrolling with the page');
  assert.equal(wrap('own-box').hasAttribute('data-page-sticky'), false, 'a table with maxHeight scrolls in its own box');
  assert.ok(wrap('own-box').hasAttribute('data-scroll'));
  assert.equal(wrap('not-sticky').hasAttribute('data-page-sticky'), false);
  await view.unmount();
}

// ---- Shell: Ctrl+K and a layer that may not be covered --------------------
{
  const { Shell } = await import('../js/shell/shell.js');
  const appRouter = await import('../js/lib/router.js');
  // A route with no page behind it: the shell is under test, not a page.
  appRouter.routeStore.replace(appRouter.parseHash('#/nowhere'));
  const realFetch = globalThis.fetch;
  globalThis.fetch = async () => new Response('{"version":"0.0.0"}', { status: 200 });
  try {
    function Host({ open, dismissable }) {
      return html`
        <${Shell} />
        <${Modal} open=${open} dismissable=${dismissable} onClose=${() => {}} title="Copy the new key">
          <${Menu} label="Format" items=${[{ label: 'Plain', onSelect: () => {} }, { label: 'As an environment variable', onSelect: () => {} }]} />
          <button id="copied">I have copied it</button>
        <//>
      `;
    }
    const view = mount(html`<${Host} open=${true} dismissable=${false} />`);
    const inside = await until(() => document.querySelector('#copied'), 'the dialog over the shell');
    await until(() => topOverlay()?.dismissable === false);
    assert.equal(overlayLocked(), true);
    await sleep(120);
    inside.focus();
    const key = () => dispatch(document.activeElement, 'keydown', { key: 'k', ctrlKey: true });
    key();
    await sleep(40);
    assert.equal(document.querySelector('.palette'), null, 'Ctrl+K does not open the palette over a layer that is not dismissable');

    // A menu open inside that dialog is the topmost layer and is itself
    // dismissable: the dialog under it still says no.
    [...document.querySelectorAll('.modal button')].find((button) => button.getAttribute('aria-label') === 'Format').click();
    const menu = await until(() => document.querySelector('.menu'), 'the menu inside the dialog');
    await until(() => menu === document.activeElement || menu.contains(document.activeElement), 'focus in the menu');
    assert.equal(topOverlay().dismissable, true, 'the menu on top is dismissable');
    assert.equal(overlayLocked(), true, 'the stack is still locked by the dialog below it');
    key();
    await sleep(120);
    assert.equal(document.querySelector('.palette'), null, 'Ctrl+K does not open the palette over a non-dismissable dialog with a menu open inside it');
    dispatch(document.activeElement, 'keydown', { key: 'Escape' });
    await until(() => !document.querySelector('.menu') || document.querySelector('.menu').getAttribute('data-state') === 'closed', 'Escape to close the menu');
    await sleep(200);

    render(html`<${Host} open=${true} dismissable=${true} />`, view.root);
    await until(() => topOverlay()?.dismissable === true);
    key();
    const palette = await until(() => document.querySelector('.palette'), 'the palette to open over a dismissable layer');
    await until(() => palette.contains(document.activeElement), 'focus in the palette');
    key();
    await until(() => !document.querySelector('.palette'), 'Ctrl+K inside the palette to close it');
    await until(() => document.activeElement === inside, 'focus to return into the dialog');
    await view.unmount();
  } finally {
    globalThis.fetch = realFetch;
  }
}

// ---- Shell: the focus after signing in, and what the palette lists -------
{
  const { Shell } = await import('../js/shell/shell.js');
  const realFetch = globalThis.fetch;
  globalThis.fetch = async () => new Response('{"version":"0.0.0"}', { status: 200 });
  const mainRegion = () => document.querySelector('main#main');
  try {
    // app.js swaps the sign-in form for the shell: the focused field goes.
    function Gate({ signedIn }) {
      return signedIn ? html`<${Shell} takeFocus=${true} />` : html`<div class="login"><input id="login-secret" /></div>`;
    }
    const view = mount(html`<${Gate} signedIn=${false} />`);
    document.querySelector('#login-secret').focus();
    render(html`<${Gate} signedIn=${true} />`, view.root);
    await until(() => mainRegion() && document.activeElement === mainRegion(), 'the shell to take the focus the sign-in form dropped, not leave it on <body>');
    await view.unmount();

    // A plain load of a remembered session: the first Tab still reaches
    // "Skip to content", so nothing is focused for the user.
    document.activeElement.blur();
    const plain = mount(html`<${Shell} />`);
    await until(() => mainRegion(), 'the shell');
    await sleep(60);
    assert.ok(document.activeElement === document.body, `a plain load leaves the focus where the browser has it; it is on ${document.activeElement.localName}#${document.activeElement.id}`);

    // Ctrl+K there: the palette lists the pages operators use, not the kit.
    dispatch(document.body, 'keydown', { key: 'k', ctrlKey: true });
    const field = await until(() => document.querySelector('.palette input'), 'the palette');
    await until(() => document.activeElement === field);
    const labels = document.querySelectorAll('.palette-item .palette-item-label').map((node) => text(node));
    assert.ok(labels.includes('Overview') && labels.includes('About'), labels.join(', '));
    assert.ok(!labels.includes('Component kit'), 'the component kit is for page authors, opened by its address');
    // Closed with nothing focused before: the focus goes to the page, not <body>.
    dispatch(field, 'keydown', { key: 'Escape' });
    await until(() => !document.querySelector('.palette'), 'Escape to close the palette');
    await until(() => document.activeElement === mainRegion(), 'the palette opened on a page just loaded to hand the focus to <main>');
    await plain.unmount();
  } finally {
    globalThis.fetch = realFetch;
  }
}

// ---- Shell: the focus on a change of page --------------------------------
{
  const { Shell } = await import('../js/shell/shell.js');
  const appRouter = await import('../js/lib/router.js');
  const { ROUTES } = await import('../js/routes.js');
  // Two small pages of the test's own: the shell is under test, not a page.
  const pages = [
    { path: '/_one', title: 'One', load: async () => ({ default: () => html`<div class="one"><input id="one-search" /></div>` }) },
    { path: '/_two', title: 'Two', load: async () => ({ default: () => html`<div class="two"><button id="two-first">Two</button></div>` }) },
  ];
  ROUTES.push(...pages);
  const realFetch = globalThis.fetch;
  globalThis.fetch = async () => new Response('{"version":"0.0.0"}', { status: 200 });
  const mainRegion = () => document.querySelector('main#main');
  const skip = () => document.querySelector('.skip-link');
  try {
    appRouter.routeStore.replace(appRouter.parseHash('#/_one'));
    const view = mount(html`<${Shell} />`);
    await until(() => document.querySelector('#one-search'), 'page one');
    // The link that was followed (here: the nav) keeps nothing; the page's
    // main region takes the focus.
    document.querySelector('#one-search').focus();
    appRouter.routeStore.replace(appRouter.parseHash('#/_two'));
    await until(() => document.querySelector('#two-first'), 'page two');
    await until(() => document.activeElement === mainRegion(), 'a new page to give <main> the focus');
    // Right after the change, a focus that lands on "Skip to content"
    // without a Tab (the browser handing it to the top of the document) is
    // passed on to <main>: the link must not end up focused and shown.
    skip().focus();
    await until(() => document.activeElement === mainRegion(), 'the skip link to pass a focus it was not tabbed to on to <main>');
    // A Tab still reaches it.
    dispatch(document.body, 'keydown', { key: 'Tab' });
    skip().focus();
    await sleep(20);
    assert.ok(document.activeElement === skip(), 'Tab reaches the skip link as usual');
    await view.unmount();
  } finally {
    ROUTES.splice(ROUTES.indexOf(pages[0]), 2);
    globalThis.fetch = realFetch;
    appRouter.routeStore.replace(appRouter.parseHash('#/nowhere'));
  }
}

console.log('component checks passed');
