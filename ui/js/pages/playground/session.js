// What the playground is working on, kept for as long as the dashboard stays
// open in this tab.
//
// The page offers links that lead away from it ("Open request" on every
// answer), and the router unmounts a page that is left. Component state
// would take the conversation, the raw body being edited and the last
// request's events with it. So that state lives here, at module level: the
// page reads it with useStore and comes back to what it left. A request that
// is still running keeps running and is finished when the reader returns.
//
// Nothing here is a secret, and nothing is written to storage: a reload
// starts afresh. Signing out forgets it.

import { auth } from '../../lib/api.js';
import { createStore } from '../../lib/store.js';

const blank = () => ({
  turns: [], // the HTTP conversation (see conversation.js)
  draft: '', // the message being written
  busy: false, // a request is in flight
  view: 'next', // which body the Request tab shows: 'next' | 'sent'
  raw: { on: false, text: '', seed: '' }, // the hand-edited body; `seed` is the text raw mode started from
  rawError: null, // why the raw body was not sent
  modelError: null, // why the model field stopped a send
  failure: null, // ApiError carrying the admin API's issues, for the form fields
  replay: null, // { id, kind: 'loaded' | 'no-body' | 'protocol' | 'asking', record, cut }
  tick: 0, // bumped when `work.run` changed in place
});

export const session = createStore(blank());

/** What goes with the session but is never rendered as it is. */
export const work = {
  run: null, // the last request, updated in place while it streams (see inspector.js)
  abort: null, // AbortController of the request in flight
  lookup: null, // AbortController of the follow-up GET /requests/{id}
  records: new Map(), // request records seen on the live connection, by id
  appliedFrom: null, // the ?from= id that was last loaded into raw mode
};

/** A setter for one field: takes a value, or a function of the current value. */
export const setter = (field) => (next) => session.set((s) => ({ [field]: typeof next === 'function' ? next(s[field]) : next }));

/** Draw again after `work.run` changed in place. */
export const repaint = () => session.set((s) => ({ tick: s.tick + 1 }));

/** The WebSocket view's transcript and frames (see socket.js). The client key is never kept here. */
export const socketMemory = {
  turns: [],
  frames: { list: [], total: 0, seq: 0, t0: 0, openedAt: null },
  closed: null, // { code, reason, opened, refusal, left } of the last socket
};

function forgetSocket() {
  socketMemory.turns = [];
  socketMemory.frames = { list: [], total: 0, seq: 0, t0: 0, openedAt: null };
  socketMemory.closed = null;
}

/** Stop what is running and forget everything. */
function forgetSession() {
  work.abort?.abort();
  work.lookup?.abort();
  work.run = null;
  work.abort = null;
  work.lookup = null;
  work.records.clear();
  work.appliedFrom = null;
  session.replace(blank());
  forgetSocket();
}

// The next person to sign in on this tab must not find the last one's conversation.
auth.subscribe(({ status }) => {
  if (status === 'anonymous') forgetSession();
});
