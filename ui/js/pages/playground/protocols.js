// Playground wire formats: the four client protocols, written out by hand.
//
// Everything here is pure (no DOM, no Preact), so it can be exercised under
// Node against a running gateway:
//
//   buildBody(protocol, settings, turns, draft)  the native request body
//   createReader(protocol)                       native response / stream -> blocks
//   readError(json, status)                      any protocol's error body
//   buildCurl(...), buildSnippets(...)           the public-API equivalents
//
// A conversation is kept protocol-neutral so that switching the protocol
// re-encodes the same history:
//
//   turn  { role: 'user', text }
//         { role: 'assistant', blocks: [block], ... }
//   block { type: 'text', text }
//         { type: 'thinking', text, signature?, origin }
//         { type: 'tool_call', id, name, args (JSON text), result?, origin }
//
// `origin` is the protocol a block was received in. Signed reasoning is only
// sent back in the protocol that signed it.

export const PROTOCOLS = [
  {
    id: 'openai-chat',
    label: 'OpenAI Chat Completions',
    short: 'Chat Completions',
    path: '/v1/chat/completions',
    toolResult: 'a tool role message',
    effortField: 'reasoning_effort',
  },
  {
    id: 'openai-responses',
    label: 'OpenAI Responses',
    short: 'Responses',
    path: '/v1/responses',
    toolResult: 'a function_call_output item',
    effortField: 'reasoning.effort',
  },
  {
    id: 'anthropic',
    label: 'Anthropic Messages',
    short: 'Messages',
    path: '/v1/messages',
    toolResult: 'a tool_result block',
    effortField: 'thinking.budget_tokens',
  },
  {
    id: 'gemini',
    label: 'Gemini',
    short: 'Gemini',
    path: '/v1beta/models/{model}:generateContent',
    toolResult: 'a functionResponse part',
    effortField: 'generationConfig.thinkingConfig.thinkingBudget',
  },
];

export const PROTOCOL_IDS = PROTOCOLS.map((p) => p.id);

export function protocolInfo(id) {
  return PROTOCOLS.find((p) => p.id === id) ?? PROTOCOLS[0];
}

/** "default" sends nothing; "none" asks for no reasoning. */
export const EFFORTS = ['default', 'none', 'low', 'medium', 'high'];

/** Token budgets the gateway itself uses for the named efforts. */
export const EFFORT_BUDGET = { low: 1024, medium: 8192, high: 24576 };

/** "model(high)" -> ["model", "high"]: the reasoning suffix the gateway splits off a model name. */
export const splitSuffix = (name) => /^(.+)\(([^()]*)\)$/.exec(String(name ?? ''))?.slice(1) ?? null;

const SUFFIX_LEVELS = ['minimal', 'low', 'medium', 'high', 'xhigh', 'max'];

/**
 * What the gateway makes of the text in a reasoning suffix, the way
 * crates/core/src/reasoning.rs reads it (case and surrounding spaces do not
 * matter): 'level' (an effort name), 'off' (none, off, disabled, 0), 'auto'
 * (auto, dynamic, -1), 'budget' (a positive number of tokens), or null when
 * it is none of these: the gateway then drops the suffix and applies nothing.
 */
export function suffixKind(raw) {
  const word = String(raw ?? '').trim().toLowerCase();
  if (SUFFIX_LEVELS.includes(word)) return 'level';
  if (word === 'none' || word === 'off' || word === 'disabled') return 'off';
  if (word === 'auto' || word === 'dynamic') return 'auto';
  if (!/^[+-]?\d+$/.test(word)) return null;
  const budget = Number(word);
  if (budget === 0) return 'off';
  if (budget === -1) return 'auto';
  return budget > 0 ? 'budget' : null;
}

/** Anthropic requires max_tokens; this is what is sent when the field is empty. */
export function anthropicMaxTokens(settings) {
  if (settings.maxTokens != null) return settings.maxTokens;
  const budget = EFFORT_BUDGET[settings.effort];
  return budget ? budget + 4096 : 1024;
}

export const SAMPLE_TOOL = {
  name: 'get_weather',
  description: 'Get the current weather for a city.',
  parameters: {
    type: 'object',
    properties: {
      city: { type: 'string', description: 'City name, for example Paris' },
      unit: { type: 'string', enum: ['celsius', 'fahrenheit'] },
    },
    required: ['city'],
  },
};

export const SAMPLE_TOOL_RESULT = '{"temperature_c": 18, "conditions": "partly cloudy"}';

export const DEFAULT_SETTINGS = {
  protocol: 'openai-chat',
  model: '',
  stream: true,
  system: '',
  temperature: null,
  maxTokens: null,
  effort: 'default',
  tools: false,
};

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

const isObject = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);

/** Tool arguments as an object; text that is not a JSON object is wrapped. */
export function parseArgs(args) {
  if (isObject(args)) return args;
  try {
    const value = JSON.parse(args || '{}');
    return isObject(value) ? value : { value };
  } catch {
    return { raw: String(args ?? '') };
  }
}

/** Tool arguments for reading: re-indented when they are JSON, as received otherwise. */
export function prettyArgs(args) {
  try {
    return JSON.stringify(JSON.parse(args), null, 2);
  } catch {
    return String(args ?? '');
  }
}

/** A tool result as the object Gemini wants. */
function resultObject(result) {
  try {
    const value = JSON.parse(result);
    return isObject(value) ? value : { result: value };
  } catch {
    return { result: String(result ?? '') };
  }
}

const hasResult = (block) => block.type === 'tool_call' && block.result != null;

/** Turns that take part in the request: detached (raw) exchanges and empty answers do not. */
function history(turns, draft) {
  const list = [];
  for (const turn of turns) {
    if (turn.detached || turn.role === 'raw') continue;
    if (turn.role === 'user') list.push({ role: 'user', text: turn.text });
    else if (turn.role === 'assistant') {
      const blocks = (turn.blocks ?? []).filter((b) => b.type === 'tool_call' || (typeof b.text === 'string' && b.text !== ''));
      if (blocks.length > 0) list.push({ role: 'assistant', blocks });
    }
  }
  const text = typeof draft === 'string' ? draft.trim() : '';
  if (text) list.push({ role: 'user', text });
  return list;
}

const joinText = (blocks) =>
  blocks
    .filter((b) => b.type === 'text')
    .map((b) => b.text)
    .join('');

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

function chatBody(settings, turns) {
  const messages = [];
  if (settings.system.trim()) messages.push({ role: 'system', content: settings.system });
  for (const turn of turns) {
    if (turn.role === 'user') {
      messages.push({ role: 'user', content: turn.text });
      continue;
    }
    const calls = turn.blocks.filter((b) => b.type === 'tool_call');
    const text = joinText(turn.blocks);
    const message = { role: 'assistant', content: text || null };
    if (calls.length > 0) {
      message.tool_calls = calls.map((c) => ({ id: c.id, type: 'function', function: { name: c.name, arguments: c.args || '{}' } }));
    }
    if (text || calls.length > 0) messages.push(message);
    for (const call of calls.filter(hasResult)) messages.push({ role: 'tool', tool_call_id: call.id, content: call.result });
  }
  const body = { model: settings.model, messages };
  if (settings.stream) {
    body.stream = true;
    body.stream_options = { include_usage: true };
  }
  if (settings.temperature != null) body.temperature = settings.temperature;
  if (settings.maxTokens != null) body.max_completion_tokens = settings.maxTokens;
  if (settings.effort !== 'default') body.reasoning_effort = settings.effort;
  if (settings.tools) body.tools = [{ type: 'function', function: SAMPLE_TOOL }];
  return body;
}

function responsesInput(turns) {
  const input = [];
  for (const turn of turns) {
    if (turn.role === 'user') {
      input.push({ role: 'user', content: [{ type: 'input_text', text: turn.text }] });
      continue;
    }
    for (const block of turn.blocks) {
      if (block.type === 'text') input.push({ role: 'assistant', content: [{ type: 'output_text', text: block.text }] });
      else if (block.type === 'tool_call') input.push({ type: 'function_call', call_id: block.id, name: block.name, arguments: block.args || '{}' });
    }
    for (const call of turn.blocks.filter(hasResult)) input.push({ type: 'function_call_output', call_id: call.id, output: call.result });
  }
  return input;
}

function responsesBody(settings, turns) {
  const body = { model: settings.model, input: responsesInput(turns) };
  if (settings.system.trim()) body.instructions = settings.system;
  if (settings.stream) body.stream = true;
  if (settings.temperature != null) body.temperature = settings.temperature;
  if (settings.maxTokens != null) body.max_output_tokens = settings.maxTokens;
  if (settings.effort === 'none') body.reasoning = { effort: 'none' };
  else if (settings.effort !== 'default') body.reasoning = { effort: settings.effort, summary: 'auto' };
  if (settings.tools) body.tools = [{ type: 'function', ...SAMPLE_TOOL }];
  return body;
}

function anthropicBody(settings, turns) {
  const messages = [];
  for (const turn of turns) {
    if (turn.role === 'user') {
      // A user message straight after tool results joins them: the API wants
      // one user turn, not two.
      const prev = messages[messages.length - 1];
      if (prev && prev.role === 'user' && Array.isArray(prev.content)) prev.content.push({ type: 'text', text: turn.text });
      else messages.push({ role: 'user', content: turn.text });
      continue;
    }
    const content = [];
    for (const block of turn.blocks) {
      if (block.type === 'thinking') {
        if (block.origin === 'anthropic' && block.signature) content.push({ type: 'thinking', thinking: block.text, signature: block.signature });
      } else if (block.type === 'text') {
        content.push({ type: 'text', text: block.text });
      } else if (block.type === 'tool_call') {
        content.push({ type: 'tool_use', id: block.id, name: block.name, input: parseArgs(block.args) });
      }
    }
    if (content.some((c) => c.type !== 'thinking')) messages.push({ role: 'assistant', content });
    const results = turn.blocks.filter(hasResult);
    if (results.length > 0) {
      messages.push({ role: 'user', content: results.map((c) => ({ type: 'tool_result', tool_use_id: c.id, content: c.result })) });
    }
  }
  const body = { model: settings.model, max_tokens: anthropicMaxTokens(settings) };
  if (settings.system.trim()) body.system = settings.system;
  body.messages = messages;
  if (settings.stream) body.stream = true;
  if (settings.temperature != null) body.temperature = settings.temperature;
  if (settings.effort === 'none') body.thinking = { type: 'disabled' };
  else if (EFFORT_BUDGET[settings.effort]) body.thinking = { type: 'enabled', budget_tokens: EFFORT_BUDGET[settings.effort] };
  if (settings.tools) body.tools = [{ name: SAMPLE_TOOL.name, description: SAMPLE_TOOL.description, input_schema: SAMPLE_TOOL.parameters }];
  return body;
}

function geminiBody(settings, turns) {
  const contents = [];
  for (const turn of turns) {
    if (turn.role === 'user') {
      contents.push({ role: 'user', parts: [{ text: turn.text }] });
      continue;
    }
    const parts = [];
    for (const block of turn.blocks) {
      if (block.type === 'thinking') {
        if (block.origin === 'gemini' && block.signature) parts.push({ text: block.text, thought: true, thoughtSignature: block.signature });
      } else if (block.type === 'text') {
        const part = { text: block.text };
        if (block.origin === 'gemini' && block.signature) part.thoughtSignature = block.signature;
        parts.push(part);
      } else if (block.type === 'tool_call') {
        const call = { name: block.name, args: parseArgs(block.args) };
        if (block.id && !block.syntheticId) call.id = block.id;
        const part = { functionCall: call };
        if (block.origin === 'gemini' && block.signature) part.thoughtSignature = block.signature;
        parts.push(part);
      }
    }
    if (parts.some((p) => !p.thought)) contents.push({ role: 'model', parts });
    const results = turn.blocks.filter(hasResult);
    if (results.length > 0) {
      contents.push({
        role: 'user',
        parts: results.map((c) => {
          const response = { name: c.name, response: resultObject(c.result) };
          if (c.id && !c.syntheticId) response.id = c.id;
          return { functionResponse: response };
        }),
      });
    }
  }
  const body = { contents };
  if (settings.system.trim()) body.systemInstruction = { parts: [{ text: settings.system }] };
  const generation = {};
  if (settings.temperature != null) generation.temperature = settings.temperature;
  if (settings.maxTokens != null) generation.maxOutputTokens = settings.maxTokens;
  if (settings.effort === 'none') generation.thinkingConfig = { thinkingBudget: 0 };
  else if (EFFORT_BUDGET[settings.effort]) generation.thinkingConfig = { thinkingBudget: EFFORT_BUDGET[settings.effort], includeThoughts: true };
  if (Object.keys(generation).length > 0) body.generationConfig = generation;
  if (settings.tools) body.tools = [{ functionDeclarations: [SAMPLE_TOOL] }];
  return body;
}

/**
 * The native request body for `protocol`: what a client of the public API
 * would send. For Gemini the model and the stream flag are not part of the
 * body (they are in the URL); for the others they are.
 *
 * @param {string} protocol
 * @param {typeof DEFAULT_SETTINGS} settings
 * @param {Array} turns  the conversation so far
 * @param {string} [draft]  text of the message being written, added as the last user turn
 */
export default function buildBody(protocol, settings, turns, draft) {
  const list = history(turns, draft);
  switch (protocol) {
    case 'openai-responses':
      return responsesBody(settings, list);
    case 'anthropic':
      return anthropicBody(settings, list);
    case 'gemini':
      return geminiBody(settings, list);
    default:
      return chatBody(settings, list);
  }
}

/**
 * The body of POST /admin/api/playground, assembled as text so that
 * `bodyText` reaches the gateway token for token (a hand-edited body with a
 * 64-bit seed or `1.0` is not rewritten by a JSON round trip here).
 */
export function buildEnvelope(protocol, bodyText, { model, stream } = {}) {
  const head = `{"protocol":${JSON.stringify(protocol)}`;
  if (protocol === 'gemini') return `${head},"model":${JSON.stringify(model ?? '')},"stream":${stream ? 'true' : 'false'},"body":${bodyText}}`;
  return `${head},"body":${bodyText}}`;
}

/** The public client-API path this request corresponds to. */
export function publicPath(protocol, model, stream) {
  if (protocol === 'gemini') {
    const name = String(model || '{model}').replace(/^models\//, '');
    return `/v1beta/models/${name}:${stream ? 'streamGenerateContent?alt=sse' : 'generateContent'}`;
  }
  return protocolInfo(protocol).path;
}

/**
 * Check hand-written body text. Returns { value } when it is a JSON object,
 * else { error } with a sentence that says where it breaks.
 */
export function checkRawBody(text) {
  if (!String(text).trim()) return { error: 'The body is empty. Write a JSON object.' };
  let value;
  try {
    value = JSON.parse(text);
  } catch (cause) {
    const found = locateJsonError(String(text));
    if (found) return { error: `Not valid JSON at line ${found.line}, column ${found.column}: ${found.message}.` };
    return { error: `Not valid JSON: ${String(cause?.message ?? 'syntax error')}` };
  }
  if (!isObject(value)) return { error: 'The body must be a JSON object, in braces.' };
  return { value };
}

const JSON_LITERAL = /-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?|true|false|null/y;

/**
 * Where JSON text stops being JSON, and what was expected there. Browsers
 * word (and place) this differently, some without a position at all, so the
 * playground finds it itself. Returns null when the text is valid.
 */
export function locateJsonError(text) {
  const n = text.length;
  let i = 0;
  const fail = (message) => {
    throw { at: Math.min(i, n), message };
  };
  const space = () => {
    while (i < n && (text[i] === ' ' || text[i] === '\n' || text[i] === '\r' || text[i] === '\t')) i += 1;
  };
  const string = () => {
    i += 1;
    while (i < n) {
      const c = text[i];
      if (c === '"') {
        i += 1;
        return;
      }
      if (c === '\\') {
        if (!/["\\/bfnrtu]/.test(text[i + 1] ?? '')) fail('this escape is not allowed in a string');
        i += 2;
      } else if (c < ' ') {
        fail('a string cannot contain a line break; write \\n');
      } else {
        i += 1;
      }
    }
    fail('a string is not closed');
  };
  const list = (close, item, inside) => {
    i += 1;
    space();
    if (text[i] === close) {
      i += 1;
      return;
    }
    for (;;) {
      item();
      space();
      if (text[i] === ',') {
        i += 1;
        space();
        if (text[i] === close) fail(`a comma cannot come right before "${close}"`);
      } else if (text[i] === close) {
        i += 1;
        return;
      } else {
        fail(i >= n ? `the text ends inside ${inside}` : `expected "," or "${close}"`);
      }
    }
  };
  const value = () => {
    space();
    if (i >= n) fail('the text ends where a value is expected');
    const c = text[i];
    if (c === '{') {
      list(
        '}',
        () => {
          space();
          if (text[i] !== '"') fail(i >= n ? 'the text ends inside an object' : 'expected a key in double quotes');
          string();
          space();
          if (text[i] !== ':') fail('expected ":" after the key');
          i += 1;
          value();
        },
        'an object',
      );
    } else if (c === '[') {
      list(']', value, 'an array');
    } else if (c === '"') {
      string();
    } else {
      JSON_LITERAL.lastIndex = i;
      const match = JSON_LITERAL.exec(text);
      if (!match) fail(`unexpected ${c === "'" ? 'single quote; JSON uses double quotes' : JSON.stringify(c)}`);
      i += match[0].length;
    }
  };
  try {
    value();
    space();
    if (i < n) fail('unexpected text after the end of the body');
    return null;
  } catch (thrown) {
    if (!thrown || typeof thrown.at !== 'number') throw thrown;
    const before = text.slice(0, thrown.at);
    return { line: before.split('\n').length, column: before.length - before.lastIndexOf('\n'), message: thrown.message };
  }
}

/** Best-effort one-line description of a hand-written body, for the transcript. */
export function summarizeRawBody(protocol, body) {
  const pick = (value) => {
    if (typeof value === 'string') return value;
    if (!Array.isArray(value)) return '';
    return value
      .map((part) => (typeof part === 'string' ? part : (part?.text ?? '')))
      .filter(Boolean)
      .join(' ');
  };
  try {
    if (protocol === 'gemini') {
      const users = (body.contents ?? []).filter((c) => (c.role ?? 'user') === 'user');
      return pick(users[users.length - 1]?.parts);
    }
    if (protocol === 'openai-responses') {
      if (typeof body.input === 'string') return body.input;
      const users = (body.input ?? []).filter((item) => item?.role === 'user');
      return pick(users[users.length - 1]?.content);
    }
    const users = (body.messages ?? []).filter((m) => m?.role === 'user');
    return pick(users[users.length - 1]?.content);
  } catch {
    return '';
  }
}

// ---------------------------------------------------------------------------
// Usage and errors
// ---------------------------------------------------------------------------

const num = (v) => (typeof v === 'number' && Number.isFinite(v) ? v : null);

/** Any protocol's usage object as { input, output, reasoning, cached, total }; null when there is none. */
export function readUsage(protocol, usage) {
  if (!isObject(usage)) return null;
  let out;
  if (protocol === 'gemini') {
    out = {
      input: num(usage.promptTokenCount),
      output: num(usage.candidatesTokenCount),
      reasoning: num(usage.thoughtsTokenCount),
      cached: num(usage.cachedContentTokenCount),
      total: num(usage.totalTokenCount),
    };
  } else if (protocol === 'anthropic') {
    const input = num(usage.input_tokens);
    const output = num(usage.output_tokens);
    const cached = num(usage.cache_read_input_tokens);
    const written = num(usage.cache_creation_input_tokens);
    out = {
      input,
      output,
      reasoning: null,
      cached,
      total: input == null && output == null ? null : (input ?? 0) + (output ?? 0) + (cached ?? 0) + (written ?? 0),
    };
  } else if (protocol === 'openai-responses') {
    out = {
      input: num(usage.input_tokens),
      output: num(usage.output_tokens),
      reasoning: num(usage.output_tokens_details?.reasoning_tokens),
      cached: num(usage.input_tokens_details?.cached_tokens),
      total: num(usage.total_tokens),
    };
  } else {
    out = {
      input: num(usage.prompt_tokens),
      output: num(usage.completion_tokens),
      reasoning: num(usage.completion_tokens_details?.reasoning_tokens),
      cached: num(usage.prompt_tokens_details?.cached_tokens),
      total: num(usage.total_tokens),
    };
  }
  return Object.values(out).every((v) => v == null) ? null : out;
}

/**
 * The error inside any of the bodies the playground can get back: the four
 * protocols' own shapes, their in-stream error frames, and the admin API's
 * envelope. Returns { message, type, code, status, issues, retryAfter }.
 *
 * `retryAfter` (seconds, or null) is read from `error.headers`, where the
 * Responses WebSocket puts a retry-after it has no response header for. An
 * HTTP answer carries it as a real header instead (see run.js).
 */
export function readError(json, status = 0) {
  const out = { message: '', type: null, code: null, status: status || 0, issues: [], retryAfter: null };
  if (typeof json === 'string') {
    out.message = json;
    return out;
  }
  if (!isObject(json)) return out;
  const err = isObject(json.error) ? json.error : isObject(json.response?.error) ? json.response.error : null;
  if (err) {
    if (typeof err.message === 'string') out.message = err.message;
    if (typeof err.type === 'string') out.type = err.type;
    else if (typeof err.status === 'string') out.type = err.status;
    if (typeof err.code === 'string') out.code = err.code;
    else if (typeof err.code === 'number' && !out.status) out.status = err.code;
    // Gemini's code is the HTTP status; the reason is in details (ErrorInfo): MODEL_NOT_FOUND, MODEL_COOLDOWN.
    if (!out.code && Array.isArray(err.details)) {
      const reason = err.details.find((d) => isObject(d) && typeof d.reason === 'string' && d.reason !== '')?.reason;
      if (reason) out.code = reason;
    }
    if (Array.isArray(err.issues)) {
      out.issues = err.issues.filter((i) => i && typeof i.message === 'string').map((i) => ({ path: typeof i.path === 'string' ? i.path : '', message: i.message }));
    }
    // A header value: a string of seconds ("7"). Anything else is not a wait.
    const wait = isObject(err.headers) ? err.headers['retry-after'] : undefined;
    if ((typeof wait === 'string' && /^\s*\d+(\.\d+)?\s*$/.test(wait)) || (typeof wait === 'number' && Number.isFinite(wait) && wait >= 0)) out.retryAfter = Number(wait);
  } else if (typeof json.error === 'string') {
    out.message = json.error;
  } else {
    if (typeof json.message === 'string') out.message = json.message;
    if (typeof json.code === 'string') out.code = json.code;
  }
  if (typeof json.status === 'number' && !out.status) out.status = json.status;
  return out;
}

/**
 * The gateway's refusals that are about its own credential at the provider
 * (crates/gateway/src/failover.rs). Chat and Responses carry the code,
 * Gemini carries it upper case as the reason in error.details, and an
 * Anthropic error has no code at all: there only the gateway's own wording
 * tells them apart from any other upstream failure.
 */
const UPSTREAM_CREDENTIAL = {
  upstream_auth_error: /^the upstream provider rejected the gateway's credential/i,
  upstream_permission_denied: /^the upstream provider does not let the gateway's credential serve this request/i,
};

function credentialRefusal({ code, message }) {
  const named = typeof code === 'string' ? code.toLowerCase() : null;
  if (named && named in UPSTREAM_CREDENTIAL) return named;
  const text = typeof message === 'string' ? message.trim() : '';
  return Object.keys(UPSTREAM_CREDENTIAL).find((name) => UPSTREAM_CREDENTIAL[name].test(text)) ?? null;
}

/** What to do about an error, in one sentence. */
export function adviceFor(error) {
  const { status, code } = error;
  // On the WebSocket there is no inspector and no "send again": the turn is
  // one frame in the frame list, and the request log has its record.
  const socket = error.via === 'socket';
  if (error.kind === 'network') return 'Check that Switchyard is running and that this device can reach it, then send again.';
  if (code === 'previous_response_not_found') {
    return 'A socket can only be continued from its latest response, and this one does not have the response that was named. The next turn is sent without previous_response_id and starts a new conversation.';
  }
  if (code === 'websocket_queue_full') return 'Wait for the response in progress to finish, then send the turn again.';
  if (code === 'model_not_found' || code === 'MODEL_NOT_FOUND') return 'Choose a model from the list, or add a provider that serves this one.';
  if (code === 'model_cooldown' || code === 'MODEL_COOLDOWN' || /cooling down/.test(error.message ?? '')) return 'Every credential for this model is resting. Wait for the cooldown, or reset it on the Providers page.';
  const credential = credentialRefusal(error);
  if (credential === 'upstream_auth_error') return "The provider did not accept the gateway's credential. Check the provider's API key on the Providers page.";
  if (credential === 'upstream_permission_denied') return "The provider's credential is not allowed to serve this request. Check the provider's API key and its access to this model on the Providers page.";
  if (status === 429) return socket ? 'The upstream rate limit was reached. Wait, then connect and send the turn again.' : 'The upstream rate limit was reached. Wait, then send again.';
  if (status === 413) return 'The body is larger than server.body_limit_mb allows. Shorten it, or raise the limit in Settings.';
  if (status === 400 || status === 422) return socket ? 'Check the frame that was sent, in the frame list, then send the turn again.' : 'Check the request body in the inspector, then send again.';
  if (status === 404) return socket ? 'Check the model name, then send the turn again.' : 'Check the model name and the protocol, then send again.';
  if (status === 401 || status === 403) return socket ? 'The gateway refused the client key for this model. Check what the key allows on the API keys page.' : 'The gateway refused the dashboard session. Sign in again.';
  if (status >= 500) return socket ? 'The upstream provider failed. The request log shows every attempt.' : 'The upstream provider failed. Open the request to see every attempt, or send again.';
  return socket ? 'The request log has the details.' : 'Open the request for the details, or send again.';
}

// ---------------------------------------------------------------------------
// Reading answers
// ---------------------------------------------------------------------------

/**
 * Turns a protocol's answer into blocks, one event (or one JSON body) at a
 * time.
 *
 *   const reader = createReader('anthropic');
 *   reader.event({ event, data, json });   // each SSE event of a stream
 *   reader.response(json);                 // or the body of a non-stream call
 *   reader.state  // { blocks, finish, usage, error, responseId, model, done }
 *   reader.snapshot()                      // blocks, copied, for rendering
 */
export function createReader(protocol) {
  const state = { blocks: [], finish: null, usage: null, error: null, responseId: null, model: null, done: false };
  const byKey = new Map();
  let synthetic = 0;
  let rawUsage = null;

  const add = (block, key) => {
    block.origin = protocol;
    state.blocks.push(block);
    if (key != null) byKey.set(key, block);
    return block;
  };
  const last = (type) => {
    const block = state.blocks[state.blocks.length - 1];
    return block && block.type === type ? block : null;
  };
  /** Append text to the block with `key`, or to the last block when it has that type. */
  const text = (type, chunk, key) => {
    if (typeof chunk !== 'string' || chunk === '') return;
    const block = (key != null ? byKey.get(key) : last(type)) ?? add({ type, text: '' }, key);
    block.text += chunk;
  };
  const tool = (key, fields) => {
    const block = add({ type: 'tool_call', id: '', name: '', args: '', ...fields }, key);
    if (!block.id) {
      synthetic += 1;
      block.id = `call_${synthetic}`;
      block.syntheticId = true;
    }
    return block;
  };
  const usage = (value) => {
    if (!isObject(value)) return;
    rawUsage = { ...rawUsage, ...value };
    state.usage = readUsage(protocol, rawUsage) ?? state.usage;
  };
  const fail = (json, status) => {
    state.error = readError(json, status);
    state.done = true;
  };

  // ---- OpenAI Chat Completions -------------------------------------------
  const chat = (json) => {
    if (json.error != null) return fail(json);
    if (typeof json.id === 'string') state.responseId = json.id;
    if (typeof json.model === 'string') state.model = json.model;
    usage(json.usage);
    const choice = json.choices?.[0];
    if (!choice) return undefined;
    const delta = choice.delta ?? choice.message ?? {};
    text('thinking', typeof delta.reasoning_content === 'string' ? delta.reasoning_content : typeof delta.reasoning === 'string' ? delta.reasoning : '');
    for (const detail of Array.isArray(delta.reasoning_details) ? delta.reasoning_details : []) {
      const block = last('thinking');
      if (block && typeof detail?.signature === 'string') block.signature = detail.signature;
    }
    if (typeof delta.content === 'string') text('text', delta.content);
    else if (Array.isArray(delta.content)) for (const part of delta.content) text('text', part?.text);
    text('text', delta.refusal);
    (Array.isArray(delta.tool_calls) ? delta.tool_calls : []).forEach((call, i) => {
      const key = `tool:${call.index ?? i}`;
      const block = byKey.get(key) ?? tool(key, { id: call.id ?? '' });
      if (call.id && block.syntheticId) {
        block.id = call.id;
        block.syntheticId = false;
      }
      if (typeof call.function?.name === 'string' && !block.name) block.name = call.function.name;
      if (typeof call.function?.arguments === 'string') block.args += call.function.arguments;
    });
    if (choice.finish_reason) state.finish = choice.finish_reason;
    return undefined;
  };

  // ---- OpenAI Responses ---------------------------------------------------
  const summaryText = (item) => {
    const parts = [...(Array.isArray(item.summary) ? item.summary : []), ...(Array.isArray(item.content) ? item.content : [])];
    return parts
      .map((p) => p?.text)
      .filter((t) => typeof t === 'string' && t)
      .join('\n\n');
  };
  const responsesItem = (item, whole) => {
    if (!isObject(item)) return;
    if (item.type === 'reasoning') {
      const block = byKey.get(item.id) ?? add({ type: 'thinking', text: '' }, item.id);
      if (whole && !block.text) block.text = summaryText(item);
      if (typeof item.encrypted_content === 'string') block.signature = item.encrypted_content;
    } else if (item.type === 'function_call') {
      const block = byKey.get(item.id) ?? tool(item.id, { id: item.call_id ?? '', name: item.name ?? '' });
      if (item.name) block.name = item.name;
      if (typeof item.arguments === 'string' && (whole || !block.args)) block.args = item.arguments;
    } else if (item.type === 'message' && whole && !byKey.has(item.id)) {
      for (const part of Array.isArray(item.content) ? item.content : []) text('text', part?.text ?? part?.refusal, item.id);
    }
  };
  const responsesDone = (response) => {
    if (!isObject(response)) return;
    if (typeof response.id === 'string') state.responseId = response.id;
    if (typeof response.model === 'string') state.model = response.model;
    usage(response.usage);
    if (response.error != null) state.error = readError({ error: response.error });
    for (const item of Array.isArray(response.output) ? response.output : []) responsesItem(item, true);
    if (response.status === 'completed') state.finish = 'completed';
    else if (response.status) state.finish = response.incomplete_details?.reason ?? response.status;
  };
  const responses = (json) => {
    switch (json.type) {
      case 'response.created':
      case 'response.in_progress':
        if (typeof json.response?.id === 'string') state.responseId = json.response.id;
        if (typeof json.response?.model === 'string') state.model = json.response.model;
        break;
      case 'response.output_item.added':
        responsesItem(json.item, false);
        break;
      case 'response.output_item.done':
        responsesItem(json.item, true);
        break;
      case 'response.reasoning_summary_part.added': {
        const block = byKey.get(json.item_id);
        if (block && block.text && json.summary_index > 0) block.text += '\n\n';
        break;
      }
      case 'response.reasoning_summary_text.delta':
      case 'response.reasoning_text.delta':
        text('thinking', json.delta, json.item_id);
        break;
      case 'response.output_text.delta':
      case 'response.refusal.delta':
        text('text', json.delta, json.item_id);
        break;
      case 'response.function_call_arguments.delta': {
        const block = byKey.get(json.item_id);
        if (block && typeof json.delta === 'string') block.args += json.delta;
        break;
      }
      case 'response.function_call_arguments.done': {
        const block = byKey.get(json.item_id);
        if (block && typeof json.arguments === 'string') block.args = json.arguments;
        break;
      }
      case 'response.completed':
      case 'response.incomplete':
      case 'response.failed':
        responsesDone(json.response);
        state.done = true;
        break;
      case 'error':
        fail(json);
        break;
      default:
    }
  };

  // ---- Anthropic Messages -------------------------------------------------
  const anthropicBlock = (content, key) => {
    if (!isObject(content)) return;
    if (content.type === 'text') add({ type: 'text', text: content.text ?? '' }, key);
    else if (content.type === 'thinking') add({ type: 'thinking', text: content.thinking ?? '', signature: content.signature || undefined }, key);
    else if (content.type === 'redacted_thinking') add({ type: 'thinking', text: '', redacted: true }, key);
    else if (content.type === 'tool_use' || content.type === 'server_tool_use') {
      const empty = !isObject(content.input) || Object.keys(content.input).length === 0;
      tool(key, { id: content.id ?? '', name: content.name ?? '', args: key == null || !empty ? JSON.stringify(content.input ?? {}) : '' });
    }
  };
  const anthropicMessage = (message) => {
    if (typeof message.id === 'string') state.responseId = message.id;
    if (typeof message.model === 'string') state.model = message.model;
    usage(message.usage);
    if (message.stop_reason) state.finish = message.stop_reason;
  };
  const anthropic = (json) => {
    const key = `block:${json.index}`;
    switch (json.type) {
      case 'message_start':
        if (isObject(json.message)) anthropicMessage(json.message);
        break;
      case 'content_block_start':
        anthropicBlock(json.content_block, key);
        break;
      case 'content_block_delta': {
        const block = byKey.get(key);
        const delta = json.delta ?? {};
        if (!block) break;
        if (delta.type === 'text_delta' && typeof delta.text === 'string') block.text += delta.text;
        else if (delta.type === 'thinking_delta' && typeof delta.thinking === 'string') block.text += delta.thinking;
        else if (delta.type === 'signature_delta' && typeof delta.signature === 'string') block.signature = (block.signature ?? '') + delta.signature;
        else if (delta.type === 'input_json_delta' && typeof delta.partial_json === 'string') block.args += delta.partial_json;
        break;
      }
      case 'content_block_stop': {
        const block = byKey.get(key);
        if (block && block.type === 'tool_call' && block.args === '') block.args = '{}';
        break;
      }
      case 'message_delta':
        if (json.delta?.stop_reason) state.finish = json.delta.stop_reason;
        usage(json.usage);
        break;
      case 'message_stop':
        state.done = true;
        break;
      case 'error':
        fail(json);
        break;
      default:
    }
  };

  // ---- Gemini -------------------------------------------------------------
  const gemini = (json) => {
    if (json.error != null) return fail(json);
    if (typeof json.responseId === 'string') state.responseId = json.responseId;
    if (typeof json.modelVersion === 'string') state.model = json.modelVersion;
    const candidate = json.candidates?.[0];
    for (const part of Array.isArray(candidate?.content?.parts) ? candidate.content.parts : []) {
      if (isObject(part.functionCall)) {
        const call = part.functionCall;
        tool(null, { id: call.id ?? '', name: call.name ?? '', args: JSON.stringify(call.args ?? {}), signature: part.thoughtSignature });
      } else if (typeof part.text === 'string') {
        const type = part.thought ? 'thinking' : 'text';
        text(type, part.text);
        const block = last(type);
        if (block && typeof part.thoughtSignature === 'string') block.signature = part.thoughtSignature;
      }
    }
    if (candidate?.finishReason) state.finish = candidate.finishReason;
    if (json.promptFeedback?.blockReason) state.finish = json.promptFeedback.blockReason;
    usage(json.usageMetadata);
    return undefined;
  };

  const feed = (json) => {
    if (!isObject(json)) return;
    if (protocol === 'openai-responses') responses(json);
    else if (protocol === 'anthropic') anthropic(json);
    else if (protocol === 'gemini') gemini(json);
    else chat(json);
  };

  return {
    state,
    /** One server-sent event of a stream (or one WebSocket frame, as { json }). */
    event(event) {
      if (event.data === '[DONE]') {
        state.done = true;
        return;
      }
      feed(event.json);
    },
    /** The JSON body of a call that was not streamed. */
    response(json) {
      if (!isObject(json)) return;
      if (protocol === 'openai-responses') {
        if (json.error != null && json.object !== 'response') fail(json);
        else responsesDone(json);
      } else if (protocol === 'anthropic') {
        if (json.type === 'error' || json.error != null) fail(json);
        else {
          anthropicMessage(json);
          for (const content of Array.isArray(json.content) ? json.content : []) anthropicBlock(content, null);
        }
      } else {
        feed(json);
      }
      state.done = true;
    },
    snapshot() {
      return state.blocks.map((block) => ({ ...block }));
    },
  };
}

const quote = (text) => JSON.stringify(text.length > 60 ? `${text.slice(0, 60)}…` : text);

/**
 * What one stream event carries, in a few words, for the event list: the
 * raw data of consecutive chunks starts with the same forty characters, so
 * the list would otherwise not say which chunk holds what. Returns '' when
 * there is nothing short to say; the raw data is always shown next to it.
 */
export function digestEvent(protocol, event) {
  if (event.data === '[DONE]') return 'end of stream';
  const json = event.json;
  if (!isObject(json)) return '';
  if (json.error != null || json.type === 'error') return `error ${quote(readError(json).message)}`;

  if (protocol === 'openai-responses') {
    const type = String(json.type ?? '');
    if (typeof json.delta === 'string') return quote(json.delta);
    if (type === 'response.output_item.added' || type === 'response.output_item.done') return [json.item?.type, json.item?.name].filter(Boolean).join(' ');
    if (type === 'response.failed') return `error ${quote(readError(json).message)}`;
    if (type === 'response.completed' || type === 'response.incomplete') {
      const usage = readUsage(protocol, json.response?.usage);
      return usage ? `${usage.input ?? 0} in, ${usage.output ?? 0} out` : String(json.response?.status ?? '');
    }
    return '';
  }

  if (protocol === 'anthropic') {
    const delta = json.delta ?? {};
    if (json.type === 'content_block_start') return [json.content_block?.type, json.content_block?.name].filter(Boolean).join(' ');
    if (delta.type === 'text_delta') return quote(delta.text ?? '');
    if (delta.type === 'thinking_delta') return `thinking ${quote(delta.thinking ?? '')}`;
    if (delta.type === 'input_json_delta') return `input ${quote(delta.partial_json ?? '')}`;
    if (delta.type === 'signature_delta') return 'signature';
    if (json.type === 'message_delta') return delta.stop_reason ? `stop_reason ${delta.stop_reason}` : '';
    return '';
  }

  if (protocol === 'gemini') {
    const candidate = json.candidates?.[0];
    const parts = Array.isArray(candidate?.content?.parts) ? candidate.content.parts : [];
    const out = [];
    for (const part of parts) {
      if (isObject(part.functionCall)) out.push(`functionCall ${part.functionCall.name ?? ''}`);
      else if (typeof part.text === 'string' && part.text !== '') out.push(`${part.thought ? 'thought ' : ''}${quote(part.text)}`);
      else if (part.thoughtSignature) out.push('signature');
    }
    if (candidate?.finishReason) out.push(`finish ${candidate.finishReason}`);
    return out.join(', ');
  }

  const choice = json.choices?.[0];
  if (!choice) {
    const usage = readUsage(protocol, json.usage);
    return usage ? `usage ${usage.input ?? 0} in, ${usage.output ?? 0} out` : '';
  }
  const delta = choice.delta ?? {};
  const out = [];
  if (typeof delta.reasoning_content === 'string' && delta.reasoning_content !== '') out.push(`reasoning ${quote(delta.reasoning_content)}`);
  if (typeof delta.content === 'string' && delta.content !== '') out.push(quote(delta.content));
  for (const call of Array.isArray(delta.tool_calls) ? delta.tool_calls : []) {
    const name = call.function?.name;
    const args = call.function?.arguments;
    out.push(name ? `tool ${name}` : `arguments ${quote(args ?? '')}`);
  }
  if (out.length === 0 && Array.isArray(delta.reasoning_details) && delta.reasoning_details.some((d) => d?.signature)) out.push('signature');
  if (choice.finish_reason) out.push(`finish ${choice.finish_reason}`);
  if (out.length === 0 && delta.role) out.push(`role ${delta.role}`);
  return out.join(', ');
}

// ---------------------------------------------------------------------------
// The same request against the public client API
// ---------------------------------------------------------------------------

/** Placeholder the copied commands use for the client key. */
export const KEY_VARIABLE = 'SWITCHYARD_KEY';

const shellQuote = (text) => `'${String(text).replace(/'/g, `'\\''`)}'`;

/**
 * curl for the public endpoint that matches the request.
 * @param {{ origin: string, protocol: string, model: string, stream: boolean, bodyText: string }} request
 */
export function buildCurl({ origin, protocol, model, stream, bodyText }) {
  const lines = [`curl ${stream ? '-N ' : ''}${shellQuote(`${origin}${publicPath(protocol, model, stream)}`)}`];
  if (protocol === 'anthropic') {
    lines.push(`-H "x-api-key: $${KEY_VARIABLE}"`, '-H "anthropic-version: 2023-06-01"');
  } else if (protocol === 'gemini') {
    lines.push(`-H "x-goog-api-key: $${KEY_VARIABLE}"`);
  } else {
    lines.push(`-H "Authorization: Bearer $${KEY_VARIABLE}"`);
  }
  lines.push('-H "Content-Type: application/json"', `-d ${shellQuote(bodyText)}`);
  return lines.join(' \\\n  ');
}

/**
 * Base-URL setup for the official SDKs, pointed at this gateway.
 * @returns {Array<{ id: string, label: string, family: string, code: string }>}
 */
export function buildSnippets({ origin, protocol, model }) {
  const name = JSON.stringify(model || 'your-model');
  const useResponses = protocol === 'openai-responses';
  const openaiPython = useResponses
    ? `response = client.responses.create(\n    model=${name},\n    input="Hello",\n)\nprint(response.output_text)`
    : `completion = client.chat.completions.create(\n    model=${name},\n    messages=[{"role": "user", "content": "Hello"}],\n)\nprint(completion.choices[0].message.content)`;
  const openaiNode = useResponses
    ? `const response = await client.responses.create({\n  model: ${name},\n  input: "Hello",\n});\nconsole.log(response.output_text);`
    : `const completion = await client.chat.completions.create({\n  model: ${name},\n  messages: [{ role: "user", content: "Hello" }],\n});\nconsole.log(completion.choices[0].message.content);`;
  return [
    {
      id: 'openai-python',
      label: 'OpenAI SDK, Python',
      family: 'openai',
      code: `import os\nfrom openai import OpenAI\n\nclient = OpenAI(\n    base_url="${origin}/v1",\n    api_key=os.environ["${KEY_VARIABLE}"],  # a Switchyard client key\n)\n\n${openaiPython}\n`,
    },
    {
      id: 'openai-node',
      label: 'OpenAI SDK, Node',
      family: 'openai',
      code: `import OpenAI from "openai";\n\nconst client = new OpenAI({\n  baseURL: "${origin}/v1",\n  apiKey: process.env.${KEY_VARIABLE}, // a Switchyard client key\n});\n\n${openaiNode}\n`,
    },
    {
      id: 'anthropic-python',
      label: 'Anthropic SDK, Python',
      family: 'anthropic',
      code: `import os\nfrom anthropic import Anthropic\n\nclient = Anthropic(\n    base_url="${origin}",\n    api_key=os.environ["${KEY_VARIABLE}"],  # a Switchyard client key\n)\n\nmessage = client.messages.create(\n    model=${name},\n    max_tokens=1024,\n    messages=[{"role": "user", "content": "Hello"}],\n)\nprint(message.content[0].text)\n`,
    },
    {
      id: 'anthropic-node',
      label: 'Anthropic SDK, TypeScript',
      family: 'anthropic',
      code: `import Anthropic from "@anthropic-ai/sdk";\n\nconst client = new Anthropic({\n  baseURL: "${origin}",\n  apiKey: process.env.${KEY_VARIABLE}, // a Switchyard client key\n});\n\nconst message = await client.messages.create({\n  model: ${name},\n  max_tokens: 1024,\n  messages: [{ role: "user", content: "Hello" }],\n});\nconsole.log(message.content[0].text);\n`,
    },
    {
      id: 'google-python',
      label: 'Google Gen AI SDK, Python',
      family: 'gemini',
      code: `import os\nfrom google import genai\nfrom google.genai import types\n\nclient = genai.Client(\n    api_key=os.environ["${KEY_VARIABLE}"],  # a Switchyard client key\n    http_options=types.HttpOptions(base_url="${origin}"),\n)\n\nresponse = client.models.generate_content(\n    model=${name},\n    contents="Hello",\n)\nprint(response.text)\n`,
    },
    {
      id: 'google-node',
      label: 'Google Gen AI SDK, JavaScript',
      family: 'gemini',
      code: `import { GoogleGenAI } from "@google/genai";\n\nconst client = new GoogleGenAI({\n  apiKey: process.env.${KEY_VARIABLE}, // a Switchyard client key\n  httpOptions: { baseUrl: "${origin}" },\n});\n\nconst response = await client.models.generateContent({\n  model: ${name},\n  contents: "Hello",\n});\nconsole.log(response.text);\n`,
    },
  ];
}

/** Which SDK family speaks a protocol natively. */
export function snippetFamily(protocol) {
  if (protocol === 'anthropic') return 'anthropic';
  if (protocol === 'gemini') return 'gemini';
  return 'openai';
}

/** Names for HTTP statuses the browser no longer supplies (HTTP/2 has no reason phrase). */
const STATUS_NAMES = {
  200: 'OK',
  400: 'Bad Request',
  401: 'Unauthorized',
  403: 'Forbidden',
  404: 'Not Found',
  408: 'Request Timeout',
  409: 'Conflict',
  413: 'Payload Too Large',
  422: 'Unprocessable Content',
  429: 'Too Many Requests',
  499: 'Client Closed Request',
  500: 'Internal Server Error',
  502: 'Bad Gateway',
  503: 'Service Unavailable',
  504: 'Gateway Timeout',
};

export function statusName(status, fallback = '') {
  return STATUS_NAMES[status] ?? fallback;
}

/** What a WebSocket close code means on the gateway's Responses endpoint. */
export function closeCodeMeaning(code) {
  switch (code) {
    case 1000:
      return 'Closed normally.';
    case 1001:
      return 'The gateway is shutting down. Connect again once it is back.';
    case 1005:
      return 'Closed without a status code.';
    case 1006:
      return 'The connection ended without a close frame: the network dropped, or the gateway stopped.';
    case 1008:
      return 'The gateway closed the connection for a policy reason.';
    case 1009:
      return 'A message was larger than server.body_limit_mb allows.';
    case 1011:
      return 'The request failed upstream. The error frame above the close says why. Connect again to send another turn.';
    case 1012:
      return 'The upstream socket failed. Connect again and send the conversation anew.';
    default:
      return code >= 4000 ? 'An application-defined close code.' : 'An unusual close code.';
  }
}
