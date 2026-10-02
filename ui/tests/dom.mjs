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
const { html, render, useState } = await import('../vendor/preact-htm.js');
const { useAsync, useResource } = await import('../js/lib/hooks.js');
const { Tabs, Segmented } = await import('../js/components/nav.js');
const { Form, NumberInput } = await import('../js/components/form.js');
const { Menu } = await import('../js/components/menu.js');
const { Drawer, Modal } = await import('../js/components/overlay.js');
const { toast, Toaster } = await import('../js/components/toast.js');

const { document, dispatch, sleep, text, until } = installDom();

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

console.log('component checks passed');
