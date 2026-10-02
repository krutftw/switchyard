// Form controls and the helpers that tie API validation issues to fields.
//
// Every control takes the same field props and draws its own label, hint and
// error when `label` is given:
//
//   label     visible label (always give one; placeholders are not labels)
//   hint      one line of help under the control
//   error     message shown under the control in the stop colour; also marks
//             the control invalid. Usually issues.at('path') (see useIssues).
//   warning   message shown under the control in the caution colour: the
//             value is allowed and will be saved, but the user should know
//             something about it ("Keys this short are easy to guess").
//             Takes the hint's place while it shows; an error takes both.
//   optional  adds "optional" after the label
//
// and reports changes the same way: onChange(value), with the new value
// first, not an event.
//
//   const [name, setName] = useState('');
//   html`<${Input} label="Name" value=${name} onChange=${setName} error=${issues.at('name')} />`

import { html, useEffect, useLayoutEffect, useMemo, useRef, useState } from '../../vendor/preact-htm.js';
import { cx } from '../lib/dom.js';
import { sentence } from '../lib/format.js';
import { useUid } from '../lib/hooks.js';
import { CopyButton, IconButton } from './button.js';
import { Icon } from './icons.js';
import { Notice } from './surface.js';

// ---------------------------------------------------------------------------
// Field
// ---------------------------------------------------------------------------

/**
 * Label + control + hint + error. The controls below use it for you; use it
 * directly to label a custom control:
 *
 *   html`<${Field} label="Strategy" htmlFor=${id} hint="How credentials are picked.">
 *     <${Segmented} ... />
 *   <//>`
 */
export function Field({ label, hint, error, warning, optional = false, htmlFor, id, class: className, children }) {
  const base = id || htmlFor;
  return html`
    <div class=${cx('field', className)}>
      ${label != null &&
      html`<label class="field-label" for=${htmlFor}>
        <span>${label}</span>
        ${optional && html`<span class="field-optional">optional</span>`}
      </label>`}
      ${children}
      ${error
        ? html`<div class="field-error" id=${base ? `${base}-error` : undefined}>
            <${Icon} name="alert-circle" size=${14} /><span>${error}</span>
          </div>`
        : warning
          ? html`<div class="field-warning" id=${base ? `${base}-warning` : undefined}>
              <${Icon} name="alert" size=${14} /><span>${warning}</span>
            </div>`
          : hint != null && html`<div class="field-hint" id=${base ? `${base}-hint` : undefined}>${hint}</div>`}
    </div>
  `;
}

/** aria wiring shared by the controls. */
function describe(id, { hint, error, warning }) {
  return {
    'aria-invalid': error ? 'true' : undefined,
    'aria-describedby': error ? `${id}-error` : warning ? `${id}-warning` : hint != null ? `${id}-hint` : undefined,
  };
}

function wrap(control, id, { label, hint, error, warning, optional, class: className }) {
  if (label == null && hint == null && !error && !warning) return control;
  return html`<${Field} label=${label} hint=${hint} error=${error} warning=${warning} optional=${optional} htmlFor=${id} class=${label != null ? className : undefined}>${control}<//>`;
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

/**
 * A single-line text field.
 *
 * value, onChange(value)
 * type         "text" (default), "search", "url", "email"
 * icon         icon name shown inside, before the text
 * suffix       short text after the value ("ms", "/v1")
 * actions      buttons inside the field, at the end
 * clearable    a clear button (an x) at the end while the field holds text.
 *              The browser's own one on type="search" is hidden (base.css),
 *              so this is on by default for a search field that brings no
 *              `actions` of its own, and off otherwise. Pass true to have
 *              it next to your actions, false to leave it out. Clearing
 *              calls onChange(''), then onClear, and puts the focus back in
 *              the field.
 * onClear      called after the clear button emptied the field, for a
 *              search that is applied on Enter and must be applied now
 * clearLabel   accessible name and tooltip of that button (default "Clear")
 * mono         monospace, for identifiers, URLs, model names
 * size         "md" | "sm" | "lg"
 * autoFocus    take focus when the surrounding modal or drawer opens
 * onEnter      called when Enter is pressed
 * inputRef     ref to the <input>
 * Other props (placeholder, disabled, readOnly, name, autocomplete,
 * maxLength, inputmode) go to the <input>.
 *
 * With neither `value` nor `onChange` the field keeps its own text.
 */
export function Input({
  label,
  hint,
  error,
  warning,
  optional,
  id: idProp,
  class: className,
  value,
  onChange,
  type = 'text',
  icon,
  suffix,
  actions,
  clearable,
  onClear,
  clearLabel = 'Clear',
  mono = false,
  size = 'md',
  autoFocus = false,
  onEnter,
  inputRef,
  disabled = false,
  ...rest
}) {
  const uid = useUid('in');
  const id = idProp || uid;
  // A field nobody controls remembers what was typed into it, so it too
  // knows when there is something to clear.
  const uncontrolled = value === undefined && onChange === undefined;
  const [own, setOwn] = useState('');
  const text = uncontrolled ? own : String(value ?? '');
  const change = (next, event) => {
    if (uncontrolled) setOwn(next);
    else onChange?.(next, event);
  };
  const showClear = (clearable ?? (type === 'search' && actions === undefined)) && text !== '' && !disabled && !rest.readOnly;
  const clear = (event) => {
    const field = event.currentTarget.closest('.input')?.querySelector('input');
    change('', event);
    onClear?.(event);
    field?.focus();
  };
  const control = html`
    <div
      class=${cx('input', label == null && className)}
      data-size=${size === 'md' ? undefined : size}
      data-mono=${mono ? '' : undefined}
      data-invalid=${error ? '' : undefined}
      data-warning=${warning && !error ? '' : undefined}
      data-disabled=${disabled ? '' : undefined}
    >
      ${icon && html`<${Icon} name=${icon} />`}
      <input
        ref=${inputRef}
        id=${id}
        class="input-el"
        type=${type}
        value=${text}
        disabled=${disabled}
        spellcheck=${false}
        autocapitalize="off"
        autocorrect="off"
        data-autofocus=${autoFocus ? '' : undefined}
        onInput=${(event) => change(event.target.value, event)}
        onKeyDown=${onEnter
          ? (event) => {
              if (event.key === 'Enter' && !event.isComposing) onEnter(event);
            }
          : undefined}
        ...${describe(id, { hint, error, warning })}
        ...${rest}
      />
      ${suffix != null && html`<span class="input-affix">${suffix}</span>`}
      ${(actions || showClear) &&
      html`<span class="input-actions">
        ${showClear &&
        html`<${IconButton}
          icon="x"
          label=${clearLabel}
          size="sm"
          class="input-clear"
          onMouseDown=${(event) => event.preventDefault() /* the field keeps the focus */}
          onClick=${clear}
        />`}
        ${actions}
      </span>`}
    </div>
  `;
  return wrap(control, id, { label, hint, error, warning, optional, class: className });
}

// ---------------------------------------------------------------------------
// Textarea
// ---------------------------------------------------------------------------

/**
 * A multi-line text field.
 *
 * rows      visible lines (default 4)
 * mono      monospace, for JSON, TOML, headers
 * autoGrow  grow with the content instead of scrolling (up to maxRows)
 */
export function Textarea({
  label,
  hint,
  error,
  warning,
  optional,
  id: idProp,
  class: className,
  value,
  onChange,
  rows = 4,
  maxRows = 24,
  mono = false,
  autoGrow = false,
  autoFocus = false,
  disabled = false,
  ...rest
}) {
  const uid = useUid('ta');
  const id = idProp || uid;
  const ref = useRef(null);

  useLayoutEffect(() => {
    const el = ref.current;
    if (!autoGrow || !el) return;
    el.style.height = 'auto';
    const line = parseFloat(getComputedStyle(el).lineHeight) || 21;
    el.style.height = `${Math.min(el.scrollHeight, line * maxRows + 16)}px`;
  }, [autoGrow, value, maxRows]);

  const control = html`
    <div
      class=${cx('input', label == null && className)}
      data-textarea=""
      data-mono=${mono ? '' : undefined}
      data-invalid=${error ? '' : undefined}
      data-warning=${warning && !error ? '' : undefined}
      data-disabled=${disabled ? '' : undefined}
    >
      <textarea
        ref=${ref}
        id=${id}
        class="input-el"
        rows=${rows}
        value=${value ?? ''}
        disabled=${disabled}
        spellcheck=${false}
        data-autofocus=${autoFocus ? '' : undefined}
        onInput=${(event) => onChange?.(event.target.value, event)}
        ...${describe(id, { hint, error, warning })}
        ...${rest}
      ></textarea>
    </div>
  `;
  return wrap(control, id, { label, hint, error, warning, optional, class: className });
}

// ---------------------------------------------------------------------------
// Select
// ---------------------------------------------------------------------------

/**
 * A native select, styled. Native keeps the platform picker on phones and
 * type-ahead on desktop.
 *
 * options      strings, or [{ value, label, disabled? }]
 * placeholder  an empty first option with this text
 */
export function Select({
  label,
  hint,
  error,
  warning,
  optional,
  id: idProp,
  class: className,
  value,
  onChange,
  options,
  placeholder,
  size = 'md',
  disabled = false,
  ...rest
}) {
  const uid = useUid('sel');
  const id = idProp || uid;
  const items = options.map((o) => (typeof o === 'object' ? o : { value: o, label: String(o) }));
  const control = html`
    <div
      class=${cx('input', label == null && className)}
      data-select=""
      data-size=${size === 'md' ? undefined : size}
      data-invalid=${error ? '' : undefined}
      data-warning=${warning && !error ? '' : undefined}
      data-disabled=${disabled ? '' : undefined}
    >
      <select
        id=${id}
        class="input-el"
        value=${value ?? ''}
        disabled=${disabled}
        onChange=${(event) => onChange?.(event.target.value, event)}
        ...${describe(id, { hint, error, warning })}
        ...${rest}
      >
        ${placeholder != null && html`<option value="">${placeholder}</option>`}
        ${items.map((o) => html`<option key=${String(o.value)} value=${o.value} disabled=${o.disabled}>${o.label}</option>`)}
      </select>
      <${Icon} name="chevron-down" size=${14} />
    </div>
  `;
  return wrap(control, id, { label, hint, error, warning, optional, class: className });
}

// ---------------------------------------------------------------------------
// Switch, Checkbox
// ---------------------------------------------------------------------------

/**
 * An on/off setting that takes effect as a state ("Capture request bodies").
 * checked, onChange(checked), label, hint, error, disabled.
 */
export function Switch({ label, hint, error, warning, id: idProp, class: className, checked = false, onChange, disabled = false, ...rest }) {
  const uid = useUid('sw');
  const id = idProp || uid;
  return html`
    <div class=${cx('field', className)} data-inline="">
      <button
        id=${id}
        type="button"
        class="switch"
        role="switch"
        aria-checked=${checked ? 'true' : 'false'}
        disabled=${disabled}
        onClick=${() => onChange?.(!checked)}
        ...${describe(id, { hint, error, warning })}
        ...${rest}
      ></button>
      ${(label != null || hint != null || error || warning) &&
      html`
        <div class="field-inline-text">
          ${label != null && html`<label class="choice-label" for=${id}>${label}</label>`}
          ${error
            ? html`<div class="field-error" id=${`${id}-error`}><${Icon} name="alert-circle" size=${14} /><span>${error}</span></div>`
            : warning
              ? html`<div class="field-warning" id=${`${id}-warning`}><${Icon} name="alert" size=${14} /><span>${warning}</span></div>`
              : hint != null && html`<div class="field-hint" id=${`${id}-hint`}>${hint}</div>`}
        </div>
      `}
    </div>
  `;
}

/**
 * A tick box, for choosing items or agreeing to one thing in a form.
 * checked, onChange(checked), indeterminate, label, hint, error, disabled.
 */
export function Checkbox({ label, hint, error, warning, id: idProp, class: className, checked = false, indeterminate = false, onChange, disabled = false, ...rest }) {
  const uid = useUid('cb');
  const id = idProp || uid;
  const ref = useRef(null);
  useEffect(() => {
    if (ref.current) ref.current.indeterminate = indeterminate && !checked;
  }, [indeterminate, checked]);
  return html`
    <div class=${cx('field', className)} data-inline="">
      <input
        ref=${ref}
        id=${id}
        type="checkbox"
        class="check"
        checked=${checked}
        disabled=${disabled}
        onChange=${(event) => onChange?.(event.target.checked, event)}
        ...${describe(id, { hint, error, warning })}
        ...${rest}
      />
      ${(label != null || hint != null || error || warning) &&
      html`
        <div class="field-inline-text">
          ${label != null && html`<label class="choice-label" for=${id}>${label}</label>`}
          ${error
            ? html`<div class="field-error" id=${`${id}-error`}><${Icon} name="alert-circle" size=${14} /><span>${error}</span></div>`
            : warning
              ? html`<div class="field-warning" id=${`${id}-warning`}><${Icon} name="alert" size=${14} /><span>${warning}</span></div>`
              : hint != null && html`<div class="field-hint" id=${`${id}-hint`}>${hint}</div>`}
        </div>
      `}
    </div>
  `;
}

// ---------------------------------------------------------------------------
// NumberInput
// ---------------------------------------------------------------------------

function parseNumber(text) {
  const t = String(text).trim().replace(/,/g, '');
  if (t === '') return null;
  const n = Number(t);
  return Number.isFinite(n) ? n : NaN;
}

/**
 * Why the text in a NumberInput cannot be taken as its value, as a sentence
 * for the user, or null when it can (an empty field is "not set", which is
 * fine). Exported for tests.
 *
 * @param {string} text  what is in the field
 * @param {{ min?: number, max?: number, whole?: boolean }} limits
 */
export function numberProblem(text, { min, max, whole = false } = {}) {
  const n = parseNumber(text);
  if (n === null) return null;
  if (Number.isNaN(n)) return 'Enter a number.';
  if (whole && !Number.isInteger(n)) return 'Enter a whole number.';
  const low = min != null && n < min;
  const high = max != null && n > max;
  if (!low && !high) return null;
  if (min != null && max != null) return `Enter a number from ${min} to ${max}.`;
  return min != null ? `Enter ${min} or more.` : `Enter ${max} or less.`;
}

/**
 * A number with stepper buttons.
 *
 * value     number, or null for "not set"
 * onChange  (number | null) => void; called with every valid edit
 * min, max  the allowed range
 * step      amount per step and per arrow key (default 1). A whole-number
 *           step means whole numbers only.
 * unit      text after the value ("s", "rpm", "MB")
 * placeholder  shown when null: use it to say what empty means ("no limit")
 *
 * What is typed is reported as it is typed, as long as it is a value the
 * field allows. Text that is not (out of range, a fraction where whole
 * numbers are wanted, not a number) is not reported: `value` keeps the last
 * good number, the field turns invalid and says what it wants, and a
 * surrounding <Form> refuses to submit, so the form can never save a number
 * other than the one on screen. Leaving the field settles it: out-of-range
 * numbers are clamped, fractions rounded, anything else reverts.
 */
export function NumberInput({
  label,
  hint,
  error,
  warning,
  optional,
  id: idProp,
  class: className,
  value,
  onChange,
  min,
  max,
  step = 1,
  unit,
  placeholder,
  size = 'md',
  disabled = false,
  ...rest
}) {
  const uid = useUid('num');
  const id = idProp || uid;
  const whole = Number.isInteger(step);
  const [textState, setText] = useState(value == null ? '' : String(value));
  // Why the text on screen is not (yet) the value, or null when it is.
  const [problemState, setProblem] = useState(null);
  let text = textState;
  let problem = problemState;

  // Follow outside changes (a reset, a reload), but never rewrite what the
  // user is typing. A new `value` is either one of our own reports coming
  // back, which must leave the text alone, or a change from outside, which
  // replaces it.
  //
  // This is decided here, while rendering, against the list of reports not
  // yet seen back. Doing it in an effect, or by comparing `value` with the
  // text, races with typing: the effect of one render runs a frame later,
  // when the text may already be a keystroke further ("500" reported,
  // "5000" on screen), and the field would put "500" back.
  const sent = useRef([]);
  const seen = useRef(value);
  if (!Object.is(value, seen.current)) {
    seen.current = value;
    const at = sent.current.lastIndexOf(value);
    if (at !== -1) {
      sent.current = sent.current.slice(at + 1);
    } else {
      sent.current = [];
      text = value == null ? '' : String(value);
      problem = null;
      setText(text);
      setProblem(null);
    }
  }

  const report = (n) => {
    if (n === value) return;
    sent.current = [...sent.current.slice(-7), n];
    onChange?.(n);
  };

  const clamp = (n) => {
    let v = whole ? Math.round(n) : Number(n.toFixed(6));
    if (min != null && v < min) v = min;
    if (max != null && v > max) v = max;
    return v;
  };

  const commit = (n) => {
    setText(n == null ? '' : String(n));
    setProblem(null);
    // The field now shows exactly `n`: older reports no longer matter.
    sent.current = [];
    report(n);
  };

  const stepBy = (direction) => {
    const current = parseNumber(text);
    const base = current == null || Number.isNaN(current) ? (direction > 0 ? (min ?? 0) - step : (max ?? min ?? 0) + step) : current;
    commit(clamp(base + direction * step));
  };

  const control = html`
    <div
      class=${cx('input', label == null && className)}
      data-size=${size === 'md' ? undefined : size}
      data-mono=""
      data-invalid=${error || problem ? '' : undefined}
      data-warning=${warning && !error && !problem ? '' : undefined}
      data-disabled=${disabled ? '' : undefined}
    >
      <input
        id=${id}
        class="input-el"
        type="text"
        data-entry-invalid=${problem ? '' : undefined}
        inputmode=${whole ? 'numeric' : 'decimal'}
        autocomplete="off"
        role="spinbutton"
        aria-valuenow=${value ?? undefined}
        aria-valuemin=${min}
        aria-valuemax=${max}
        value=${text}
        placeholder=${placeholder}
        disabled=${disabled}
        onInput=${(event) => {
          const next = event.target.value;
          setText(next);
          // Text the field cannot accept is never reported, and never
          // silently replaced under the cursor either ("5" on the way to
          // "50" must not be rewritten): it is marked, and the value stays
          // at the last good number until the text is one.
          const issue = numberProblem(next, { min, max, whole });
          setProblem(issue);
          if (issue) return;
          report(parseNumber(next));
        }}
        onBlur=${(event) => {
          // From the element, not from state: a blur can follow the last
          // keystroke before the next render.
          const n = parseNumber(event.target.value);
          if (Number.isNaN(n)) commit(value ?? null);
          else commit(n == null ? null : clamp(n));
        }}
        onKeyDown=${(event) => {
          if (event.key === 'ArrowUp') {
            event.preventDefault();
            stepBy(1);
          } else if (event.key === 'ArrowDown') {
            event.preventDefault();
            stepBy(-1);
          }
        }}
        ...${describe(id, { hint, error: error || problem, warning })}
        ...${rest}
      />
      ${unit != null && html`<span class="input-affix">${unit}</span>`}
      <span class="num-steps">
        <button type="button" class="num-step" tabindex="-1" aria-label="Increase" disabled=${disabled || (max != null && value != null && value >= max)} onClick=${() => stepBy(1)}>
          <${Icon} name="chevron-up" size=${12} />
        </button>
        <button type="button" class="num-step" tabindex="-1" aria-label="Decrease" disabled=${disabled || (min != null && value != null && value <= min)} onClick=${() => stepBy(-1)}>
          <${Icon} name="chevron-down" size=${12} />
        </button>
      </span>
    </div>
  `;
  return wrap(control, id, { label, hint, error: error || problem, warning, optional, class: className });
}

// ---------------------------------------------------------------------------
// TagInput
// ---------------------------------------------------------------------------

/**
 * A list of short strings: model patterns, allowed models, excluded models.
 *
 * value     string[]
 * onChange  (string[]) => void
 * validate  (tag) => error message or null; invalid tags stay visible,
 *           marked in the stop colour, so the user can see what to fix
 * placeholder  shown while the list is empty
 *
 * Enter, comma or leaving the field adds what was typed. Pasting a list
 * separated by commas, spaces or new lines adds every entry. Backspace in an
 * empty field removes the last tag. Duplicates are ignored.
 */
export function TagInput({ label, hint, error, warning, optional, id: idProp, class: className, value = [], onChange, validate, placeholder, disabled = false, ...rest }) {
  const uid = useUid('tags');
  const id = idProp || uid;
  const [draft, setDraft] = useState('');
  const input = useRef(null);

  const add = (raw) => {
    const parts = String(raw)
      .split(/[\s,]+/)
      .map((p) => p.trim())
      .filter(Boolean);
    if (parts.length === 0) return false;
    const seen = new Set(value.map((v) => v.toLowerCase()));
    const next = [...value];
    for (const part of parts) {
      if (seen.has(part.toLowerCase())) continue;
      seen.add(part.toLowerCase());
      next.push(part);
    }
    if (next.length !== value.length) onChange?.(next);
    return true;
  };

  const removeAt = (index) => {
    onChange?.(value.filter((_, i) => i !== index));
    input.current?.focus();
  };

  const control = html`
    <div
      class=${cx('input', 'tags', label == null && className)}
      data-invalid=${error ? '' : undefined}
      data-warning=${warning && !error ? '' : undefined}
      data-disabled=${disabled ? '' : undefined}
      onClick=${(event) => {
        if (event.target === event.currentTarget) input.current?.focus();
      }}
    >
      ${value.map((tag, index) => {
        const problem = validate?.(tag) || null;
        return html`
          <span class="tag" key=${tag} data-invalid=${problem ? '' : undefined} title=${problem || undefined}>
            <span class="tag-text">${tag}</span>
            <button type="button" class="tag-x" aria-label=${`Remove ${tag}`} disabled=${disabled} onClick=${() => removeAt(index)}>
              <${Icon} name="x" size=${12} />
            </button>
          </span>
        `;
      })}
      <input
        ref=${input}
        id=${id}
        class="input-el mono"
        type="text"
        value=${draft}
        placeholder=${value.length === 0 ? placeholder : undefined}
        disabled=${disabled}
        spellcheck=${false}
        autocapitalize="off"
        autocomplete="off"
        onInput=${(event) => {
          const text = event.target.value;
          // A typed or pasted separator commits everything before it.
          if (/[,\n]/.test(text) || /\S\s/.test(text)) {
            add(text);
            setDraft('');
            // Clear the field itself as well. When every pasted entry is
            // already in the list nothing changes: the draft was '' and
            // stays '', the value is the same array, so nothing renders
            // again and the pasted text would be left standing in the field.
            event.target.value = '';
          } else {
            setDraft(text);
          }
        }}
        onKeyDown=${(event) => {
          if (event.isComposing) return;
          if (event.key === 'Enter' || event.key === ',') {
            if (draft.trim()) {
              event.preventDefault();
              add(draft);
              setDraft('');
            } else if (event.key === ',') {
              event.preventDefault();
            }
          } else if (event.key === 'Backspace' && draft === '' && value.length > 0) {
            event.preventDefault();
            onChange?.(value.slice(0, -1));
          }
        }}
        onBlur=${() => {
          if (draft.trim()) {
            add(draft);
            setDraft('');
          }
        }}
        ...${describe(id, { hint, error, warning })}
        ...${rest}
      />
    </div>
  `;
  return wrap(control, id, { label, hint, error, warning, optional, class: className });
}

// ---------------------------------------------------------------------------
// SecretInput
// ---------------------------------------------------------------------------

/**
 * A secret: masked, with a reveal toggle and optional copy.
 *
 * Two uses:
 *
 * 1. Entering a secret (API key, admin secret):
 *      html`<${SecretInput} label="API key" value=${key} onChange=${setKey}
 *            placeholder="Leave empty to keep the current key" />`
 *    The eye shows what was typed. Password managers are told to stay out
 *    unless `autocomplete` is set (the sign-in page sets "current-password").
 *
 * 2. Showing a stored secret the server masks (client keys):
 *      html`<${SecretInput} label="Key" value=${key.masked} readOnly copy
 *            onReveal=${() => api.post(`/keys/${key.id}/reveal`).then((r) => r.key)} />`
 *    The eye fetches the real value with onReveal, shows it, and hides it
 *    again on the second press. Copy fetches it too.
 *
 * copy   show a copy button
 */
export function SecretInput({
  label,
  hint,
  error,
  warning,
  optional,
  id: idProp,
  class: className,
  value,
  onChange,
  onReveal,
  copy = false,
  readOnly = false,
  autocomplete,
  autoFocus = false,
  size = 'md',
  disabled = false,
  ...rest
}) {
  const uid = useUid('sec');
  const id = idProp || uid;
  const [visible, setVisible] = useState(false);
  const [revealed, setRevealed] = useState(null);
  const [busy, setBusy] = useState(false);
  const [revealError, setRevealError] = useState(null);
  const remote = typeof onReveal === 'function';

  // A new masked value means a different secret: forget the old reveal.
  useEffect(() => {
    setRevealed(null);
    setVisible(false);
    setRevealError(null);
  }, [remote ? value : null]);

  const fetchSecret = async () => {
    if (revealed != null) return revealed;
    const secret = await onReveal();
    setRevealed(secret);
    return secret;
  };

  const toggle = async () => {
    if (!remote) {
      setVisible((v) => !v);
      return;
    }
    if (visible) {
      setVisible(false);
      return;
    }
    setBusy(true);
    setRevealError(null);
    try {
      await fetchSecret();
      setVisible(true);
    } catch (cause) {
      setRevealError(cause?.message || 'Could not reveal the secret.');
    } finally {
      setBusy(false);
    }
  };

  const shown = remote ? (visible && revealed != null ? revealed : (value ?? '')) : (value ?? '');
  const type = remote || visible ? 'text' : 'password';
  const managerOff = autocomplete == null;

  const control = html`
    <div
      class=${cx('input', label == null && className)}
      data-size=${size === 'md' ? undefined : size}
      data-mono=""
      data-invalid=${error || revealError ? '' : undefined}
      data-warning=${warning && !error && !revealError ? '' : undefined}
      data-disabled=${disabled ? '' : undefined}
    >
      <input
        id=${id}
        class="input-el"
        type=${type}
        value=${shown}
        readOnly=${readOnly || remote}
        disabled=${disabled}
        spellcheck=${false}
        autocapitalize="off"
        autocorrect="off"
        autocomplete=${autocomplete ?? 'off'}
        data-1p-ignore=${managerOff ? '' : undefined}
        data-lpignore=${managerOff ? 'true' : undefined}
        data-bwignore=${managerOff ? '' : undefined}
        data-autofocus=${autoFocus ? '' : undefined}
        onInput=${(event) => onChange?.(event.target.value, event)}
        ...${describe(id, { hint, error: error || revealError, warning })}
        ...${rest}
      />
      <span class="input-actions">
        <${IconButton}
          icon=${visible ? 'eye-off' : 'eye'}
          label=${visible ? 'Hide' : 'Show'}
          size="sm"
          loading=${busy}
          disabled=${disabled}
          aria-pressed=${visible ? 'true' : 'false'}
          onClick=${toggle}
        />
        ${copy && html`<${CopyButton} value=${remote ? fetchSecret : () => value ?? ''} label="Copy" disabled=${disabled || (!remote && !value)} />`}
      </span>
    </div>
  `;
  return wrap(control, id, { label, hint, error: error || revealError, warning, optional, class: className });
}

// ---------------------------------------------------------------------------
// Form helpers
// ---------------------------------------------------------------------------

/**
 * A <form> that calls onSubmit() without reloading the page. Enter in any
 * field submits; put a type="submit" Button in FormActions.
 *
 * It does not submit while a control shows text it could not report as its
 * value (a NumberInput holding "5000" where the maximum is 1000): what would
 * be saved is not what is on screen. Focus goes to that control, which
 * already says what it wants. Controls flag this state with
 * data-entry-invalid on their input.
 */
export function Form({ onSubmit, class: className, children, ...rest }) {
  return html`
    <form
      class=${cx('form', className)}
      novalidate
      onSubmit=${(event) => {
        event.preventDefault();
        const unsettled = event.currentTarget.querySelector('[data-entry-invalid]');
        if (unsettled) {
          unsettled.focus();
          return;
        }
        onSubmit?.(event);
      }}
      ...${rest}
    >
      ${children}
    </form>
  `;
}

/** Fields side by side where there is room, stacked where there is not. */
export function FormRow({ class: className, children }) {
  return html`<div class=${cx('form-row', className)}>${children}</div>`;
}

/** The button row of a form: right-aligned, primary action last. */
export function FormActions({ class: className, children }) {
  return html`<div class=${cx('form-actions', className)}>${children}</div>`;
}

/** "providers[0].api_keys[1]" and "providers.0.api_keys.1" compare equal. */
function normalizePath(path) {
  return String(path ?? '')
    .replace(/\[(\w+)\]/g, '.$1')
    .replace(/^\./, '');
}

/**
 * Turn an ApiError's issues ([{ path, message }]) into per-field messages.
 *
 *   const save = useAsync((body) => api.put(`/providers/${name}`, body));
 *   const issues = useIssues(save.error);
 *
 *   html`<${Form} onSubmit=${() => save.run(draft)}>
 *     <${Input} label="Base URL" value=${draft.base_url} onChange=${set('base_url')}
 *               error=${issues.at('base_url')} />
 *     <${FormError} error=${save.error} issues=${issues} />
 *     <${FormActions}><${Button} type="submit" variant="primary" loading=${save.loading}>Save<//><//>
 *   <//>`
 *
 * issues.at(path)     the message for exactly that path, or undefined
 * issues.under(path)  every issue at or below that path (for a list editor)
 * issues.rest()       issues no field has asked for; FormError lists these
 *
 * Call at()/under() in the same render function that renders FormError so it
 * knows which issues already have a home.
 */
export function useIssues(error) {
  return useMemo(() => {
    const list = (error?.issues ?? []).map((issue) => ({ path: normalizePath(issue.path), message: issue.message }));
    const claimed = new Set();
    return {
      all: list,
      at(path) {
        const key = normalizePath(path);
        const hits = list.filter((issue) => issue.path === key);
        hits.forEach((issue) => claimed.add(issue));
        return hits.length ? hits.map((issue) => issue.message).join(' ') : undefined;
      },
      under(path) {
        const key = normalizePath(path);
        const hits = list.filter((issue) => issue.path === key || issue.path.startsWith(`${key}.`));
        hits.forEach((issue) => claimed.add(issue));
        return hits;
      },
      rest() {
        return list.filter((issue) => !claimed.has(issue));
      },
    };
  }, [error]);
}

/**
 * The form-level error: what went wrong, then any issues that no field
 * showed. Renders nothing while there is no error.
 *
 * error   ApiError from useAsync
 * issues  the object from useIssues(error); optional
 * title   overrides the heading (default "Could not save")
 *
 * The gateway's message is printed as a sentence (lib/format.js, sentence):
 * capital first word, closing full stop. "Check the highlighted field."
 * follows it, and would otherwise run straight on from a message that has
 * no full stop of its own.
 */
export function FormError({ error, issues, title = 'Could not save', class: className }) {
  if (!error || error.aborted) return null;
  const rest = issues ? issues.rest() : (error.issues ?? []);
  const fieldCount = (error.issues?.length ?? 0) - rest.length;
  return html`
    <${Notice} tone="stop" title=${title} class=${className}>
      <span>${sentence(error.message)}</span>
      ${fieldCount > 0 && html`<span> Check the highlighted ${fieldCount === 1 ? 'field' : 'fields'}.</span>`}
      ${rest.length > 0 &&
      html`<ul class="issue-list">
        ${rest.map((issue, i) => html`<li key=${i}>${issue.path && html`<span class="issue-path">${issue.path}</span>`}<span>${issue.message}</span></li>`)}
      </ul>`}
    <//>
  `;
}
