// API keys page: the "Create client key" modal.
//
// Two stages in one dialog. The form; then, once the gateway has made the
// key, the key itself in full with ready-to-paste examples. The second stage
// has one way out, the "I have copied it" button: Escape, the scrim and the
// close button are off, so the only showing of the key cannot be lost to a
// stray click. Back, Forward, a link and an edited address are held off by a
// leave guard, and the shell keeps the command palette shut over a dialog
// that is not dismissable.
//
// The full key lives in this component's state and nowhere else: not in the
// URL, not in storage, not in the console. Whatever the dialog held (the
// created key, or a key value typed into a form that was then cancelled) is
// dropped when it closes.

import { html, useEffect, useRef, useState } from '../../../vendor/preact-htm.js';
import { Button, Checkbox, CopyButton, Form, FormError, Icon, Input, Modal, Notice, NumberInput, SecretInput, toast, useIssues } from '../../components/index.js';
import { api } from '../../lib/api.js';
import { formatNumber, plural } from '../../lib/format.js';
import { useAsync, useUid } from '../../lib/hooks.js';
import { useLeaveGuard } from '../../lib/router.js';
import { ConnectExamples, ModelPatternsField, SecretText } from './parts.js';
import { emptyReference, referenceName } from './util.js';

export const NAME_MAX = 100;
/** The gateway stores the limit as a u32; this is a ceiling nobody means to pass. */
export const RPM_MAX = 1_000_000;
/** Below this a hand-picked literal key is easy to guess. */
const SHORT_KEY = 20;

/** A little longer than the dialog takes to fade out (140ms). */
const EXIT_MS = 180;

const EMPTY = { name: '', models: [], rpm: null, own: false, key: '' };

/**
 * What the user can fix before anything is sent. `others` are the keys the
 * name must differ from (names tell keys apart in usage, compared without
 * regard to case, as the gateway does).
 */
export function nameProblem(name, others) {
  const text = String(name ?? '').trim();
  if (!text) return 'Enter a name.';
  if ([...text].length > NAME_MAX) return `Use at most ${NAME_MAX} characters.`;
  const twin = (others ?? []).find((key) => String(key.name ?? '').trim().toLowerCase() === text.toLowerCase());
  if (twin) return `A key named ${String(twin.name).trim()} already exists. Names tell keys apart in requests and usage, so each key needs its own.`;
  return undefined;
}

function keyProblem(draft) {
  if (!draft.own) return undefined;
  const text = draft.key.trim();
  if (!text) return 'Enter a key value, or untick the box to have the gateway generate one.';
  // The gateway refuses a reference without a variable name (422).
  if (emptyReference(text)) return 'Name the variable the key is read from, as in env:TEAM_KEY.';
  if (/\s/.test(text)) return 'A key cannot contain spaces.';
  return undefined;
}

function OwnKeyNote({ value }) {
  const text = value.trim();
  if (!text) return null;
  const variable = referenceName(text);
  if (variable) {
    return html`
      <div class="keys-preview">
        <div class="keys-preview-line"><${Icon} name="info" size=${14} /><span>The gateway reads the key from <span class="mono">${variable}</span> in its own environment. Clients send that variable's value, not this reference.</span></div>
      </div>
    `;
  }
  if ([...text].length < SHORT_KEY) {
    return html`
      <div class="keys-preview" data-tone="caution">
        <div class="keys-preview-line"><${Icon} name="alert" size=${14} /><span>Short keys are easy to guess. Use ${SHORT_KEY} or more random characters, or let the gateway generate the key.</span></div>
      </div>
    `;
  }
  return null;
}

function Created({ created, entry, models, listen, tls, innerRef }) {
  const variable = created.is_reference ? referenceName(created.key) : null;
  const limits = [
    created.models.length === 0 ? 'All models' : plural(created.models.length, 'model pattern'),
    created.rpm == null ? 'no rate limit' : `${formatNumber(created.rpm)} requests per minute`,
  ].join(', ');
  return html`
    <div class="stack" ref=${innerRef}>
      ${created.is_reference
        ? entry && entry.resolved === false
          ? html`<${Notice} tone="caution" title=${`${variable} is not set`}>The key was saved, but the gateway has no such environment variable, so it cannot be used yet. Set it where the gateway runs, then restart the gateway.<//>`
          : html`<${Notice} tone="clear" title=${`Key ${created.name} created`}>It takes its value from <span class="mono">${variable}</span> in the gateway's environment. Clients send that value, not the reference below.<//>`
        : html`<${Notice} tone="clear" title=${`Key ${created.name} created`}>It works from now on. Copy it before you close this: the list shows it masked, and the full key appears again only when you reveal it in the key's details.<//>`}
      <div class="keys-created">
        <${SecretText} value=${created.key} label=${created.is_reference ? 'Key reference' : 'The new key'} />
        <${CopyButton} value=${created.key} variant="secondary" size="md" label=${created.is_reference ? 'Copy reference' : 'Copy key'}>${created.is_reference ? 'Copy reference' : 'Copy key'}<//>
      </div>
      <p class="field-hint">${limits}. Change either in the key's details.</p>
      <div class="keys-section">
        <h3>Connect a client</h3>
        <${ConnectExamples}
          keyText=${created.is_reference ? `<value of ${variable}>` : created.key}
          patterns=${created.models}
          models=${models.data}
          listen=${listen}
          tls=${tls}
        />
      </div>
    </div>
  `;
}

/**
 * open, onClose   the dialog; onClose is not called while a key is on show
 * existing        the current keys (GET /keys), for the name check and to
 *                 learn whether a reference resolved
 * models          useResource('/models')
 * listen, tls     status.listen and status.tls
 * onCreated       ({ id, key, is_reference }) => void, as soon as the key exists
 */
export function CreateKeyModal({ open, onClose, existing, models, listen, tls, onCreated }) {
  const formId = useUid('keys-create');
  const [draft, setDraft] = useState(EMPTY);
  const [problems, setProblems] = useState({});
  // Fields edited since the last answer from the gateway: its complaint
  // about them is about text that is no longer there.
  const [touched, setTouched] = useState({});
  const [created, setCreated] = useState(null);
  const formRef = useRef(null);
  const createdRef = useRef(null);
  const focusInvalid = useRef(false);

  const create = useAsync((payload) => api.post('/keys', payload));
  const issues = useIssues(create.error);

  // Closing forgets everything: a cancelled form does not come back filled
  // in (a key value typed into it least of all), and the created key is gone.
  // Not before the dialog has faded out, so it leaves showing what it showed.
  const used = useRef(false);
  useEffect(() => {
    if (open) {
      used.current = true;
      return undefined;
    }
    if (!used.current) return undefined;
    let done = false;
    const forget = () => {
      if (done) return;
      done = true;
      used.current = false;
      setDraft(EMPTY);
      setProblems({});
      setTouched({});
      setCreated(null);
      create.reset();
    };
    const timer = setTimeout(forget, EXIT_MS);
    // Opened again before the timer: start clean all the same.
    return () => {
      clearTimeout(timer);
      forget();
    };
  }, [open]);

  // While the key is on show the dialog has one way out. Every change of
  // the route is refused, a change of the query included.
  useLeaveGuard(open && created != null, {
    matters: () => true,
    ask: () => {
      toast.info(created?.is_reference ? 'Close the dialog first' : 'The new key is still on show', {
        id: 'keys-created-guard',
        description: created?.is_reference ? 'Press Done to go on.' : 'Press "I have copied it" before you leave this page. Until then the key stays up.',
      });
      return false;
    },
  });

  const set = (field) => (value) => {
    setDraft((d) => ({ ...d, [field]: value }));
    setProblems((p) => (p[field] ? { ...p, [field]: undefined } : p));
    setTouched((t) => (t[field] ? t : { ...t, [field]: true }));
  };

  // After a refused submit, put the cursor where the problem is.
  useEffect(() => {
    if (!focusInvalid.current) return;
    focusInvalid.current = false;
    formRef.current?.querySelector('[aria-invalid="true"]')?.focus();
  });

  // The form's buttons are gone once the key is shown: move focus to Copy.
  useEffect(() => {
    if (created) createdRef.current?.querySelector('button')?.focus();
  }, [created?.id]);

  const submit = async () => {
    if (create.loading) return;
    const found = { name: nameProblem(draft.name, existing), key: keyProblem(draft) };
    setProblems(found);
    if (found.name || found.key) {
      focusInvalid.current = true;
      return;
    }
    const payload = { name: draft.name.trim() };
    if (draft.models.length > 0) payload.models = draft.models;
    if (draft.rpm != null) payload.rate_limit_rpm = draft.rpm;
    if (draft.own) payload.key = draft.key.trim();
    setTouched({});
    const result = await create.run(payload);
    if (!result) {
      // The gateway's complaint is on screen by the next render; that render
      // may already be past, so ask for one more to run the focus effect.
      focusInvalid.current = true;
      setProblems((p) => ({ ...p }));
      return;
    }
    setCreated({ ...result, name: payload.name, models: payload.models ?? [], rpm: payload.rate_limit_rpm ?? null });
    setDraft(EMPTY);
    setProblems({});
    create.reset();
    onCreated?.(result);
  };

  // `created` stays until the dialog has faded out (see above).
  const finish = () => onClose?.();

  if (created) {
    return html`
      <${Modal}
        open=${open}
        title=${created.is_reference ? 'Key reference saved' : 'Copy the new key'}
        size="md"
        dismissable=${false}
        footer=${html`<${Button} variant="primary" onClick=${finish}>${created.is_reference ? 'Done' : 'I have copied it'}<//>`}
      >
        <${Created} created=${created} entry=${(existing ?? []).find((key) => key.id === created.id)} models=${models} listen=${listen} tls=${tls} innerRef=${createdRef} />
      <//>
    `;
  }

  // The gateway names the field in every refusal (400, 409 and 422 alike).
  // An issue for the key field while that field is not on screen is listed
  // by FormError instead.
  const nameError = problems.name ?? (touched.name ? undefined : issues.at('name'));
  const keyError = problems.key ?? (touched.key || !draft.own ? undefined : issues.at('key'));
  const modelIssues = touched.models ? [] : issues.under('models');
  const rpmError = touched.rpm ? undefined : issues.at('rate_limit_rpm');

  return html`
    <${Modal}
      open=${open}
      onClose=${() => onClose?.()}
      title="Create client key"
      description="A key for one application or person. You see it in full once it is created."
      size="md"
      dismissable=${!create.loading}
      footer=${html`
        <${Button} disabled=${create.loading} onClick=${() => onClose?.()}>Cancel<//>
        <${Button} type="submit" form=${formId} variant="primary" loading=${create.loading}>Create key<//>
      `}
    >
      <div ref=${formRef}>
        <${Form} id=${formId} onSubmit=${submit}>
          <${Input}
            label="Name"
            value=${draft.name}
            onChange=${set('name')}
            autoFocus
            maxLength=${NAME_MAX}
            placeholder="build-bot"
            hint="Who or what uses this key. Requests and usage are shown under this name."
            error=${nameError}
          />
          <${ModelPatternsField}
            value=${draft.models}
            onChange=${set('models')}
            models=${models}
            error=${modelIssues.length > 0 ? modelIssues.map((issue) => issue.message).join(' ') : undefined}
          />
          <${NumberInput}
            label="Rate limit"
            optional
            value=${draft.rpm}
            onChange=${set('rpm')}
            min=${1}
            max=${RPM_MAX}
            step=${1}
            unit="rpm"
            placeholder="No limit"
            hint="Requests per minute. Requests over the limit are refused with 429."
            error=${rpmError}
          />
          <div class="stack" style="--gap:var(--space-3)">
            <${Checkbox}
              label="Use my own key value"
              hint="Advanced. Without this the gateway generates a random key, which is what you want unless a client already has one."
              checked=${draft.own}
              onChange=${set('own')}
            />
            ${draft.own &&
            html`
              <${SecretInput}
                label="Key value"
                value=${draft.key}
                onChange=${set('key')}
                placeholder="A literal key, or env:NAME"
                hint="A literal key without spaces, or a reference such as env:TEAM_KEY to read it from the gateway's environment."
                error=${keyError}
              />
              <${OwnKeyNote} value=${draft.key} />
            `}
          </div>
          <${FormError} error=${create.error} issues=${issues} title="Could not create the key" />
        <//>
      </div>
    <//>
  `;
}
