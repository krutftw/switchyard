// The WebSocket view: the public Responses endpoint, GET /v1/responses, driven
// by hand. Connect with a client key, send response.create turns, read every
// frame in both directions, and continue a conversation on the same socket
// with previous_response_id.
//
// This is the client API, not the admin API: it needs a client key, and a
// browser can only present one in the socket's URL (?key=). The key lives in
// this component's state and nowhere else: not in the page address, not in
// storage. Leaving the page closes the socket and forgets the key; the turns
// and frames (which never contain it) are kept in session.js, so they are
// still there when the reader comes back.
//
// A browser prints the full address of a socket that fails to open in its
// console, key included, and nothing the page does can stop it. So the page
// gives it no failed socket to print: it asks the gateway over HTTP whether
// the key is accepted (the key in a header, which the console never shows)
// before it opens the socket, and a socket still connecting when the reader
// cancels or leaves is closed once it is open instead of being abandoned. A
// socket that is refused all the same (a proxy that blocks the upgrade) is
// printed there; the notice above the key field says so.

import { html, useEffect, useLayoutEffect, useRef, useState } from '../../../vendor/preact-htm.js';
import { Button, CodeBlock, EmptyState, IconButton, Notice, Panel, SecretInput, StatusLamp, Switch, Textarea, confirm } from '../../components/index.js';
import { nextId } from '../../lib/dom.js';
import { formatTime } from '../../lib/format.js';
import { useMediaQuery, useResource } from '../../lib/hooks.js';
import { useLive, useLiveGap } from '../../lib/live.js';
import { href } from '../../lib/router.js';
import Combobox from './combobox.js';
import Conversation from './conversation.js';
import { focusAdrift, focusFirst, rescueFocus } from './focus.js';
import FrameList from './framelist.js';
import { closeCodeMeaning, createReader, digestEvent, readError } from './protocols.js';
import { socketMemory } from './session.js';

const FRAME_CAP = 500;

/** A frame as one line of text, for "copy all". */
const frameText = (frame) => `${frame.dir === 'out' ? '>' : frame.dir === 'in' ? '<' : '#'} ${frame.data || frame.name}`;

/**
 * Whether the gateway takes this key, asked over HTTP: before a socket is
 * opened (a refused socket puts its address, key included, in the console),
 * and again for a socket that never opened, which browsers report as close
 * code 1006 with no reason. 'key' (refused), 'upgrade' (accepted: a socket
 * that fails now is blocked on its way), 'unreachable', 'http-<status>'.
 */
async function diagnose(origin, key, signal) {
  try {
    const res = await fetch(`${origin}/v1/models`, {
      headers: key ? { authorization: `Bearer ${key}` } : {},
      cache: 'no-store',
      signal,
    });
    if (res.status === 401 || res.status === 403) return 'key';
    if (res.ok) return 'upgrade';
    return `http-${res.status}`;
  } catch {
    return 'unreachable';
  }
}

/**
 * Let go of a socket that is still connecting without the browser calling it
 * a failure (closing it now prints "closed before the connection is
 * established" with its address): close it the moment it opens.
 */
function abandon(ws, reason) {
  ws.onmessage = null;
  ws.onclose = null;
  ws.onerror = null;
  ws.onopen = () => ws.close(1000, reason);
}

function refusalText(reason, hasKey) {
  if (reason === 'key') {
    return hasKey
      ? 'The gateway does not accept this client key (HTTP 401). Copy a key from the API keys page and check that it is enabled.'
      : 'The gateway requires a client key (HTTP 401). Paste one and connect again.';
  }
  if (reason === 'upgrade') {
    return 'The key is accepted over HTTP, so the WebSocket upgrade itself is being blocked. A reverse proxy in front of the gateway must forward the Upgrade and Connection headers.';
  }
  if (reason === 'unreachable') return 'Switchyard did not answer this browser. Check that it is running and reachable from this device, then connect again.';
  if (reason?.startsWith('http-')) return `The gateway answers HTTP ${reason.slice(5)} on the client API. Check the gateway log for the reason.`;
  return 'Browsers do not say why a WebSocket was refused. Checking the key over HTTP.';
}

/**
 * origin        the gateway's own origin (http://host:port, with any path prefix)
 * model         the model name shared with the HTTP view
 * onModel       (name) => void
 * modelOptions, modelsLoading, modelsError, onModelsRetry   for the model field
 */
export default function SocketPanel({ origin, model, onModel, modelOptions, modelsLoading, modelsError, onModelsRetry }) {
  const status = useResource('/status');
  // auth.required can change while the page is open (Settings in another tab, an edited config file).
  useLive('config.reloaded', status.refresh);
  useLiveGap(status.refresh);
  const authRequired = status.data?.auth_required;
  const coarse = useMediaQuery('(pointer: coarse)');

  const [key, setKey] = useState('');
  const [keyError, setKeyError] = useState(null);
  const [modelError, setModelError] = useState(null);
  // Coming back to the page: the socket of the last visit is closed, its turns and frames are still here.
  const [state, setState] = useState(socketMemory.closed ? 'closed' : 'idle'); // idle | connecting | open | closed
  const [closed, setClosedState] = useState(socketMemory.closed); // { code, reason, opened, refusal, left }
  const [turns, setTurnsState] = useState(socketMemory.turns);
  const [text, setText] = useState('');
  const [chain, setChain] = useState(true);
  const [lastResponseId, setLastResponseId] = useState(null);
  const [, setVersion] = useState(0);

  const socket = useRef(null);
  const frames = useRef(socketMemory.frames);
  const reader = useRef(null);
  const active = useRef(null); // id of the assistant turn being received
  const raf = useRef(0);
  const probe = useRef(null); // the HTTP key check in flight
  const pending = useRef(null); // { secret, fromField } from connect() until the socket opens or is refused
  const mounted = useRef(true);
  const keyId = useRef(null);
  if (!keyId.current) keyId.current = nextId('ws-key');

  const wsUrl = `${origin.replace(/^http/, 'ws')}/v1/responses`;
  const busy = active.current != null;

  // Where the keyboard goes when the control that had it is disabled by
  // what it started (see focus.js): the key field while a socket is open,
  // "Send turn" while a turn runs, the message field once the socket closed.
  const root = useRef(null);
  // On a touch screen a focused text field brings the keyboard up over the
  // answer: there the transcript takes the focus instead.
  const messageField = () => root.current?.querySelector(coarse ? '.play-transcript[tabindex]' : '.play-composer textarea');
  const connectButton = () => root.current?.querySelector('[data-play-connect]');
  const keyField = () => document.getElementById(keyId.current);
  useLayoutEffect(() => {
    const at = document.activeElement;
    if (at && at.disabled && root.current?.contains(at)) focusFirst(messageField(), connectButton());
  }, [state, busy, lastResponseId]);

  // Both are written through to session.js first, so what happens while the
  // page is being left (the socket closing) is kept as well.
  const setTurns = (next) => {
    socketMemory.turns = typeof next === 'function' ? next(socketMemory.turns) : next;
    if (mounted.current) setTurnsState(socketMemory.turns);
  };
  const setClosed = (next) => {
    socketMemory.closed = typeof next === 'function' ? next(socketMemory.closed) : next;
    if (mounted.current) setClosedState(socketMemory.closed);
  };

  const repaint = () => {
    if (raf.current || !mounted.current) return;
    raf.current = requestAnimationFrame(() => {
      raf.current = 0;
      const id = active.current;
      const r = reader.current;
      if (id && r) setTurns((list) => list.map((t) => (t.id === id ? { ...t, blocks: r.snapshot(), status: t.status === 'waiting' ? 'streaming' : t.status } : t)));
      setVersion((v) => v + 1);
    });
  };

  const record = (frame) => {
    const store = frames.current;
    store.seq += 1;
    store.total += 1;
    store.list.push({ seq: store.seq, at: performance.now() - store.t0, ...frame });
    if (store.list.length > FRAME_CAP) store.list = store.list.slice(-FRAME_CAP);
    repaint();
  };

  const finishTurn = (patch) => {
    const id = active.current;
    const r = reader.current;
    active.current = null;
    if (!id) return;
    setTurns((list) =>
      list.map((t) =>
        t.id === id ? { ...t, blocks: r ? r.snapshot() : t.blocks, status: 'done', finish: r?.state.finish ?? null, usage: r?.state.usage ?? null, model: r?.state.model ?? t.model, ...patch } : t,
      ),
    );
  };

  /** The attempt in progress was called off by the reader, before any socket opened. */
  const cancelled = (secret) => {
    setState('closed');
    record({ name: 'cancelled', data: 'Stopped by the playground before the socket opened.', tone: 'info' });
    setClosed({ code: 0, reason: '', opened: false, refusal: null, cancelled: true, hadKey: !!secret });
  };

  const disconnect = () => {
    const ws = socket.current;
    if (ws?.readyState === 1) {
      ws.close(1000, 'closed by the playground');
      return;
    }
    // Still checking the key over HTTP, or the socket is still connecting.
    const attempt = pending.current;
    if (!attempt) return;
    pending.current = null;
    probe.current?.abort();
    probe.current = null;
    socket.current = null;
    if (ws?.readyState === 0) abandon(ws, 'cancelled by the playground');
    cancelled(attempt.secret);
  };

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      cancelAnimationFrame(raf.current);
      probe.current?.abort();
      const ws = socket.current;
      socket.current = null;
      if (!ws || ws.readyState > 1) return;
      if (ws.readyState === 0) {
        abandon(ws, 'left the playground');
        return;
      }
      ws.close(1000, 'left the playground');
      // The close event arrives after the page is gone: write the end of this socket down now.
      record({ name: 'close 1000', data: 'Closed by the playground when the page was left.', tone: 'info' });
      if (active.current) {
        finishTurn({ status: 'error', error: { kind: 'stream', via: 'socket', status: 0, message: 'The socket was closed before the response finished, because the page was left.', issues: [] } });
      }
      setClosed({ code: 1000, reason: '', opened: true, refusal: null, left: true });
    };
  }, []);

  /**
   * fromField  the attempt was started with Enter in the key field: a refusal
   *            hands the focus back there, to correct the key.
   */
  const connect = ({ fromField = false } = {}) => {
    const secret = key.trim();
    if (/\s/.test(secret) || /[^!-~]/.test(secret)) {
      setKeyError('A client key has no spaces or special characters. Paste it again as one line.');
      return;
    }
    if (!secret && authRequired !== false) {
      setKeyError('Paste a client key. They are listed on the API keys page.');
      return;
    }
    setKeyError(null);
    setClosed(null);
    probe.current?.abort();
    // The turns and frames of an earlier socket stay on the page; a new
    // socket knows none of its responses.
    frames.current.t0 = performance.now();
    frames.current.openedAt = Date.now();
    setLastResponseId(null);
    active.current = null;
    const attempt = { secret, fromField };
    pending.current = attempt;
    setState('connecting');

    // The key is asked about over HTTP first: a socket the gateway refuses
    // would put its address, key included, in the browser's console.
    const controller = new AbortController();
    probe.current = controller;
    diagnose(origin, secret, controller.signal).then((answer) => {
      if (controller.signal.aborted || pending.current !== attempt || !mounted.current) return;
      probe.current = null;
      if (answer === 'key' || answer === 'unreachable') refuse(attempt, answer);
      else open(attempt);
    });
  };

  /** No socket is opened: the HTTP check says it would be refused. */
  const refuse = (attempt, refusal, why) => {
    pending.current = null;
    setState('closed');
    record({
      name: 'not opened',
      data: why ?? (refusal === 'key' ? 'The key was refused over HTTP. No socket was opened.' : 'No answer over HTTP. No socket was opened.'),
      tone: 'stop',
    });
    setClosed({ code: 0, reason: '', opened: false, refusal, hadKey: !!attempt.secret });
    if (attempt.fromField) {
      setTimeout(() => {
        if (focusAdrift() || document.activeElement === connectButton()) focusFirst(keyField());
      }, 0);
    }
  };

  const open = (attempt) => {
    const { secret } = attempt;
    let ws;
    try {
      ws = new WebSocket(secret ? `${wsUrl}?key=${encodeURIComponent(secret)}` : wsUrl);
    } catch {
      refuse(attempt, 'unreachable', 'The browser would not open a socket to this address.');
      return;
    }
    socket.current = ws;
    let opened = false;

    ws.onopen = () => {
      if (socket.current !== ws) return;
      opened = true;
      pending.current = null;
      setState('open');
      // Connected from the key field or the Connect button: the message is what comes next.
      setTimeout(() => {
        if (focusAdrift() || document.activeElement === connectButton()) focusFirst(messageField());
      }, 0);
      setTurns((list) =>
        list.some((t) => t.role !== 'note') && list[list.length - 1]?.role !== 'note'
          ? [...list, { id: nextId('ws-turn'), role: 'note', text: 'A new socket was opened here. It does not remember the turns above: the next turn starts a new conversation.' }]
          : list,
      );
      record({ name: 'open', data: `${wsUrl}${secret ? '?key=(hidden)' : ''}`, tone: 'info' });
    };

    ws.onmessage = (event) => {
      if (socket.current !== ws) return;
      if (typeof event.data !== 'string') {
        record({ dir: 'in', name: 'binary', data: '(a binary frame; the Responses endpoint only sends text)' });
        return;
      }
      let json;
      try {
        json = JSON.parse(event.data);
      } catch {
        json = undefined;
      }
      const type = typeof json?.type === 'string' ? json.type : 'text';
      record({ dir: 'in', name: type, note: json ? digestEvent('openai-responses', { json, data: event.data }) : '', data: event.data, tone: type === 'error' || type === 'response.failed' ? 'stop' : undefined });
      const r = reader.current;
      if (!json || !r || !active.current) return;
      // An error frame in answer to the request itself (nothing of a
      // response was sent yet) is a refusal; one after that is a stream
      // that broke.
      const started = r.state.responseId != null;
      r.event({ json, data: event.data });
      if (type === 'response.completed' || type === 'response.incomplete') {
        if (typeof json.response?.id === 'string') setLastResponseId(json.response.id);
        finishTurn({});
      } else if (type === 'error' || type === 'response.failed') {
        // readError takes the wait from error.headers["retry-after"]: a socket has no response headers.
        const error = { kind: started ? 'stream' : 'frame', via: 'socket', ...readError(json) };
        // The gateway does not have the response this page named: stop naming it.
        if (error.code === 'previous_response_not_found') setLastResponseId(null);
        finishTurn({ status: 'error', error });
      }
    };

    ws.onclose = (event) => {
      if (socket.current !== ws) return;
      socket.current = null;
      pending.current = null;
      setState('closed');
      // Its responses went with it: the next socket could not continue any of them.
      setLastResponseId(null);
      record({ name: `close ${event.code}`, data: event.reason || closeCodeMeaning(event.code), tone: event.code === 1000 ? 'info' : 'stop' });
      if (active.current) {
        finishTurn({ status: 'error', error: { kind: 'stream', via: 'socket', status: 0, message: `The socket closed (${event.code}) before the response finished.`, issues: [] } });
      }
      const info = { code: event.code, reason: event.reason, opened, refusal: null, hadKey: !!secret };
      setClosed(info);
      if (!opened) {
        const controller = new AbortController();
        probe.current = controller;
        diagnose(origin, secret, controller.signal).then((refusal) => {
          if (!controller.signal.aborted) setClosed((c) => (c === info ? { ...info, refusal } : c));
        });
      }
    };
  };

  const sendFrame = (frame, userText) => {
    const ws = socket.current;
    if (!ws || ws.readyState !== 1) return;
    const data = JSON.stringify(frame);
    const id = nextId('ws-turn');
    reader.current = createReader('openai-responses');
    active.current = id;
    // A response.create without previous_response_id is a request of its
    // own: the gateway sends the model this input and nothing that came
    // before. Say so where the turns above would suggest otherwise (after a
    // new socket the note is already there).
    const fresh = !frame.previous_response_id;
    setTurns((list) => [
      ...list,
      ...(fresh && list.some((t) => t.role !== 'note') && list[list.length - 1]?.role !== 'note'
        ? [{ id: nextId('ws-turn'), role: 'note', text: 'A new conversation starts here: the next turn was sent without previous_response_id, so the model does not see the turns above.' }]
        : []),
      ...(userText != null ? [{ id: nextId('ws-turn'), role: 'user', text: userText }] : []),
      { id, role: 'assistant', status: 'waiting', blocks: [], model: frame.model ?? model, protocol: 'openai-responses' },
    ]);
    ws.send(data);
    record({ dir: 'out', name: frame.type, note: fresh ? 'new conversation' : 'continues the latest response', data });
  };

  const input = text.trim();
  const chained = chain && !!lastResponseId;
  const nextFrame = (() => {
    const frame = { type: 'response.create', model: model.trim() };
    if (chained) frame.previous_response_id = lastResponseId;
    frame.input = [{ role: 'user', content: [{ type: 'input_text', text: input || 'Hello' }] }];
    return frame;
  })();

  const send = () => {
    if (!model.trim()) {
      setModelError('Choose a model, or type its name.');
      return;
    }
    if (!input || busy || state !== 'open') return;
    setModelError(null);
    sendFrame(nextFrame, input);
    setText('');
  };

  const sendToolResults = (turnId, results) => {
    setTurns((list) => list.map((t) => (t.id === turnId ? { ...t, blocks: t.blocks.map((b) => (b.type === 'tool_call' && b.id in results ? { ...b, result: results[b.id] } : b)) } : t)));
    // The button that sent them leaves with the result form.
    rescueFocus(messageField);
    const frame = { type: 'response.create', model: model.trim() };
    if (lastResponseId) frame.previous_response_id = lastResponseId;
    frame.input = Object.entries(results).map(([call_id, output]) => ({ type: 'function_call_output', call_id, output }));
    sendFrame(frame, null);
  };

  const store = frames.current;

  const clear = async () => {
    const ok = await confirm({
      danger: true,
      title: 'Clear the turns and frames?',
      message: `The turns and frames listed here are removed from this page. ${state === 'open' ? 'The socket stays open and still remembers its last response. ' : ''}Entries in the request log are kept.`,
      confirmLabel: 'Clear turns and frames',
    });
    if (!ok || active.current) return;
    socketMemory.frames = { list: [], total: 0, seq: 0, t0: store.t0, openedAt: store.openedAt };
    frames.current = socketMemory.frames;
    setTurns([]);
    setVersion((v) => v + 1);
  };

  const lamp =
    state === 'open'
      ? html`<${StatusLamp} tone="clear" pulse label="Connected" />`
      : state === 'connecting'
        ? html`<${StatusLamp} tone="info" pulse label="Connecting" />`
        : state === 'closed' && !closed?.cancelled
          ? html`<${StatusLamp} tone=${closed && closed.opened && closed.code === 1000 ? 'off' : 'stop'} label=${closed?.opened ? `Closed, code ${closed.code}` : 'Refused'} />`
          : html`<${StatusLamp} tone="off" label="Not connected" />`;

  // A connection the reader called off is not a refusal: the frame list says it was cancelled.
  const closedNotice = !closed || closed.cancelled
    ? null
    : closed.left
      ? html`<${Notice} title="The socket was closed when you left this page">
          ${authRequired === false ? 'Connect again to open a new one.' : 'The key was forgotten with it. Paste a key and connect again.'} The turns and frames of the closed socket are still listed.
        <//>`
      : closed.opened
        ? html`<${Notice} tone=${closed.code === 1000 ? 'neutral' : 'stop'} title=${`Closed with code ${closed.code}${closed.reason ? `: ${closed.reason}` : ''}`}>
            ${closeCodeMeaning(closed.code)}
          <//>`
        : html`<${Notice} tone="stop" title=${closed.refusal === 'unreachable' ? 'The gateway could not be reached' : 'The gateway refused the connection'}>
            ${refusalText(closed.refusal, closed.hadKey)}
          <//>`;

  return html`
    <div class="play-grid" data-layout="socket" ref=${root}>
      <div class="play-col">
        <${Panel} title="Connection" description=${html`<span class="mono">${wsUrl}</span>`} actions=${lamp}>
          <div class="stack" style="--gap:var(--space-3)">
            <${Notice} tone="caution" title="The key travels in the socket's address">
              Browsers cannot set headers on a WebSocket, so the key is sent as ?key= in the socket URL. It is kept in this tab's memory only: never in the page address, never in storage. Developer tools, the browser's console when a socket is refused on its way to the gateway, and proxies that log URLs can still see it, so use a key you can revoke.
            <//>
            <${SecretInput}
              id=${keyId.current}
              label="Client key"
              value=${key}
              onChange=${(value) => {
                setKey(value);
                setKeyError(null);
              }}
              error=${keyError}
              optional=${authRequired === false}
              disabled=${state === 'connecting' || state === 'open'}
              placeholder="sy-…"
              hint=${authRequired === false
                ? 'This gateway accepts clients without a key (auth.required is off). Leave the field empty.'
                : html`Paste a key from the <a href=${href('/keys')}>API keys</a> page. It is forgotten when you leave this page.`}
              onKeyDown=${(event) => {
                if (event.key !== 'Enter' || event.isComposing) return;
                // Connecting locks this field, and the focus moves to the
                // button beside it, which reads Disconnect by then: the
                // keypress that follows must not reach it.
                event.preventDefault();
                if (state !== 'open' && state !== 'connecting') connect({ fromField: true });
              }}
            />
            <div class="row row-wrap">
              ${state === 'open' || state === 'connecting'
                ? html`<${Button} data-play-connect="" onClick=${() => disconnect()}>Disconnect<//>`
                : html`<${Button} data-play-connect="" variant="primary" icon="plug" onClick=${() => connect()}>Connect<//>`}
            </div>
            ${closedNotice}
          </div>
        <//>

        <${Panel}
          title="Turns"
          description="Each message is one response.create frame on the open socket."
          flush
          class="play-chat"
          actions=${html`<${IconButton} icon="trash" label="Clear the turns and frames" disabled=${busy || (turns.length === 0 && store.list.length === 0)} onClick=${clear} />`}
        >
          <${Conversation}
            turns=${turns}
            busy=${busy}
            protocol="openai-responses"
            onToolResults=${sendToolResults}
            toolNote=${state === 'open' ? null : 'The socket that made this call is closed, so its result cannot be sent. Connect again and ask anew.'}
            empty=${html`<${EmptyState}
              compact
              icon="plug"
              title=${state === 'open' ? 'Connected. Send the first turn' : 'Not connected'}
              description=${state === 'open'
                ? 'Write a message below. The answer streams in as frames, one Responses event per frame.'
                : 'Connect with a client key, then send a turn. The frames of both directions are listed next to the conversation.'}
            />`}
          />
          <div class="play-composer">
            <${Combobox}
              label="Model"
              value=${model}
              onChange=${(value) => {
                onModel(value);
                setModelError(null);
              }}
              error=${modelError}
              options=${modelOptions}
              loading=${modelsLoading}
              loadError=${modelsError}
              onRetry=${onModelsRetry}
              noun="models"
              placeholder="Model name"
            />
            <${Textarea}
              label="Message"
              value=${text}
              onChange=${setText}
              rows=${2}
              autoGrow
              maxRows=${8}
              disabled=${state !== 'open'}
              placeholder=${state === 'open' ? 'Write a message' : 'Connect first'}
              hint=${coarse ? undefined : 'Enter sends. Shift and Enter starts a new line.'}
              onKeyDown=${(event) => {
                // On a touch keyboard Return is the only way to a new line.
                if (event.key === 'Enter' && !event.shiftKey && !event.isComposing && !coarse) {
                  event.preventDefault();
                  send();
                }
              }}
            />
            <${Switch}
              label="Continue from the previous response"
              checked=${chained}
              disabled=${!lastResponseId}
              onChange=${setChain}
              hint=${!lastResponseId
                ? 'Available once this socket has a response to continue from.'
                : chained
                  ? html`Adds <span class="mono">previous_response_id</span>, naming the latest response: the gateway puts the conversation that led to it in front of the new message.`
                  : html`Sent without <span class="mono">previous_response_id</span>: a request of its own, which starts a new conversation on the same socket. The model sees this message only.`}
            />
            <div class="play-composer-actions">
              <span class="faint">${busy ? 'Receiving the response. One turn runs at a time.' : ''}</span>
              <${Button} variant=${state === 'open' ? 'primary' : 'secondary'} icon="send" disabled=${state !== 'open' || busy || !input} onClick=${send}>Send turn<//>
            </div>
          </div>
        <//>
      </div>

      <div class="play-col">
        <${Panel}
          title="Frames"
          description=${store.openedAt ? `Both directions, in order. Times count from the latest connection, ${formatTime(store.openedAt)}.` : 'Both directions, in order.'}
          flush
          class="play-ws-frames"
        >
          <${FrameList}
            frames=${store.list}
            total=${store.total}
            cap=${FRAME_CAP}
            noun="frame"
            label="WebSocket frames, both directions"
            toText=${frameText}
            empty=${html`<${EmptyState} compact icon="logs" title="No frames yet" description="Frames you send and frames the gateway sends are listed here with their time since the socket opened, followed by its close code." />`}
          />
        <//>
        <${CodeBlock} title=${state === 'open' ? 'Next frame' : 'Next frame, once connected'} value=${nextFrame} maxHeight="260px" />
      </div>
    </div>
  `;
}
