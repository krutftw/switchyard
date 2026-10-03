// API keys page: the drawer for one key.
//
// Top to bottom: the enabled switch (applies at once), the key itself
// (masked; revealed on request and hidden again after 30 seconds), 30 days
// of usage, the editable settings (saved with the footer button), and
// examples for connecting a client. Delete is in the footer.

import { html, useEffect, useRef, useState } from '../../../vendor/preact-htm.js';
import {
  Badge,
  Button,
  CopyButton,
  Drawer,
  EmptyState,
  ErrorState,
  Field,
  Form,
  FormError,
  Input,
  KeyValue,
  Notice,
  NumberInput,
  Skeleton,
  Switch,
  confirm,
  toast,
  useIssues,
} from '../../components/index.js';
import { api } from '../../lib/api.js';
import { formatCurrency, formatDateTime, formatNumber, formatPercent, formatRelativeTime } from '../../lib/format.js';
import { useAsync, useNow, useUid } from '../../lib/hooks.js';
import { href, useLeaveGuard } from '../../lib/router.js';
import { NAME_MAX, RPM_MAX, nameProblem } from './create.js';
import { ConnectExamples, ModelPatternsField, SecretText } from './parts.js';
import { referenceName, sameList } from './util.js';

/** How long a revealed key stays on screen. */
export const REVEAL_MS = 30_000;

/** The words for what happens to clients of a key that is gone or off. */
export function refusalText(authRequired) {
  return authRequired
    ? 'Clients using it stop working at once: their requests get 401.'
    : 'Authentication is not required, so clients using it keep working, but as anonymous clients: without its model allow-list, its rate limit and its usage numbers.';
}

/**
 * Ask, then delete. Resolves true when the key is gone.
 * The dialog names the key and says what happens to its clients.
 */
export async function deleteKey(entry, authRequired) {
  const ok = await confirm({
    danger: true,
    title: `Delete key ${entry.name || entry.id}?`,
    message: `${refusalText(authRequired)} The key cannot be restored, only replaced by a new one. Its past usage stays in the statistics.`,
    confirmLabel: 'Delete key',
    action: () => api.del(`/keys/${entry.id}`),
  });
  if (ok) toast.success(`Key ${entry.name || entry.id} deleted`);
  return ok;
}

// ---------------------------------------------------------------------------
// The key itself
// ---------------------------------------------------------------------------

/** Whole seconds until `until`, never more than the reveal lasts. */
const secondsLeft = (until) => Math.min(REVEAL_MS / 1000, Math.max(0, Math.ceil((until - Date.now()) / 1000)));

// Its own clock, read four times a second: the shared one ticks once a
// second at a phase of its own, and the figure would run up to a second
// behind the moment the key is hidden.
function Countdown({ until }) {
  const [left, setLeft] = useState(() => secondsLeft(until));
  useEffect(() => {
    setLeft(secondsLeft(until));
    const timer = setInterval(() => setLeft(secondsLeft(until)), 250);
    return () => clearInterval(timer);
  }, [until]);
  return html`<span class="num">${left}s</span>`;
}

/**
 * The masked key with Reveal and Copy. `secret` is owned by the editor so
 * the examples below can use it while it is on show.
 */
function KeyReveal({ entry, secret, onSecret }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  const [hideAt, setHideAt] = useState(0);

  // Hide after 30 seconds, and at once when the tab goes to the background.
  useEffect(() => {
    if (secret == null) return undefined;
    const timer = setTimeout(() => onSecret(null), Math.max(0, hideAt - Date.now()));
    const onVisibility = () => {
      if (document.visibilityState !== 'visible') onSecret(null);
    };
    document.addEventListener('visibilitychange', onVisibility);
    return () => {
      clearTimeout(timer);
      document.removeEventListener('visibilitychange', onVisibility);
    };
  }, [secret, hideAt]);

  const fetchSecret = async () => (await api.post(`/keys/${entry.id}/reveal`)).key;

  const toggle = async () => {
    if (secret != null) {
      onSecret(null);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const value = await fetchSecret();
      setHideAt(Date.now() + REVEAL_MS);
      onSecret(value);
    } catch (cause) {
      setError(cause?.message || 'The gateway did not reveal the key.');
    } finally {
      setBusy(false);
    }
  };

  if (entry.is_reference) {
    const variable = referenceName(entry.masked);
    return html`
      <${Field} label="Key" hint=${html`A reference, not a secret: the gateway reads the key from <span class="mono">${variable ?? 'the variable'}</span> in its environment. Clients send that variable's value.`}>
        <div class="keys-reveal">
          <${SecretText} value=${entry.masked} label="Key reference" />
          <${CopyButton} value=${entry.masked} variant="secondary" size="md" label="Copy reference">Copy<//>
        </div>
      <//>
    `;
  }

  return html`
    <${Field}
      label="Key"
      error=${error ? `Could not reveal the key: ${error}` : undefined}
      hint=${secret != null
        ? html`Shown in full. Hides in <${Countdown} until=${hideAt} />.`
        : 'Shown masked. Reveal fetches the full key from the gateway and hides it again after 30 seconds.'}
    >
      <div class="keys-reveal">
        <${SecretText} value=${secret ?? entry.masked} label=${secret != null ? 'Key, shown in full' : 'Key, masked'} />
        <${Button} icon=${secret != null ? 'eye-off' : 'eye'} loading=${busy} aria-pressed=${secret != null ? 'true' : 'false'} onClick=${toggle}>
          ${secret != null ? 'Hide' : 'Reveal'}
        <//>
        <${CopyButton} value=${() => secret ?? fetchSecret()} variant="secondary" size="md" label="Copy key">Copy<//>
      </div>
    <//>
  `;
}

// ---------------------------------------------------------------------------
// Editor
// ---------------------------------------------------------------------------

// The stored values as the form holds them. GET /keys gives the name trimmed
// and the patterns without blanks and repeats, also for a file edited by
// hand, so an untouched form compares equal to them.
const storedOf = (entry) => ({ name: entry.name, models: entry.models, rpm: entry.rate_limit_rpm ?? null });

const FIELDS = ['name', 'models', 'rpm'];
const FIELD_LABELS = { name: 'Name', models: 'Allowed models', rpm: 'Rate limit' };

/** Two values of one field are the same setting (a name is compared trimmed). */
export function sameValue(field, a, b) {
  if (field === 'models') return sameList(a ?? [], b ?? []);
  if (field === 'name') return String(a ?? '').trim() === String(b ?? '').trim();
  return (a ?? null) === (b ?? null);
}

/**
 * Bring a draft up to date with the stored key after it changed elsewhere
 * (the API, another tab, a hand edit of the file).
 *
 * `base` is what the draft was taken from. A field the operator has not
 * edited (draft equals base) takes the stored value; so does one whose
 * edit the gateway now holds anyway. An edited field the gateway changed
 * as well is a conflict: it keeps the operator's value and its old base,
 * so it still counts as edited.
 *
 * Returns { draft, base, refreshed, conflicts }; `refreshed` names the
 * fields that took a new stored value while the operator had edits
 * elsewhere in the form, `conflicts` the edited fields the gateway changed.
 */
export function mergeStored(draft, base, stored) {
  const next = { draft: { ...draft }, base: { ...base }, refreshed: [], conflicts: [] };
  const edits = FIELDS.some((field) => !sameValue(field, draft[field], base[field]));
  for (const field of FIELDS) {
    if (sameValue(field, stored[field], base[field])) continue;
    const edited = !sameValue(field, draft[field], base[field]);
    if (!edited || sameValue(field, draft[field], stored[field])) {
      if (!edited) {
        next.draft[field] = stored[field];
        if (edits) next.refreshed.push(field);
      }
      next.base[field] = stored[field];
    } else {
      next.conflicts.push(field);
    }
  }
  return next;
}

/** The fields to send: edited by the operator and not already stored. */
export function pendingFields(draft, base, stored) {
  return FIELDS.filter((field) => !sameValue(field, draft[field], base[field]) && !sameValue(field, draft[field], stored[field]));
}

/** A stored value in words, for the notice about a key changed elsewhere. */
function storedText(field, value) {
  if (field === 'models') return value.length === 0 ? 'every model' : value.join(', ');
  if (field === 'rpm') return value == null ? 'no limit' : `${formatNumber(value)} rpm`;
  return value || 'no name';
}

/** "Name", "Name and Rate limit", "Name, Allowed models and Rate limit". */
function fieldList(fields) {
  const labels = fields.map((field) => FIELD_LABELS[field]);
  return labels.length < 2 ? labels.join('') : `${labels.slice(0, -1).join(', ')} and ${labels[labels.length - 1]}`;
}

/**
 * Says that the key changed elsewhere while the operator had edits open.
 * conflicts  fields edited here that the gateway changed as well
 * refreshed  fields not edited here that now show the new stored value
 */
function ChangedNotice({ conflicts, refreshed, stored, onLoad }) {
  if (conflicts.length === 0 && refreshed.length === 0) return null;
  const action = html`<${Button} size="sm" onClick=${onLoad}>Load the new version<//>`;
  const followed = refreshed.length > 0 && html`<span> ${fieldList(refreshed)} ${refreshed.length === 1 ? 'shows' : 'show'} the new stored ${refreshed.length === 1 ? 'value' : 'values'}.</span>`;
  if (conflicts.length === 0) {
    return html`
      <${Notice} tone="info" title="This key changed while the form was open" action=${action}>
        ${followed}<span> Saving sends only the fields you edited.</span>
      <//>
    `;
  }
  return html`
    <${Notice} tone="caution" title="This key changed while the form was open" action=${action}>
      ${conflicts.map(
        (field) => html`<span key=${field}>${FIELD_LABELS[field]} is now <span class=${field === 'rpm' ? undefined : 'mono'}>${storedText(field, stored[field])}</span> on the gateway. </span>`,
      )}
      <span>Saving replaces ${conflicts.length === 1 ? 'it' : 'them'} with what is shown here.</span>${followed}
    <//>
  `;
}

function KeyEditor({ entry, active, formId, others, models, listen, tls, authRequired, toggling, onToggle, onSaved, onState }) {
  const [draft, setDraft] = useState(() => storedOf(entry));
  // The stored values the draft was taken from. What differs from them is
  // what the operator changed, and only that is sent: a field changed
  // elsewhere while the form was open is never put back.
  const [base, setBase] = useState(() => storedOf(entry));
  // Fields that took a new stored value while the operator had edits.
  const [refreshed, setRefreshed] = useState([]);
  const [problems, setProblems] = useState({});
  const [touched, setTouched] = useState({});
  const [secret, setSecret] = useState(null);
  // Text typed into the allow-list field that is not a pattern yet (it
  // becomes one on Enter, comma or leaving the field). It counts as an edit,
  // so Save is ready for the click that also commits it, and closing asks.
  const [typing, setTyping] = useState(false);
  const now = useNow(10_000);
  const formRef = useRef(null);
  const focusInvalid = useRef(false);

  const save = useAsync((patch) => api.patch(`/keys/${entry.id}`, patch));
  const issues = useIssues(save.error);

  const stored = storedOf(entry);
  const name = draft.name.trim();
  const pending = pendingFields(draft, base, stored);
  const renamed = pending.includes('name');
  const changed = pending.length > 0;
  const dirty = changed || typing;
  // Edited here and changed on the gateway too: saving would replace theirs.
  const conflicts = pending.filter((field) => !sameValue(field, stored[field], base[field]));

  // The key changed elsewhere (live events and polling refresh the list):
  // fields the operator has not edited follow it.
  const storedKey = JSON.stringify(stored);
  useEffect(() => {
    const next = mergeStored(draft, base, stored);
    if (FIELDS.every((field) => sameValue(field, next.base[field], base[field]))) return;
    setDraft(next.draft);
    setBase(next.base);
    if (next.refreshed.length > 0) setRefreshed((list) => [...new Set([...list, ...next.refreshed])]);
  }, [storedKey]);

  useEffect(() => {
    onState({ dirty, saving: save.loading });
  }, [dirty, save.loading]);

  // A closed drawer shows no secret, also while it slides out.
  useEffect(() => {
    if (!active) setSecret(null);
  }, [active]);

  useEffect(() => {
    if (!focusInvalid.current) return;
    focusInvalid.current = false;
    formRef.current?.querySelector('[aria-invalid="true"]')?.focus();
  });

  const set = (field) => (value) => {
    setDraft((d) => ({ ...d, [field]: value }));
    setProblems((p) => (p[field] ? { ...p, [field]: undefined } : p));
    setTouched((t) => (t[field] ? t : { ...t, [field]: true }));
  };

  const watchTyping = (event) => {
    const input = event.target;
    if (!input?.matches?.('.tags .input-el')) return;
    if (event.type === 'blur') {
      setTyping(false);
      return;
    }
    // Read once the field has handled the event: Enter and comma empty it.
    setTimeout(() => setTyping(document.activeElement === input && input.value.trim() !== ''), 0);
  };

  const submit = async () => {
    if (save.loading || !changed) return;
    const found = { name: renamed ? nameProblem(draft.name, others) : undefined };
    setProblems(found);
    if (found.name) {
      focusInvalid.current = true;
      return;
    }
    // Only what the operator changed: a field changed elsewhere while the
    // form was open (a new allow-list, a rename) is left as it is stored.
    const patch = {};
    if (renamed) patch.name = name;
    if (pending.includes('models')) patch.models = draft.models;
    // An emptied limit is sent as null: that is how the gateway removes it.
    if (pending.includes('rpm')) patch.rate_limit_rpm = draft.rpm ?? null;
    setTouched({});
    const updated = await save.run(patch);
    if (!updated) {
      focusInvalid.current = true;
      setProblems((p) => ({ ...p }));
      return;
    }
    toast.success('Key saved');
    setDraft(storedOf(updated));
    setBase(storedOf(updated));
    setRefreshed([]);
    save.reset();
    onSaved(updated);
  };

  // Drop the edits and show the key as it is stored now.
  const loadStored = () => {
    setDraft(stored);
    setBase(stored);
    setRefreshed([]);
    setProblems({});
    setTouched({});
    save.reset();
    // The button goes with its notice: the keyboard carries on in the form.
    setTimeout(() => formRef.current?.querySelector('input')?.focus(), 0);
  };

  // The gateway names the field in every refusal (400, 409 and 422 alike).
  const nameError = problems.name ?? (touched.name ? undefined : issues.at('name'));
  const modelIssues = touched.models ? [] : issues.under('models');
  const rpmError = touched.rpm ? undefined : issues.at('rate_limit_rpm');
  const variable = entry.is_reference ? referenceName(entry.masked) : null;

  const usage = entry.usage ?? {};
  const lastUsed = usage.last_used_at
    ? `${formatRelativeTime(usage.last_used_at, now)} (${formatDateTime(usage.last_used_at)})`
    : 'No request on record in the last 30 days';

  return html`
    <div class="keys-drawer">
      ${entry.is_reference &&
      entry.resolved === false &&
      html`<${Notice} tone="caution" title=${`${variable ?? 'The variable'} is not set`}>This key reads its value from that environment variable, and the gateway does not have it. No client can use the key until it is set where the gateway runs and the gateway is restarted.<//>`}

      <${Switch}
        label="Enabled"
        hint=${entry.enabled
          ? entry.is_reference && entry.resolved === false
            ? 'On, but unusable until its variable is set.'
            : 'Clients can use this key.'
          : authRequired
            ? 'Off: requests with this key are refused with 401.'
            : 'Off: requests with this key are served as anonymous, without its limits.'}
        checked=${entry.enabled}
        disabled=${toggling}
        onChange=${(on) => onToggle(entry, on)}
      />

      <${KeyReveal} entry=${entry} secret=${secret} onSecret=${setSecret} />

      <hr />

      <section class="keys-section" aria-labelledby=${`${formId}-usage`}>
        <div class="keys-section-head">
          <h3 id=${`${formId}-usage`}>Usage, last 30 days</h3>
          <a href=${href('/requests', { key: entry.id })}>View requests</a>
        </div>
        <${KeyValue}
          items=${[
            { label: 'Requests', value: formatNumber(usage.requests) },
            {
              label: 'Errors',
              value: usage.requests > 0 && usage.errors > 0 ? `${formatNumber(usage.errors)} (${formatPercent(usage.errors / usage.requests)})` : formatNumber(usage.errors),
            },
            { label: 'Tokens', value: formatNumber(usage.tokens) },
            { label: 'Estimated cost', value: formatCurrency(usage.cost) },
            { label: 'Last used', value: lastUsed },
          ]}
        />
      </section>

      <hr />

      <section class="keys-section" aria-labelledby=${`${formId}-settings`}>
        <h3 id=${`${formId}-settings`}>Settings</h3>
        <${ChangedNotice} conflicts=${conflicts} refreshed=${changed ? refreshed.filter((field) => !pending.includes(field)) : []} stored=${stored} onLoad=${loadStored} />
        <div ref=${formRef}>
          <${Form} id=${formId} onSubmit=${submit}>
            <${Input}
              label="Name"
              value=${draft.name}
              onChange=${set('name')}
              maxLength=${NAME_MAX}
              error=${nameError}
              hint=${renamed
                ? 'New requests are shown under this name. Those made so far stay under the old one on the Requests and Usage pages; the numbers above stay with the key.'
                : 'Requests and usage are shown under this name.'}
            />
            <div class="keys-field-wrap" onInput=${watchTyping} onKeyUp=${watchTyping} onBlurCapture=${watchTyping}>
              <${ModelPatternsField}
                value=${draft.models}
                onChange=${set('models')}
                models=${models}
                error=${modelIssues.length > 0 ? modelIssues.map((issue) => issue.message).join(' ') : undefined}
              />
            </div>
            <${NumberInput}
              label="Rate limit"
              optional
              value=${draft.rpm}
              onChange=${set('rpm')}
              min=${1}
              max=${Math.max(RPM_MAX, stored.rpm ?? 0)}
              step=${1}
              unit="rpm"
              placeholder="No limit"
              hint="Requests per minute. Requests over the limit are refused with 429. Empty the field to remove the limit."
              error=${rpmError}
            />
            <${FormError} error=${save.error} issues=${issues} title="Could not save the key" />
          <//>
        </div>
      </section>

      <hr />

      <section class="keys-section" aria-labelledby=${`${formId}-connect`}>
        <div class="keys-section-head">
          <h3 id=${`${formId}-connect`}>Connect a client</h3>
          ${!entry.is_reference && html`<${Badge} outline>${secret != null ? 'With the key' : 'Key left out'}<//>`}
        </div>
        <${ConnectExamples}
          keyText=${entry.is_reference ? `<value of ${variable ?? 'the variable'}>` : (secret ?? 'YOUR_KEY')}
          patterns=${stored.models}
          models=${models.data}
          listen=${listen}
          tls=${tls}
        />
        ${!entry.is_reference && secret == null && html`<p class="field-hint">The examples say <span class="mono">YOUR_KEY</span> where the key goes. Reveal the key above to have it filled in.</p>`}
      </section>
    </div>
  `;
}

// ---------------------------------------------------------------------------
// Drawer
// ---------------------------------------------------------------------------

/**
 * id            the key's id from the URL, "" when closed
 * keys          useResource('/keys')
 * models        useResource('/models')
 * listen, tls   status.listen and status.tls
 * authRequired  status.auth_required
 * toggling      Set of key ids with an enable/disable in flight
 * onToggle      (entry, enabled) => void
 * onSaved       (updated entry) => void
 * onDeleted     (id) => void
 * onClose       () => void
 */
export function KeyDrawer({ id, keys, models, listen, tls, authRequired, toggling, onToggle, onSaved, onDeleted, onClose }) {
  const formId = useUid('keys-edit');
  // The id of the key being closed. The address may follow a moment later
  // (closing can be a step back in history); the drawer leaves at once.
  const [closing, setClosing] = useState('');
  const open = !!id && closing !== id;
  const [editor, setEditor] = useState({ dirty: false, saving: false });
  // Each opening gets a fresh editor: what was typed and discarded is gone.
  const opening = useRef({ count: 0, id: '' });
  if (id && opening.current.id !== id) opening.current = { count: opening.current.count + 1, id };
  if (!id && opening.current.id) opening.current = { count: opening.current.count, id: '' };

  const list = keys.data;
  const found = id && list ? (list.find((key) => key.id === id) ?? null) : null;
  // Keep the last key on screen while the drawer slides out.
  const last = useRef(null);
  if (found) last.current = found;
  const entry = found ?? (open ? null : last.current);

  useEffect(() => {
    if (!open) setEditor({ dirty: false, saving: false });
  }, [open]);

  useEffect(() => {
    if (!id) setClosing('');
  }, [id]);

  // Should the address never follow (the step back was held up), show the
  // drawer again rather than leave the page out of step with its address.
  useEffect(() => {
    if (!closing) return undefined;
    const timer = setTimeout(() => setClosing(''), 2000);
    return () => clearTimeout(timer);
  }, [closing]);

  const confirmDiscard = () =>
    confirm({
      danger: true,
      title: 'Discard unsaved changes?',
      message: `Your changes to ${entry?.name || 'this key'} have not been saved.`,
      confirmLabel: 'Discard changes',
      cancelLabel: 'Keep editing',
    });

  // Back, a link, the command palette or signing out would otherwise take
  // the drawer away without the question its own Close asks. The drawer is
  // part of the address (?open=), so another key there is leaving as well.
  const guard = useLeaveGuard(open && editor.dirty, {
    ask: confirmDiscard,
    matters: (to, from) => !to || to.path !== from.path || to.query.open !== from.query.open,
  });

  const close = () => {
    guard.release();
    setClosing(id);
    onClose();
  };

  const requestClose = async () => {
    if (editor.saving) return;
    if (editor.dirty && entry && !(await confirmDiscard())) return;
    close();
  };

  const remove = async () => {
    if (!entry) return;
    if (!(await deleteKey(entry, authRequired))) return;
    guard.release();
    setClosing(entry.id);
    onDeleted(entry.id);
  };

  let body;
  if (entry) {
    body = html`
      <${KeyEditor}
        key=${`${entry.id}:${opening.current.count}`}
        entry=${entry}
        active=${open}
        formId=${formId}
        others=${(list ?? []).filter((key) => key.id !== entry.id)}
        models=${models}
        listen=${listen}
        tls=${tls}
        authRequired=${authRequired}
        toggling=${toggling.has(entry.id)}
        onToggle=${onToggle}
        onSaved=${onSaved}
        onState=${setEditor}
      />
    `;
  } else if (keys.loading || (!list && !keys.error)) {
    body = html`<div class="stack" aria-busy="true"><${Skeleton} width="40%" height="20px" /><${Skeleton} lines=${4} /><${Skeleton} lines=${3} /></div>`;
  } else if (!list) {
    body = html`<${ErrorState} title="Could not load the key" error=${keys.error} onRetry=${keys.refresh} />`;
  } else {
    body = html`
      <${EmptyState}
        icon="key"
        title="No key with this id"
        description="It may have been deleted, or its value changed in the configuration file, which gives it a new id."
        action=${html`<${Button} onClick=${close}>Back to the list<//>`}
      />
    `;
  }

  return html`
    <${Drawer}
      open=${open}
      onClose=${requestClose}
      title=${entry ? entry.name || 'Unnamed key' : 'Client key'}
      subtitle=${entry?.id ?? (id || undefined)}
      dismissable=${!editor.saving}
      class="keys-drawer-layer"
      footer=${entry
        ? html`
            <${Button} class="keys-foot-start" variant="danger-quiet" icon="trash" disabled=${editor.saving} onClick=${remove}>Delete key<//>
            <${Button} disabled=${editor.saving} onClick=${requestClose}>${editor.dirty ? 'Cancel' : 'Close'}<//>
            <${Button} type="submit" form=${formId} variant="primary" loading=${editor.saving} disabled=${!editor.dirty}>Save changes<//>
          `
        : null}
    >
      ${body}
    <//>
  `;
}
