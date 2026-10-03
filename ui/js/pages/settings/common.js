// Settings: the pieces every tab shares.
//
//   helpers        getPath, deepEqual, nestPatch, durationHint, wildcardMatch,
//                  focusSoon (a place for the keyboard when its control has
//                  gone)
//   guard          useUnsavedGuard, confirmDiscard: the open tab asks before
//                  it is left with unsaved edits
//   expectSecret   keeps this browser signed in after the admin secret changed
//   useEdits       a form as "the configuration plus what the user changed"
//   useSettingsSave, SettingsForm, SaveBar, SaveError
//   SettingRow and the row controls (TextRow, NumberRow, SwitchRow, ...)
//   OptionList     a radio group whose options carry an explanation
//   useListDraft   payload rules and prices: a whole document edited as a draft

import { html, useEffect, useMemo, useRef, useState } from '../../../vendor/preact-htm.js';
import { Badge, Button, Form, FormError, Input, Kbd, Notice, NumberInput, Panel, Select, Skeleton, Switch, confirm, toast, useIssues } from '../../components/index.js';
import { rovingIndex } from '../../components/nav.js';
import { ApiError, api, auth, isRemembered } from '../../lib/api.js';
import { formatDurationWords, plural } from '../../lib/format.js';
import { hotkeyLabel, useAsync, useHotkey, useIsPhone, usePresence, useResource, useUid } from '../../lib/hooks.js';
import { liveState, useLive, useLiveGap } from '../../lib/live.js';
import { setQuery, useLeaveGuard } from '../../lib/router.js';
import { useStore } from '../../lib/store.js';

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/** getPath({ a: { b: 1 } }, 'a.b') -> 1; undefined when a step is missing. */
export function getPath(object, path) {
  let value = object;
  for (const key of String(path).split('.')) {
    if (value == null || typeof value !== 'object') return undefined;
    value = value[key];
  }
  return value;
}

/** Structural equality for JSON values (key order does not matter). */
export function deepEqual(a, b) {
  if (Object.is(a, b)) return true;
  if (typeof a !== 'object' || typeof b !== 'object' || a === null || b === null) return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  if (Array.isArray(a)) return a.length === b.length && a.every((item, i) => deepEqual(item, b[i]));
  const ka = Object.keys(a);
  const kb = Object.keys(b);
  return ka.length === kb.length && ka.every((key) => Object.prototype.hasOwnProperty.call(b, key) && deepEqual(a[key], b[key]));
}

/** { 'a.b': 1, 'a.c': 2 } -> { a: { b: 1, c: 2 } }: a JSON merge patch. */
export function nestPatch(flat) {
  const out = {};
  for (const [path, value] of Object.entries(flat)) {
    const keys = path.split('.');
    let node = out;
    keys.slice(0, -1).forEach((key) => {
      if (node[key] == null || typeof node[key] !== 'object') node[key] = {};
      node = node[key];
    });
    node[keys[keys.length - 1]] = value;
  }
  return out;
}

/**
 * What to print under a seconds field: the duration in words ("30 minutes",
 * "1 hour 30 minutes"), or what zero means.
 */
export function durationHint(seconds, zero) {
  if (typeof seconds !== 'number' || !Number.isFinite(seconds) || seconds < 0) return undefined;
  if (seconds === 0 && zero) return zero;
  return formatDurationWords(seconds * 1000);
}

/**
 * The gateway's wildcard match (crates/core/src/util.rs): case-insensitive,
 * `*` matches any run of characters including none, nothing else is special.
 */
export function wildcardMatch(pattern, text) {
  const p = [...String(pattern).toLowerCase()];
  const t = [...String(text).toLowerCase()];
  let pi = 0;
  let ti = 0;
  let star = -1;
  let mark = 0;
  while (ti < t.length) {
    if (pi < p.length && p[pi] === '*') {
      star = pi;
      mark = ti;
      pi += 1;
    } else if (pi < p.length && p[pi] === t[ti]) {
      pi += 1;
      ti += 1;
    } else if (star !== -1) {
      pi = star + 1;
      mark += 1;
      ti = mark;
    } else {
      return false;
    }
  }
  while (pi < p.length && p[pi] === '*') pi += 1;
  return pi === p.length;
}

let localId = 0;
/** A key for a draft row that survives reordering. */
export function rowId() {
  localId += 1;
  return `row-${localId}`;
}

/** Move the item at `index` one place up (-1) or down (+1). */
export function moveItem(list, index, direction) {
  const target = index + direction;
  if (index < 0 || target < 0 || target >= list.length) return list;
  const next = [...list];
  [next[index], next[target]] = [next[target], next[index]];
  return next;
}

/**
 * After a row moved, keep the keyboard on it: on the same arrow, or on the
 * other one when the row has reached the end of the list. Rows carry
 * data-row, their arrows data-move="up" | "down".
 */
export function focusMoved(id, direction) {
  requestAnimationFrame(() => {
    const row = document.querySelector(`[data-row="${id}"]`);
    if (!row) return;
    const same = row.querySelector(`[data-move="${direction < 0 ? 'up' : 'down'}"]`);
    const other = row.querySelector(`[data-move="${direction < 0 ? 'down' : 'up'}"]`);
    (same && !same.disabled ? same : other)?.focus();
  });
}

/** True when the keyboard has no place: nothing is focused, or what was has left the page. */
const focusLost = () => !document.activeElement || document.activeElement === document.body;

/**
 * Give the keyboard a place after the control it was on has gone (a deleted
 * row's button, the save bar). `find` returns the element to focus, looked
 * up when the page has rendered.
 *
 *   leaving  () => boolean: focus is on something that is about to go. With
 *            it, focus only moves when that is true or focus is already lost;
 *            without it, focus moves in any case.
 *   scroll   false keeps the page where it is (after a mouse click)
 *
 * A confirm dialog hands focus back to the control that opened it a moment
 * after it closes, and a save bar stays mounted while it fades: so focus is
 * checked twice more and picked up again if it fell to the page.
 */
export function focusSoon(find, { leaving, scroll = true } = {}) {
  const go = (always) => {
    if (!(always || focusLost() || leaving?.())) return;
    const target = find();
    if (target && target !== document.activeElement) target.focus({ preventScroll: !scroll });
  };
  requestAnimationFrame(() => go(!leaving));
  setTimeout(() => go(false), 90);
  setTimeout(() => go(false), 260);
}

/** The selected tab of the settings page: where focus goes when a tab's own controls are gone. */
export const selectedTab = () => document.querySelector('.settings-main > .tabs [role="tab"][aria-selected="true"]');

/**
 * A notice that goes away takes its buttons with it. Call this with the
 * action of such a button: if that leaves the keyboard without a place, it
 * goes to `find()` (default: the selected tab). The page is not scrolled.
 */
export function focusAfterNotice(find = selectedTab) {
  focusSoon(find, { leaving: () => false, scroll: false });
}

/** A secret written as a reference to an environment variable ("env:NAME", "${NAME}"), not as the secret itself. */
export const isSecretReference = (secret) => /^env:/.test(secret) || /^\$\{[^}]*\}$/.test(secret);

// ---------------------------------------------------------------------------
// Unsaved-changes guard
// ---------------------------------------------------------------------------

/** The question asked before unsaved edits to `what` are thrown away. */
const discardQuestion = (what) => ({
  danger: true,
  title: `Discard unsaved changes to ${what}?`,
  message: 'Your edits have not been saved. Discarding them cannot be undone.',
  confirmLabel: 'Discard changes',
  cancelLabel: 'Keep editing',
});

/** Ask before throwing edits away. Resolves true when the user agrees. */
export function confirmDiscard(what) {
  return confirm(discardQuestion(what));
}

const tabOf = (route) => route.query.tab || 'general';

/**
 * The open tab has unsaved edits to `what` while `dirty` is true. Every way
 * of leaving it then asks first: another tab (the tab is in the query),
 * another page, the command palette, Back, sign-out, closing the window.
 */
export function useUnsavedGuard(dirty, what) {
  useLeaveGuard(dirty, {
    ...discardQuestion(what),
    matters: (to, from) => !to || to.path !== from.path || tabOf(to) !== tabOf(from),
  });
}

// ---------------------------------------------------------------------------
// Staying signed in when the admin secret changes
// ---------------------------------------------------------------------------

// A request that was already on its way with the old secret gets a 401 and
// ends the session. For a few seconds after a change the new secret is kept
// here so the session can be picked up again without the sign-in page.
let pending = null; // { secret, remember, timer }

function forgetPending() {
  if (pending) clearTimeout(pending.timer);
  pending = null;
}

/**
 * The admin secret is about to become one this page cannot know (a reference
 * to a variable in the gateway's environment): a secret adopted a moment ago
 * must not be tried again when the session ends.
 */
export const forgetSecret = forgetPending;

let settling = Promise.resolve();

/**
 * Call before sending a save that changes the admin secret, then exactly
 * one of:
 *
 *   adopt()   the save went through: sign this browser in with the new
 *             secret. Resolves true when the gateway accepts it, false when
 *             it does not (an environment variable overrides the file, so
 *             the old secret stays valid).
 *   cancel()  the save failed: forget the new secret.
 */
export function expectSecret(secret) {
  forgetPending();
  const remember = isRemembered();
  pending = { secret, remember, timer: setTimeout(forgetPending, 30_000) };
  let release;
  settling = new Promise((resolve) => {
    release = resolve;
  });
  return {
    async adopt() {
      let accepted = true;
      try {
        await api.login(secret, { remember });
      } catch {
        accepted = false;
        forgetPending();
      }
      release();
      return accepted;
    },
    cancel() {
      forgetPending();
      release();
    },
  };
}

/**
 * Resolves once a secret change in progress has settled (at once when there
 * is none). Refetches triggered by the change wait for it, so they go out
 * with the secret the gateway now expects.
 */
export function secretSettled() {
  return settling;
}

if (typeof window !== 'undefined') {
  auth.subscribe(({ status, reason }) => {
    if (status !== 'anonymous' || reason !== 'expired' || !pending) return;
    const { secret, remember } = pending;
    forgetPending();
    api.login(secret, { remember }).catch(() => {});
  });
}

// ---------------------------------------------------------------------------
// Forms over PATCH /settings
// ---------------------------------------------------------------------------

/**
 * A form as "the live configuration plus the fields the user changed".
 * Keeping only the edits means a reload from elsewhere updates every field
 * the user has not touched, and the patch that is sent contains exactly what
 * was changed here.
 *
 *   const form = useEdits(config);
 *   form.value('routing.max_attempts')        // edited value, else the live one
 *   form.set('routing.max_attempts')(5)       // an onChange handler
 *   form.changed(path), form.dirty, form.count
 *   form.patch()                              // nested merge patch of the edits
 *   form.snapshot()                           // { path: value } of the edits, as sent
 *   form.settle(snapshot)                     // after a save: drop the edits that were sent
 *   form.reset()
 *
 * settle() rather than reset() after a save: the fields stay editable while
 * the request is on its way, and what was typed meanwhile was not saved, so
 * it must stay an unsaved edit.
 */
export function useEdits(base) {
  const [edits, setEdits] = useState({});
  const baseRef = useRef(base);
  baseRef.current = base;

  return useMemo(() => {
    const live = Object.keys(edits).filter((path) => !deepEqual(edits[path], getPath(base, path)));
    return {
      value: (path) => (Object.prototype.hasOwnProperty.call(edits, path) ? edits[path] : getPath(base, path)),
      set: (path) => (value) =>
        setEdits((current) => {
          const next = { ...current };
          if (deepEqual(value, getPath(baseRef.current, path))) delete next[path];
          else next[path] = value;
          return next;
        }),
      changed: (path) => live.includes(path),
      paths: live,
      count: live.length,
      dirty: live.length > 0,
      patch: () => nestPatch(Object.fromEntries(live.map((path) => [path, edits[path]]))),
      snapshot: () => Object.fromEntries(live.map((path) => [path, edits[path]])),
      settle: (sent) =>
        setEdits((current) => {
          const next = { ...current };
          for (const [path, value] of Object.entries(sent)) {
            if (Object.prototype.hasOwnProperty.call(next, path) && deepEqual(next[path], value)) delete next[path];
          }
          return next;
        }),
      reset: () => setEdits({}),
      baseKey: JSON.stringify(base),
    };
  }, [edits, base]);
}

/**
 * Save a useEdits form through PATCH /settings.
 *
 *   form     from useEdits
 *   config   the useResource('/config') of the page; it is updated in place
 *   name     "Routing": the toast reads "Routing settings saved"
 *   toPatch  (patch) => patch, to reshape the merge patch before it is sent
 *   check    () => [{ path, message }]: client-side problems that block the save
 *   risks    (patch) => string[]: consequences the user must confirm first
 *   prepare  (patch) => { done?, failed? } | null: called right before the
 *            request. `done(result)` runs after a successful save and may
 *            resolve a string that becomes the toast's second line;
 *            `failed()` runs when the save was refused
 *   onDiskInvalid  (error) => void: the save was refused because the file
 *            on disk is not valid (see isDiskInvalid)
 *
 * Returns { submit, saving, error, issues, refused, clear }:
 *
 *   refused  the error as it came back
 *   error    what of it still applies (see stillOpen): an issue goes once
 *            its field is edited, the whole error once nothing is unsaved
 *   clear()  forget the error (Discard)
 */
export function useSettingsSave({ form, config, name, toPatch, check, risks, prepare, onDiskInvalid }) {
  const [clientError, setClientError] = useState(null);
  const save = useAsync((patch) => api.patch('/settings', patch));
  // The form as it was when the last save was checked and sent: what the
  // refusal is about.
  const sentForm = useRef(null);
  const refused = clientError ?? save.error;
  const error = useMemo(() => stillOpen(refused, form, sentForm.current), [refused, form]);
  const issues = useFieldIssues(error);
  useDiskInvalid(save.error, onDiskInvalid);

  const clear = () => {
    setClientError(null);
    // reset() also drops the answer of a save on its way: never then.
    if (!save.loading) save.reset();
  };
  // Nothing unsaved (Discard, every edit typed back, the same values saved
  // elsewhere): a refusal is about edits that are gone.
  useEffect(() => {
    if (!form.dirty && !save.loading && refused) clear();
  }, [form.dirty]);

  const submit = async () => {
    if (!form.dirty || save.loading) return;
    sentForm.current = form;
    const problems = check?.() ?? [];
    if (problems.length > 0) {
      save.reset();
      setClientError(new ApiError(0, 'Nothing was saved.', { issues: problems, code: 'invalid' }));
      return;
    }
    setClientError(null);
    const patch = toPatch ? toPatch(form.patch()) : form.patch();
    const warnings = risks?.(patch) ?? [];
    if (warnings.length > 0) {
      const ok = await confirm({
        danger: true,
        title: `Save these ${name.toLowerCase()} settings?`,
        message: html`${warnings.map((line, i) => html`<span class="settings-confirm-line" key=${i}>${line}</span>`)}`,
        confirmLabel: 'Save changes',
      });
      if (!ok) return;
    }
    const touched = form.paths;
    const sent = form.snapshot();
    const step = prepare?.(patch) ?? null;
    const result = await save.run(patch);
    if (!result) {
      step?.failed?.();
      return;
    }
    // Before anything refetches: when the admin secret changed, the session
    // must be on the new secret first.
    const extra = await step?.done?.(result);
    config.mutate(result);
    // Only what was sent is saved: anything typed while the request was on
    // its way stays an unsaved edit.
    form.settle(sent);
    const notes = [];
    const restart = (result.restart_required ?? []).filter((setting) => touched.some((path) => path === setting || path.startsWith(`${setting}.`)));
    if (restart.length > 0) notes.push(`Restart the gateway to apply ${restart.join(', ')}.`);
    // A field that was emptied went out as null, which the gateway reads as
    // "back to the default": say which value that turned out to be.
    for (const path of touched) {
      if (sent[path] !== null) continue;
      const now = getPath(result.config, path);
      notes.push(now == null ? `${path} is back at its default.` : `${path} is back at its default, ${now}.`);
    }
    if (extra) notes.push(extra);
    const options = notes.length > 0 ? { description: notes.join(' ') } : undefined;
    if (restart.length > 0) toast.warning(`${name} settings saved`, options);
    else toast.success(`${name} settings saved`, options);
  };

  return { submit, saving: save.loading, error, issues, refused, clear };
}

/** "providers[0].name" -> "providers.0.name", as useIssues compares paths. */
const issuePath = (path) =>
  String(path ?? '')
    .replace(/\[(\w+)\]/g, '.$1')
    .replace(/^\./, '');

/**
 * What still applies of a refused save of `form`, which was `sent` when it
 * was checked and sent:
 *
 *   - nothing once nothing is unsaved;
 *   - an issue goes once the value it is about differs from the refused one
 *     (a field at, under or above its path). Typed back to the refused
 *     value, the field gets its message back;
 *   - with the last issue, the whole error: the notice would otherwise send
 *     the user to a field that is no longer highlighted. While some are
 *     left, the notice says "Nothing was saved." instead of the gateway's
 *     message, which names the fixed ones too.
 *
 * An error without issues (the gateway could not be reached) stays until a
 * save works or the edits are discarded. A refusal because the file on disk
 * is not valid is about the file, not about the fields, and stays whole.
 */
function stillOpen(error, form, sent) {
  if (!error) return null;
  if (!form.dirty) return null;
  if (isDiskInvalid(error) || !sent) return error;
  const issues = error.issues ?? [];
  if (issues.length === 0) return error;
  const paths = [...new Set([...sent.paths, ...form.paths])];
  const edited = (raw) => {
    const at = issuePath(raw);
    if (!at) return false;
    return [at, ...paths].some((path) => (path === at || path.startsWith(`${at}.`) || at.startsWith(`${path}.`)) && !deepEqual(form.value(path), sent.value(path)));
  };
  const open = issues.filter((issue) => !edited(issue.path));
  if (open.length === issues.length) return error;
  if (open.length === 0) return null;
  // The gateway's message spells out every issue, the fixed ones too; the
  // ones that are left are at their fields (or listed in the notice).
  return new ApiError(error.status, 'Nothing was saved.', { issues: open, retryAfter: error.retryAfter, code: error.code, body: error.body });
}

// ---------------------------------------------------------------------------
// A save refused because the file on disk is not valid
// ---------------------------------------------------------------------------

/**
 * True for the 409 the gateway gives to every edit while switchyard.toml on
 * disk holds something it refused: writing the edit would rebuild the file
 * from the last valid configuration and discard what was typed there. The
 * error's issues are then the file's (`line 12, column 8`, or a place in the
 * configuration such as `routing.max_attempts`), not those of what was sent,
 * so they must not be handed to the form's fields. None of the endpoints this
 * page saves to answers 409 for anything else.
 */
export const isDiskInvalid = (error) => error?.status === 409;

/** useIssues, except that the file's issues of a disk-invalid refusal belong to no field. */
export function useFieldIssues(error) {
  return useIssues(isDiskInvalid(error) ? null : error);
}

/** The file's problems of a disk-invalid refusal in one line, for the notice above the tabs. */
export const fileProblems = (error) => (error?.issues ?? []).map((issue) => (issue.path ? `${issue.path}: ${issue.message}` : issue.message)).join('; ');

/** Tell the page, once per refusal, that a save found the file on disk invalid. */
export function useDiskInvalid(error, onDiskInvalid) {
  const report = useRef(onDiskInvalid);
  report.current = onDiskInvalid;
  useEffect(() => {
    if (isDiskInvalid(error)) report.current?.(error);
  }, [error]);
}

/**
 * The form-level error of a save: FormError, or for a disk-invalid refusal
 * what is wrong with the file and the way to the tab that can repair it.
 * The edits stay in the form; the unsaved-changes guard asks before the tab
 * is left. That notice goes when the gateway says the file is valid again:
 * the save bar is still there, and saving now works.
 *
 *   error, issues  the save's error and its useFieldIssues
 *   title          "Could not save the prices"
 */
export function SaveError({ error, issues, title }) {
  const [lifted, setLifted] = useState(null);
  useLive('config.reloaded', (data) => {
    if (data?.ok && isDiskInvalid(error)) setLifted(error);
  });
  useEffect(() => {
    if (lifted !== null && lifted === error) focusAfterNotice();
  }, [lifted, error]);
  if (!isDiskInvalid(error)) return html`<${FormError} error=${error} issues=${issues} title=${title} />`;
  if (lifted === error) return null;
  return html`
    <${Notice} tone="stop" title=${title} action=${html`<${Button} size="sm" onClick=${() => setQuery({ tab: 'raw' })}>Open the raw file<//>`}>
      <span>switchyard.toml on disk is not valid, and saving would overwrite it, so nothing was written. Your changes are still here. Fix or restore the file, on the Raw file tab or in an editor, then save again.</span>
      ${error.issues.length > 0 &&
      html`<ul class="issue-list" aria-label="Problems in the file">
        ${error.issues.map((issue, i) => html`<li key=${i}>${issue.path && html`<span class="issue-path">${issue.path}</span>`}<span>${issue.message}</span></li>`)}
      </ul>`}
    <//>
  `;
}

/**
 * The sticky Save / Discard bar. Rendered inside a <Form>, so Save is that
 * form's submit button. It is only on the page while there is something to
 * save.
 *
 *   dirty      show the bar
 *   summary    what is unsaved ("3 unsaved changes")
 *   saving     a save is in flight
 *   onDiscard  called after the user confirmed
 *   what       named in the discard confirmation ("routing settings")
 *   saveLabel  default "Save changes"
 *   returnFocus  () => element: where the keyboard goes when the bar leaves
 *              with focus on one of its buttons (default: the selected tab)
 */
export function SaveBar({ dirty, summary, saving = false, onDiscard, what, saveLabel = 'Save changes', hotkey = true, returnFocus = selectedTab }) {
  const open = dirty || saving;
  const { mounted, state } = usePresence(open, 140);
  const isPhone = useIsPhone();
  const bar = useRef(null);
  useToastLift(bar, open);

  // The bar goes away after Save and after Discard, and the button that was
  // pressed with it. Hand the keyboard on instead of dropping it on <body>.
  // A click made with the mouse must not scroll the page to the new place.
  const byKeyboard = useRef(false);
  const wasOpen = useRef(open);
  const returnRef = useRef(returnFocus);
  returnRef.current = returnFocus;
  useEffect(() => {
    if (wasOpen.current && !open) {
      focusSoon(() => returnRef.current?.(), { leaving: () => !!bar.current?.contains(document.activeElement), scroll: byKeyboard.current });
      byKeyboard.current = false;
    }
    wasOpen.current = open;
  }, [open]);
  // A click from the keyboard (Enter or Space on the button) has no click count.
  const note = (event) => {
    byKeyboard.current = event.detail === 0;
  };

  if (!mounted) return null;
  return html`
    <div ref=${bar} class="settings-savebar" data-state=${state} role="region" aria-label="Unsaved changes">
      <div class="settings-savebar-text">
        <span class="settings-savebar-mark" aria-hidden="true"></span>
        <span>${summary}</span>
      </div>
      <div class="settings-savebar-actions">
        ${hotkey && !isPhone && html`<span class="settings-savebar-key"><${Kbd}>${hotkeyLabel('mod+s')}<//></span>`}
        <${Button}
          disabled=${saving}
          onClick=${async (event) => {
            note(event);
            if (await confirmDiscard(what)) onDiscard();
          }}
        >
          Discard
        <//>
        <${Button} type="submit" variant="primary" loading=${saving} onClick=${note}>${saveLabel}<//>
      </div>
    </div>
  `;
}

// One toast is about this tall; the stack is measured when it is taller.
const TOAST_MIN = 72;
const TOAST_GAP = 8;

/**
 * Toasts rise from the corner the save bar's buttons are in, and they are
 * drawn above everything: the toast that confirms a save would sit on Save
 * and Discard for as long as it shows. While the bar is up, the stack is
 * raised with the shared --toast-lift (UI_GUIDE, "Variables a page sets").
 *
 * How far is measured, not fixed: the bar is one line or two, it sticks to
 * the bottom of the window while the form runs on below and sits above the
 * page's own margin at the end of it, and a bar that is high enough on the
 * page for the stack to fit under it needs no lift at all.
 */
function useToastLift(ref, active) {
  useEffect(() => {
    const el = ref.current;
    const toasts = document.querySelector('.toasts');
    if (!active || !el || !toasts) return undefined;
    const root = document.documentElement;
    let frame = 0;
    const measure = () => {
      frame = 0;
      const rect = el.getBoundingClientRect();
      // Where the bar comes to rest, not where its entrance has it right now.
      let slide = 0;
      try {
        slide = new DOMMatrixReadOnly(getComputedStyle(el).transform).m42 || 0;
      } catch {
        slide = 0;
      }
      const top = rect.top - slide;
      const bottom = rect.bottom - slide;
      // Where the stack rests when nothing lifts it.
      root.style.setProperty('--toast-lift', '0px');
      const rest = parseFloat(getComputedStyle(toasts).bottom) || 0;
      const fitsBelow = window.innerHeight - bottom >= rest + Math.max(toasts.offsetHeight, TOAST_MIN) + TOAST_GAP;
      const lift = fitsBelow ? 0 : Math.max(0, Math.round(window.innerHeight - top + TOAST_GAP - rest));
      root.style.setProperty('--toast-lift', `${lift}px`);
    };
    const schedule = () => {
      if (!frame) frame = requestAnimationFrame(measure);
    };
    measure();
    const resized = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(schedule);
    resized?.observe(el);
    const stacked = typeof MutationObserver === 'undefined' ? null : new MutationObserver(schedule);
    stacked?.observe(toasts, { childList: true });
    window.addEventListener('resize', schedule);
    // The bar moves with the page while the end of the form is on screen.
    // Only what scrolls the bar counts: a text field scrolls as one types.
    const scrolled = (event) => {
      if (event.target === document || event.target?.contains?.(el)) schedule();
    };
    document.addEventListener('scroll', scrolled, { capture: true, passive: true });
    return () => {
      cancelAnimationFrame(frame);
      resized?.disconnect();
      stacked?.disconnect();
      window.removeEventListener('resize', schedule);
      document.removeEventListener('scroll', scrolled, { capture: true });
      root.style.removeProperty('--toast-lift');
    };
  }, [active, ref]);
}

/** Ctrl/Cmd+S submits the form with this id while `enabled`. */
export function useSaveHotkey(formId, enabled) {
  useHotkey('mod+s', () => document.getElementById(formId)?.requestSubmit(), { enabled });
}

/**
 * After a refused save, bring the first problem into view and put the
 * cursor in it: the field the gateway complained about may be a screen away
 * from the Save button.
 */
export function useRevealProblem(formId, error) {
  useEffect(() => {
    if (!error) return;
    const form = document.getElementById(formId);
    const target = form?.querySelector('[aria-invalid="true"], .field-error, .notice[data-tone="stop"]');
    if (!target) return;
    target.scrollIntoView({ block: 'center' });
    if (target.matches('input, textarea, select, button')) target.focus({ preventScroll: true });
  }, [error, formId]);
}

/**
 * The frame of a PATCH /settings tab: the form, the "changed elsewhere"
 * notice, the form-level error and the save bar.
 *
 *   form, saver  from useEdits and useSettingsSave
 *   what         "routing settings": used in confirmations and the save bar
 */
export function SettingsForm({ form, saver, what, children }) {
  const formId = useUid('settings-form');
  useUnsavedGuard(form.dirty, what);
  useSaveHotkey(formId, form.dirty && !saver.saving);
  // The refusal itself: a field that is fixed must not send the cursor on
  // to the next problem while the user is typing in it.
  useRevealProblem(formId, saver.refused);

  // The configuration changed underneath unsaved edits (another tab, the
  // file on disk). The edits stay; say that the rest moved.
  const [elsewhere, setElsewhere] = useState(false);
  const seen = useRef(form.baseKey);
  useEffect(() => {
    if (seen.current === form.baseKey) return;
    seen.current = form.baseKey;
    if (form.dirty) setElsewhere(true);
  }, [form.baseKey]);
  useEffect(() => {
    if (!form.dirty) setElsewhere(false);
  }, [form.dirty]);

  return html`
    <${Form} id=${formId} class="settings-form" onSubmit=${saver.submit}>
      ${elsewhere &&
      html`<${Notice} tone="info" title="The configuration changed elsewhere" action=${html`<${Button} size="sm" onClick=${() => { setElsewhere(false); focusAfterNotice(); }}>Dismiss<//>`}>
        Fields you have not edited now show the new values. Your edits are kept and will be saved on top of them.
      <//>`}
      ${children}
      <${SaveError} error=${saver.error} issues=${saver.issues} title=${`Could not save the ${what}`} />
      <${SaveBar} dirty=${form.dirty} saving=${saver.saving} what=${what} onDiscard=${() => { form.reset(); saver.clear(); }} summary=${`${plural(form.count, 'unsaved change')}`} />
    <//>
  `;
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/** "routing.max_attempts" -> "set-routing-max_attempts": the control's id. */
export function fieldId(path) {
  return `set-${String(path).replace(/[^a-zA-Z0-9_]+/g, '-')}`;
}

/**
 * One setting: its name and what it does on the left, its control on the
 * right (stacked on narrow screens).
 *
 *   id           id of the control inside; the label points at it
 *   label        the setting's name
 *   description  one or two sentences: what it does, what the values mean
 *   restart      shows the "Needs restart" badge
 *   changed      the value differs from what is saved
 *   kind         "switch" keeps the control beside the text on phones too;
 *                "wide" puts the control under the text at every width
 */
export function SettingRow({ id, label, description, restart = false, changed = false, kind, children }) {
  return html`
    <div class="settings-row" data-kind=${kind} data-changed=${changed ? '' : undefined}>
      <div class="settings-row-text">
        <div class="settings-row-head">
          <label class="settings-row-label" id=${`${id}-label`} for=${id}>${label}</label>
          ${restart && html`<${Badge} outline title="Takes effect after the gateway restarts">Needs restart<//>`}
          ${changed && html`<span class="settings-row-edited">Edited</span>`}
        </div>
        ${description != null && html`<div class="settings-row-desc">${description}</div>`}
      </div>
      <div class="settings-row-control">${children}</div>
    </div>
  `;
}

/** A group of rows with hairlines between them. */
export function Rows({ children }) {
  return html`<div class="settings-rows">${children}</div>`;
}

function rowProps({ form, issues, path, label, description, restart, kind }) {
  return { id: fieldId(path), label, description, restart, kind, changed: form.changed(path) };
}

// A row's control has no label of its own (the row has it), and most have no
// hint. The shared controls put such a control in a field wrapper only once
// it has an error to show, which builds the input anew: it loses the focus,
// and a NumberInput that loses the focus settles its text, so "70000" typed
// into the port turned into 65535 under the cursor. An empty hint keeps the
// wrapper there from the start (settings.css hides the empty line).
const NO_HINT = '';

/** A text setting. Extra props (mono, placeholder, icon) go to the Input. */
export function TextRow({ form, issues, path, label, description, restart, hint, error, ...rest }) {
  return html`
    <${SettingRow} ...${rowProps({ form, issues, path, label, description, restart })}>
      <${Input} id=${fieldId(path)} value=${form.value(path) ?? ''} onChange=${form.set(path)} hint=${hint ?? NO_HINT} error=${error ?? issues.at(path)} ...${rest} />
    <//>
  `;
}

/**
 * A number setting. Extra props (min, max, step, unit, placeholder) go to the NumberInput.
 *
 * Every number here has a default in the gateway, and an emptied field is
 * saved as "use the default" (null in the patch). The row says so while it
 * is empty, and the toast after saving names the value that came back.
 */
export function NumberRow({ form, issues, path, label, description, restart, hint, error, ...rest }) {
  const value = form.value(path) ?? null;
  return html`
    <${SettingRow} ...${rowProps({ form, issues, path, label, description, restart })}>
      <${NumberInput}
        id=${fieldId(path)}
        value=${value}
        onChange=${form.set(path)}
        placeholder="Default"
        max=${Number.MAX_SAFE_INTEGER}
        hint=${value === null ? "Empty: saving puts this back to the gateway's default." : (hint ?? NO_HINT)}
        error=${error ?? issues.at(path)}
        ...${rest}
      />
    <//>
  `;
}

/** A seconds setting: the number, and under it the same duration in words. */
export function SecondsRow({ zero, hint, form, path, ...rest }) {
  return html`<${NumberRow} form=${form} path=${path} min=${0} unit="s" hint=${hint ?? durationHint(form.value(path), zero)} ...${rest} />`;
}
/** An on/off setting. */
export function SwitchRow({ form, issues, path, label, description, restart, disabled }) {
  return html`
    <${SettingRow} ...${rowProps({ form, issues, path, label, description, restart, kind: 'switch' })}>
      <${Switch} id=${fieldId(path)} checked=${!!form.value(path)} onChange=${form.set(path)} disabled=${disabled} error=${issues.at(path)} />
    <//>
  `;
}

/** A choice from a short list of plain values. */
export function SelectRow({ form, issues, path, label, description, options, ...rest }) {
  return html`
    <${SettingRow} ...${rowProps({ form, issues, path, label, description })}>
      <${Select} id=${fieldId(path)} value=${form.value(path)} onChange=${form.set(path)} options=${options} hint=${NO_HINT} error=${issues.at(path)} ...${rest} />
    <//>
  `;
}

/**
 * A radio group whose options each carry a line of explanation.
 *
 *   options     [{ value, label, description }]
 *   value, onChange
 *   labelledBy  id of the element that names the group
 *   id          id of the group (a label's `for` cannot point at it; the
 *               group is named through labelledBy)
 *
 * One tab stop; arrows, Home and End move and select.
 */
export function OptionList({ id, options, value, onChange, labelledBy, error }) {
  const group = useRef(null);
  const onKeyDown = (event, index) => {
    const next = rovingIndex(event.key, index, options.length);
    if (next === -1) return;
    event.preventDefault();
    group.current.querySelectorAll('[role="radio"]')[next]?.focus();
    if (options[next].value !== value) onChange(options[next].value);
  };
  const checkedAt = options.findIndex((option) => option.value === value);
  const stop = checkedAt === -1 ? 0 : checkedAt;
  return html`
    <div class="settings-options-wrap">
      <div ref=${group} id=${id} class="settings-options" style=${`--cols:${options.length === 4 ? 2 : Math.min(options.length, 3)}`} role="radiogroup" aria-labelledby=${labelledBy} aria-invalid=${error ? 'true' : undefined}>
        ${options.map(
          (option, index) => html`
            <button
              key=${option.value}
              type="button"
              class="settings-option"
              role="radio"
              aria-checked=${option.value === value ? 'true' : 'false'}
              tabindex=${index === stop ? 0 : -1}
              onClick=${() => onChange(option.value)}
              onKeyDown=${(event) => onKeyDown(event, index)}
            >
              <span class="settings-option-mark" aria-hidden="true"></span>
              <span class="settings-option-text">
                <span class="settings-option-label">${option.label}</span>
                <span class="settings-option-desc">${option.description}</span>
              </span>
            </button>
          `,
        )}
      </div>
      ${error && html`<div class="field-error">${error}</div>`}
    </div>
  `;
}

/** A setting chosen from an OptionList; the options sit under the text. */
export function OptionRow({ form, issues, path, label, description, options }) {
  const id = fieldId(path);
  return html`
    <${SettingRow} ...${rowProps({ form, issues, path, label, description, kind: 'wide' })}>
      <${OptionList} id=${id} labelledBy=${`${id}-label`} options=${options} value=${form.value(path)} onChange=${form.set(path)} error=${issues.at(path)} />
    <//>
  `;
}

/** What a tab shows while the configuration is loading for the first time. */
export function FormSkeleton({ rows = 5 }) {
  return html`
    <${Panel} aria-busy="true" aria-label="Loading settings">
      <div class="settings-rows">
        ${Array.from(
          { length: rows },
          (_, i) => html`
            <div class="settings-row" key=${i}>
              <div class="settings-row-text">
                <${Skeleton} width="36%" height="14px" />
                <${Skeleton} width="82%" height="12px" />
              </div>
              <div class="settings-row-control"><${Skeleton} width="100%" height="34px" /></div>
            </div>
          `,
        )}
      </div>
    <//>
  `;
}

// ---------------------------------------------------------------------------
// Whole-document drafts (payload rules, prices)
// ---------------------------------------------------------------------------

/**
 * Load a document, edit a copy, save the copy back whole.
 *
 *   path     admin API path ("/pricing")
 *   toDraft  (data) => draft: add row ids, fill in blanks
 *   toBody   (draft) => the PUT body; also what "unsaved" is measured by
 *   load     optional (signal) => data: read the document some other way
 *            than api.get(path). The answer to the PUT is then not trusted
 *            to be the same data, and the document is read again after a save
 *
 * setDraft takes the next draft or a function of the current one.
 *
 * Returns { draft, data, setDraft, dirty, loading, error, refresh, save(), saving,
 * saveError, clearSaveError(), conflict, takeTheirs(), keepMine(), discard() }.
 *
 * A config.reloaded frame (or a poll, while the live connection is down)
 * refetches. Without unsaved edits the draft follows; with them, `conflict`
 * turns true and the caller offers both ways out.
 */
export function useListDraft(path, { toDraft, toBody, load }) {
  const liveOpen = useStore(liveState, (s) => s.status === 'open');
  const resource = useResource(load ?? path, { pollMs: liveOpen ? 0 : 20_000 });
  useLive('config.reloaded', () => secretSettled().then(resource.refresh));
  useLiveGap(resource.refresh);

  const [draft, setDraft] = useState(null);
  const [adopted, setAdopted] = useState(null); // JSON of the server version the draft started from
  const [conflict, setConflict] = useState(false);
  // Resolves { saved }: the document as the gateway holds it after the save.
  // With a `load` of its own the caller does not want the answer to the PUT
  // (it went through JSON.parse): the document is read again instead, and
  // `saved` is undefined when that read fails.
  const save = useAsync(async (body) => {
    const result = await api.put(path, body);
    if (!load) return { saved: result };
    return { saved: await load().catch(() => undefined) };
  });

  const remoteKey = useMemo(() => (resource.data === undefined ? null : JSON.stringify(toBody(toDraft(resource.data)))), [resource.data]);
  const draftKey = useMemo(() => (draft === null ? null : JSON.stringify(toBody(draft))), [draft]);
  const dirty = draft !== null && draftKey !== adopted;
  // The draft as it is when a save comes back, which may be later than the
  // draft that was sent.
  const latest = useRef(draft);
  latest.current = draft;

  const adopt = () => {
    setDraft(toDraft(resource.data));
    setAdopted(remoteKey);
    setConflict(false);
  };

  useEffect(() => {
    if (remoteKey === null || remoteKey === adopted) return;
    if (draft === null || !dirty) adopt();
    else if (remoteKey === draftKey) {
      // Someone saved exactly what is on screen: nothing is unsaved any more.
      setAdopted(remoteKey);
      setConflict(false);
    } else setConflict(true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [remoteKey]);

  return {
    draft,
    // The document as last read from the gateway.
    data: resource.data,
    setDraft: (next) => {
      if (save.error) save.reset();
      setDraft(next);
    },
    dirty,
    loading: resource.loading || (draft === null && !resource.error),
    error: draft === null ? resource.error : null,
    stale: draft !== null ? resource.error : null,
    refresh: resource.refresh,
    saving: save.loading,
    saveError: save.error,
    clearSaveError: save.reset,
    async save() {
      const sentKey = draftKey;
      const done = await save.run(toBody(draft));
      if (!done) return false;
      setConflict(false);
      if (done.saved === undefined) {
        // Saved, but it could not be read back: what was sent is what is
        // saved, and the next read brings the gateway's version of it.
        setAdopted(sentKey);
        resource.refresh();
        return true;
      }
      const key = JSON.stringify(toBody(toDraft(done.saved)));
      resource.mutate(done.saved);
      setAdopted(key);
      // The fields stay editable while the request is on its way: what was
      // typed meanwhile was not saved and stays in the draft, unsaved. Rows
      // keep their ids (and with them the focus) unless the gateway changed
      // what was sent.
      const typedMeanwhile = JSON.stringify(toBody(latest.current)) !== sentKey;
      if (!typedMeanwhile && key !== sentKey) setDraft(toDraft(done.saved));
      return true;
    },
    conflict,
    takeTheirs: adopt,
    keepMine: () => {
      // From here on "unsaved" is measured against the newer version.
      setAdopted(remoteKey);
      setConflict(false);
    },
    discard: () => {
      save.reset();
      adopt();
    },
  };
}

/** The notice both list tabs show when their document changed underneath unsaved edits. */
export function ConflictNotice({ list, noun }) {
  if (!list.conflict) return null;
  return html`
    <${Notice}
      tone="caution"
      title=${`The ${noun} changed elsewhere while you were editing`}
      action=${html`<div class="btn-group">
        <${Button} size="sm" onClick=${() => { list.takeTheirs(); focusAfterNotice(); }}>Load the new ${noun}<//>
        <${Button} size="sm" onClick=${() => { list.keepMine(); focusAfterNotice(); }}>Keep my edits<//>
      </div>`}
    >
      Loading the new ${noun} discards your edits. Keeping yours means saving will replace what was changed elsewhere.
    <//>
  `;
}
