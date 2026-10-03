// About page: the reference data. Nothing here touches the DOM, so the
// module imports cleanly under Node (ui/tests/check.mjs).
//
// What is written here is a description of the gateway, so it must follow
// the gateway:
//   ENDPOINTS        docs/DESIGN.md section 9 and the banner at GET /
//   REASONING        crates/core/src/reasoning.rs (parse_model_suffix,
//                    normalize_depth) and each codec's write_reasoning
//   SHORTCUTS        every useHotkey call (js/shell/shell.js, the "/" of
//                    pages/requests/filters.js, pages/models/table.js and
//                    pages/logs.js, useSaveHotkey in pages/settings/common.js)
//                    and the key handlers of the palette and the shared
//                    components
//   MIT_LICENCE      LICENSE at the repository root

// ---------------------------------------------------------------------------
// Client API endpoints
// ---------------------------------------------------------------------------

/**
 * family   which vendor's API the route speaks ("Gateway" for the gateway's own)
 * paths    every path the row covers; the first one is what the copy button copies
 * ws       a WebSocket upgrade, not a plain request
 * open     answers without a client key
 */
export const ENDPOINTS = [
  { id: 'chat', method: 'POST', family: 'OpenAI', paths: ['/v1/chat/completions'], text: 'Chat Completions. Streams server-sent events with "stream": true.' },
  { id: 'responses', method: 'POST', family: 'OpenAI', paths: ['/v1/responses'], text: 'Responses API, streaming and not.' },
  { id: 'responses-ws', method: 'GET', family: 'OpenAI', ws: true, paths: ['/v1/responses'], text: 'Responses API over a WebSocket: one JSON message per turn in, one streaming event per frame out. Works with every model.' },
  { id: 'responses-tokens', method: 'POST', family: 'OpenAI', paths: ['/v1/responses/input_tokens'], text: 'Counts the input tokens of a Responses request.' },
  { id: 'completions', method: 'POST', family: 'OpenAI', paths: ['/v1/completions'], text: 'Legacy completions, served through chat.' },
  { id: 'models', method: 'GET', family: 'OpenAI', paths: ['/v1/models', '/v1/models/{id}'], text: 'The model list. OpenAI shape; Anthropic shape when the request carries an anthropic-version header.' },
  { id: 'realtime', method: 'GET', family: 'OpenAI', ws: true, paths: ['/v1/realtime?model={model}'], text: 'WebSocket relay to an OpenAI Realtime upstream.' },
  { id: 'proxy', method: 'POST', family: 'OpenAI', paths: ['/v1/embeddings', '/v1/images/generations', '/v1/moderations', '/v1/audio/speech'], text: 'Forwarded as JSON to the provider that serves the model.' },
  { id: 'messages', method: 'POST', family: 'Anthropic', paths: ['/v1/messages'], text: 'Messages API, streaming and not.' },
  { id: 'messages-tokens', method: 'POST', family: 'Anthropic', paths: ['/v1/messages/count_tokens'], text: 'Counts the input tokens of a Messages request.' },
  { id: 'gemini-generate', method: 'POST', family: 'Gemini', paths: ['/v1beta/models/{model}:generateContent'], text: 'Gemini generateContent.' },
  { id: 'gemini-stream', method: 'POST', family: 'Gemini', paths: ['/v1beta/models/{model}:streamGenerateContent'], text: 'Streaming. Server-sent events with ?alt=sse, one JSON array otherwise.' },
  { id: 'gemini-tokens', method: 'POST', family: 'Gemini', paths: ['/v1beta/models/{model}:countTokens'], text: 'Counts the tokens of a Gemini request.' },
  { id: 'gemini-models', method: 'GET', family: 'Gemini', paths: ['/v1beta/models', '/v1beta/models/{name}'], text: 'The model list in Gemini’s shape.' },
  { id: 'health', method: 'GET', family: 'Gateway', open: true, paths: ['/healthz'], text: 'Liveness check for load balancers and scripts. Also answers HEAD.' },
  { id: 'banner', method: 'GET', family: 'Gateway', open: true, paths: ['/'], text: 'Name, version and the endpoint list as JSON.' },
];

/**
 * The URL of an endpoint's first path on a gateway reached at `base`
 * (no trailing slash): ws:// or wss:// for the WebSocket routes.
 */
export function endpointUrl(endpoint, base) {
  const root = endpoint.ws ? String(base).replace(/^http/i, 'ws') : String(base);
  return `${root}${endpoint.paths[0]}`;
}

/** Where a client may put its key, in the order the gateway looks. */
export const KEY_PLACES = ['Authorization: Bearer', 'x-api-key', 'x-goog-api-key', '?key='];

// ---------------------------------------------------------------------------
// Reasoning suffix
// ---------------------------------------------------------------------------

/**
 * One row per kind of suffix. Each provider cell has the fields the gateway
 * writes into the upstream request (`code`) and the exceptions (`note`). A
 * cell is a list when the result depends on the model: one entry per case.
 *
 * Which case a model falls in comes from its `thinking` entry in the catalog
 * (crates/scheduler/catalog/models.json; GET /admin/api/catalog): a budget
 * range (min, max), named levels, zero_allowed, dynamic_allowed. A model
 * with both a range and levels keeps the form the caller used. Every cell
 * was checked against what the gateway sends upstream; check again when the
 * catalog gains a model family.
 *
 * model(auto) stays auto wherever the upstream can say it (normalize_depth):
 * no effort for the OpenAI family, adaptive thinking for Claude models with
 * levels, the dynamic budget for Gemini models that allow it. Only a model
 * with no such mode gets an explicit value.
 */
export const REASONING = [
  {
    suffix: 'model(high)',
    meaning: 'An effort level: minimal, low, medium, high, xhigh or max.',
    openai: {
      code: ['reasoning_effort: "high"', 'reasoning.effort: "high"'],
      note: 'Chat Completions and Responses. A level the model lacks becomes the nearest one it has.',
    },
    anthropic: {
      code: ['thinking.type: "adaptive"', 'output_config.effort: "high"'],
      note: 'Models that only take budgets, such as claude-sonnet-4-5, get thinking.type: "enabled" with budget_tokens: 24576.',
    },
    gemini: {
      code: ['thinkingLevel: "high"'],
      note: 'Budget models (2.5) and models the gateway does not know get thinkingBudget: 24576, clamped to the model’s range.',
    },
  },
  {
    suffix: 'model(16000)',
    meaning: 'A thinking budget in tokens.',
    openai: {
      code: ['reasoning_effort: "high"', 'reasoning.effort: "high"'],
      note: 'OpenAI has no budgets, so the budget is bucketed: 16000 falls in high.',
    },
    anthropic: {
      code: ['thinking.type: "enabled"', 'thinking.budget_tokens: 16000'],
      note: 'Kept below max_tokens. Models that only take levels, such as claude-opus-5, get adaptive thinking with effort high.',
    },
    gemini: {
      code: ['thinkingBudget: 16000'],
      note: 'Clamped to the model’s range. Gemini 3 models take a budget as well as a level, so it stays a budget.',
    },
  },
  {
    suffix: 'model(none)',
    meaning: 'Reasoning off. model(0) means the same.',
    openai: {
      code: ['reasoning_effort: "none"', 'reasoning.effort: "none"'],
      note: 'A model that cannot stop reasoning gets its lowest level: the GPT models in the catalog get low.',
    },
    anthropic: {
      code: ['thinking.type: "disabled"'],
      note: null,
    },
    gemini: {
      code: ['thinkingBudget: 0'],
      note: 'A model that cannot stop thinking gets its lowest level (Gemini 3: low or minimal) or its smallest budget (gemini-2.5-pro: 128).',
    },
  },
  {
    suffix: 'model(auto)',
    meaning: 'The provider decides. model(-1) means the same.',
    openai: {
      code: null,
      note: 'No effort is sent: on OpenAI’s APIs that is how to say auto, and the model’s own default applies. An effort in the request body is removed. The same for every model, known to the gateway or not.',
    },
    anthropic: [
      {
        code: ['thinking.type: "adaptive"'],
        note: 'Without an effort. Every Claude model that takes effort levels (4.6 and later, claude-fable-5 included) and models the gateway does not know.',
      },
      {
        code: ['thinking.type: "enabled"', 'thinking.budget_tokens: 63999'],
        note: 'Models that only take budgets, such as claude-sonnet-4-5: the middle of the budget range, 64512, lowered to stay below max_tokens (63999 under that model’s limit of 64000).',
      },
    ],
    gemini: {
      code: ['thinkingBudget: -1'],
      note: 'The dynamic budget: every Gemini model in the catalog allows it, and models the gateway does not know get it too. A model defined without it gets the middle of its range, or medium.',
    },
  },
];

/** Effort levels and the token budget each stands for (Effort::budget). */
export const EFFORT_BUDGETS = [
  { level: 'minimal', budget: 512 },
  { level: 'low', budget: 1024 },
  { level: 'medium', budget: 8192 },
  { level: 'high', budget: 24576 },
  { level: 'xhigh', budget: 32768 },
  { level: 'max', budget: 128000 },
];

/** The rules that hold for every suffix. Plain sentences, shown as a list. */
export const REASONING_RULES = [
  'The suffix is the text in the last pair of parentheses at the end of the model name. It is removed before the request goes upstream.',
  'A suffix wins over reasoning settings in the request body. Without a suffix, a request forwarded in its own protocol is left as the client wrote it.',
  'Nothing is rejected: a value the model cannot take is moved to the nearest one it can.',
  'For a model that is known not to reason, the reasoning fields are removed.',
  'A suffix the gateway does not recognise, such as model(ultra), is removed and changes nothing.',
  'Alias targets may carry a suffix too, which pins the effort for that target.',
  'The fields are written as paths into the upstream request body. Gemini’s sit under generationConfig.thinkingConfig.',
];

// ---------------------------------------------------------------------------
// Keyboard shortcuts
// ---------------------------------------------------------------------------

// A key is a combo string for hotkeyLabel ("mod+k") or one of these names,
// which the page draws as an arrow.
export const ARROWS = {
  up: { icon: 'arrow-up', label: 'Up arrow' },
  down: { icon: 'arrow-down', label: 'Down arrow' },
  left: { icon: 'arrow-left', label: 'Left arrow' },
  right: { icon: 'arrow-right', label: 'Right arrow' },
};

/**
 * keys     alternatives, each a list of keys pressed together:
 *          [['mod', 'k']] is one chord, [['up'], ['down']] is "up or down"
 * action   what it does
 * desktop  true when the shortcut does not exist on phones
 */
export const SHORTCUTS = [
  {
    title: 'Anywhere',
    items: [
      { keys: [['mod', 'k']], action: 'Open or close the command palette: jump to a page or run an action.' },
      { keys: [['mod', 'b']], action: 'Collapse or expand the sidebar.', desktop: true },
      { keys: [['escape']], action: 'Close the dialog, drawer, menu or palette on top.' },
      { keys: [['tab'], ['shift', 'tab']], action: 'Move between controls. Focus stays inside an open dialog.' },
    ],
  },
  {
    title: 'Pages',
    items: [
      { keys: [['/']], action: 'Put the cursor in the search box on Requests, Models and Logs.' },
      { keys: [['mod', 's']], action: 'Save the open Settings tab while it has unsaved changes.' },
    ],
  },
  {
    title: 'Command palette',
    items: [
      { keys: [['up'], ['down']], action: 'Move through the results.' },
      { keys: [['enter']], action: 'Run the selected command.' },
      { keys: [['home'], ['end']], action: 'First or last result, while the search box is empty.' },
    ],
  },
  {
    title: 'Tabs and segmented controls',
    items: [
      { keys: [['left'], ['right']], action: 'Select the previous or next option.' },
      { keys: [['home'], ['end']], action: 'Select the first or last option.' },
    ],
  },
  {
    title: 'Menus',
    items: [
      { keys: [['down'], ['up']], action: 'Open a menu from its button, then move through it.' },
      { keys: [['enter'], ['space']], action: 'Choose the highlighted item.' },
    ],
  },
  {
    title: 'Tables and charts',
    items: [
      { keys: [['enter'], ['space']], action: 'Open the focused row.' },
      { keys: [['left'], ['right']], action: 'Read a focused chart point by point.' },
      { keys: [['home'], ['end']], action: 'First or last point of a focused chart.' },
    ],
  },
  {
    title: 'Forms',
    items: [
      { keys: [['enter']], action: 'Submit the form. In a tag field, add the tag.' },
      { keys: [[',']], action: 'Add the tag being typed.' },
      { keys: [['backspace']], action: 'In an empty tag field, remove the last tag.' },
      { keys: [['up'], ['down']], action: 'Step a number field.' },
    ],
  },
];

// ---------------------------------------------------------------------------
// Licence
// ---------------------------------------------------------------------------

export const REPOSITORY_URL = 'https://github.com/krutftw/switchyard';
export const CLIPROXY_URL = 'https://github.com/router-for-me/CLIProxyAPI';

/** LICENSE at the repository root. The dashboard's files do not include it. */
export const MIT_LICENCE = `MIT License

Copyright (c) 2026 Switchyard contributors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
`;

/** Licence texts the gateway serves next to the dashboard. */
export const LICENCE_FILES = [
  { id: 'vendor', title: 'Third-party licence texts', path: 'vendor/LICENSES.txt' },
  { id: 'archivo', title: 'Archivo font licence', path: 'fonts/Archivo-OFL.txt' },
  { id: 'jetbrains', title: 'JetBrains Mono font licence', path: 'fonts/JetBrainsMono-OFL.txt' },
];
