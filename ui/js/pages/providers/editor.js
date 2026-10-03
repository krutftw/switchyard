// Providers page: the add / edit drawer.
//
// The form edits a draft (model.js) and sends the whole provider entry:
// POST /providers to create, PUT /providers/{name} to replace. Secrets
// follow the admin API's mask rule: a stored key is shown as the gateway
// masked it and is sent back untouched unless the user replaces it, so the
// dashboard never holds a secret it was not just given.

import { html, useEffect, useMemo, useRef, useState } from '../../../vendor/preact-htm.js';
import {
  Badge,
  Button,
  Drawer,
  Field,
  Form,
  FormError,
  FormRow,
  Icon,
  IconButton,
  Input,
  Notice,
  NumberInput,
  SecretInput,
  Segmented,
  Select,
  Switch,
  confirm,
  toast,
  useIssues,
} from '../../components/index.js';
import { api } from '../../lib/api.js';
import { plural } from '../../lib/format.js';
import { useAsync } from '../../lib/hooks.js';
import { useLeaveGuard } from '../../lib/router.js';
import {
  COMPAT_PRESETS,
  KINDS,
  blankCredential,
  blankHeader,
  canSwap,
  configFromDraft,
  draftFromConfig,
  emptyDraft,
  envVarFor,
  freeName,
  hasSettings,
  headerKeepsStored,
  isAmbiguousMaskIssue,
  isCredentialHeader,
  isOpenAiKind,
  isReference,
  kindInfo,
  localIssues,
  maskShortfall,
  referenceName,
  sectionOfPath,
  servesNothing,
} from './model.js';
import ModelsSection from './models-editor.js';
import { Disclosure, KindPicker, Section, StoredSecret, TriState, firstFieldOf, focusAfterRemoval, focusSoon } from './parts.js';

const SECTIONS = [
  ['basics', 'Basics'],
  ['credentials', 'Credentials'],
  ['routing', 'Routing'],
  ['models', 'Models'],
];

const normalize = (path) =>
  String(path ?? '')
    .replace(/\[(\w+)\]/g, '.$1')
    .replace(/^\./, '');

/** The name a new provider of this kind gets until the user types one. */
const suggestedName = (kind, taken) => freeName(kind === 'openai-compat' ? '' : kind, taken);

function makeSession(target, takenNames) {
  let draft;
  if (target.mode === 'edit') {
    draft = draftFromConfig(target.provider.config);
  } else {
    const quick = target.quick;
    draft = emptyDraft(quick?.kind ?? 'openai', quick?.preset ?? null);
    draft.name = quick ? freeName(quick.id, takenNames) : suggestedName(draft.kind, takenNames);
  }
  return {
    key: target.key,
    mode: target.mode,
    originalName: target.mode === 'edit' ? target.provider.name : null,
    originalKind: target.mode === 'edit' ? target.provider.kind : null,
    draft,
    // The credential rows as the form opened: how many stored keys share a mask.
    initialCredentials: draft.credentials,
    // What the form would send if nothing were touched: the measure of "dirty".
    baseline: JSON.stringify(configFromDraft(draft).config),
    // The stored entry as it was when the form opened, to notice outside edits.
    snapshot: target.mode === 'edit' ? JSON.stringify(target.provider.config) : null,
    nameTouched: false,
  };
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

function settingsCount(row, vertex) {
  return (
    (row.label.trim() ? 1 : 0) +
    (row.weight != null ? 1 : 0) +
    (row.priority != null ? 1 : 0) +
    (row.proxy.trim() ? 1 : 0) +
    (row.disabled ? 1 : 0) +
    (vertex ? (!row.noKey && (row.stored || row.entered.trim()) ? 1 : 0) : row.service_account_file.trim() ? 1 : 0)
  );
}

/** A field label with the credential's own label after it, so rows whose keys mask alike can be told apart. */
function rowLabel(text, row) {
  const name = row.label.trim();
  return name ? html`<span>${text}</span><span class="prov-row-name" title=${name}>${name}</span>` : text;
}

/**
 * The key of one credential row: the stored key as the gateway masked it,
 * the input that replaces it, an environment reference, or (where a
 * credential can do without) "no key".
 *
 * envVar      the variable suggested for a reference
 * warning     a caveat about the stored key (it masks like another one)
 * showName    put the credential's label after the field's (the row's main field)
 * canBeEmpty  the credential may be kept without a key
 */
function KeyField({ row, envVar, error, warning, onChange, showName = false, optional = false, canBeEmpty = false }) {
  const title = (text) => (showName ? rowLabel(text, row) : text);
  const label = title('API key');
  const storedIsReference = isReference(row.stored);
  const storedWord = storedIsReference ? 'reference' : 'key';
  // Each of the three faces replaces the control that was pressed: the focus
  // goes to what took its place, the new input or the remaining button.
  const boxId = `${row.uid}-key`;
  const change = (patch, focus) => {
    onChange(patch);
    focusSoon(() => document.getElementById(boxId)?.querySelector(focus));
  };
  const face = (content) => html`<div id=${boxId}>${content}</div>`;
  if (row.noKey) {
    return face(html`
      <${Field} label=${label} error=${error} optional=${optional} hint=${hasSettings(row) ? `Saving removes the stored ${storedWord} from the configuration file.` : `Without a key and without settings there is nothing to save: the row is dropped, and the stored ${storedWord} with it.`}>
        <div class="prov-stored">
          <span class="prov-stored-value">No key</span>
          <${Button} size="sm" variant="ghost" onClick=${() => change({ noKey: false }, 'button')}>Keep the stored ${storedWord}<//>
        </div>
      <//>
    `);
  }
  if (!row.editing) {
    return face(html`
      <${Field} label=${label} error=${error} warning=${warning} optional=${optional}>
        <${StoredSecret}
          value=${row.stored}
          reference=${row.reference}
          replaceLabel=${row.reference ? 'Change' : 'Replace'}
          what=${row.reference ? 'reference' : 'key'}
          onReplace=${() => change(row.reference ? { editing: true, entered: row.stored } : { editing: true, entered: '' }, 'input')}
        />
      <//>
    `);
  }
  const keep = row.stored
    ? html`
        <div class="prov-keep">
          <${Button} size="sm" variant="ghost" onClick=${() => change({ editing: false, entered: '', reference: storedIsReference }, 'button')}>Keep the stored ${storedWord}<//>
          ${canBeEmpty && html`<${Button} size="sm" variant="ghost" onClick=${() => change({ noKey: true, editing: false, entered: '', reference: storedIsReference }, 'button')}>Use no key<//>`}
        </div>
      `
    : null;
  if (row.reference) {
    const typed = row.entered.trim();
    const hint = !typed
      ? 'Names an environment variable. It is stored as written and read when the gateway starts.'
      : isReference(typed)
        ? `Read from the variable ${referenceName(typed) || '(unnamed)'} when the gateway starts.`
        : 'This is not a reference: as written, the text itself would be stored as the key. Start it with env:';
    return face(html`
      <div class="stack" style="--gap:var(--space-1)">
        <${Input} mono label=${title('Environment reference')} optional=${optional} value=${row.entered} onChange=${(v) => onChange({ entered: v })} placeholder=${`env:${envVar}`} error=${error} hint=${hint} />
        ${keep}
      </div>
    `);
  }
  return face(html`
    <div class="stack" style="--gap:var(--space-1)">
      <${SecretInput}
        label=${label}
        optional=${optional}
        value=${row.entered}
        onChange=${(v) => onChange({ entered: v })}
        placeholder=${row.stored ? 'Type or paste the new key' : canBeEmpty ? 'Paste the key, or leave empty for none' : 'Paste the key'}
        error=${error}
        warning=${row.entered.trim() ? undefined : warning}
        hint=${isReference(row.entered) ? `Stored as a reference to the variable ${referenceName(row.entered) || '(unnamed)'}, not as a key.` : row.stored ? `Left empty, the stored ${storedWord} is kept.` : undefined}
      />
      ${keep}
    </div>
  `);
}

/**
 * What a row says about a key the gateway could not tell from others that
 * mask alike (its issue on `api_keys[j]` or `credentials[j].api_key`). The
 * gateway's own words name no key and no way out; these do.
 */
function ambiguousKeyText(mask) {
  return `Refused: ${mask ? `other stored keys show as ${mask} too` : 'other stored keys mask alike'} and one of them was removed or replaced, so the mask does not say which key this row is. Type the full value of each key of this mask you keep, or cancel and give the keys labels before removing one.`;
}

function CredentialRow({ row, index, count, kind, envVar, providerPriority, issues, path, forceOpen, upBlocked, downBlocked, shortfall, onChange, onMove, onRemove }) {
  const info = kindInfo(kind);
  const vertex = kind === 'vertex';
  const at = (field) => (path ? issues.at(field ? `${path}.${field}` : path) : undefined);
  // Once the full key has been typed, the refusal of the mask no longer applies to the row.
  const typed = row.editing && row.entered.trim() !== '';
  const keyIssue = (message) => (!isAmbiguousMaskIssue(message) ? message : typed ? '' : ambiguousKeyText(row.stored));
  const keyError = [at(''), at('api_key')].filter(Boolean).map(keyIssue).filter(Boolean).join(' ') || undefined;
  // Before saving: the same refusal, foreseen.
  const keyWarning = shortfall
    ? `${shortfall.left === 1 ? 'This is the only row' : `${shortfall.left} rows are`} left for ${shortfall.stored} stored keys that show as ${shortfall.mask}: the gateway cannot tell which ${shortfall.stored - shortfall.left === 1 ? 'one was' : 'ones were'} removed and will refuse to save. Type the full value of each key of this mask you keep.`
    : undefined;
  const open = row.open || forceOpen;
  const regionId = `${row.uid}-settings`;
  const named = row.label.trim() || (row.noKey ? '' : row.stored) || (vertex && row.service_account_file.trim()) || `credential ${index + 1}`;

  return html`
    <li class="prov-item" id=${row.uid} data-open=${open ? '' : undefined}>
      <div class="prov-item-main prov-cred-grid">
        <div class="prov-order">
          <span class="prov-order-n num" aria-hidden="true">${index + 1}</span>
          <${IconButton} id=${`${row.uid}-up`} icon="arrow-up" size="sm" label=${`Move ${named} up`} disabled=${index === 0 || upBlocked} onClick=${() => onMove(-1)} />
          <${IconButton} id=${`${row.uid}-down`} icon="arrow-down" size="sm" label=${`Move ${named} down`} disabled=${index === count - 1 || downBlocked} onClick=${() => onMove(1)} />
        </div>
        <div class="prov-item-field">
          ${vertex
            ? html`<${Input}
                mono
                label=${rowLabel('Service account file', row)}
                value=${row.service_account_file}
                onChange=${(v) => onChange({ service_account_file: v })}
                placeholder="vertex-sa.json"
                hint="Path to the JSON key file, relative to the folder of the configuration file."
                error=${[at(''), at('service_account_file')].filter(Boolean).join(' ') || undefined}
              />`
            : html`<${KeyField} row=${row} envVar=${envVar} error=${keyError} warning=${keyWarning} onChange=${onChange} showName canBeEmpty=${info.keyless} />`}
        </div>
        <div class="prov-item-tools">
          ${row.disabled && html`<${Badge} outline>disabled<//>`}
          <${Disclosure} open=${open} onToggle=${(next) => onChange({ open: next })} controls=${regionId} count=${open ? 0 : settingsCount(row, vertex)}>Settings<//>
          <${IconButton} id=${`${row.uid}-remove`} icon="trash" size="sm" label=${`Remove ${named}`} onClick=${onRemove} />
        </div>
      </div>
      ${open &&
      html`
        <div class="prov-item-more" id=${regionId}>
          <${FormRow}>
            <${Input} label="Label" optional value=${row.label} onChange=${(v) => onChange({ label: v })} placeholder="The masked key" hint="How it is named in the dashboard and in logs." error=${at('label')} />
            <${NumberInput}
              label="Weight"
              optional
              min=${0}
              value=${row.weight}
              onChange=${(v) => onChange({ weight: v })}
              placeholder="Default: 1"
              hint="Share of traffic under the weighted strategy."
              error=${at('weight')}
            />
            <${NumberInput}
              label="Priority"
              optional
              value=${row.priority}
              onChange=${(v) => onChange({ priority: v })}
              placeholder=${`The provider's (${providerPriority ?? 0})`}
              hint="Higher is tried first."
              error=${at('priority')}
            />
          <//>
          <${Input}
            mono
            label="Proxy"
            optional
            value=${row.proxy}
            onChange=${(v) => onChange({ proxy: v })}
            placeholder="The provider's proxy"
            hint="Outbound proxy for this credential only."
            error=${at('proxy')}
          />
          ${vertex && html`<${KeyField} row=${row} envVar=${envVar} error=${(at('api_key') && keyIssue(at('api_key'))) || undefined} warning=${keyWarning} onChange=${onChange} optional canBeEmpty />`}
          ${!vertex &&
          row.service_account_file &&
          html`<${Input} mono label="Service account file" optional value=${row.service_account_file} onChange=${(v) => onChange({ service_account_file: v })} hint="Only Vertex AI providers use it." error=${at('service_account_file')} />`}
          <${Switch} label="Disabled" checked=${row.disabled} onChange=${(v) => onChange({ disabled: v })} hint="Stays in the configuration and takes no requests." error=${at('disabled')} />
        </div>
      `}
    </li>
  `;
}

/** "1", "1 and 2", "1, 2 and 3". */
const listNumbers = (numbers) => (numbers.length < 2 ? String(numbers[0] ?? '') : `${numbers.slice(0, -1).join(', ')} and ${numbers.at(-1)}`);

function CredentialsSection({ draft, update, issues, paths, hasIssueUnder, initialRows = [] }) {
  const info = kindInfo(draft.kind);
  const vertex = draft.kind === 'vertex';
  const rows = draft.credentials;
  const setRows = (fn) => update((d) => ({ credentials: fn(d.credentials) }));
  const listIssue = [issues.at('api_keys'), issues.at('credentials')].filter(Boolean).join(' ');

  // Keys that mask alike are told apart by the gateway only by their order
  // (or a label they were stored with): such neighbours cannot trade places,
  // and removing one of them cannot be saved while the rest come back masked.
  const blocked = rows.map((row, index) => index > 0 && !canSwap(rows[index - 1], row, rows));
  const shortfall = maskShortfall(initialRows, rows);
  const stuck = new Set();
  blocked.forEach((isBlocked, index) => {
    if (!isBlocked) return;
    stuck.add(index);
    stuck.add(index + 1);
  });
  const stuckNumbers = [...stuck].sort((a, b) => a - b);

  const move = (uid, delta) => {
    setRows((list) => {
      const from = list.findIndex((r) => r.uid === uid);
      const to = from + delta;
      if (from === -1 || to < 0 || to >= list.length) return list;
      const next = list.slice();
      [next[from], next[to]] = [next[to], next[from]];
      return next;
    });
    // The row moved in the document, which drops focus: put it back on the
    // button that was pressed, or on its twin when that one is now disabled.
    setTimeout(() => {
      const pressed = document.getElementById(`${uid}-${delta < 0 ? 'up' : 'down'}`);
      const twin = document.getElementById(`${uid}-${delta < 0 ? 'down' : 'up'}`);
      (pressed && !pressed.disabled ? pressed : twin)?.focus();
    }, 40);
  };

  // The variable a key of this provider is usually kept in: the endpoint's
  // own for a well-known one (GROQ_API_KEY), else the kind's.
  const envVar = envVarFor(draft.kind, draft.base_url);
  const ADD_ID = 'prov-add-credential';

  const add = (extra) => {
    const row = blankCredential(extra);
    setRows((list) => [...list, row]);
    focusSoon(firstFieldOf(row.uid));
  };

  const addReference = () => {
    const used = new Set(rows.map((r) => (r.editing ? r.entered.trim() : r.stored)));
    let name = envVar;
    for (let n = 2; used.has(`env:${name}`); n += 1) name = `${envVar}_${n}`;
    add({ reference: true, entered: `env:${name}` });
  };

  const remove = (uid) => {
    focusAfterRemoval(rows, rows.findIndex((r) => r.uid === uid), ADD_ID);
    setRows((list) => list.filter((r) => r.uid !== uid));
  };

  const description = vertex
    ? 'One entry per service account. Requests rotate across them and fail over when one rests.'
    : draft.kind === 'mock'
      ? 'The mock provider needs none.'
      : info.keyless
        ? 'Optional for this kind: a local server needs no key. With several keys, requests rotate across them and fail over when one is rate limited.'
        : 'One credential per key. Requests rotate across them and fail over when one is rate limited.';

  return html`
    <${Section} id="prov-sec-credentials" title="Credentials" description=${description}>
      ${rows.length === 0 &&
      (info.keyless
        ? html`<p class="prov-note">No keys: the provider is called without one.</p>`
        : html`<${Notice} tone="caution" title="No credentials yet">Without ${vertex ? 'a service account' : 'a key'} this provider cannot serve requests.<//>`)}
      ${rows.length > 0 &&
      html`
        <ul class="prov-items">
          ${rows.map(
            (row, index) => html`
              <${CredentialRow}
                key=${row.uid}
                row=${row}
                index=${index}
                count=${rows.length}
                kind=${draft.kind}
                envVar=${envVar}
                providerPriority=${draft.priority}
                issues=${issues}
                path=${paths[row.uid]}
                forceOpen=${!!paths[row.uid] && ['label', 'weight', 'priority', 'proxy', 'disabled', ...(vertex ? ['api_key'] : [])].some((f) => hasIssueUnder(`${paths[row.uid]}.${f}`))}
                upBlocked=${blocked[index]}
                downBlocked=${!!blocked[index + 1]}
                shortfall=${shortfall.get(row.uid)}
                onChange=${(patch) => setRows((list) => list.map((r) => (r.uid === row.uid ? { ...r, ...patch } : r)))}
                onMove=${(delta) => move(row.uid, delta)}
                onRemove=${() => remove(row.uid)}
              />
            `,
          )}
        </ul>
      `}
      ${listIssue && html`<${Notice} tone="stop">${listIssue}<//>`}
      <div class="row row-wrap">
        ${vertex
          ? html`<${Button} id=${ADD_ID} size="sm" icon="plus" onClick=${() => add()}>Add service account<//>`
          : html`
              <${Button} id=${ADD_ID} size="sm" icon="plus" onClick=${() => add()}>Add key<//>
              <${Button} size="sm" icon="link" onClick=${addReference}>Add environment reference<//>
            `}
      </div>
      ${!vertex &&
      html`<p class="prov-note">
        Keys are written to the configuration file and shown masked from then on. A reference such as <span class="mono">env:${envVar}</span> names an environment variable instead of holding the key.
      </p>`}
      ${rows.length > 1 && html`<p class="prov-note">The order is the order credentials are tried in under the fill-first strategy, and the first usable one runs connection tests.</p>`}
      ${stuckNumbers.length > 0 &&
      html`<p class="prov-note" id="prov-mask-order">${vertex ? 'Service accounts' : 'Keys'} ${listNumbers(stuckNumbers)} are shown with the same mask, and the gateway knows them apart only by their order, so neighbours among them cannot trade places here. To reorder them, type their full values, or give each a label, save, and move them then.</p>`}
    <//>
  `;
}

// ---------------------------------------------------------------------------
// Headers
// ---------------------------------------------------------------------------

/**
 * One header: its name and its value.
 *
 * A value is a secret only when the header's name says so (Authorization,
 * X-Api-Key). Such a value comes back masked and is shown as stored, with a
 * button to replace it; every other value is ordinary text, edited in place.
 * The mask stands for the secret only under the name it was stored under, so
 * a renamed header asks for its value again, and gets the stored one back
 * when the old name is typed again.
 */
function HeaderRow({ row, index, duplicate, error, onChange, onRemove }) {
  const secret = isCredentialHeader(row.name);
  const named = row.name.trim() || `header ${index + 1}`;
  const keeps = headerKeepsStored(row);
  const reference = isReference(row.stored);
  const moved = row.stored !== '' && !keeps;
  const valueId = `${row.uid}-value`;
  // Replace and Keep swap the control that was pressed for another: the focus follows.
  const change = (patch, focus) => {
    onChange(patch);
    focusSoon(() => document.getElementById(valueId)?.querySelector(focus));
  };
  let value;
  if (keeps && !row.editing) {
    value = html`<${StoredSecret}
      value=${row.stored}
      reference=${reference}
      what=${reference ? 'reference' : 'value'}
      replaceLabel=${reference ? 'Change' : 'Replace'}
      onReplace=${() => change({ editing: true, entered: reference ? row.stored : '' }, 'input')}
    />`;
  } else if (secret && !(keeps && reference)) {
    value = html`<${SecretInput} aria-label=${`Value of ${named}`} value=${row.entered} onChange=${(v) => onChange({ entered: v })} placeholder=${keeps ? 'Type the new value' : 'Value'} />`;
  } else {
    value = html`<${Input} mono aria-label=${`Value of ${named}`} value=${row.entered} onChange=${(v) => onChange({ entered: v })} placeholder="Value" />`;
  }
  // The gateway reports a header under one path, "headers.<name>", whether
  // the name or the value is at fault; its message says which.
  const aboutName = !!error && /header name/i.test(error);
  const nameError = duplicate ? 'Listed twice: the last one wins.' : aboutName ? error : undefined;
  const valueError = error && !aboutName ? error : undefined;
  return html`
    <li class="prov-header-row" id=${row.uid}>
      <${Input} mono aria-label=${`Name of header ${index + 1}`} value=${row.name} onChange=${(name) => onChange({ name })} placeholder="X-Title" error=${nameError} />
      <div class="prov-header-value" id=${valueId}>
        ${value}
        ${keeps && row.editing && html`<div class="prov-keep"><${Button} size="sm" variant="ghost" onClick=${() => change({ editing: false, entered: '' }, 'button')}>Keep the stored value<//></div>`}
        ${valueError
          ? html`<div class="field-error"><${Icon} name="alert-circle" size=${14} /><span>${valueError}</span></div>`
          : moved && html`<p class="field-hint">The value stored for ${row.storedName} does not follow a renamed header. Type it again, or change the name back.</p>`}
      </div>
      <${IconButton} id=${`${row.uid}-remove`} icon="trash" size="sm" label=${`Remove ${named}`} onClick=${onRemove} />
    </li>
  `;
}

function HeadersEditor({ draft, update, issues }) {
  const rows = draft.headers;
  const setRows = (fn) => update((d) => ({ headers: fn(d.headers) }));
  const ADD_ID = 'prov-add-header';
  const names = rows.map((r) => r.name.trim().toLowerCase());
  // Issues name a header by its name: each goes to its row, the rest under the list.
  const rowErrors = rows.map((row) => (row.name.trim() ? issues.at(`headers.${row.name.trim()}`) : undefined));
  const problems = issues.under('headers').filter((issue) => !rows.some((row) => row.name.trim() && issue.path === `headers.${row.name.trim()}`));

  const add = () => {
    const row = blankHeader();
    setRows((list) => [...list, row]);
    focusSoon(firstFieldOf(row.uid));
  };
  const remove = (uid) => {
    focusAfterRemoval(rows, rows.findIndex((r) => r.uid === uid), ADD_ID);
    setRows((list) => list.filter((r) => r.uid !== uid));
  };

  return html`
    <${Field}
      label="Custom headers"
      optional
      hint="Sent with every request to this provider. Values of headers that carry a credential (Authorization, X-Api-Key and the like) are masked once saved."
      error=${problems.length > 0 ? problems.map((i) => i.message).join(' ') : undefined}
    >
      ${rows.length > 0 &&
      html`
        <ul class="prov-headers">
          ${rows.map(
            (row, index) => html`
              <${HeaderRow}
                key=${row.uid}
                row=${row}
                index=${index}
                duplicate=${!!names[index] && names.indexOf(names[index]) !== index}
                error=${rowErrors[index]}
                onChange=${(patch) => setRows((list) => list.map((r) => (r.uid === row.uid ? { ...r, ...patch } : r)))}
                onRemove=${() => remove(row.uid)}
              />
            `,
          )}
        </ul>
      `}
      <div class="row">
        <${Button} id=${ADD_ID} size="sm" icon="plus" onClick=${add}>Add header<//>
      </div>
    <//>
  `;
}

// ---------------------------------------------------------------------------
// The drawer
// ---------------------------------------------------------------------------

/**
 * target      null (closed), { key, mode: "new", quick } or
 *             { key, mode: "edit", provider, gone? }. A new `key` starts a
 *             fresh form; the same key keeps what was typed. `gone` is set
 *             when the provider left the configuration while the form had
 *             unsaved changes: `{ candidate }` names the provider it was
 *             probably renamed to, or null. A target whose provider carries
 *             another name than the form was opened for moves the form over
 *             to that provider.
 * takenNames  names of the existing providers
 * onClose()   leave without saving (already confirmed when there were changes)
 * onSaved(view, previousName)
 * onRetarget(name)  carry the form over to the provider of that name
 * leaves(to, from)  whether a route change takes the form away (the page
 *             keeps the form in the URL and knows); such a change, and
 *             signing out, asks first while there are unsaved changes
 * onModelsFetched()  the form asked the upstream for its model list: the
 *             provider's model-list state has changed
 * dirtyRef    kept true while the open form has unsaved changes
 * savingRef   kept true while a save is on its way
 */
export default function ProviderEditor({ target, takenNames, onClose, onSaved, onRetarget, onModelsFetched, leaves, dirtyRef, savingRef }) {
  const lastTarget = useRef(null);
  if (target) lastTarget.current = target;
  const shown = target ?? lastTarget.current;

  const [sessionState, setSession] = useState(null);
  // The name of the provider the target last pointed at, to notice when the
  // page points the same form at another one.
  const targetName = useRef(null);
  let session = sessionState;
  if (shown && (!session || session.key !== shown.key)) {
    session = makeSession(shown, takenNames);
    targetName.current = shown.mode === 'edit' ? shown.provider.name : null;
    setSession(session);
  } else if (session && shown && shown.mode === 'edit' && !shown.gone && shown.provider.name !== targetName.current) {
    targetName.current = shown.provider.name;
    if (shown.provider.name !== session.originalName) {
      // The form was carried over to the provider under its new name: what
      // was typed stays, and saving now replaces that entry.
      const provider = shown.provider;
      session = {
        ...session,
        originalName: provider.name,
        originalKind: provider.kind,
        baseline: JSON.stringify(configFromDraft(draftFromConfig(provider.config)).config),
        snapshot: JSON.stringify(provider.config),
        initialCredentials: draftFromConfig(provider.config).credentials,
        draft: session.nameTouched ? session.draft : { ...session.draft, name: provider.name },
      };
      setSession(session);
    }
  }

  const [localError, setLocalError] = useState(null);
  const sentPaths = useRef({});
  const formId = useMemo(() => `prov-form-${Math.random().toString(36).slice(2, 8)}`, []);

  const sessionRef = useRef(session);
  sessionRef.current = session;
  const gone = target?.mode === 'edit' ? (target.gone ?? null) : null;
  const goneRef = useRef(gone);
  goneRef.current = gone;

  // One request says everything: the whole entry, every credential with its
  // key (as shown, or typed anew) or with `null` for "no key".
  const save = useAsync(() => {
    const s = sessionRef.current;
    const { config, paths } = configFromDraft(s.draft, { resetForKind: s.mode === 'new' || s.draft.kind !== s.originalKind });
    sentPaths.current = paths;
    // A provider that is no longer in the configuration is created again.
    return s.mode === 'new' || goneRef.current ? api.post('/providers', config) : api.put(`/providers/${encodeURIComponent(s.originalName)}`, config);
  });

  // A new form starts without the previous one's errors.
  const sessionKey = session?.key;
  useEffect(() => {
    save.reset();
    setLocalError(null);
    sentPaths.current = {};
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionKey]);

  // The gateway's issue paths are places in the entry that was sent, which
  // is how the fields below ask for them.
  const error = localError ?? save.error;
  const issues = useIssues(error);

  const open = !!target;
  const draft = session?.draft;
  const dirty = !!session && JSON.stringify(configFromDraft(draft).config) !== session.baseline;
  // Written only while open: the page reads it after the form has closed, to
  // tell a form that was left from one whose provider vanished under it.
  if (dirtyRef && open) dirtyRef.current = dirty;
  if (savingRef) savingRef.current = save.loading;

  const confirmDiscard = () =>
    confirm({
      danger: true,
      title: sessionRef.current?.mode === 'new' ? 'Discard the new provider?' : `Discard changes to ${sessionRef.current?.originalName}?`,
      message: 'What you entered in this form has not been saved.',
      confirmLabel: 'Discard changes',
      cancelLabel: 'Keep editing',
    });

  // Every way out that takes the form away asks first while it has unsaved
  // changes: Back, a link, the command palette, another form, signing out,
  // and (the browser's own prompt) closing or reloading the tab.
  const guard = useLeaveGuard(open && dirty, {
    matters: (to, from) => to === null || !leaves || leaves(to, from),
    ask: () => {
      if (savingRef?.current) {
        toast.info('The provider is being saved', { description: 'Stay on the form until the gateway has answered.' });
        return false;
      }
      return confirmDiscard();
    },
  });

  if (!session) return null;

  const { mode, originalName } = session;
  const info = kindInfo(draft.kind);
  const openai = isOpenAiKind(draft.kind);
  const paths = sentPaths.current;

  const update = (patch) =>
    setSession((prev) => {
      if (!prev) return prev;
      const change = typeof patch === 'function' ? patch(prev.draft) : patch;
      return { ...prev, draft: { ...prev.draft, ...change } };
    });

  const hasIssueUnder = (path) => {
    const key = normalize(path);
    return issues.all.some((issue) => issue.path === key || issue.path.startsWith(`${key}.`));
  };

  const sectionCounts = {};
  for (const issue of issues.all) {
    const section = sectionOfPath(issue.path);
    if (section) sectionCounts[section] = (sectionCounts[section] ?? 0) + 1;
  }

  /** No credential row of a new provider has anything in it yet. */
  const credentialsUntouched = (d) => d.credentials.every((row) => !row.stored && !row.entered.trim() && !hasSettings(row));

  const setKind = (kind) =>
    setSession((prev) => {
      const next = { ...prev.draft, kind };
      if (prev.mode === 'new') {
        if (!prev.nameTouched) next.name = suggestedName(kind, takenNames);
        if (credentialsUntouched(prev.draft)) next.credentials = kindInfo(kind).keyless ? [] : [blankCredential()];
        // A base URL filled in by a preset belongs to the kind it was picked for.
        if (kind !== 'openai-compat' && COMPAT_PRESETS.some((p) => p.base_url === prev.draft.base_url.trim())) next.base_url = '';
      }
      return { ...prev, draft: next };
    });

  const applyPreset = (preset) =>
    setSession((prev) => {
      const next = { ...prev.draft, base_url: preset.base_url };
      if (prev.mode === 'new') {
        if (!prev.nameTouched) next.name = freeName(preset.id, takenNames);
        // A hosted endpoint needs a key, a local server does not: offer the row, or take the empty one away.
        if (credentialsUntouched(prev.draft)) next.credentials = preset.local ? [] : prev.draft.credentials.length > 0 ? prev.draft.credentials : [blankCredential()];
      }
      return { ...prev, draft: next };
    });

  const showFirstError = () => {
    // After the render that puts the messages on screen. A timer, not an
    // animation frame: frames stop while the tab is in the background.
    setTimeout(() => {
      const form = document.getElementById(formId);
      const first = form?.querySelector('.field-error, .notice[data-tone="stop"]');
      first?.scrollIntoView({ block: 'center' });
      first?.closest('.field')?.querySelector('input, select, textarea, button')?.focus({ preventScroll: true });
    }, 40);
  };

  const submit = async () => {
    if (save.loading) return;
    const others = mode === 'new' || gone ? takenNames : takenNames.filter((n) => n !== originalName);
    const local = localIssues(draft, { takenNames: others });
    if (local.length > 0) {
      save.reset();
      sentPaths.current = {};
      setLocalError({ status: 0, message: local.length === 1 ? 'One field needs attention before this can be saved.' : `${local.length} fields need attention before this can be saved.`, issues: local });
      showFirstError();
      return;
    }
    setLocalError(null);
    const view = await save.run();
    if (view && typeof view === 'object') {
      const created = mode === 'new' || !!gone;
      const credentials = plural((view.credentials ?? []).length, 'credential');
      const title = created ? `Provider ${view.name} created` : `Provider ${view.name} saved`;
      if ((view.model_count ?? 0) === 0 && servesNothing({ kind: view.kind, discover: view.discover, explicit: view.config?.models?.length ?? 0 })) {
        // Saved as asked, and left with nothing to route to.
        toast.warning(title, { description: 'It serves no models: discovery is off and its explicit list is empty. Add models to the list, or switch discovery on.' });
      } else {
        toast.success(title, {
          // While the gateway is still asking the upstream, the model count is not the provider's yet.
          description: !created ? undefined : view.discovery?.state === 'pending' ? `${credentials}. Its model list is being fetched.` : `${credentials}, ${plural(view.model_count ?? 0, 'model')}.`,
        });
      }
      // Saved: closing the form is not leaving anything behind.
      guard.release();
      if (dirtyRef) dirtyRef.current = false;
      onSaved(view, originalName);
    } else {
      showFirstError();
    }
  };

  const requestClose = async () => {
    if (save.loading) return;
    if (dirty && !(await confirmDiscard())) return;
    // The question has been asked here: the leave guard does not ask again.
    guard.release();
    if (dirtyRef) dirtyRef.current = false;
    onClose();
  };

  const renamed = mode === 'edit' && !gone && draft.name.trim() !== '' && draft.name.trim() !== originalName;
  const changedOutside = mode === 'edit' && !gone && target?.mode === 'edit' && target.provider.name === originalName && JSON.stringify(target.provider.config) !== session.snapshot;
  const reload = () => {
    if (target) setSession(makeSession({ ...target, key: session.key }, takenNames));
    save.reset();
    setLocalError(null);
  };

  const baseUrlHint =
    draft.kind === 'openai-compat'
      ? 'Required. The root that /chat/completions is appended to, usually ending in /v1.'
      : draft.kind === 'mock'
        ? 'Not used: the mock provider answers inside the gateway.'
        : `Leave empty for ${info.defaultUrl}. Set it to go through a regional endpoint or a relay.`;

  return html`
    <${Drawer}
      open=${open}
      onClose=${requestClose}
      dismissable=${!save.loading}
      width="760px"
      class="prov-drawer prov-editor"
      title=${mode === 'new' ? 'Add provider' : 'Edit provider'}
      subtitle=${mode === 'edit' ? originalName : undefined}
      footer=${html`
        <${Button} disabled=${save.loading} onClick=${requestClose}>Cancel<//>
        <${Button} type="submit" form=${formId} variant="primary" loading=${save.loading}>${mode === 'new' || gone ? 'Create provider' : 'Save provider'}<//>
      `}
    >
      <nav class="prov-jump" aria-label="Sections of this form">
        ${SECTIONS.map(
          ([id, label]) => html`
            <button type="button" key=${id} class="prov-jump-item" onClick=${() => document.getElementById(`prov-sec-${id}`)?.scrollIntoView({ block: 'start' })}>
              <span>${label}</span>
              ${sectionCounts[id] > 0 && html`<${Badge} tone="stop" title=${`${plural(sectionCounts[id], 'problem')} in this section`}>${sectionCounts[id]}<//>`}
            </button>
          `,
        )}
      </nav>

      <${Form} id=${formId} class="prov-form" onSubmit=${submit}>
        ${changedOutside &&
        html`
          <${Notice} tone="caution" title="This provider changed while the form was open" action=${html`<${Button} size="sm" onClick=${reload}>Load the new version<//>`}>
            Saving replaces the stored entry with what is shown here.
          <//>
        `}
        ${gone &&
        html`
          <${Notice}
            tone="caution"
            title=${`${originalName} is no longer in the configuration`}
            action=${gone.candidate ? html`<${Button} size="sm" disabled=${save.loading} onClick=${() => onRetarget?.(gone.candidate)}>Continue with ${gone.candidate}<//>` : null}
          >
            ${gone.candidate
              ? html`<span>It was renamed or deleted while this form was open, and <span class="mono">${gone.candidate}</span> appeared at the same time. What you entered is still here. Continue with ${gone.candidate} to apply it there.</span>`
              : html`<span>It was renamed or deleted while this form was open. What you entered is still here.</span>`}
            <span> Create provider adds it as a new entry: keys shown masked went with the old one and have to be typed again.</span>
          <//>
        `}

        <${Section} id="prov-sec-basics" title="Basics">
          ${mode === 'new'
            ? html`<${KindPicker} kinds=${KINDS} value=${draft.kind} onChange=${setKind} error=${issues.at('kind')} />`
            : html`<${Select}
                label="Kind"
                value=${draft.kind}
                onChange=${setKind}
                options=${KINDS.map((k) => ({ value: k.value, label: `${k.label} (${k.value})` }))}
                hint=${info.blurb}
                error=${issues.at('kind')}
              />`}

          ${draft.kind === 'openai-compat' &&
          html`
            <${Field} label="Well-known endpoints" hint="Picking one fills in the base URL. Everything stays editable.">
              <div class="prov-chips" role="group" aria-label="Well-known endpoints">
                ${COMPAT_PRESETS.map(
                  (preset) => html`
                    <button type="button" key=${preset.id} class="prov-chip" aria-pressed=${draft.base_url.trim() === preset.base_url ? 'true' : 'false'} onClick=${() => applyPreset(preset)}>
                      <span>${preset.label}</span>
                    </button>
                  `,
                )}
              </div>
            <//>
          `}

          <${Input}
            mono
            label="Name"
            value=${draft.name}
            onChange=${(v) => setSession((prev) => ({ ...prev, nameTouched: true, draft: { ...prev.draft, name: v } }))}
            placeholder="openai-main"
            autoFocus=${mode === 'new'}
            maxLength=${64}
            error=${issues.at('name')}
            hint=${renamed
              ? `Renaming ${originalName} changes the ids of its credentials, so their counters start again. Payload rules that name it follow.`
              : 'Lowercase letters, digits, - and _. Used in routes, logs and credential ids.'}
          />

          <${Input}
            mono
            icon="link"
            label="Base URL"
            optional=${draft.kind !== 'openai-compat'}
            value=${draft.base_url}
            onChange=${(v) => update({ base_url: v })}
            placeholder=${info.defaultUrl || 'https://api.example.com/v1'}
            disabled=${draft.kind === 'mock' && !draft.base_url}
            error=${issues.at('base_url')}
            hint=${baseUrlHint}
          />

          ${draft.kind === 'vertex' &&
          html`
            <${FormRow}>
              <${Input} mono label="Project" optional value=${draft.project} onChange=${(v) => update({ project: v })} placeholder="The service account's project" error=${issues.at('project')} hint="Google Cloud project id." />
              <${Input} mono label="Location" optional value=${draft.location} onChange=${(v) => update({ location: v })} placeholder="global" error=${issues.at('location')} hint="A region such as europe-west4, or global." />
            <//>
          `}

          <${Switch} label="Enabled" checked=${draft.enabled} onChange=${(v) => update({ enabled: v })} error=${issues.at('enabled')} hint="A disabled provider stays in the configuration and is skipped by the router." />
        <//>

        <${CredentialsSection} draft=${draft} update=${update} issues=${issues} paths=${paths} hasIssueUnder=${hasIssueUnder} initialRows=${session.initialCredentials} />

        <${Section} id="prov-sec-routing" title="Routing" description="How requests reach this provider, and how it is spoken to.">
          <${FormRow}>
            <${Input}
              mono
              label="Prefix"
              optional
              value=${draft.prefix}
              onChange=${(v) => update({ prefix: v })}
              placeholder="None"
              suffix=${draft.prefix.trim() ? '/model' : undefined}
              error=${issues.at('prefix')}
              hint="Its models are also served as prefix/model. One path segment, no slash."
            />
            <${NumberInput}
              label="Priority"
              value=${draft.priority}
              onChange=${(v) => update({ priority: v ?? 0 })}
              error=${issues.at('priority')}
              hint="Higher is tried first; lower only when every higher provider is unavailable."
            />
          <//>
          <${Input}
            mono
            label="Proxy"
            optional
            value=${draft.proxy}
            onChange=${(v) => update({ proxy: v })}
            placeholder="The gateway's proxy setting"
            error=${issues.at('proxy')}
            hint="An http://, https://, socks5:// or socks5h:// URL, or direct to bypass the gateway's proxy. A password in it is shown masked; leave the mask to keep the password."
          />

          ${openai &&
          html`
            <${Field}
              label="Wire API"
              error=${issues.at('wire_api')}
              hint=${draft.kind === 'openai'
                ? 'Auto uses whichever of the two the client speaks, and Responses for clients of other protocols.'
                : 'Auto means Chat Completions, which every compatible server has.'}
            >
              <${Segmented}
                label="Wire API"
                value=${draft.wire_api}
                onChange=${(v) => update({ wire_api: v })}
                options=${[
                  { value: 'auto', label: 'Auto' },
                  { value: 'chat', label: 'Chat Completions' },
                  { value: 'responses', label: 'Responses' },
                ]}
              />
            <//>
            <${FormRow}>
              <${TriState}
                label="Send the legacy max_tokens field"
                value=${draft.legacy_max_tokens}
                onChange=${(v) => update({ legacy_max_tokens: v })}
                defaultMeans=${draft.kind === 'openai-compat' ? 'on' : 'off'}
                error=${issues.at('legacy_max_tokens')}
                hint="Chat Completions only. Off sends max_completion_tokens."
              />
              <${TriState}
                label="Ask for usage in streams"
                value=${draft.stream_usage}
                onChange=${(v) => update({ stream_usage: v })}
                defaultMeans="on"
                error=${issues.at('stream_usage')}
                hint="Chat Completions only. Switch off for servers that reject stream_options."
              />
            <//>
          `}
          <${HeadersEditor} draft=${draft} update=${update} issues=${issues} />
        <//>

        <${ModelsSection} draft=${draft} update=${update} issues=${issues} paths=${paths} hasIssueUnder=${hasIssueUnder} providerName=${mode === 'edit' ? originalName : null} dirty=${dirty} onModelsFetched=${onModelsFetched} />

        <${FormError} error=${error} issues=${issues} title=${mode === 'new' || gone ? 'Could not create the provider' : 'Could not save the provider'} />
      <//>
    <//>
  `;
}
