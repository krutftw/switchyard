// Settings, Payload rules tab: patches to the JSON body sent upstream.
// GET /payload loads the three lists; PUT /payload replaces all of them.

import { html, useMemo, useState } from '../../../vendor/preact-htm.js';
import {
  Badge,
  Button,
  Drawer,
  EmptyState,
  ErrorState,
  Field,
  Form,
  Icon,
  IconButton,
  Input,
  Notice,
  Panel,
  Select,
  Skeleton,
  TagInput,
  confirm,
  toast,
} from '../../components/index.js';
import { prefersReducedMotion } from '../../lib/dom.js';
import { plural, sentence } from '../../lib/format.js';
import { useResource, useUid } from '../../lib/hooks.js';
import { ApiError } from '../../lib/api.js';
import { ConflictNotice, SaveBar, SaveError, confirmDiscard, focusMoved, focusSoon, moveItem, rowId, useDiskInvalid, useFieldIssues, useListDraft, useRevealProblem, useSaveHotkey, useUnsavedGuard } from './common.js';
import { exact, getExact, holdsNull, inexactNumbers, lossIn, oversizedIntegers, parseExact } from './exact.js';

// The rules are read with their numbers kept exact (see exact.js): saving
// one rule sends all of them back, and the others must arrive unchanged.
const loadRules = (signal) => getExact('/payload', signal);

const KINDS = [
  {
    id: 'default',
    title: 'Default',
    summary: 'Sets a field only when the client did not send it.',
    detail: 'When several default rules set the same field, the first one in the list wins.',
    empty: 'No default rules. Add one to fill in a field that clients leave out.',
  },
  {
    id: 'override',
    title: 'Override',
    summary: 'Always sets a field, replacing what the client sent.',
    detail: 'When several override rules set the same field, the last one in the list wins.',
    empty: 'No override rules. Add one to force a field to a value.',
  },
  {
    id: 'filter',
    title: 'Filter',
    summary: 'Removes fields before the request goes upstream.',
    detail: 'Runs last, so it also removes what a default or an override rule has set.',
    empty: 'No filter rules. Add one to strip a field a provider rejects.',
  },
];

const PROTOCOLS = [
  { value: 'openai-chat', label: 'OpenAI Chat Completions' },
  { value: 'openai-responses', label: 'OpenAI Responses' },
  { value: 'anthropic', label: 'Anthropic Messages' },
  { value: 'gemini', label: 'Gemini' },
];

const EXAMPLES = [
  {
    kind: 'default',
    title: 'Ask Gemini for thought summaries',
    text: 'Gemini returns a summary of its reasoning only when asked. This asks for it, unless the client made the choice itself.',
    rule: { models: ['gemini-*'], protocol: 'gemini', provider: '', set: { 'generationConfig.thinkingConfig.includeThoughts': true }, remove: [] },
    sends: '{"contents": [...]}',
    gets: '{"contents": [...], "generationConfig": {"thinkingConfig": {"includeThoughts": true}}}',
    note: 'A client that sends includeThoughts: false keeps false.',
  },
  {
    kind: 'filter',
    title: 'Drop fields a local server rejects',
    text: 'Some OpenAI-compatible servers answer 400 to fields they do not know. This removes two of them for every model.',
    rule: { models: ['*'], protocol: 'openai-chat', provider: '', set: {}, remove: ['metadata', 'store'] },
    sends: '{"model": "llama3", "messages": [...], "store": true, "metadata": {"user": "42"}}',
    gets: '{"model": "llama3", "messages": [...]}',
    note: 'Pick a provider in the rule to limit it to that server.',
  },
];

// ---------------------------------------------------------------------------
// Shapes
// ---------------------------------------------------------------------------

const toRule = (rule) => ({
  _id: rowId(),
  models: [...(rule.models ?? [])],
  protocol: rule.protocol ?? null,
  provider: rule.provider ?? '',
  set: { ...(rule.set ?? {}) },
  remove: [...(rule.remove ?? [])],
});

const toDraft = (data) => Object.fromEntries(KINDS.map((kind) => [kind.id, (data?.[kind.id] ?? []).map(toRule)]));

/** A rule as PUT /payload takes it: only the fields that apply to its list. */
function cleanRule(kind, rule) {
  const out = { models: rule.models };
  if (rule.protocol) out.protocol = rule.protocol;
  if (rule.provider) out.provider = rule.provider;
  if (kind === 'filter') out.remove = rule.remove;
  else out.set = rule.set;
  return out;
}

const toBody = (draft) => Object.fromEntries(KINDS.map((kind) => [kind.id, draft[kind.id].map((rule) => cleanRule(kind.id, rule))]));

/**
 * Where an issue of a refused save points. Its path is its place in the body
 * that was sent, as useIssues writes it: "override.0.set.x" ->
 * { kind: 'override', index: 0, rest: 'set.x' }.
 */
function parseIssuePath(path) {
  const match = /^(default|override|filter)(?:\[(\d+)\]|\.(\d+))(?:\.(.*))?$/.exec(path);
  return match ? { kind: match[1], index: Number(match[2] ?? match[3]), rest: match[4] ?? '' } : null;
}

const protocolLabel = (value) => PROTOCOLS.find((p) => p.value === value)?.label ?? value;

/** One line saying what a rule does, for confirmations. */
function describeRule(kind, rule) {
  const what = kind === 'filter' ? `removes ${rule.remove.join(', ') || 'nothing'}` : `sets ${Object.keys(rule.set).join(', ') || 'nothing'}`;
  return `It ${what} for ${rule.models.join(', ') || 'no model'}.`;
}

// ---------------------------------------------------------------------------
// Rule editor
// ---------------------------------------------------------------------------

/**
 * Why `text` cannot be a rule's value, as a sentence, or null when it can.
 * It has to be JSON, and it has to fit the configuration file, which is
 * TOML: no null anywhere, and whole numbers of at most 64 bits.
 */
export function jsonProblem(text) {
  const trimmed = text.trim();
  if (trimmed === '') return 'Enter a JSON value, for example true, 0.2 or "high".';
  let value;
  try {
    value = parseExact(trimmed);
  } catch {
    if (/^[A-Za-z_][\w .-]*$/.test(trimmed)) return `Not valid JSON. Text needs double quotes: "${trimmed}".`;
    return 'Not valid JSON. Use true, false, a number, "text" in double quotes, [a list] or {"an": "object"}.';
  }
  if (value === null) return 'null cannot be saved: the configuration file has no null. To take a field out of the request, add a filter rule that removes it.';
  if (holdsNull(value)) return 'The value contains null, which the configuration file cannot hold. Leave that part out, or take the field out with a filter rule.';
  const oversized = oversizedIntegers(trimmed);
  if (oversized.length > 0) return `${oversized[0]} is too large: whole numbers in the configuration file end at 9223372036854775807. Put it in double quotes if the provider takes it as text.`;
  if (!exact) {
    const rounded = inexactNumbers(trimmed).find((token) => /^-?\d+$/.test(token));
    if (rounded) return `This browser would save ${rounded} as ${Number(rounded)}. Enter it in a current browser, or edit the rule on the Raw file tab.`;
  }
  return null;
}

function pathProblem(path) {
  if (path.trim() === '') return 'Enter a path, for example reasoning.effort.';
  if (path.trim() !== path) return 'A path cannot start or end with a space.';
  if (/[\s\u0000-\u001f\u007f-\u009f]/u.test(path)) return 'A path cannot contain spaces or control characters.';
  // Match the gateway grammar: only a dot or backslash can be escaped.
  // An unescaped dot separates parts; none of those parts may be empty.
  let part = '';
  for (let i = 0; i < path.length; i += 1) {
    const ch = path[i];
    if (ch === '\\' && (path[i + 1] === '.' || path[i + 1] === '\\')) part += path[++i];
    else if (ch === '.') {
      if (!part) return 'A path cannot have an empty part. Escape a literal dot with a backslash.';
      part = '';
    } else part += ch;
  }
  if (!part) return 'A path cannot end with a dot. Escape a literal dot with a backslash.';
  return null;
}

function RuleEditor({ open, target, providers, serverIssues = [], onApply, onClose }) {
  const kind = KINDS.find((k) => k.id === target.kind);
  const isFilter = kind.id === 'filter';
  const initial = target.rule;
  const [models, setModels] = useState(initial.models);
  const [protocol, setProtocol] = useState(initial.protocol ?? '');
  const [provider, setProvider] = useState(initial.provider ?? '');
  const [fields, setFields] = useState(() => {
    const rows = Object.entries(initial.set).map(([path, value]) => ({ id: rowId(), path, sourcePath: path, text: JSON.stringify(value) }));
    return rows.length > 0 ? rows : [{ id: rowId(), path: '', text: '' }];
  });
  const [remove, setRemove] = useState(initial.remove);
  const [touched, setTouched] = useState(false);
  const [problems, setProblems] = useState({});
  const formId = useUid('rule-form');
  const serverAt = (path) => serverIssues.filter((issue) => issue.path === path).map((issue) => issue.message).join(' ') || undefined;
  const unchanged = (value, original) => JSON.stringify(value) === JSON.stringify(original);
  const removeError = unchanged(remove, initial.remove)
    ? serverIssues.filter((issue) => /^remove(?:\[\d+\]|\.\d+)?$/.test(issue.path)).map((issue) => {
        const index = /(?:\[|\.)(\d+)\]?$/.exec(issue.path)?.[1];
        return index == null ? issue.message : `${initial.remove[Number(index)]}: ${issue.message}`;
      }).join(' ') || undefined
    : undefined;

  const edit = (setter) => (value) => {
    setTouched(true);
    setProblems({});
    setter(value);
  };
  const setField = (id, patch) => {
    setTouched(true);
    setProblems((current) => ({ ...current, [`path:${id}`]: undefined, [`value:${id}`]: undefined }));
    setFields((rows) => rows.map((row) => (row.id === id ? { ...row, ...patch } : row)));
  };

  const close = async () => {
    if (!touched || (await confirmDiscard('this rule'))) onClose();
  };

  const apply = () => {
    const found = {};
    if (models.length === 0) found.models = 'Add at least one model pattern. Use * to match every model.';
    const set = {};
    if (isFilter) {
      if (remove.length === 0) found.remove = 'Add at least one path to remove.';
      else {
        const bad = remove.map((path) => ({ path, message: pathProblem(path) })).filter((issue) => issue.message);
        if (bad.length) found.remove = bad.map((issue) => `${issue.path}: ${issue.message}`).join(' ');
      }
    } else {
      const seen = new Set();
      fields.forEach((row) => {
        const path = row.path.trim();
        const pathIssue = pathProblem(row.path) ?? (seen.has(path) ? 'This path is set twice in this rule.' : null);
        const valueIssue = jsonProblem(row.text);
        if (pathIssue) found[`path:${row.id}`] = pathIssue;
        if (valueIssue) found[`value:${row.id}`] = valueIssue;
        seen.add(path);
        if (!pathIssue && !valueIssue) set[path] = parseExact(row.text.trim());
      });
    }
    setProblems(found);
    if (Object.keys(found).length > 0) return;
    onApply({ ...initial, models, protocol: protocol || null, provider, set: isFilter ? {} : set, remove: isFilter ? remove : [] });
  };

  // The saved provider may have been deleted since: keep it selectable.
  const providerOptions = useMemo(() => {
    const names = providers ?? [];
    const list = names.map((name) => ({ value: name, label: name }));
    if (provider && !names.includes(provider)) list.push({ value: provider, label: `${provider} (not configured)` });
    return list;
  }, [providers, provider]);

  return html`
    <${Drawer}
      open=${open}
      onClose=${close}
      title=${`${target.index == null ? 'Add' : 'Edit'} ${kind.title.toLowerCase()} rule`}
      width="600px"
      footer=${html`
        <${Button} onClick=${close}>Cancel<//>
        <${Button} type="submit" form=${formId} variant="primary">${target.index == null ? 'Add rule' : 'Apply'}<//>
      `}
    >
      <${Form} id=${formId} onSubmit=${apply}>
        <p class="muted">${kind.summary} The rule joins the list when you apply it; nothing reaches the gateway until you save.</p>
        <${TagInput}
          label="Model patterns"
          value=${models}
          onChange=${edit(setModels)}
          placeholder="gpt-*, claude-sonnet-4-5"
          hint="Matched against the provider's model id and the name the client asked for. * matches any run of characters."
          error=${problems.models || (unchanged(models, initial.models) ? serverAt('models') : undefined)}
          data-autofocus=""
        />
        <${Select}
          label="Protocol"
          optional
          value=${protocol}
          onChange=${edit(setProtocol)}
          placeholder="Any protocol"
          options=${PROTOCOLS}
          error=${protocol === (initial.protocol ?? '') ? serverAt('protocol') : undefined}
          hint="Paths are in the layout of the request sent to the provider. Limit the rule to the protocol whose layout the paths follow."
        />
        ${providers === null
          ? html`<${Input} label="Provider" optional mono value=${provider} onChange=${edit(setProvider)} error=${provider === (initial.provider ?? '') ? serverAt('provider') : undefined} placeholder="Any provider" hint="The provider list could not be loaded. Enter the provider's name exactly as configured." />`
          : html`<${Select} label="Provider" optional value=${provider} onChange=${edit(setProvider)} error=${provider === (initial.provider ?? '') ? serverAt('provider') : undefined} placeholder="Any provider" options=${providerOptions} hint="Apply only to requests this provider serves." />`}

        ${isFilter
          ? html`<${TagInput}
              label="Paths to remove"
              value=${remove}
              onChange=${edit(setRemove)}
              placeholder="metadata, store"
              hint=${html`Dotted paths: <span class="mono">metadata.trace_id</span>, <span class="mono">tools.0.strict</span>. Write a literal dot as <span class="mono">\\.</span>`}
              validate=${(tag) => pathProblem(tag)}
              error=${problems.remove || removeError}
            />`
          : html`
              <${Field} label="Fields to set" hint=${html`Paths are dotted: <span class="mono">reasoning.effort</span>, <span class="mono">messages.0.role</span>; write a literal dot as <span class="mono">\\.</span> Values are JSON: <span class="mono">true</span>, <span class="mono">0.2</span>, <span class="mono">"high"</span>, <span class="mono">{"type": "enabled"}</span>. Not <span class="mono">null</span>: to take a field out, use a filter rule.`}>
                <div class="settings-fields" role="group" aria-label="Fields to set">
                  ${fields.map(
                    (row, index) => html`
                      <div class="settings-fields-row" key=${row.id}>
                        <${Input}
                          mono
                          aria-label=${`Path ${index + 1}`}
                          placeholder="reasoning.effort"
                          value=${row.path}
                          onChange=${(path) => setField(row.id, { path })}
                          error=${problems[`path:${row.id}`] || (row.path === row.sourcePath ? serverAt(`set.${row.sourcePath}`) : undefined)}
                        />
                        <${Input}
                          mono
                          aria-label=${`JSON value ${index + 1}`}
                          placeholder='"high"'
                          value=${row.text}
                          onChange=${(text) => setField(row.id, { text })}
                          error=${problems[`value:${row.id}`]}
                        />
                        <${IconButton}
                          icon="x"
                          label=${`Remove field ${index + 1}`}
                          disabled=${fields.length === 1}
                          onClick=${() => {
                            setTouched(true);
                            setFields((rows) => rows.filter((r) => r.id !== row.id));
                          }}
                        />
                      </div>
                    `,
                  )}
                </div>
                <div>
                  <${Button}
                    size="sm"
                    icon="plus"
                    onClick=${() => {
                      setTouched(true);
                      setFields((rows) => [...rows, { id: rowId(), path: '', text: '' }]);
                    }}
                  >
                    Add field
                  <//>
                </div>
              <//>
            `}
      <//>
    <//>
  `;
}

// ---------------------------------------------------------------------------
// Lists
// ---------------------------------------------------------------------------

/** A dotted path that may break after its dots, not in the middle of a name. */
const breakable = (path) => path.split('.').flatMap((part, i) => (i === 0 ? [part] : [html`<wbr />`, `.${part}`]));

function RuleBody({ kind, rule }) {
  if (kind === 'filter') {
    return html`
      <ul class="settings-rule-ops">
        ${rule.remove.map((path) => html`<li key=${path}><span class="settings-rule-verb">remove</span><span class="mono settings-rule-path">${breakable(path)}</span></li>`)}
      </ul>
    `;
  }
  return html`
    <ul class="settings-rule-ops">
      ${Object.entries(rule.set).map(
        ([path, value]) => html`
          <li key=${path}>
            <span class="settings-rule-verb">set</span>
            <span class="mono settings-rule-path">${breakable(path)}</span>
            <span class="settings-rule-eq" aria-hidden="true">=</span>
            <span class="mono settings-rule-value" title=${JSON.stringify(value)}>${JSON.stringify(value)}</span>
          </li>
        `,
      )}
    </ul>
  `;
}

function RuleRow({ kind, rule, index, count, problems, onMove, onEdit, onDelete }) {
  const position = `${kind.title} rule ${index + 1}`;
  return html`
    <li class="settings-rule" data-row=${rule._id} data-invalid=${problems.length > 0 ? '' : undefined}>
      <div class="settings-rule-order">
        <span class="num settings-rule-index">${index + 1}</span>
        <${IconButton} icon="arrow-up" size="sm" data-move="up" label=${`Move ${position} up`} disabled=${index === 0} onClick=${() => onMove(-1)} />
        <${IconButton} icon="arrow-down" size="sm" data-move="down" label=${`Move ${position} down`} disabled=${index === count - 1} onClick=${() => onMove(1)} />
      </div>
      <div class="settings-rule-main">
        <div class="settings-rule-match">
          ${rule.models.map((pattern) => html`<${Badge} mono key=${pattern} title=${pattern}><span class="settings-rule-model">${pattern}</span><//>`)}
          ${rule.protocol && html`<${Badge} outline title="Only when the upstream request uses this protocol">${protocolLabel(rule.protocol)}<//>`}
          ${rule.provider && html`<${Badge} outline mono title="Only for this provider">${rule.provider}<//>`}
        </div>
        <${RuleBody} kind=${kind.id} rule=${rule} />
        ${problems.length > 0 &&
        html`<ul class="settings-rule-problems">
          ${problems.map((problem, i) => html`<li key=${i} class="field-error"><${Icon} name="alert-circle" size=${14} /><span>${problem}</span></li>`)}
        </ul>`}
      </div>
      <div class="settings-rule-actions">
        <${IconButton} icon="edit" label=${`Edit ${position}`} onClick=${onEdit} />
        <${IconButton} icon="trash" data-delete="" label=${`Delete ${position}`} onClick=${onDelete} />
      </div>
    </li>
  `;
}

function Pipeline() {
  return html`
    <ol class="settings-pipeline" aria-label="Order in which the lists apply">
      ${KINDS.map(
        (kind, index) => html`
          <li class="settings-pipeline-step" key=${kind.id}>
            <span class="settings-pipeline-num num" aria-hidden="true">${index + 1}</span>
            <span class="settings-pipeline-text">
              <strong>${kind.title}</strong>
              <span>${kind.summary}</span>
            </span>
          </li>
        `,
      )}
    </ol>
  `;
}

function Example({ example, onInsert }) {
  const kind = KINDS.find((k) => k.id === example.kind);
  return html`
    <div class="settings-example">
      <div class="settings-example-head">
        <${Badge} outline>${kind.title}<//>
        <h3>${example.title}</h3>
      </div>
      <p class="muted">${example.text}</p>
      <dl class="settings-example-io">
        <div><dt>Client sends</dt><dd class="mono">${example.sends}</dd></div>
        <div><dt>Provider gets</dt><dd class="mono">${example.gets}</dd></div>
      </dl>
      <p class="faint">${example.note}</p>
      <div><${Button} size="sm" icon="plus" onClick=${onInsert}>Insert this rule<//></div>
    </div>
  `;
}

// ---------------------------------------------------------------------------
// Tab
// ---------------------------------------------------------------------------

export function PayloadTab({ onDiskInvalid }) {
  const list = useListDraft('/payload', { toDraft, toBody, load: loadRules });
  const providers = useResource('/providers');
  const formId = useUid('payload-form');
  // Numbers this browser cannot send back the way they are in the file (only
  // where JSON.rawJSON is missing). Saving would rewrite them in rules the
  // user did not touch, so it is refused.
  const lost = lossIn(list.data);
  const [blocked, setBlocked] = useState(null);
  // The rule in the editor: { kind, index | null, rule, nonce }. It stays set
  // while the drawer animates out; `editorOpen` is what shows it.
  const [editing, setEditing] = useState(null);
  const [editorOpen, setEditorOpen] = useState(false);
  const openEditor = (target) => {
    setEditing({ ...target, nonce: rowId() });
    setEditorOpen(true);
  };
  const saveError = blocked ?? list.saveError;
  const issues = useFieldIssues(saveError);
  useDiskInvalid(list.saveError, onDiskInvalid);

  useUnsavedGuard(list.dirty, 'payload rules');
  // Nothing to do about the rule editor: a page shortcut is ignored while a drawer is open.
  useSaveHotkey(formId, list.dirty && !list.saving);
  useRevealProblem(formId, saveError);

  // Issues of the last refused save, by rule. Claiming them here keeps them
  // out of the form-level list.
  const byRule = useMemo(() => {
    const map = new Map();
    for (const issue of KINDS.flatMap((kind) => issues.under(kind.id))) {
      const at = parseIssuePath(issue.rawPath ?? issue.path);
      if (!at) continue;
      const key = `${at.kind}:${at.index}`;
      const text = at.rest ? `${at.rest}: ${issue.message}` : issue.message;
      map.set(key, [...(map.get(key) ?? []), text]);
    }
    return map;
  }, [issues]);

  if (list.error) {
    return html`<${Panel} flush><${ErrorState} title="Could not load the payload rules" error=${list.error} onRetry=${list.refresh} /><//>`;
  }
  if (list.loading || !list.draft) {
    return html`
      <div class="settings-form" aria-busy="true" aria-label="Loading payload rules">
        <${Panel}><${Skeleton} lines=${3} /><//>
        ${KINDS.map((kind) => html`<${Panel} key=${kind.id} title=${kind.title}><${Skeleton} lines=${2} /><//>`)}
      </div>
    `;
  }

  const draft = list.draft;
  const total = KINDS.reduce((n, kind) => n + draft[kind.id].length, 0);
  // Updates are functions of the current list, so two edits in one tick both land.
  const update = (kind, change) => list.setDraft((current) => ({ ...current, [kind]: change(current[kind]) }));
  const providerNames = providers.data ? providers.data.map((p) => p.name) : providers.error ? null : [];

  const remove = async (kind, index) => {
    const rule = draft[kind.id][index];
    const ok = await confirm({
      danger: true,
      title: `Delete ${kind.title.toLowerCase()} rule ${index + 1}?`,
      message: `${describeRule(kind.id, rule)} The rule leaves the list now and stops applying when you save.`,
      confirmLabel: 'Delete rule',
    });
    if (!ok) return;
    update(kind.id, (rules) => rules.filter((other) => other._id !== rule._id));
    // The button that was pressed is gone with its rule: the keyboard moves
    // to the rule that took its place, else to the one before, else to the
    // list's Add button.
    const neighbour = draft[kind.id][index + 1] ?? draft[kind.id][index - 1];
    focusSoon(() => (neighbour ? document.querySelector(`[data-row="${neighbour._id}"] [data-delete]`) : document.querySelector(`#payload-${kind.id} [data-add]`)));
  };

  const applyEdit = (rule) => {
    const { kind, index } = editing;
    update(kind, (rules) => (index == null ? [...rules, rule] : rules.map((other) => (other._id === rule._id ? rule : other))));
    setEditorOpen(false);
  };

  const insertExample = (example) => {
    update(example.kind, (rules) => [...rules, toRule(example.rule)]);
    // Show where it went: the list is further down the page.
    requestAnimationFrame(() => document.getElementById(`payload-${example.kind}`)?.scrollIntoView({ behavior: prefersReducedMotion() ? 'auto' : 'smooth', block: 'center' }));
  };

  const submit = async () => {
    if (lost.length > 0) {
      list.clearSaveError();
      setBlocked(new ApiError(0, `Nothing was saved. This browser cannot send ${lost.length === 1 ? 'one number' : `${lost.length} numbers`} in these rules back unchanged (${lost.slice(0, 3).join(', ')}${lost.length > 3 ? ', …' : ''}). Use a current browser, or edit the rules on the Raw file tab.`, { code: 'invalid' }));
      return;
    }
    setBlocked(null);
    if (await list.save()) toast.success('Payload rules saved', { description: total === 0 ? 'No rules: request bodies go upstream unchanged.' : `${plural(total, 'rule')} in effect.` });
  };

  return html`
    <${Form} id=${formId} class="settings-form" onSubmit=${submit}>
      <${ConflictNotice} list=${list} noun="payload rules" />
      ${list.stale && html`<${Notice} tone="caution" title="Could not refresh the payload rules">${sentence(list.stale.message)} The rules below are the last ones loaded.<//>`}
      ${lost.length > 0 &&
      html`<${Notice} tone="caution" title="These rules cannot be saved from this browser">
        ${lost.length === 1 ? 'One number' : `${lost.length} numbers`} in the rules (<span class="mono settings-break">${lost.slice(0, 3).join(', ')}${lost.length > 3 ? ', …' : ''}</span>) would be rewritten on the way back, for example <span class="mono">${lost[0]}</span> as <span class="mono">${String(Number(lost[0]))}</span>. You can look at the rules here; to change them, use a current browser or the Raw file tab.
      <//>`}

      <${Panel} title="How rules apply" description="Rules patch the JSON body on its way to the provider, after it has been translated to the provider's protocol.">
        <div class="stack">
          <p class="muted">Every rule whose model pattern, protocol and provider match the request takes part. The three lists always run in this order:</p>
          <${Pipeline} />
          <hr />
          <div class="settings-examples">
            ${EXAMPLES.map((example) => html`<${Example} key=${example.title} example=${example} onInsert=${() => insertExample(example)} />`)}
          </div>
        </div>
      <//>

      ${KINDS.map((kind, step) => {
        const rules = draft[kind.id];
        return html`
          <${Panel}
            key=${kind.id}
            id=${`payload-${kind.id}`}
            flush
            title=${html`<span class="settings-step num" aria-hidden="true">${step + 1}</span>${kind.title}`}
            description=${`${kind.summary} ${kind.detail}`}
            actions=${html`<${Button} size="sm" icon="plus" data-add="" onClick=${() => openEditor({ kind: kind.id, index: null, rule: toRule({}) })}>Add rule<//>`}
          >
            ${rules.length === 0
              ? html`<${EmptyState} compact icon="filter" title=${`No ${kind.title.toLowerCase()} rules`} description=${kind.empty} />`
              : html`
                  <ol class="settings-rules">
                    ${rules.map(
                      (rule, index) => html`
                        <${RuleRow}
                          key=${rule._id}
                          kind=${kind}
                          rule=${rule}
                          index=${index}
                          count=${rules.length}
                          problems=${byRule.get(`${kind.id}:${index}`) ?? []}
                          onMove=${(direction) => {
                            update(kind.id, (current) => moveItem(current, current.findIndex((other) => other._id === rule._id), direction));
                            focusMoved(rule._id, direction);
                          }}
                          onEdit=${() => openEditor({ kind: kind.id, index, rule })}
                          onDelete=${() => remove(kind, index)}
                        />
                      `,
                    )}
                  </ol>
                `}
          <//>
        `;
      })}

      <${SaveError} error=${saveError} issues=${issues} title="Could not save the payload rules" />
      <${SaveBar}
        dirty=${list.dirty}
        saving=${list.saving}
        what="payload rules"
        onDiscard=${() => {
          setBlocked(null);
          list.discard();
        }}
        summary="Unsaved changes to the payload rules"
        saveLabel="Save rules"
      />
    <//>
    ${editing && html`<${RuleEditor} key=${editing.nonce} open=${editorOpen} target=${editing} providers=${providerNames} serverIssues=${issues.all.flatMap((issue) => {
      const at = parseIssuePath(issue.rawPath ?? issue.path);
      return at && at.kind === editing.kind && at.index === editing.index ? [{ path: at.rest, message: issue.message }] : [];
    })} onApply=${applyEdit} onClose=${() => setEditorOpen(false)} />`}
  `;
}
