# Switchyard — design

Switchyard is an LLM API gateway written in Rust. Clients talk to it in any of
four wire protocols — OpenAI Chat Completions, OpenAI Responses (HTTP and
WebSocket), Anthropic Messages, Google Gemini — and it routes each request to
a configured upstream provider, translating between protocols when the client
and the upstream speak different ones. It pools credentials with failover and
cooldowns, records usage, hot-reloads its config, and ships an embedded web
dashboard.

It is an original implementation inspired by CLIProxyAPI. Deliberately **out of
scope**: subscription OAuth logins, client impersonation, request "cloaking" or
fingerprint spoofing. Upstreams are reached with API keys or service accounts.

This document is the contract between crates. Where it says MUST, tests should
exist.

---------------------------------------------------------------------------

## 1. Workspace

```
crates/
  core/             switchyard-core             IR, stream model, Codec trait, config schema, reasoning, errors, SSE
  codec-chat/       switchyard-codec-chat       OpenAI Chat Completions codec
  codec-responses/  switchyard-codec-responses  OpenAI Responses codec
  codec-anthropic/  switchyard-codec-anthropic  Anthropic Messages codec
  codec-gemini/     switchyard-codec-gemini     Gemini generateContent codec
  codecs/           switchyard-codecs           registry of the four codecs + cross-protocol test matrix
  translate/        switchyard-translate        codec-agnostic pipeline pieces: reasoning application,
                                                reasoning store, payload rules, JSON path, stream transcoder
  upstream/         switchyard-upstream         HTTP/WebSocket transport to providers, auth, error
                                                classification, model discovery, mock provider
  scheduler/        switchyard-scheduler        model registry, credential pool, selection, cooldowns, catalog
  telemetry/        switchyard-telemetry        event bus, request records, usage aggregation, log capture
  config-store/     switchyard-config-store     load, validate, watch and rewrite switchyard.toml (comment-preserving)
  gateway/          switchyard-gateway          the engine: the request pipeline
  server/           switchyard-server           axum routes for the client API (HTTP, SSE, WebSocket)
  admin/            switchyard-admin            admin REST API, live event WebSocket, embedded dashboard
  switchyard/       switchyard (bin)            CLI: serve, init, check, import
ui/                 dashboard sources (no build step; embedded into the admin crate)
```

Dependency direction (an arrow means "depends on"):

```
switchyard ─► server ─┐
           └► admin ──┴► gateway ─► translate, codecs, upstream, scheduler, telemetry ─► core
codecs ─► codec-chat, codec-responses, codec-anthropic, codec-gemini ─► core
```

Leaf crates (`codec-*`, `translate`, `upstream`, `scheduler`, `telemetry`, `config-store`)
depend on `core` only (plus third-party crates) and MUST NOT depend on each
other.

### Rules for everyone

* Rust 2024 edition, stable toolchain. `cargo clippy -p <crate> --all-targets -- -D warnings`
  and `cargo test -p <crate>` must pass for your crate.
* On Windows, use a short `CARGO_TARGET_DIR` when a deep checkout path exceeds
  linker path limits. Coordinate builds that share the same target directory.
* Only edit files inside the crate (or `ui/` sub-tree) you own. Never edit the
  root `Cargo.toml`, `crates/core`, or another crate. If you need a dependency
  that is not in `[workspace.dependencies]`, declare it with an explicit
  version in your own crate's `Cargo.toml`. If you believe `core` has a bug or
  is missing something, work around it locally and report it in your summary.
* Third-party crate versions are recent (reqwest 0.13, axum 0.8, rand 0.9,
  tokio-tungstenite 0.30 …). Do not rely on memory for their APIs: read the
  sources under `~/.cargo/registry/src/` when unsure.
* JSON is `serde_json::Value` built with `preserve_order`; key order of
  bodies you forward is preserved. Never log or return API keys; use
  `switchyard_core::util::mask_secret`.
* No `unsafe`. No `unwrap()`/`expect()` on data that comes from the network or
  the config file. Panics are bugs.
* Comments explain *why*, public items get rustdoc. No TODOs left behind: if
  something is intentionally unimplemented, say so in rustdoc and in your
  summary.
* Tests must be deterministic and offline. Anything that needs an HTTP peer
  starts a local axum server on `127.0.0.1:0`.
* Write original code. The reference notes describe behaviour; do not
  transliterate Go.

---------------------------------------------------------------------------

## 2. Core concepts (see `crates/core/src`)

* `Protocol` — `openai-chat`, `openai-responses`, `anthropic`, `gemini`.
* `ir::Request` / `ir::Response` — canonical model. Tool results are parts of
  **user** messages. Leading system text lives in `Request::system`.
* `stream::StreamEvent` — canonical stream; strict sequence contract in the
  module docs, checkable with `stream::validate_sequence`.
* `Codec` — one per protocol. Client side (`decode_request`,
  `encode_response`, `stream_encoder`, `encode_error`, model listings) and
  upstream side (`encode_request`, `decode_response`, `stream_decoder`,
  `decode_error`), plus raw-body helpers for same-protocol passthrough.
* `reasoning` — `ReasoningConfig { depth, summary }`, `normalize_depth`
  (never rejects; clamps), `parse_model_suffix` (`model(high)`, `model(8192)`,
  `model(none)`, `model(auto)`).
* `sig` — opaque provider signatures crossing protocols are wrapped as
  `sy1.<tag>.<blob>`; codecs MUST use `sig::encode_for_client` when writing a
  `Signature` into a client-facing payload and `sig::decode_from_client` when
  reading one from a client request, and MUST drop signatures whose
  `Signature::valid_for(target)` is false when encoding an upstream request.
  Exception: Gemini's signature field is typed `bytes`, so the Gemini codec
  base64-armours wrapped signatures it hands to Gemini clients and strips
  foreign ones in `prepare_passthrough`, which the gateway therefore always
  runs on forwarded bodies.
* `Usage` — disjoint buckets; codecs convert vendor conventions at the edge.
* `config::Config` — the TOML schema, validation, secret references.

### Passthrough vs translation

For every attempt the gateway knows the client protocol `C` and picks an
upstream protocol `U` (= `C` if the provider supports it, else the provider's
first protocol).

* `C == U` and the body contains no wrapped signature (`sig::contains_wrapped`)
  → **passthrough**: the client's JSON is forwarded with only the model name
  replaced, reasoning rewritten if a model suffix demands it, payload rules
  applied, and `Codec::prepare_passthrough` run. Responses are forwarded
  as-is with the model name rewritten back to what the client asked for. A
  stream decoder runs on the side to collect usage.
* otherwise → **translation**: `C.decode_request` → fit reasoning → restore
  stored reasoning → `U.encode_request` → payload rules → upstream →
  `U.decode_response`/`U.stream_decoder` → `C.encode_response`/`C.stream_encoder`.

---------------------------------------------------------------------------

## 3. Codecs (`codec-chat`, `codec-responses`, `codec-anthropic`, `codec-gemini`)

Each crate exposes one unit struct implementing `Codec` (`ChatCodec`,
`ResponsesCodec`, `AnthropicCodec`, `GeminiCodec`) and nothing else public
except helpers worth testing.

General requirements:

* **Tolerant decoding.** Unknown fields are ignored; content given as a string
  or as a part array are both accepted; numbers may arrive as floats. A
  decoder only errors when the request cannot be understood at all (missing
  `messages`/`input`/`contents`, wrong JSON type for a required field).
* **Faithful encoding.** Output must be valid for the real vendor API: field
  names, required fields, nesting, event order.
* **Round-trip property.** For any IR request `r` expressible in protocol P:
  `P.decode_request(P.encode_request(r))` preserves messages, tools, tool
  choice, sampling, reasoning depth. For any IR response `x`:
  `P.decode_response(P.encode_response(x))` preserves parts, finish reason and
  usage. For any valid event sequence `s`: feeding `P.stream_encoder(s)`
  output into `P.stream_decoder` yields a sequence that accumulates
  (`stream::Accumulator`) to the same `Response`. Test all three.
* **Stream decoders** uphold the sequence contract for any input, including
  truncated streams (`finish()` closes blocks and emits
  `Finish { reason: Error }` if no terminal event was seen), upstream error
  events (→ `StreamEvent::Error`), and unknown event types (skipped).
* **Stream encoders** produce a protocol-valid stream for any contract-valid
  sequence, including one ending in `StreamEvent::Error` (render the
  protocol's in-stream error shape) and blocks the protocol cannot express
  (drop them).
* **Errors.** `encode_error` renders `ApiError` in the vendor envelope with the
  vendor's type strings; `decode_error` parses vendor error bodies (and plain
  text / HTML bodies) into `UpstreamErrorInfo`, including retry hints found in
  the body.
* **Reasoning.** `read_reasoning` reads depth + summary intent from a raw body;
  `write_reasoning` writes a `Fitted` depth into a raw body (used both by
  passthrough and, internally, by `encode_request`).
  `encode_request` writes `request.reasoning` as given (already fitted) and
  must keep the body valid (Anthropic: `budget_tokens < max_tokens`, drop
  thinking when tool choice is forced, sampling restrictions).
* **Model listings** in the vendor's shape (`encode_models`, `encode_model`).
* **Token counting** where the vendor has an endpoint (Anthropic
  `count_tokens`, Gemini `countTokens`, OpenAI Responses `input_tokens`).

Protocol specifics, mapping tables and edge cases are in the reference notes
(`06`–`09`, `12`, `15`, `99`). The notes describe pairwise translators; here
each protocol maps to and from the IR instead. When the notes show a pairwise
rule ("Claude `tool_use` → OpenAI `tool_calls`"), split it at the IR.

---------------------------------------------------------------------------

## 4. `translate`

Codec-agnostic building blocks used by the gateway. Everything takes
`&dyn Codec`; there is no dependency on the codec crates.

* `jsonpath` — `get`, `set`, `remove`, `exists` on `serde_json::Value` with
  dotted paths (`a.b.0.c`, `\.` escapes a dot). `set` creates intermediate
  objects.
* `payload` — applies `config::PayloadConfig` to an upstream body:
  `default` (only when the path is absent from the *client's original body*
  and from the upstream body), then `override`, then `filter`. A rule matches
  when any of its `models` patterns matches the upstream model id or the
  client-requested name (suffix stripped), and its optional `protocol` /
  `provider` constraints hold.
* `thinking` — given the client's `ReasoningConfig` (from the body), an
  optional suffix depth, the target `ModelThinking` and the target protocol,
  computes the `Fitted` depth: suffix wins over body; nothing requested →
  leave the body alone; passthrough without suffix → leave alone.
* `reasoning_store` — bounded, TTL'd in-memory store that remembers reasoning
  parts and tool-call signatures by tool-call id from upstream responses, and
  re-attaches them to later requests whose assistant turns lost them (clients
  speaking a protocol with no signature slot, e.g. Chat Completions). Only
  restores blobs valid for the target family.
* `transcode` — `Transcoder` that owns a `StreamDecoder` (upstream protocol),
  an optional `StreamEncoder` (client protocol) and an `Accumulator`:
  `push(SseEvent) -> Vec<SseEvent>` and `finish() -> Vec<SseEvent>`, exposing
  the accumulated `Response`/`Usage`, whether a terminal event was seen, and
  the first error. In passthrough mode it forwards the original events
  (rewriting the model name) while still decoding on the side.
* `convert` — one-shot helpers: `translate_request`, `translate_response`,
  `translate_error`.

---------------------------------------------------------------------------

## 5. `upstream`

Transport to providers. No routing decisions.

* One shared rustls `ClientConfig` (ring provider; roots = webpki-roots ∪
  native certs) used by reqwest and tokio-tungstenite. HTTP clients are cached
  per proxy setting (`config::parse_proxy`: inherit → environment, `direct`,
  URL). Precedence: credential proxy > provider proxy > `upstream.proxy`.
* `Target` — everything needed to reach one credential: provider kind, base
  URL, upstream protocol, upstream model id, secret (API key or service
  account), extra headers, proxy, project/location, quirks.
* URL + auth per kind:
  * `openai` / `openai-compat`: `{base}/chat/completions`, `{base}/responses`,
    `{base}/responses/input_tokens`, `{base}/models`, raw `{base}/{path}`;
    `Authorization: Bearer`. Keyless allowed for `openai-compat`.
  * `anthropic`: `{base}/v1/messages`, `/v1/messages/count_tokens`,
    `/v1/models`; `x-api-key`, `anthropic-version: 2023-06-01`; the client's
    `anthropic-beta` header is forwarded.
  * `gemini`: `{base}/v1beta/models/{model}:generateContent`,
    `:streamGenerateContent?alt=sse`, `:countTokens`, `/v1beta/models`;
    `x-goog-api-key`.
  * `vertex`: service account → OAuth2 access token (RS256 JWT signed with
    `ring`, cached until shortly before expiry), or API key. Gemini models:
    `/v1/projects/{p}/locations/{l}/publishers/google/models/{m}:…`
    (`global` location uses the bare host, regions use `{l}-aiplatform…`).
  * `mock`: no network; see below.
* `send(target, op, body, client_headers) -> Result<UpstreamResponse, UpstreamError>`
  where the response is either a full body or a byte stream. Non-2xx responses
  become `UpstreamError` with `FailureClass` (status + body-aware: quota vs
  rate limit, request faults), `retry_after_ms` (header or body hint), raw
  body (truncated) and content type.
* Model discovery: `list_models(target) -> Vec<ModelInfo>` per kind.
* WebSocket: `connect_ws(target, path, extra_headers)` for the Responses
  WebSocket endpoint and the Realtime API (`wss://…/v1/realtime?model=`).
* **Mock provider** (`mock://`): produces IR `StreamEvent`s locally so the
  dashboard playground and the test-suite work without keys. Model names
  select behaviour: `mock-echo` (repeats the last user text), `mock-lorem`
  (streams paragraphs), `mock-think` (reasoning then answer), `mock-tools`
  (calls the first offered tool, then answers once a tool result is present),
  `mock-slow` (delays), `mock-error-429` / `mock-error-500` /
  `mock-error-401` (fail). Reports plausible usage.

---------------------------------------------------------------------------

## 6. `scheduler`

Decides *which credential serves which model*.

* **Catalog** — built-in metadata for well-known models (context window, max
  output, thinking support) keyed by vendor id, embedded as JSON.
* **Registry** — built from `Config` (+ discovered model lists): maps each
  client-facing model name to the provider entries that serve it. A provider
  model is visible as `alias-or-id`, and when the provider has a prefix as
  `prefix/name` (bare name too unless `routing.force_model_prefix`).
  `exclude` patterns hide models. Global `aliases` map a name to an ordered
  list of targets (each may carry a reasoning suffix that pins the depth).
  Lookup is exact, then case-insensitive.
* **Credentials** — one per `api_keys` entry / `credentials` entry, with a
  stable id (hash of provider name + key material, never the key itself),
  label, weight, priority, runtime state.
* **Selection** — highest priority tier first; within a tier the configured
  strategy (`round-robin`, `fill-first`, `weighted`, `least-latency`);
  credentials that are disabled, cooling down (for this model or entirely) or
  already tried in this request are skipped. Session affinity pins a session
  key to the credential that last served it while it stays healthy.
* **Outcome reporting** — success resets the failure streak and updates the
  latency EWMA; failures start a cooldown by `FailureClass`
  (`config::CooldownConfig`; upstream `Retry-After` wins; rate-limit backoff
  doubles per consecutive failure up to the cap; 404 cools only that model on
  that credential; request faults cool nothing).
* **Errors** — unknown model; no credential configured; all cooling down (with
  the soonest recovery time, surfaced as `Retry-After`).
* **Introspection** for the admin API: credential states, per-credential
  counters, model table; manual cooldown reset and runtime enable/disable.
* Rebuilding from a new `Config` keeps runtime state of credentials whose id
  did not change.

---------------------------------------------------------------------------

## 7. `telemetry`

* **Event bus** (`tokio::sync::broadcast`): request started/finished, log
  line, credential state change, config reloaded. Slow subscribers lose
  events, never block the gateway.
* **Request records** — one per client request: id, timestamps, duration,
  time-to-first-byte, client key (name + id, never the key), client protocol,
  endpoint, transport (http / sse / websocket), requested model, upstream
  model, provider, credential (id + label), upstream protocol, mode
  (passthrough / translated), status, error, usage, estimated cost, reasoning
  label, attempts. A bounded ring keeps recent records in memory.
* **Usage aggregation** — per-minute buckets for the last 24 h and per-hour
  buckets for the retention window, each broken down by model, provider and
  client key; totals; latency percentiles (hdrhistogram). Query functions for
  summary / timeseries / breakdown.
* **Persistence** — append-only JSONL of request records per UTC day under
  `<data_dir>/usage/`, reloaded on start, pruned by retention.
* **Body capture** — when `logging.request_log` is `errors` or `all`, stores
  the client request, upstream request, upstream response and client response
  bodies (truncated, secrets redacted) per request id under
  `<data_dir>/requests/`.
* **Log capture** — a `tracing` layer that keeps the last N application log
  lines in a ring, publishes them on the bus, and optionally writes rotating
  files with a total-size cap.

---------------------------------------------------------------------------

## 8. `gateway`

The engine. Owns everything above and exposes a small API to `server` and
`admin`.

### Config store

Loads `switchyard.toml`, validates, publishes `Arc<Config>` (arc-swap).
Watches the file (notify, debounced ~300 ms) and applies valid changes live;
an invalid file is rejected with the issues logged and the old config kept.
Admin edits go through `update(|&mut Config|)`: validate → write the file
**preserving comments and formatting** (toml_edit: merge the new value tree
into the existing document, touching only what changed) → apply. An edit may
also name settings it put back to their default (`update_unsetting`, used
for `null` in `PATCH /settings`): those keys leave the file — a table left
empty loses its header — so the default applies rather than a copy of it.
Tables are read in file order (toml `preserve_order`), and the configuration
an edit leaves live is read back from the text it wrote, so maps such as a
payload rule's `set` list their fields as the file does. An edit is
refused while the file on disk holds an invalid manual edit (it would be
overwritten); replacing the whole text is the way out. Applying a config
rebuilds the scheduler once (keeping credential state), checks Vertex
service-account files (a missing or invalid one makes its credential
unusable), re-runs model discovery in the background — only for the
providers the change concerns; the state of each provider's discovery is
kept — and updates log level, rate limits, etc. Applied and refused
configurations are announced on the event bus in the order they happened.
Listener address, TLS and data dir need a restart (reported as such).

### Pipeline (one client request)

1. Parse body as JSON → `Codec::request_meta` (model, stream). Errors are
   rendered in the client protocol's envelope.
2. Split the reasoning suffix off the model name.
3. Resolve the model to its registered name and candidate routes (alias
   targets in order).
4. Revalidate the client identity against the current key table; check the
   registered model or alias name against its current allow-list, then the
   rate limit (requests/minute). Restricted keys cannot use upstream
   alternative-model selectors (`models`, `route`, `fallbacks`,
   `context_window_fallbacks`, `preset`).
5. Attempt loop, at most `routing.max_attempts` upstream calls:
   pick a credential (excluding ones already tried) → build the upstream body
   (passthrough or translation, §2) → payload rules → send.
   * `FailureClass::Request` → stop, return the error (no failover, no
     cooldown).
   * any other failure → report to the scheduler, try the next credential.
   * streaming: the first upstream event must arrive before anything is sent
     to the client; a failure before that (including an in-stream error
     event) counts as a failed attempt and is retried, up to
     `streaming.bootstrap_retries` extra times. After the first event there
     is no retry.
   * when every candidate is cooling down and the soonest recovery is within
     `routing.max_wait_secs`, wait and retry; otherwise fail with 429/503 and
     `Retry-After`.
6. Final failure: same-protocol → forward the upstream's own error body and
   status; otherwise convert (`UpstreamError::to_api_error`) and render in the
   client protocol. Upstream auth failures are reported as 502 (the client's
   key is fine).
7. Success, non-stream: passthrough forwards the body (model rewritten);
   translation re-encodes. Usage is extracted either way.
8. Success, stream: a task pumps upstream bytes → `SseParser` → `Transcoder`
   → channel → client. Client disconnect cancels the upstream call. No bytes
   for `streaming.idle_timeout_secs` aborts with an in-stream error. A stream
   that ends without a terminal event is finished by the transcoder.
9. Always: report the outcome to the scheduler, remember reasoning blobs,
   publish a request record (with every attempt), capture bodies if enabled.

Also: token counting (native endpoint when the upstream protocol has one,
translated when needed, local estimate ≈ chars/4 as last resort); model
listings per protocol filtered by the client key's allow-list; raw proxying of
OpenAI-style side endpoints (embeddings, images, audio, moderations) to an
`openai`/`openai-compat` provider chosen by the body's `model`; client
authentication.

---------------------------------------------------------------------------

## 9. `server` — client API

| Method | Path | Notes |
|---|---|---|
| GET | `/` | JSON banner (name, version, endpoint list) |
| GET, HEAD | `/healthz` | `{"status":"ok"}` |
| GET | `/v1/models`, `/v1/models/{id}` | OpenAI shape; Anthropic shape when the request has an `anthropic-version` header |
| POST | `/v1/chat/completions` | |
| POST | `/v1/completions` | legacy shim over chat |
| POST | `/v1/responses` | |
| GET | `/v1/responses` | WebSocket upgrade (§10) |
| POST | `/v1/responses/input_tokens` | token count |
| POST | `/v1/messages` | |
| POST | `/v1/messages/count_tokens` | |
| GET | `/v1beta/models`, `/v1beta/models/{name}` | Gemini shape |
| POST | `/v1beta/models/{model}:{method}` | `generateContent`, `streamGenerateContent` (SSE with `alt=sse`, JSON array otherwise), `countTokens`; unknown method → 404 |
| GET | `/v1/realtime` | WebSocket relay to an OpenAI Realtime upstream (§10) |
| POST | `/v1/embeddings`, `/v1/images/generations`, `/v1/moderations`, `/v1/audio/speech` | raw JSON proxy by `model` |

* **Auth**: key from `Authorization: Bearer`, else the raw `Authorization`
  value, `x-api-key`, `x-goog-api-key`, query `key`; first *matching*
  candidate wins. Missing/invalid → 401 in the route's protocol envelope.
  `auth.required = false` admits anonymous clients. Every gateway operation
  rechecks issued identities against current key settings, including
  revocation and changed model restrictions or rate limits.
* **CORS** (when `server.cors`): `*` origin, any header, preflight 204 before
  auth. Requests that perform work still require a key when sent by another
  origin's web page, even with CORS enabled. Anonymous requests require a
  trusted Host and browser Origin: the configured listener name, localhost
  or loopback IP; IP literals are also allowed for wildcard listeners.
  Unconfigured DNS names cannot gain anonymous access through a same-origin
  browser verdict. Authenticated proxy hostnames remain supported. With CORS
  off, foreign browser origins are rejected for authenticated requests too.
* **Bodies**: limit `server.body_limit_mb`; gzip / br / zstd request bodies
  are decoded. Raw JSON side endpoints serialize the same parsed body used
  to authorize its model, including requests labeled text/plain.
* **Headers out**: `x-request-id` on every response; `Retry-After` from
  errors; when `upstream.passthrough_headers`, the upstream's rate-limit and
  request-id headers.
* **SSE**: `text/event-stream`, `cache-control: no-cache`,
  `x-accel-buffering: no`; flush per event; `: keep-alive` comment after
  `streaming.keepalive_secs` of silence.
* **Graceful shutdown** on Ctrl-C / SIGTERM: stop accepting, let in-flight
  requests finish (bounded wait).
* Optional TLS (`server.tls`).

---------------------------------------------------------------------------

## 10. WebSockets

### Tickets — `POST /v1/ws-ticket`

A browser cannot set headers on a WebSocket, and a key in the URL (`?key=`)
ends up in proxy logs, history and the console. So a client may buy a
ticket with its key instead and open the socket with `?ticket=<ticket>`:

* `POST /v1/ws-ticket`, authenticated like any client request
  (`Authorization: Bearer`, `x-api-key`, `x-goog-api-key`, `?key=`), any body
  ignored → `201 {"ticket":"<43 url-safe chars>","expires_in":30}`,
  `cache-control: no-store`. Wrong or missing key → the usual 401.
* A ticket is accepted as `?ticket=` on every WebSocket upgrade route
  (`GET /v1/responses`, `GET /v1/realtime`) and nowhere else; it is looked
  at only when no key on the request matched. It is good for **one**
  request (any request presenting it uses it up, whatever comes of it) within
  30 seconds. Unknown, expired or used → the same 401 `invalid API key` as a
  wrong key.
* It stands for the key that bought it, as configured when it is used: key
  name and id on the records, model allow-list, rate limit (the key's own
  window). A key disabled or removed in between takes its tickets with it.
* Minting is not a generation request: no request record, no usage, no
  rate-limit hit. Each key has at most 32 tickets outstanding (minting more
  drops its oldest); tickets live in memory only.
* With `auth.required = false` minting works without a key: the ticket is
  for the anonymous client, and stops working if keys become required.
* Tickets are credentials: never logged, never passed upstream (the relay
  drops `ticket` from the query like `key`), never on a record.

### Responses over WebSocket — `GET /v1/responses`

Same auth as HTTP (or a ticket, above), on the upgrade request. Each client
message is one JSON object; each server message is one Responses streaming
event as a text frame (no SSE framing, no `[DONE]`). Access is checked between
messages and every second while idle or streaming. Removing/disabling a key,
changing its model allow-list or rate limit, or requiring keys after an
anonymous connection opened closes it with 1008 and cancels its active turn.

Client messages: `response.create` (a Responses request body plus `type`),
`response.append` (legacy; same as a follow-up create). One turn at a time;
messages received during a turn are processed afterwards in order.

The gateway keeps a per-connection transcript so that stateless upstreams can
serve incremental turns:

* first `response.create` needs `model`; `input` defaults to `[]` and must be
  an array; later messages inherit `model` when absent;
* a continuation — `previous_response_id` naming the latest response of its
  lane, or `response.append` — gets upstream input = previous input ++
  previous response output ++ new input, de-duplicating items by `id` and tool
  calls by `call_id`, and inherits `instructions`;
* a `response.create` **without** `previous_response_id` is a request of its
  own, as in the vendor protocol: it starts a fresh transcript and inherits
  nothing but the model;
* `previous_response_id` that is not the latest response on the connection
  (no history, or an older response) → in-band error status 409 code
  `previous_response_not_found` (connection stays open);
* `"generate": false` (prewarm) is answered locally with
  `response.created` + `response.completed` (empty output, zero usage,
  synthetic id) and its input becomes the transcript root;
* `type`, `generate`, `previous_response_id` never reach a stateless upstream;
  `stream` is forced true;
* orphaned tool calls / outputs in the reconstructed input are repaired
  (dropped) so the upstream does not reject the transcript.

Validation errors are sent as
`{"type":"error","status":<n>,"error":{"message","type","code"?,"param"?}}`
and keep the connection open, as do turns the pipeline rejects as the
request's own fault (400/403/404/409/413/422). Other failed turns send the
error frame and then close (1011). The error frame of a turn the pipeline
ran — refused, or an `error` event in the middle of its stream — also
carries `"request_id"`, the id of the turn's request record (what an HTTP
request gets as `x-request-id`); frames for messages that never became a
request (validation errors, a full queue) have none. A `429` puts
`retry-after` into `error.headers`. Ping every `streaming.keepalive_secs`; a
client that stays silent through two pings during a turn, or for ten minutes
between turns, is dropped. Message size limit = `server.body_limit_mb`.

Any model can be used over this endpoint: turns run through the normal
pipeline with client protocol `openai-responses`, so the upstream is always
reached over HTTP streaming whatever protocol it speaks.

### Realtime relay — `GET /v1/realtime?model=…`

Authenticates the client (headers, `?key=`, an
`openai-insecure-api-key.<key>` subprotocol, or a ticket), picks an
`openai`-kind credential for the model, opens
`wss://<base>/realtime?model=<upstream model>` with the credential, and
relays text/binary/ping/pong/close frames both ways until either side closes.
Access changes close both sides with 1008, checked between messages and on a
one-second timer. Upstream error frames have decoded string values scrubbed
of the upstream credential before forwarding, including JSON-escaped values;
ordinary content remains unchanged. Records one request record per session
(duration, bytes).

### Admin events — `GET /admin/api/ws?ticket=…` (§11)

---------------------------------------------------------------------------

## 11. `admin` — admin API and dashboard

Base path `/admin/api`. The dashboard is served at `/admin/` (static files
from `ui/`, embedded with rust-embed; `/admin` redirects).

**Auth.** `Authorization: Bearer <secret>` where the secret is
`SWITCHYARD_ADMIN_SECRET` or `admin.secret`. No secret configured → every
admin route is 404. Non-loopback peers are refused (403) unless
`admin.allow_remote` (or the environment variable
`SWITCHYARD_ADMIN_ALLOW_REMOTE` is `1`/`true`, which the container image sets). Five consecutive bad secrets from one IP → 30 min
lockout (429 with `Retry-After`). Constant-time comparison. Browsers cannot
set headers on WebSockets, so `POST /admin/api/ws-ticket` returns a
single-use ticket valid 30 s.

**Secrets in responses** are masked (`mask_secret`); secret references
(`env:NAME`) are shown as written. In update payloads, a secret field equal to
its masked form or empty means "keep the current value". One caveat: short
keys (and some long ones) share a mask, and a mask alone cannot say which of
several such keys it stands for. Sending back as many equal masks as there
are stored keys keeps them all in their stored order; an edit that is
ambiguous — fewer equal masks than stored keys, i.e. one of them was
deleted — is refused with `422` rather than guessed (`AMBIGUOUS_MASK` in
`config-store`: the issue on `api_keys[j]` / `credentials[j].api_key` says
the value "is masked like several stored secrets that cannot be told
apart"; error bodies carry no separate code), and reordering keys with
identical masks cannot be expressed at all (send the keys in full, or give
them labels). A
credential without a key is sent and shown as `"api_key": null`, so a
provider entry round-trips exactly.

All bodies are JSON. Errors: `{"error":{"message":"…","issues":[{"path","message"}]?}}`.
A repeated query parameter, or a query value a route refuses (`since` and
`before` of `GET /requests`), is a `400` whose issue path is the
parameter's name; other unreadable query values fall back to their
defaults.

```
GET    /status
POST   /login                         validates the secret → {"ok":true}
POST   /ws-ticket                     → {"ticket","expires_in"}
GET    /ws?ticket=                    live events (see below)

GET    /config                        → {"config":<Config, secrets masked>,"path","restart_required":[…]}
GET    /config/raw                    → {"text","path","modified_at"}
PUT    /config/raw                    {"text"} → validate, persist, apply
POST   /config/validate               {"text"} → {"ok","issues"}
PATCH  /settings                      JSON merge-patch over server/admin/auth.required/routing/streaming/upstream/logging/usage
POST   /reload                        re-read the file from disk

GET    /providers                     list with credential runtime state
POST   /providers                     create (ProviderConfig)
GET    /providers/{name}
PUT    /providers/{name}              replace
DELETE /providers/{name}
POST   /providers/{name}/test         {"model"?} → {"ok","latency_ms","status","model","credential","error"?}
POST   /providers/{name}/discover     → {"models":[ModelInfo]}
POST   /credentials/{id}/reset        clear cooldowns
POST   /credentials/{id}/enable | /disable

GET    /models                        client-facing model table with routes and availability
                                      (aliases: `shadows_model` when they hide a served model)
GET    /catalog                       built-in model catalog
GET    /aliases   PUT /aliases        [AliasConfig]
GET    /payload   PUT /payload        PayloadConfig
GET    /pricing   PUT /pricing        [PriceConfig]

GET    /keys                          client keys (masked) with usage
POST   /keys                          {"name","models"?,"rate_limit_rpm"?,"key"?} → 201 {"id","key","is_reference"}
                                      (full key shown once)
PATCH  /keys/{id}                     {"name"?,"enabled"?,"models"?,"rate_limit_rpm"?}
DELETE /keys/{id}
POST   /keys/{id}/reveal              → {"key","is_reference"}

GET    /usage/summary?range=1h|24h|7d|30d
GET    /usage/timeseries?range=&bucket=&group_by=model|provider|key
GET    /requests?limit=&before=&since=&model=&client_model=&provider=&key=&status=&q=
                                      client_model: the by_model name; since: started_at >= unix ms
GET    /requests/{id}                 record + captured bodies when available
DELETE /usage                         clear statistics

GET    /logs?limit=&level=&q=&target=&before=

POST   /playground                    {"protocol","body","model"?,"stream"?} → runs through the pipeline
                                      as the built-in "dashboard" client; returns the protocol's JSON or SSE
```

`GET /ws` pushes JSON text frames `{"type":…,"data":…}`:
`hello`, `request.started`, `request.finished`, `log`, `credential`,
`config.reloaded`, and `stats` (once a second: in-flight requests, active
streams, requests/tokens in the last minute, latency percentiles). The client
may send `{"type":"subscribe","topics":[…]}` to narrow what it receives.

---------------------------------------------------------------------------

## 12. Dashboard (`ui/`)

No build step: ES modules + Preact/htm (vendored single file) + hand-written
CSS, embedded into the binary. No external network requests at runtime (fonts
and icons are local). Works at phone width. Light and dark themes.

Pages: Overview (live), Requests (live table + detail drawer), Providers,
Models (routes + aliases), API keys, Usage (charts, cost), Playground
(any protocol, streaming, raw events), Logs (live tail), Settings (forms +
raw TOML editor with validation), About.

---------------------------------------------------------------------------

## 13. `switchyard` binary

```
switchyard [serve] [--config <path>] [--host <h>] [--port <p>]
switchyard init [--config <path>] [--force]      write a starter config (random admin secret + client key)
switchyard check [--config <path>]               validate and print issues
switchyard import-cliproxy <config.yaml> [-o <path>]   convert the API-key sections of a CLIProxyAPI config
switchyard version
```

Config lookup: `--config`, `SWITCHYARD_CONFIG`, `./switchyard.toml`. If no
file exists `serve` creates one (as `init` does) and prints the dashboard URL
and configuration path. It shows the admin secret and client key once only
when stderr is a terminal; redirected startup logs tell the operator to read
them from the file. The explicit `init` command still prints its new secrets.
When that first run has `--host`
or `--port`, the file says so at `[server]` (the flag overrides the values
below it, which apply without it) and its `[admin]` comment names the
dashboard URL of that run. Logs go to stderr (pretty when a
terminal, JSON otherwise).

The CLIProxyAPI importer leaves out source secrets written as `env:NAME` or
`${NAME}` and lists them in its not-imported report. These are literal text
in the source format but environment lookups in Switchyard; importing must
not silently grant a foreign file access to the gateway's environment.
An operator can add an intentional reference after reviewing the output.
