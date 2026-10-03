// Playground (#/playground): try any model through any of the four client
// protocols and see exactly what goes over the wire.
//
// The HTTP view sends through POST /admin/api/playground (no client key
// needed; the request runs as the built-in "dashboard" client). The
// WebSocket view mints a single-use ticket with a header-only client key,
// then talks to GET /v1/responses?ticket=… without putting the key in a URL.
//
//   #/playground?mode=ws           the WebSocket view (each pick is a history step)
//   #/playground?inspect=events    the inspector tab (each pick is a history step)
//   #/playground?from=<request id> load that request's captured body in raw mode
//
// Sub-modules, in pages/playground/:
//   protocols.js     request bodies, answer parsing, errors, curl, SDK snippets
//   run.js           the call itself: headers first, events with time offsets
//   conversation.js  turns, reasoning, tool calls, error notes
//   inspector.js     Request / Events / Response / Info / Code
//   framelist.js     the windowed list of events and frames
//   combobox.js      the model field
//   socket.js        the WebSocket view
//   session.js       the conversation and the last request, kept while the page is left
//   focus.js         where the keyboard goes when a control removes or disables itself

import { html, useEffect, useLayoutEffect, useMemo, useRef } from '../../vendor/preact-htm.js';
import {
  Button,
  CopyButton,
  EmptyState,
  Icon,
  IconButton,
  Menu,
  Notice,
  NumberInput,
  Page,
  Panel,
  Select,
  Switch,
  Tabs,
  Textarea,
  confirm,
  formatJson,
  toast,
  useIssues,
} from '../components/index.js';
import { ApiError, api } from '../lib/api.js';
import { useCommands } from '../lib/commands.js';
import { copyText, loadStyles, nextId } from '../lib/dom.js';
import { formatDateTime, formatNumber, plural } from '../lib/format.js';
import { useDebounced, useLocalStorage, useMediaQuery, useResource } from '../lib/hooks.js';
import { liveState, useLive, useLiveGap } from '../lib/live.js';
import { replayProblem } from '../lib/replay.js';
import { href, useQueryParam } from '../lib/router.js';
import { useStore } from '../lib/store.js';
import Combobox from './playground/combobox.js';
import Conversation from './playground/conversation.js';
import { focusFirst, pageMain, placeFocus, rescueFocus } from './playground/focus.js';
import Inspector, { EVENT_CAP, INSPECTOR_TABS } from './playground/inspector.js';
import buildBody, {
  DEFAULT_SETTINGS,
  EFFORTS,
  EFFORT_BUDGET,
  PROTOCOLS,
  PROTOCOL_IDS,
  anthropicMaxTokens,
  buildCurl,
  buildEnvelope,
  buildSnippets,
  checkRawBody,
  createReader,
  digestEvent,
  protocolInfo,
  publicPath,
  readError,
  splitSuffix,
  suffixKind,
  summarizeRawBody,
} from './playground/protocols.js';
import runPlayground from './playground/run.js';
import { repaint, session, setter, work } from './playground/session.js';
import SocketPanel from './playground/socket.js';

await loadStyles('pages/playground.css');

/** Request records seen on the live connection, by id, for the Info tab. */
const RECORD_MEMORY = 24;

/** Issue paths of the playground envelope that have a field on this page. */
const FIELD_PATHS = ['protocol', 'model', 'stream', 'body'];

/** Where the gateway itself lives: this page's origin, minus the dashboard's own directory. */
function gatewayOrigin() {
  if (typeof location === 'undefined') return '';
  const dir = location.pathname.replace(/[^/]*$/, '');
  return location.origin + dir.replace(/\/admin\/$/, '/').replace(/\/$/, '');
}

function cleanSettings(stored) {
  const s = { ...DEFAULT_SETTINGS, ...(stored && typeof stored === 'object' ? stored : {}) };
  if (!PROTOCOL_IDS.includes(s.protocol)) s.protocol = DEFAULT_SETTINGS.protocol;
  if (!EFFORTS.includes(s.effort)) s.effort = 'default';
  if (typeof s.model !== 'string') s.model = '';
  if (typeof s.system !== 'string') s.system = '';
  if (typeof s.temperature !== 'number') s.temperature = null;
  if (typeof s.maxTokens !== 'number') s.maxTokens = null;
  s.stream = !!s.stream;
  s.tools = !!s.tools;
  return s;
}

/** How the reasoning setting is written in the protocol's own field. */
function effortHint(protocol, effort) {
  if (effort === 'default') return 'Nothing is sent. The model decides how much to reason.';
  const budget = EFFORT_BUDGET[effort];
  if (protocol === 'anthropic') return effort === 'none' ? 'Sent as thinking.type: "disabled".' : `Sent as thinking.budget_tokens: ${formatNumber(budget)}.`;
  if (protocol === 'gemini') return `Sent as generationConfig.thinkingConfig.thinkingBudget: ${effort === 'none' ? 0 : formatNumber(budget)}.`;
  return `Sent as ${protocolInfo(protocol).effortField}: "${effort}".`;
}

const MAX_TOKENS_FIELD = {
  'openai-chat': 'max_completion_tokens',
  'openai-responses': 'max_output_tokens',
  anthropic: 'max_tokens',
  gemini: 'generationConfig.maxOutputTokens',
};

const hasContent = (blocks) => blocks.some((b) => b.type === 'tool_call' || b.redacted || b.text !== '');

// The page's working state lives in session.js, not in the component: the
// page is unmounted when the reader follows "Open request", and comes back
// to the same conversation. These write to that store.
const setTurns = setter('turns');
const setDraft = setter('draft');
const setBusy = setter('busy');
const setView = setter('view');
const setRaw = setter('raw');
const setRawError = setter('rawError');
const setModelError = setter('modelError');
const setFailure = setter('failure');
const setReplay = setter('replay');
const patchTurn = (id, change) => setTurns((list) => list.map((t) => (t.id === id ? { ...t, ...change } : t)));
const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * What a reasoning suffix on the model name does, as the gateway applies it
 * (crates/core: a suffix wins over the body; `auto` is left to the upstream).
 */
function suffixHint(raw) {
  const kind = suffixKind(raw);
  const shown = `(${raw})`;
  if (kind === 'level') return `The gateway reads ${shown} as the reasoning effort; it wins over the setting below.`;
  if (kind === 'off') return `The gateway reads ${shown} as reasoning switched off; it wins over the setting below.`;
  if (kind === 'budget') return `The gateway reads ${shown} as a thinking budget in tokens and fits it to what the model accepts; it wins over the setting below.`;
  if (kind === 'auto') {
    return `The gateway reads ${shown} as the model's own choice of depth: an OpenAI upstream is sent no effort, a Claude model with effort levels thinks adaptively, a Gemini model that allows it gets a dynamic budget. It wins over the setting below.`;
  }
  return `${shown} is not a reasoning depth the gateway knows (an effort name, auto, none or a token budget): it is dropped, and the setting below applies.`;
}

export default function Playground() {
  const origin = useMemo(gatewayOrigin, []);
  // The transport and the inspector tab are picked from a few choices: each
  // pick is a step in the history, so Back returns to the previous one.
  // Nothing on the page is lost by such a step (the conversation and the
  // raw body live in session.js), so none of them asks first.
  const [mode, setMode] = useQueryParam('mode', 'http', { push: true });
  const [inspect, setInspect] = useQueryParam('inspect', 'request', { push: true });
  const [from, setFrom] = useQueryParam('from', '');
  const coarse = useMediaQuery('(pointer: coarse)');

  // ---- Settings (kept on this device; none of them is a secret) -----------
  const [stored, setStored] = useLocalStorage('playground.settings', DEFAULT_SETTINGS);
  const settings = useMemo(() => cleanSettings(stored), [stored]);
  const patch = (change) => setStored((prev) => ({ ...cleanSettings(prev), ...change }));
  const set = (field) => (value) => patch({ [field]: value });
  const [showParams, setShowParams] = useLocalStorage('playground.params', false);
  const { protocol } = settings;
  const model = settings.model.trim();

  // ---- Models: live when the connection is up, polled when it is not ------
  const liveStatus = useStore(liveState, (s) => s.status);
  const models = useResource('/models', { pollMs: liveStatus === 'open' ? 0 : 30_000 });
  useLive('config.reloaded', models.refresh);
  useLive('credential', models.refresh);
  // Frames sent while the connection was down, or dropped for it, are gone: ask again.
  useLiveGap(models.refresh);

  const modelList = Array.isArray(models.data) ? models.data : [];
  const modelOptions = useMemo(
    () =>
      modelList.map((m) => {
        const routes = Array.isArray(m.routes) ? m.routes : [];
        const ready = routes.reduce((n, r) => n + (r.credentials_available ?? 0), 0);
        const providers = [...new Set(routes.map((r) => r.provider))];
        // `ignored`: an alias none of whose targets can be routed. `credentials_available`
        // counts the credentials that could serve the model now: 0 when every one is
        // resting, switched off or unusable, which this list cannot tell apart.
        const dead = m.ignored || routes.length === 0;
        return {
          value: m.name,
          hint: m.alias_targets ? `alias of ${m.alias_targets.join(', ')}` : providers.join(', '),
          tone: dead ? 'stop' : ready === 0 ? 'caution' : undefined,
          toneLabel: dead ? 'no route' : 'no credential',
        };
      }),
    [models.data],
  );

  // Start with a model that can answer, once, when none was ever chosen.
  const seeded = useRef(false);
  useEffect(() => {
    if (seeded.current || modelList.length === 0) return;
    seeded.current = true;
    if (settings.model) return;
    // A model a provider serves under its own name, before an alias: the list is alphabetical.
    const ready = modelList.filter((m) => (m.routes ?? []).some((r) => r.credentials_available > 0));
    const usable = ready.find((m) => !m.alias_targets) ?? ready[0] ?? modelList[0];
    patch({ model: usable.name });
  }, [modelList.length]);

  // "name(high)" is the listed model "name" with a reasoning suffix.
  const suffixed = modelList.some((m) => m.name === model) ? null : splitSuffix(model);
  const knownModel = modelList.find((m) => m.name === (suffixed ? suffixed[0] : model));
  let modelHint = 'Pick a listed model, or type any name.';
  if (knownModel) {
    const routes = knownModel.routes ?? [];
    const ready = routes.reduce((n, r) => n + (r.credentials_available ?? 0), 0);
    if (knownModel.ignored) modelHint = `None of this alias's targets (${(knownModel.alias_targets ?? []).join(', ')}) can be routed, so requests for it fail. Change the targets on the Models page.`;
    else if (routes.length === 0) modelHint = 'No provider serves this name right now. Requests for it fail.';
    else if (ready === 0) modelHint = 'No credential can serve this model right now: each one is resting, switched off or unusable. Requests fail until one is back.';
    else modelHint = `Served by ${[...new Set(routes.map((r) => r.provider))].join(', ')}.`;
    // An alias target written with a suffix pins the depth on its routes:
    // there it wins over a suffix on the alias and over the request body.
    const pinned = [...new Set(routes.map((r) => r.target).filter((t) => typeof t === 'string' && suffixKind(splitSuffix(t)?.[1]) != null))];
    if (pinned.length > 0) {
      modelHint += ` Its target ${pinned.join(', ')} pins the reasoning depth: on that route ${suffixed ? `(${suffixed[1]}) and the setting below change` : 'the setting below changes'} nothing.`;
    } else if (suffixed) {
      modelHint += ` ${suffixHint(suffixed[1])}`;
    }
  } else if (models.error && !models.data) {
    modelHint = 'The model list could not be loaded. Type a name; it is sent as written.';
  } else if (model && models.data) {
    modelHint = 'Not in the model list. It is sent as written; the gateway resolves suffixes such as model(high).';
  }

  // ---- Conversation and the last request ----------------------------------
  const { turns, draft, busy, view, raw, rawError, modelError, failure, replay } = useStore(session);
  const modelInput = useRef(null);
  const run = work.run;

  // Where the keyboard goes when the control that had it removes or disables
  // itself (see focus.js): the message field, else the composer's main button.
  const composerBox = useRef(null);
  // On a touch screen a focused text field brings the keyboard up over the
  // answer: there the transcript takes the focus instead.
  const composerTargets = () =>
    coarse
      ? [composerBox.current?.parentElement?.querySelector('.play-transcript[tabindex]'), pageMain()]
      : [composerBox.current?.querySelector('textarea'), composerBox.current?.querySelector('.play-composer-actions > .btn:last-child'), pageMain()];
  // "Stop", once the request has ended, is a "Send" that is disabled while
  // there is no message. This runs before the browser lets go of the focus.
  useLayoutEffect(() => {
    const at = document.activeElement;
    if (!busy && at && at.disabled && composerBox.current?.contains(at)) focusFirst(...composerTargets());
  }, [busy]);

  const issues = useIssues(failure);

  // The finished record of our own request adds what the response headers do
  // not say: the upstream protocol, the credential, the attempts, the cost.
  useLive('request.finished', (record) => {
    if (!record || typeof record.id !== 'string') return;
    const seen = work.records;
    seen.set(record.id, record);
    if (seen.size > RECORD_MEMORY) seen.delete(seen.keys().next().value);
    const current = work.run;
    if (current && current.meta?.requestId === record.id && !current.record) {
      current.record = record;
      repaint();
    }
  });

  // ---- The body of the next request ---------------------------------------
  const requestSettings = { ...settings, model };
  const nextText = useMemo(
    () => JSON.stringify(buildBody(protocol, requestSettings, turns, draft), null, 2),
    [protocol, model, settings.stream, settings.system, settings.temperature, settings.maxTokens, settings.effort, settings.tools, turns, draft],
  );
  // The text is checked as it is typed, so the model and the path follow the
  // body at once. Only the complaint waits until the typing pauses: a body is
  // not valid JSON halfway through a word.
  const rawCheck = useMemo(() => (raw.on ? checkRawBody(raw.text) : null), [raw.on, raw.text]);
  const rawSettled = useDebounced(raw.text, 400) === raw.text;
  // While the text is broken mid-edit, the last body that parsed still says
  // which model and stream flag are meant. A new seed (raw mode entered
  // again, a replay loaded) starts over.
  const lastGood = useRef({ seed: null, value: null });
  if (!raw.on || lastGood.current.seed !== raw.seed) lastGood.current = { seed: raw.on ? raw.seed : null, value: null };
  if (rawCheck?.value) lastGood.current.value = rawCheck.value;
  // In raw mode the body names its own model and stream flag, except for
  // Gemini, where both are part of the URL and so still come from the form.
  const rawValue = rawCheck?.value ?? lastGood.current.value;
  const effectiveModel = raw.on && protocol !== 'gemini' ? (typeof rawValue?.model === 'string' ? rawValue.model : model) : model;
  const effectiveStream = raw.on && protocol !== 'gemini' ? (rawValue ? rawValue.stream === true : settings.stream) : settings.stream;
  const nextPath = publicPath(protocol, effectiveModel, effectiveStream);
  const currentBodyText = raw.on ? raw.text : nextText;
  const curl = useMemo(
    () => buildCurl({ origin, protocol, model: effectiveModel, stream: effectiveStream, bodyText: currentBodyText }),
    [origin, protocol, effectiveModel, effectiveStream, currentBodyText],
  );
  const snippets = useMemo(() => buildSnippets({ origin, protocol, model: effectiveModel }), [origin, protocol, effectiveModel]);

  // ---- Sending -------------------------------------------------------------
  /**
   * Send one request and follow it to the end. `history` is the conversation
   * the answer is appended to; `request` is what goes over the wire.
   */
  const start = async (history, request) => {
    work.abort?.abort();
    work.lookup?.abort();
    const controller = new AbortController();
    work.abort = controller;
    const id = nextId('turn');
    const reader = createReader(request.protocol);
    const current = {
      id,
      status: 'waiting',
      protocol: request.protocol,
      model: request.model,
      stream: request.stream,
      path: request.path,
      bodyText: request.bodyText,
      startedAt: Date.now(),
      meta: null,
      firstContentMs: null,
      events: [],
      eventTotal: 0,
      responseText: null,
      usage: null,
      finish: null,
      error: null,
      record: null,
    };
    work.run = current;
    setFailure(null);
    setModelError(null);
    setRawError(null);
    setTurns([...history, { id, role: 'assistant', status: 'waiting', blocks: [], model: request.model, protocol: request.protocol, detached: !!request.detached, request }]);
    setBusy(true);
    // In raw mode the editor stays in view: the next thing the reader does is
    // change the body and send it again.
    setView(session.get().raw.on ? 'next' : 'sent');

    const isCurrent = () => work.run === current;
    let frame = 0;
    const paint = () => {
      frame = 0;
      if (!isCurrent()) return;
      patchTurn(id, { blocks: reader.snapshot(), status: current.status === 'streaming' ? 'streaming' : 'waiting' });
      repaint();
    };
    const schedule = () => {
      if (!frame) frame = requestAnimationFrame(paint);
    };

    let seq = 0;
    let error = null;
    let status = 'done';
    try {
      await runPlayground(request.envelope, {
        signal: controller.signal,
        onResponse(meta) {
          current.meta = meta;
          schedule();
        },
        onEvent(event, at) {
          seq += 1;
          current.eventTotal += 1;
          const failed = event.event === 'error' || (event.json && typeof event.json === 'object' && (event.json.error != null || event.json.type === 'response.failed'));
          current.events.push({ seq, at, name: event.event, note: digestEvent(request.protocol, event), data: event.data, tone: failed ? 'stop' : undefined });
          if (current.events.length > EVENT_CAP) current.events.splice(0, current.events.length - EVENT_CAP);
          reader.event(event);
          if (current.firstContentMs == null && hasContent(reader.state.blocks)) current.firstContentMs = at;
          current.status = 'streaming';
          current.usage = reader.state.usage;
          current.finish = reader.state.finish;
          schedule();
        },
        onBody(text, json) {
          current.responseText = text;
          const meta = current.meta;
          if (meta.ok) {
            reader.response(json);
            if (json === undefined) error = { kind: 'http', status: meta.status, message: 'The gateway answered with a body that is not JSON. It is under Response in the inspector.', issues: [] };
          } else {
            const parsed = readError(json ?? text.slice(0, 600), meta.status);
            error = { kind: 'http', ...parsed, status: meta.status, retryAfter: meta.retryAfter ?? parsed.retryAfter };
          }
        },
      });
      if (!error && reader.state.error) error = { kind: current.meta?.streamed ? 'stream' : 'http', ...reader.state.error };
      if (error) status = 'error';
    } catch (thrown) {
      if (thrown instanceof ApiError && thrown.aborted) {
        status = 'stopped';
      } else if (thrown instanceof ApiError) {
        status = 'error';
        error = { kind: 'network', status: 0, message: thrown.message, issues: [] };
      } else {
        if (frame) cancelAnimationFrame(frame);
        if (isCurrent()) setBusy(false);
        throw thrown;
      }
    }
    if (frame) cancelAnimationFrame(frame);
    if (work.abort === controller) work.abort = null;
    if (!isCurrent()) return;

    const meta = current.meta;
    if (meta && meta.totalMs == null) meta.totalMs = Date.now() - current.startedAt;
    current.status = status;
    current.usage = reader.state.usage;
    current.finish = reader.state.finish;
    current.error = error;

    // The admin API's own refusals name the field at fault: show them there.
    let shown = error;
    if (error && error.issues?.length > 0) {
      setFailure(new ApiError(error.status, error.message, { issues: error.issues }));
      shown = { ...error, issues: error.issues.filter((issue) => !FIELD_PATHS.includes(issue.path)) };
    }
    patchTurn(id, {
      status,
      blocks: reader.snapshot(),
      finish: reader.state.finish,
      usage: reader.state.usage,
      error: shown,
      requestId: meta?.requestId ?? null,
      durationMs: meta?.totalMs ?? Date.now() - current.startedAt,
    });
    setBusy(false);
    repaint();

    // The record: from the live connection when it brought it, else asked for.
    const requestId = meta?.requestId;
    if (!requestId) return;
    const known = work.records.get(requestId);
    if (known) {
      current.record = known;
      repaint();
      return;
    }
    // Its own signal: the request's is already aborted when the reader
    // pressed Stop, and a stopped request has a record like any other.
    const lookup = new AbortController();
    work.lookup = lookup;
    await wait(700);
    for (let attempt = 0; attempt < 3 && isCurrent() && !current.record && !lookup.signal.aborted; attempt += 1) {
      try {
        const detail = await api.get(`/requests/${encodeURIComponent(requestId)}`, { signal: lookup.signal });
        if (isCurrent() && detail?.record) {
          current.record = detail.record;
          repaint();
        }
        break;
      } catch (cause) {
        // Not written yet (404), or statistics are off: the Info tab simply has fewer rows.
        if (!(cause instanceof ApiError) || cause.status !== 404) break;
        await wait(1200);
      }
    }
    if (work.lookup === lookup) work.lookup = null;
  };

  /** Send `history` (which ends with what the model should answer) built from the form. */
  const sendForm = (history) => {
    if (!model) {
      setModelError('Choose a model, or type its name.');
      modelInput.current?.focus();
      return false;
    }
    const bodyText = JSON.stringify(buildBody(protocol, requestSettings, history), null, 2);
    start(history, {
      protocol,
      model,
      stream: settings.stream,
      path: publicPath(protocol, model, settings.stream),
      bodyText,
      envelope: buildEnvelope(protocol, bodyText, { model, stream: settings.stream }),
    });
    return true;
  };

  const sendText = (text) => {
    const message = text.trim();
    if (!message || busy) return;
    if (sendForm([...turns, { id: nextId('turn'), role: 'user', text: message }])) setDraft('');
  };

  /** Send the body in the raw editor as it is written now, as the next exchange after `history`. */
  const sendRaw = (history = turns) => {
    if (busy) return;
    const check = checkRawBody(raw.text);
    if (check.error) {
      setRawError(check.error);
      // Shows the field at fault; the reader did not pick the tab.
      setInspect('request', { replace: true });
      setView('next');
      return;
    }
    const body = check.value;
    const rawModel = protocol === 'gemini' ? model : typeof body.model === 'string' ? body.model : '';
    if (protocol === 'gemini' && !rawModel) {
      setModelError('Gemini names the model in the URL. Choose one here.');
      modelInput.current?.focus();
      return;
    }
    const stream = protocol === 'gemini' ? settings.stream : body.stream === true;
    start([...history, { id: nextId('turn'), role: 'raw', text: summarizeRawBody(protocol, body) }], {
      protocol,
      model: rawModel,
      stream,
      path: publicPath(protocol, rawModel, stream),
      bodyText: raw.text,
      envelope: buildEnvelope(protocol, raw.text, { model: rawModel, stream }),
      detached: true,
    });
  };

  const sendToolResults = (turnId, results) => {
    // Raw mode sends only what is in the editor; the result form is not offered then.
    if (busy || raw.on) return;
    // The button that sent them leaves with the result form.
    rescueFocus(composerTargets);
    sendForm(turns.map((t) => (t.id === turnId ? { ...t, blocks: t.blocks.map((b) => (b.type === 'tool_call' && b.id in results ? { ...b, result: results[b.id] } : b)) } : t)));
  };

  let lastAnswer = -1;
  for (let i = turns.length - 1; i >= 0; i -= 1) {
    if (turns[i].role === 'assistant') {
      lastAnswer = i;
      break;
    }
  }

  /**
   * Ask again. With the form: drop the last answer and send the conversation
   * with the settings as they are now. In raw mode: send the body as it is in
   * the editor now, in place of the raw exchange it corrects (an answer the
   * form produced is kept, and the raw exchange follows it). A raw exchange
   * met after raw mode was left can only be repeated as it was sent.
   */
  const regenerate = () => {
    if (busy || lastAnswer === -1) return;
    // "Send again" in an error note goes away with the answer it belongs to.
    rescueFocus(composerTargets);
    const target = turns[lastAnswer];
    if (raw.on) {
      let keep = turns.length;
      if (target.detached) keep = turns[lastAnswer - 1]?.role === 'raw' ? lastAnswer - 1 : lastAnswer;
      sendRaw(turns.slice(0, keep));
      return;
    }
    const history = turns.slice(0, lastAnswer);
    if (target.detached && target.request) start(history, target.request);
    else sendForm(history);
  };

  const stop = () => work.abort?.abort();

  const clear = async () => {
    if (turns.length === 0) return;
    const ok = await confirm({
      danger: true,
      title: 'Clear the conversation?',
      message: `${plural(turns.length, 'turn')} and the inspector's copy of the last request are removed from this page. Entries in the request log are kept.`,
      confirmLabel: 'Clear conversation',
    });
    if (!ok) return;
    work.abort?.abort();
    work.lookup?.abort();
    work.run = null;
    setTurns([]);
    setBusy(false);
    setFailure(null);
    setView('next');
  };

  // ---- Raw mode ------------------------------------------------------------
  const enterRaw = (text) => {
    setRaw({ on: true, text, seed: text });
    setRawError(null);
    setView('next');
  };

  /** Resolves true when raw mode was left (false: the reader kept the edited body). */
  const toggleRaw = async (on) => {
    if (on) {
      enterRaw(nextText);
      return false;
    }
    if (raw.text !== raw.seed) {
      const ok = await confirm({
        danger: true,
        title: 'Discard the edited body?',
        message: 'Your changes to the raw JSON are lost. The body is built from the form again.',
        confirmLabel: 'Discard changes',
      });
      if (!ok) return false;
    }
    setRaw({ on: false, text: '', seed: '' });
    setRawError(null);
    return true;
  };

  /** "Leave raw mode" in the composer: the button goes, the message field takes its place. */
  const leaveRaw = async () => {
    if (await toggleRaw(false)) placeFocus(composerTargets);
  };

  const rebuildRaw = async () => {
    if (raw.text !== raw.seed) {
      const ok = await confirm({
        danger: true,
        title: 'Replace the edited body?',
        message: 'Your changes to the raw JSON are replaced by the body the form builds now.',
        confirmLabel: 'Replace body',
      });
      if (!ok) return;
    }
    enterRaw(nextText);
  };

  // ---- Replaying a recorded request (?from=) -------------------------------
  const source = useResource(from ? `/requests/${encodeURIComponent(from)}` : null);
  useEffect(() => {
    if (!from) {
      work.appliedFrom = null;
      return;
    }
    // Coming back to the page with the same ?from= does not load it again
    // over what was edited since.
    if (!source.data || work.appliedFrom === from) return;
    const record = source.data.record ?? {};
    work.appliedFrom = from;
    // "no-body", "protocol" (not one of the four) or "endpoint" (embeddings,
    // token counts and the like): the same check the request drawer makes
    // before it offers "Open in playground".
    const problem = replayProblem(record, source.data.bodies);
    if (problem) {
      setReplay({ id: from, kind: problem, record });
      return;
    }
    const editing = session.get().raw;
    // The editor still holds this request's body, as it was loaded or as it
    // was edited since: Back or Forward to an entry with this ?from= (the
    // tabs and the transport are steps in the history), or the same link
    // followed again. Keep it, and say again where it is.
    const loaded = work.loaded;
    if (loaded?.id === from && editing.on && editing.seed === loaded.seed) {
      setReplay({ id: from, kind: 'loaded', record, cut: loaded.cut, edited: editing.text !== editing.seed });
      return;
    }
    const text = source.data.bodies.client_request;
    const pretty = formatJson(text.trim());
    const load = () => {
      patch({
        protocol: record.client_protocol,
        // The name as the client sent it: client_model has lost a reasoning
        // suffix such as (high), and Gemini carries the model nowhere else.
        model: record.requested_model || record.client_model || settings.model,
        stream: !!record.stream,
      });
      enterRaw(pretty ?? text);
      work.loaded = { id: from, seed: pretty ?? text, cut: pretty === null };
      // Not steps of their own: the link that brought ?from= is the step.
      setInspect('request', { replace: true });
      if (mode !== 'http') setMode('http', { replace: true });
      setReplay({ id: from, kind: 'loaded', record, cut: pretty === null, edited: false });
    };
    if (!editing.on || editing.text === editing.seed) {
      load();
      return;
    }
    // A hand-edited body is in the editor: ask before it is replaced.
    setReplay({ id: from, kind: 'asking', record });
    confirm({
      danger: true,
      title: 'Replace the edited body?',
      message: 'Your changes to the raw JSON are replaced by the body of the recorded request.',
      confirmLabel: 'Replace body',
    }).then((ok) => {
      if (work.appliedFrom !== from) return;
      if (ok) load();
      else setFrom('');
    });
  }, [from, source.data]);

  const replayNotice = (() => {
    if (!from) return null;
    // Both buttons leave with their notice: the keyboard goes to the page, not to <body>.
    const leaving = (action) => () => {
      action();
      rescueFocus(pageMain);
    };
    const dismiss = html`<${Button} size="sm" onClick=${leaving(() => setFrom(''))}>Dismiss<//>`;
    const name = html`<span class="mono">${from}</span>`;
    if (source.error && !source.data) {
      return html`<${Notice} tone="stop" title="Could not load the request to replay" action=${html`<span class="row"><${Button} size="sm" icon="refresh" onClick=${leaving(source.refresh)}>Try again<//>${dismiss}</span>`}>
        ${source.error.message}
      <//>`;
    }
    if (source.loading || !replay || replay.id !== from) return html`<${Notice} tone="info" title="Loading the request to replay">Fetching the captured body of ${name}.<//>`;
    if (replay.kind === 'asking') return html`<${Notice} tone="info" title="A recorded request is ready to load">The body of ${name} replaces the raw JSON you edited once you confirm.<//>`;
    if (replay.kind === 'no-body') {
      // Named as the Settings page names them (and as the request drawer does).
      return html`<${Notice} tone="caution" title="This request has no captured body" action=${dismiss}>
        Bodies are captured only while Capture is set to Failed requests or Every request under Request bodies, on the Logging and usage tab of <a href=${href('/settings', { tab: 'logging' })}>Settings</a>. Send the request again once it is, then replay it from the request log.
      <//>`;
    }
    if (replay.kind === 'protocol') {
      return html`<${Notice} tone="caution" title="This request cannot be replayed here" action=${dismiss}>
        It used <span class="mono">${replay.record.client_protocol ?? 'an unknown protocol'}</span>, which is not one of the four protocols the playground sends.
      <//>`;
    }
    if (replay.kind === 'endpoint') {
      return html`<${Notice} tone="caution" title="This request cannot be replayed here" action=${dismiss}>
        It was sent to <span class="mono">${replay.record.endpoint}</span>. The playground sends generation requests only: chat completions, responses, messages and generateContent.
      <//>`;
    }
    return html`<${Notice} tone=${replay.cut ? 'caution' : 'info'} title="Loaded a recorded request" action=${dismiss}>
      The body of ${name}${replay.record.started_at ? `, sent ${formatDateTime(replay.record.started_at)}` : ''}, is in raw mode under Request${replay.edited ? ', with your edits' : ''}. ${replay.cut && !replay.edited
        ? 'The capture was cut off or redacted, so it is not valid JSON yet: complete it before sending.'
        : replay.edited
          ? 'Send it when you are ready.'
          : 'Edit it if you like, then send it.'}
    <//>`;
  })();

  // ---- Palette -------------------------------------------------------------
  useCommands(
    () =>
      PROTOCOLS.filter((p) => p.id !== protocol).map((p) => ({
        id: `playground:protocol:${p.id}`,
        label: `Playground: use ${p.label}`,
        group: 'Playground',
        icon: 'playground',
        run: () => patch({ protocol: p.id }),
      })),
    [protocol],
  );

  // ---- Render --------------------------------------------------------------
  const paramsSet = [settings.system.trim() !== '', settings.temperature != null, settings.maxTokens != null, settings.effort !== 'default', settings.tools].filter(Boolean).length;
  const last = turns[turns.length - 1];
  const awaitingTools = !!last && last.role === 'assistant' && last.status === 'done' && !last.detached && !last.error && last.blocks.some((b) => b.type === 'tool_call' && b.result == null);
  const formLocked = raw.on;
  const urlFieldsLocked = raw.on && protocol !== 'gemini';
  const modelFieldError = modelError ?? issues.at('model');
  const rawFieldError = rawError ?? issues.at('body') ?? (raw.on && rawSettled ? rawCheck?.error : undefined);
  // Anthropic refuses a thinking budget that max_tokens leaves no room for.
  const budget = protocol === 'anthropic' ? EFFORT_BUDGET[settings.effort] : undefined;
  const budgetConflict = !raw.on && budget != null && settings.maxTokens != null && settings.maxTokens <= budget;

  const copySnippet = async (snippet) => {
    if (await copyText(snippet.code)) toast.success(`${snippet.label} setup copied`);
    else toast.error('Could not copy the code', { description: 'The browser refused clipboard access. Copy it from the Code tab of the inspector.' });
  };

  const actions =
    mode === 'ws'
      ? null
      : html`
          <${CopyButton} variant="secondary" size="md" value=${curl} label="Copy curl for bash/zsh">Copy curl (bash/zsh)<//>
          <${Menu}
            label="Copy SDK setup"
            trigger=${(props) => html`<${Button} iconRight="chevron-down" ...${props}>Copy code<//>`}
            items=${[{ heading: 'Base URL setup' }, ...snippets.map((s) => ({ label: s.label, icon: 'copy', onSelect: () => copySnippet(s) }))]}
          />
        `;

  const composer = raw.on
    ? html`
        <div class="play-composer" ref=${composerBox}>
          <p class="muted">Raw mode: the body under Request in the inspector is sent as it is written there. The conversation above is not added to it.</p>
          <div class="play-composer-actions">
            <${Button} variant="ghost" disabled=${busy} onClick=${leaveRaw}>Leave raw mode<//>
            ${busy
              ? html`<${Button} icon="stop" onClick=${stop}>Stop<//>`
              : html`<${Button} variant="primary" icon="send" onClick=${() => sendRaw()}>Send raw request<//>`}
          </div>
        </div>
      `
    : html`
        <div class="play-composer" ref=${composerBox}>
          <${Textarea}
            label="Message"
            value=${draft}
            onChange=${setDraft}
            rows=${2}
            autoGrow
            maxRows=${10}
            placeholder=${awaitingTools ? 'Or write a message instead of sending the tool result' : 'Write a message'}
            onKeyDown=${(event) => {
              if (event.key === 'Enter' && !event.shiftKey && !event.isComposing && !coarse) {
                event.preventDefault();
                sendText(draft);
              }
            }}
          />
          <div class="play-composer-actions">
            <span class="faint hide-phone">Enter sends. Shift and Enter starts a new line.</span>
            ${busy
              ? html`<${Button} icon="stop" onClick=${stop}>Stop<//>`
              : html`<${Button} variant=${awaitingTools ? 'secondary' : 'primary'} icon="send" disabled=${!draft.trim()} onClick=${() => sendText(draft)}>Send<//>`}
          </div>
        </div>
      `;

  return html`
    <${Page} title="Playground" description="Send a request through the gateway in any protocol and read exactly what goes over the wire." actions=${actions} class="play">
      <${Tabs}
        label="Transport"
        value=${mode === 'ws' ? 'ws' : 'http'}
        onChange=${setMode}
        tabs=${[
          { id: 'http', label: 'HTTP and SSE' },
          { id: 'ws', label: 'WebSocket' },
        ]}
      />

      <div class="play-view" hidden=${mode === 'ws'}>
        ${replayNotice}
        <div class="play-grid" data-layout="http">
          <${Panel} title="Request" class="play-settings">
            <div class="play-form">
              <${Select}
                label="Protocol"
                value=${protocol}
                onChange=${set('protocol')}
                options=${PROTOCOLS.map((p) => ({ value: p.id, label: p.label }))}
                error=${issues.at('protocol')}
                hint=${raw.on ? 'How the raw body is read and how the answer is written.' : undefined}
              />
              <${Combobox}
                label="Model"
                value=${urlFieldsLocked ? effectiveModel : settings.model}
                onChange=${(value) => {
                  patch({ model: value });
                  setModelError(null);
                }}
                inputRef=${modelInput}
                options=${modelOptions}
                loading=${models.loading}
                loadError=${models.error}
                onRetry=${models.refresh}
                noun="models"
                emptyText="No models yet. Add a provider on the Providers page, or type a name."
                placeholder="Model name"
                disabled=${urlFieldsLocked}
                error=${modelFieldError}
                hint=${urlFieldsLocked ? 'Raw mode: the model is the one named in the body.' : modelHint}
              />
              <${Switch}
                label="Stream the answer"
                hint=${urlFieldsLocked ? 'Raw mode: the body decides with its stream field.' : 'Server-sent events, as the model writes.'}
                checked=${urlFieldsLocked ? effectiveStream : settings.stream}
                disabled=${urlFieldsLocked}
                onChange=${set('stream')}
                error=${issues.at('stream')}
              />

              <button type="button" class="play-more" aria-expanded=${showParams ? 'true' : 'false'} aria-controls="play-params" onClick=${() => setShowParams(!showParams)}>
                <${Icon} name="chevron-right" size=${14} />
                <span>Parameters</span>
                <span class="faint">${paramsSet === 0 ? 'none set' : `${paramsSet} set`}</span>
              </button>
              <div id="play-params" class="play-params" data-open=${showParams ? '' : undefined}>
                ${formLocked && html`<${Notice} tone="info">Raw mode is on. These settings do not change the body any more.<//>`}
                <${Textarea}
                  label="System prompt"
                  optional
                  rows=${3}
                  autoGrow
                  maxRows=${10}
                  value=${settings.system}
                  onChange=${set('system')}
                  disabled=${formLocked}
                  placeholder="Instructions the model follows for the whole conversation"
                />
                <div class="play-pair">
                  <${NumberInput}
                    label="Temperature"
                    optional
                    value=${settings.temperature}
                    onChange=${set('temperature')}
                    min=${0}
                    max=${2}
                    step=${0.1}
                    placeholder="Default"
                    disabled=${formLocked}
                  />
                  <${NumberInput}
                    label="Max output tokens"
                    optional=${protocol !== 'anthropic'}
                    value=${settings.maxTokens}
                    onChange=${set('maxTokens')}
                    min=${1}
                    step=${1}
                    placeholder=${protocol === 'anthropic' ? String(anthropicMaxTokens(settings)) : 'Default'}
                    disabled=${formLocked}
                    warning=${budgetConflict
                      ? `Not above the thinking budget, ${formatNumber(budget)} tokens at this effort: an Anthropic upstream answers 400. Raise it, clear the field, or lower the effort.`
                      : undefined}
                  />
                </div>
                ${!budgetConflict &&
                html`<p class="field-hint">
                  ${protocol === 'anthropic' && settings.maxTokens == null
                    ? `Anthropic requires max_tokens: ${formatNumber(anthropicMaxTokens(settings))} is sent while the field is empty.`
                    : `Max output tokens is sent as ${MAX_TOKENS_FIELD[protocol]}.`}
                </p>`}
                <${Select}
                  label="Reasoning effort"
                  value=${settings.effort}
                  onChange=${set('effort')}
                  disabled=${formLocked}
                  hint=${effortHint(protocol, settings.effort)}
                  options=${[
                    { value: 'default', label: 'Model default' },
                    { value: 'none', label: 'None' },
                    { value: 'low', label: 'Low' },
                    { value: 'medium', label: 'Medium' },
                    { value: 'high', label: 'High' },
                  ]}
                />
                <${Switch}
                  label="Offer a sample tool"
                  hint="Adds get_weather as a function tool, in the protocol's own shape."
                  checked=${settings.tools}
                  disabled=${formLocked}
                  onChange=${set('tools')}
                />
              </div>
            </div>
          <//>

          <${Panel}
            title="Conversation"
            flush
            class="play-chat"
            actions=${html`
              <${IconButton}
                icon="refresh"
                label=${raw.on ? 'Send the raw request again' : 'Regenerate the last answer'}
                disabled=${lastAnswer === -1}
                aria-disabled=${busy && lastAnswer !== -1 ? 'true' : undefined}
                onClick=${regenerate}
              />
              <${IconButton} icon="trash" label="Clear the conversation" disabled=${turns.length === 0} onClick=${clear} />
            `}
          >
            <${Conversation}
              turns=${turns}
              busy=${busy}
              protocol=${protocol}
              onToolResults=${sendToolResults}
              onRetry=${regenerate}
              retryLabel=${raw.on ? 'Send raw request' : 'Send again'}
              toolNote=${raw.on ? 'Raw mode is on, so the result form is not offered: a raw request carries only what is in the editor. Write the result into the raw body, or leave raw mode to send it from here.' : null}
              empty=${html`<${EmptyState}
                icon="playground"
                title="Nothing sent yet"
                description="Choose a model, write a message and send it. The answer appears here; the inspector shows the exact body and every event."
                action=${raw.on
                  ? null
                  : html`<${Button}
                      disabled=${busy}
                      onClick=${() => {
                        sendText('Hello');
                        rescueFocus(composerTargets);
                      }}
                    >
                      Send "Hello"
                    <//>`}
              />`}
            />
            ${composer}
          <//>

          <${Inspector}
            tab=${INSPECTOR_TABS.includes(inspect) ? inspect : 'request'}
            onTab=${setInspect}
            run=${run}
            view=${view}
            onView=${setView}
            next=${{ text: nextText, path: nextPath }}
            raw=${{ on: raw.on, text: raw.text, error: rawFieldError }}
            onRawToggle=${toggleRaw}
            onRawChange=${(text) => {
              setRaw((r) => ({ ...r, text }));
              setRawError(null);
              if (failure) setFailure(null);
            }}
            onRawReset=${rebuildRaw}
            loading=${!!from && source.loading}
            protocol=${protocol}
            curl=${curl}
            snippets=${snippets}
          />
        </div>
      </div>

      <div class="play-view" hidden=${mode !== 'ws'}>
        <${SocketPanel}
          origin=${origin}
          model=${settings.model}
          onModel=${set('model')}
          modelOptions=${modelOptions}
          modelsLoading=${models.loading}
          modelsError=${models.error}
          onModelsRetry=${models.refresh}
        />
      </div>
    <//>
  `;
}
