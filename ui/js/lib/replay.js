// Which recorded requests the playground can send again.
//
//   import { canReplayInPlayground, replayProblem } from '../lib/replay.js';
//   canReplayInPlayground(record)            // the kind of request it replays
//   canReplayInPlayground(record, bodies)    // ... and its client body was captured
//   replayProblem(record, bodies)            // null, or why not
//
// The one place that answers the question. The request drawer offers "Open in
// playground" with it, and the playground uses it to decide what to do with
// ?from=<request id>, so the button is never offered for a request the
// playground then refuses. Pure: no DOM, no Preact.

/**
 * The client protocols the playground speaks, in its own order. They are the
 * ids of PROTOCOLS in pages/playground/protocols.js; the self-check holds the
 * two lists together.
 */
export const REPLAY_PROTOCOLS = ['openai-chat', 'openai-responses', 'anthropic', 'gemini'];

/**
 * Recorded endpoints whose captured body is a generation request the
 * playground can send again: chat completions, responses (over HTTP or a
 * WebSocket), messages, generateContent and its streaming twin, and the
 * playground's own proxy. Embeddings, images, token counts and the like are
 * recorded under one of the four protocols too, but their bodies are not
 * generation requests.
 */
export const REPLAY_ENDPOINTS = /\/v1\/chat\/completions$|\/v1\/responses(?: \(WebSocket\))?$|\/v1\/messages$|:(?:stream)?[gG]enerateContent$|\/playground$/;

/**
 * Why the playground cannot replay this record, or null when it can:
 *
 *   "no-body"   no client body was captured (checked only when `bodies` is given)
 *   "protocol"  not one of the four protocols the playground sends
 *   "endpoint"  one of them, but not a generation endpoint
 *
 * Checked in that order. A record without an `endpoint` (an older gateway)
 * is judged by its protocol alone. Pass `bodies` (the `bodies` of
 * GET /requests/{id}, or null/undefined when there are none) to ask about
 * the body as well; leave the argument out to ask only whether the request
 * is the kind the playground replays.
 */
export function replayProblem(record, ...rest) {
  if (rest.length > 0) {
    const text = rest[0]?.client_request;
    if (typeof text !== 'string' || text === '') return 'no-body';
  }
  if (!record || !REPLAY_PROTOCOLS.includes(record.client_protocol)) return 'protocol';
  if (typeof record.endpoint === 'string' && !REPLAY_ENDPOINTS.test(record.endpoint)) return 'endpoint';
  return null;
}

/**
 * True when the playground can replay this record. With `bodies` it also
 * needs the captured client body: that is what "Open in playground" loads.
 * Without the argument, only the kind of request is judged (whether it would
 * be replayable once a body is captured).
 */
export function canReplayInPlayground(record, ...rest) {
  return replayProblem(record, ...rest) === null;
}
