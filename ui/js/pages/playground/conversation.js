// The conversation: user and assistant turns, with reasoning, tool calls and
// errors drawn as what they are.
//
// A turn (see protocols.js for blocks):
//   { id, role: 'user', text }
//   { id, role: 'raw', text }                    a hand-written body was sent
//   { id, role: 'note', text }                   a remark between turns (a new socket was opened)
//   { id, role: 'assistant', status, blocks, model, protocol, finish, usage,
//     error, requestId, durationMs, detached }
//   status: 'waiting' | 'streaming' | 'done' | 'stopped' | 'error'

import { html, useLayoutEffect, useRef, useState } from '../../../vendor/preact-htm.js';
import { Badge, Button, CodeBlock, Icon, Notice, Skeleton, StatusLamp, Textarea } from '../../components/index.js';
import { cx } from '../../lib/dom.js';
import { formatDuration, formatNumber, formatTokens, plural } from '../../lib/format.js';
import { href } from '../../lib/router.js';
import { SAMPLE_TOOL, SAMPLE_TOOL_RESULT, adviceFor, prettyArgs, protocolInfo, statusName } from './protocols.js';

// ---------------------------------------------------------------------------
// Blocks
// ---------------------------------------------------------------------------

/** Reasoning, folded away once the answer starts unless the reader opened it. */
function Thinking({ block, active }) {
  // null: follow the stream (open while it is being written). A click pins it.
  const [pinned, setPinned] = useState(null);
  const open = pinned ?? active;
  const length = block.text.length;
  return html`
    <div class="play-think" data-open=${open ? '' : undefined}>
      <button type="button" class="play-think-head" aria-expanded=${open ? 'true' : 'false'} onClick=${() => setPinned(!open)}>
        <${Icon} name="chevron-right" size=${14} class="play-think-chevron" />
        <span class="play-think-title">Thinking</span>
        ${active
          ? html`<${StatusLamp} tone="info" pulse label="in progress" />`
          : html`<span class="faint">${block.redacted ? 'redacted by the provider' : `${formatNumber(length)} characters`}</span>`}
        ${block.signature && !active && html`<span class="faint">signed</span>`}
      </button>
      ${open &&
      html`<div class="play-think-body">
        ${block.redacted && !block.text ? 'The provider returned this reasoning encrypted. It is sent back as it is on the next turn.' : block.text}
      </div>`}
    </div>
  `;
}

function ToolCall({ block, writing, form }) {
  const args = writing ? block.args : prettyArgs(block.args);
  return html`
    <div class="play-tool">
      <${CodeBlock}
        title=${`Tool call: ${block.name || 'unnamed'}`}
        value=${args || (writing ? '' : '{}')}
        language=${writing ? 'text' : 'json'}
        maxHeight="220px"
        note=${block.syntheticId ? null : html`Call id <span class="mono">${block.id}</span>`}
        actions=${writing ? html`<${StatusLamp} tone="info" pulse label="arguments arriving" />` : null}
      />
      ${form &&
      html`<${Textarea}
        label=${`Result of ${block.name || 'the call'}`}
        hint=${form.hint}
        mono
        rows=${2}
        autoGrow
        maxRows=${10}
        value=${form.value}
        onChange=${form.onChange}
        disabled=${form.disabled}
        placeholder="What the tool returned: JSON or plain text"
      />`}
      ${block.result != null &&
      html`<div class="play-tool-result">
        <span class="plate-label">Result sent</span>
        <pre class="mono">${block.result === '' ? '(empty)' : block.result}</pre>
      </div>`}
    </div>
  `;
}

/** The blocks of one assistant turn. Exported for the WebSocket panel. */
export function Blocks({ blocks, streaming = false, forms = null }) {
  const visible = blocks.filter((b) => b.type === 'tool_call' || b.redacted || b.text !== '');
  return visible.map((block, index) => {
    const isLast = index === visible.length - 1;
    const writing = streaming && isLast;
    if (block.type === 'thinking') return html`<${Thinking} key=${index} block=${block} active=${writing} />`;
    if (block.type === 'tool_call') return html`<${ToolCall} key=${index} block=${block} writing=${writing} form=${forms?.(block) ?? null} />`;
    return html`<div key=${index} class="play-text">${block.text}${writing && html`<span class="play-caret" aria-hidden="true"></span>`}</div>`;
  });
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/** Retry-After as words: whole seconds while it is short. */
const waitText = (seconds) => (seconds < 120 ? plural(Math.ceil(seconds), 'second') : formatDuration(seconds * 1000));

function errorTitle(error) {
  if (error.kind === 'network') return 'No answer from the gateway';
  if (error.kind === 'stream') return error.status ? `The stream failed with status ${error.status}` : 'The stream failed after it started';
  const name = statusName(error.status);
  return `HTTP ${error.status}${name ? ` ${name}` : ''}`;
}

/**
 * An error as part of the conversation: the status, the server's own
 * message, and what to do next. Exported for the WebSocket panel.
 *
 * error  { kind: 'http' | 'stream' | 'network', status, message, type, code, issues, retryAfter }
 */
export function ErrorNote({ error, requestId, onRetry, retryLabel = 'Send again', busy = false }) {
  const issues = error.issues ?? [];
  return html`
    <${Notice} tone="stop" title=${errorTitle(error)} class="play-error">
      <span class="play-error-message">${error.message || 'The gateway did not give a reason.'}</span>
      ${(error.type || error.code) &&
      html`<span class="row row-wrap" style="--gap:var(--space-1)">
        ${error.type && html`<${Badge} mono outline>${error.type}<//>`}
        ${error.code && error.code !== error.type && html`<${Badge} mono outline>${error.code}<//>`}
      </span>`}
      ${issues.length > 0 &&
      html`<ul class="issue-list">
        ${issues.map((issue, i) => html`<li key=${i}>${issue.path && html`<span class="issue-path">${issue.path}</span>`}<span>${issue.message}</span></li>`)}
      </ul>`}
      <span>${error.retryAfter != null ? `The gateway asks to wait ${waitText(error.retryAfter)}. ` : ''}${adviceFor(error)}</span>
      ${(onRetry || requestId) &&
      html`<span class="row row-wrap play-error-actions">
        ${onRetry && html`<${Button} size="sm" icon="refresh" disabled=${busy} onClick=${() => onRetry()}>${retryLabel}<//>`}
        ${requestId && html`<a href=${href('/requests', { id: requestId })}>Open request</a>`}
      </span>`}
    <//>
  `;
}

// ---------------------------------------------------------------------------
// Turns
// ---------------------------------------------------------------------------

function usageParts(usage) {
  if (!usage) return [];
  const parts = [];
  if (usage.input != null) parts.push(`${formatTokens(usage.input)} in`);
  if (usage.output != null) parts.push(`${formatTokens(usage.output)} out`);
  if (usage.reasoning) parts.push(`${formatTokens(usage.reasoning)} reasoning`);
  if (usage.cached) parts.push(`${formatTokens(usage.cached)} cached`);
  return parts;
}

function TurnFooter({ turn }) {
  const usage = usageParts(turn.usage);
  const items = [];
  if (turn.status === 'stopped') items.push(html`<span>Stopped before the answer finished</span>`);
  if (turn.finish) items.push(html`<${Badge} mono outline title="Finish reason">${turn.finish}<//>`);
  if (usage.length > 0) items.push(html`<span class="num" title="Tokens, as the response reports them">${usage.join(', ')}</span>`);
  if (turn.durationMs != null) items.push(html`<span class="num">${formatDuration(turn.durationMs)}</span>`);
  // A failed turn already links to its request from the error note.
  if (turn.requestId && !turn.error) items.push(html`<a href=${href('/requests', { id: turn.requestId })}>Open request</a>`);
  if (items.length === 0) return null;
  return html`<div class="play-turn-foot">${items}</div>`;
}

function AssistantTurn({ turn, forms, onRetry, retryLabel, canRetry, busy }) {
  const live = turn.status === 'waiting' || turn.status === 'streaming';
  const empty = !turn.blocks.some((b) => b.type === 'tool_call' || b.redacted || b.text !== '');
  return html`
    <li class="play-turn" data-role="assistant">
      <div class="play-turn-head">
        <span class="plate-label">Assistant</span>
        ${turn.model && html`<span class="mono play-turn-model">${turn.model}</span>`}
        ${turn.status === 'waiting' && html`<${StatusLamp} tone="info" pulse label="Waiting for the gateway" />`}
        ${turn.status === 'streaming' && html`<${StatusLamp} tone="info" pulse label="Streaming" />`}
      </div>
      ${live && empty && html`<${Skeleton} lines=${2} class="play-turn-wait" />`}
      <${Blocks} blocks=${turn.blocks} streaming=${turn.status === 'streaming'} forms=${forms} />
      ${turn.status === 'done' && empty && !turn.error && html`<p class="faint">The answer has no text, reasoning or tool calls. The raw response is in the inspector.</p>`}
      ${turn.error && html`<${ErrorNote} error=${turn.error} requestId=${turn.requestId} onRetry=${canRetry ? onRetry : null} retryLabel=${retryLabel} busy=${busy} />`}
      ${!live && html`<${TurnFooter} turn=${turn} />`}
    </li>
  `;
}

/**
 * turns        the conversation
 * busy         a request is in flight
 * protocol     the protocol the next request will use (words the tool-result hint)
 * onToolResults(turnId, { [callId]: text })  send the results and continue
 * onRetry      resend the last request
 * retryLabel   what the resend button of an error says
 * toolNote     why tool results cannot be sent from here right now (the form is then not offered)
 * empty        what to show before anything was sent
 */
export default function Conversation({ turns, busy, protocol, onToolResults, onRetry, retryLabel, toolNote = null, empty, class: className }) {
  const scroller = useRef(null);
  const stick = useRef(true);
  // Tool results being written, by `${turn.id}:${call id}`.
  const [drafts, setDrafts] = useState({});

  const last = turns[turns.length - 1];
  const lastId = last?.id;
  const lastSize = last?.role === 'assistant' ? last.blocks.reduce((n, b) => n + (b.text?.length ?? 0) + (b.args?.length ?? 0), 0) : 0;

  // Follow the answer while the reader is at the end of the transcript.
  useLayoutEffect(() => {
    const el = scroller.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  }, [turns.length, lastId, lastSize, last?.status]);

  // A turn that was just added is always brought into view.
  useLayoutEffect(() => {
    stick.current = true;
    const el = scroller.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [turns.length]);

  if (turns.length === 0) return html`<div class=${cx('play-transcript', className)} data-empty="">${empty}</div>`;

  // Tool calls of the last answer that still wait for their result.
  const waiting =
    last && last.role === 'assistant' && last.status === 'done' && !last.detached && !last.error
      ? last.blocks.filter((b) => b.type === 'tool_call' && b.result == null)
      : [];
  const pending = toolNote ? [] : waiting;
  const draftKey = (block) => `${last.id}:${block.id}`;
  const draftOf = (block) => drafts[draftKey(block)] ?? (block.name === SAMPLE_TOOL.name ? SAMPLE_TOOL_RESULT : '');
  const sendResults = () => {
    const results = {};
    for (const block of pending) results[block.id] = draftOf(block);
    onToolResults(last.id, results);
  };
  const forms = (block) =>
    pending.includes(block)
      ? {
          value: draftOf(block),
          onChange: (text) => setDrafts((d) => ({ ...d, [draftKey(block)]: text })),
          disabled: busy,
          hint: `Sent back as ${protocolInfo(protocol).toolResult}.`,
        }
      : null;

  return html`
    <div
      ref=${scroller}
      class=${cx('play-transcript', className)}
      role="log"
      aria-label="Conversation"
      aria-busy=${busy ? 'true' : 'false'}
      tabindex="0"
      onScroll=${(event) => {
        const el = event.currentTarget;
        stick.current = el.scrollTop + el.clientHeight >= el.scrollHeight - 24;
      }}
    >
      <ol class="play-turns">
        ${turns.map((turn, index) => {
          if (turn.role === 'assistant') {
            return html`<${AssistantTurn}
              key=${turn.id}
              turn=${turn}
              forms=${turn === last ? forms : null}
              onRetry=${onRetry}
              retryLabel=${retryLabel}
              canRetry=${index === turns.length - 1}
              busy=${busy}
            />`;
          }
          if (turn.role === 'note') return html`<li class="play-turn" data-role="note" key=${turn.id}>${turn.text}</li>`;
          return html`
            <li class="play-turn" data-role=${turn.role} key=${turn.id}>
              <div class="play-turn-head">
                <span class="plate-label">${turn.role === 'raw' ? 'Raw request' : 'You'}</span>
              </div>
              ${turn.role === 'raw'
                ? html`<div class="play-text">${turn.text || html`<span class="faint">The body was written by hand. It is under Request in the inspector.</span>`}</div>`
                : html`<div class="play-text">${turn.text}</div>`}
            </li>
          `;
        })}
      </ol>
      ${pending.length > 0 &&
      html`<div class="play-tool-send">
        <${Button} variant="primary" icon="send" disabled=${busy} onClick=${sendResults}>
          ${pending.length === 1 ? 'Send result and continue' : `Send ${plural(pending.length, 'result')} and continue`}
        <//>
        <span class="faint">The model is waiting for ${pending.length === 1 ? 'this result' : 'these results'}.</span>
      </div>`}
      ${toolNote && waiting.length > 0 && html`<p class="play-tool-send muted">${toolNote}</p>`}
    </div>
  `;
}
