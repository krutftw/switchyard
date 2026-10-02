# `switchyard-gateway` — public API

The engine behind the client API (`server`) and the admin API (`admin`). One
`Gateway` owns the configuration store, the scheduler, the upstream transport,
telemetry and the reasoning store; everything below is a method on it or a type
it takes or returns. All of it is re-exported from the crate root.

```rust
use switchyard_gateway::{
    ClientIdentity, ClientRequest, DiscoveryState, DiscoveryStatus, FullReply, Gateway,
    GatewayOptions, PresentedCredentials, ProviderTest, RawRequest, Reply, StartError,
    StreamReply, UpstreamWsSession, WsEnd, WsOpenRequest, WsOutcome,
    // conveniences re-exported from dependencies:
    ConfigStore, Transport, UpstreamWebSocket, WsMessage,
};
```

## Lifecycle

### `GatewayOptions { config_path: PathBuf, watch_config: bool }`

How to start. `GatewayOptions::new(path)` watches the file (`watch_config =
true`); `.watch(false)` turns the file watcher off (edits made through the
config store are still applied). `Default` is `./switchyard.toml`, watched.

### `Gateway::start(options) -> Result<Gateway, StartError>` (async)

Loads and validates the configuration (the file must exist), builds telemetry
(data directory = `server.data_dir` resolved against the config file's
directory; usage history is loaded), the scheduler, the upstream client and
the reasoning store, registers the mock providers' models (all of them in one
go), checks the service-account files (see Configuration changes), and spawns
the background tasks: config watcher, the task that applies configuration
changes, model discovery, telemetry writers and the hourly prune. Needs a
tokio runtime. `Gateway` is `Clone + Send + Sync + 'static`; clones share
everything.

### `StartError`

`Config(ConfigStoreError)` — the file is missing, unreadable or invalid (the
message lists every issue); `Upstream(String)` — the outbound TLS stack could
not be set up.

### `Gateway::shutdown(&self)` (async)

Stops the background tasks and flushes telemetry (usage records, captured
bodies, log lines). Requests in progress are not interrupted: call it after
the server has stopped accepting and drained. The config store's file
watcher belongs to the store and ends with it (when the last `Gateway` clone
is dropped); after `shutdown` a change it picks up is no longer applied to
the scheduler or the key table.

### `Gateway::version() -> &'static str`, `started_at() -> SystemTime`

The crate version and the start time, for the banner and `GET /status`.

### `Gateway::on_log_level(&self, f: impl Fn(&str) + Send + Sync + 'static)`

Registers the function that applies `logging.level` to the process's log
subscriber. Called every time a configuration is applied (not at
registration: the binary sets the initial level itself).

## Accessors

| Method | Returns | Use |
|---|---|---|
| `config()` | `Arc<Config>` | the configuration in effect now |
| `config_store()` | `&ConfigStore` | admin edits (`update`, `replace_text`, `reload_from_disk`), `restart_required()`, raw text |
| `telemetry()` | `&Telemetry` | request records, usage queries, event bus, log buffer, gauges (`track_ws()` for client WebSockets) |
| `scheduler()` | `&Arc<Scheduler>` | provider / credential snapshots, model table, cooldown reset, runtime enable/disable |
| `upstream()` | `&UpstreamClient` | the transport, for callers that need it directly |
| `codec(protocol)` | `&'static dyn Codec` | protocol helpers the server needs (legacy completions shim, WebSocket transcript helpers live in `switchyard_codecs`) |

## Events

Besides `request.started` / `request.finished` (one pair per client request)
and `config.reloaded`, the gateway publishes `credential` on the telemetry bus
whenever a failed upstream attempt was reported to the scheduler (or a
provider test ran): the payload is
`{"provider": <name>, "credential": <CredentialSnapshot>}` and never contains
a secret. Nothing is published while the bus has no subscriber.

## Configuration changes

The gateway reacts to `ConfigStore::subscribe()`: whatever the store applies —
a file change or an admin edit — rebuilds the scheduler once (credential
state is kept; the mock providers' model lists go in with the rebuild), reads
the service-account files again (below), rebuilds the client-key table
(rate-limit counts of unchanged keys are kept), reconfigures telemetry,
forgets which providers and models refused reasoning summaries (see
Generation), calls the log-level hook, publishes
`Event::ConfigReloaded { ok: true, .. }` on the telemetry bus and then starts
model discovery in the background — for the providers the change concerns
(see `discovery_states` under Admin operations). Applying costs the same
whether the configuration has three providers or three hundred times that
per provider: nothing in it is done once per provider *per* provider.

**Service-account files.** Every credential that names a
`service_account_file` has the file (resolved against the configuration
file's directory) read and validated when a configuration is applied. A file
that is missing, unreadable or not a usable key file marks the credential
unusable through `Scheduler::set_unusable`, with a reason that names the file
and the problem — `cannot read the service account file `sa.json`: …`, `the
service account file `sa.json` is not usable: `private_key` is not a PEM
block` — and never anything the file holds, nor the directory it is looked
for in. Such a credential is never picked, shows `usable: false`, status
`unusable` and the reason in snapshots, and is listed by
`Scheduler::warnings()`. A file that is fine takes the mark off, and so does
a credential that stops naming a file: one that has an `api_key` as well
keeps its id (and with it the scheduler's mark) when its
`service_account_file` is removed, so the gateway remembers which marks it
set and takes off those whose credential no longer names a file. The check
runs again before `test_provider` and `discover`, so repairing the file and
testing the provider brings the credential back without a reload. Checks run
one at a time, and the one for a configuration always runs after any that
began under the previous one.

A configuration the store **refuses** (`ConfigEvent::Rejected`: a broken edit
of the file, a failed `reload_from_disk`) changes nothing and is announced as
`Event::ConfigReloaded { ok: false, message }`, the message naming the first
issues — so the dashboard can say why a save had no effect without following
`ConfigStore::events()` itself.

**Order of the announcements.** `ConfigReloaded` events are published in the
order the store decided things, however far the gateway is behind: `ok:
false` for a refused file comes before the `ok: true` of the file that put
it right, and after the `ok: true` of a configuration applied before it. The
latest event therefore says how the file stands. (The store reports verdicts
in order on `events()` and the configuration itself on `subscribe()`; the
gateway takes its order from the verdicts and applies whatever configuration
is waiting when it reads an `Applied`. When several were applied and refused
before it got to look, the configuration is applied once, on the first
`Applied`, and a later `Applied` that follows a refusal is announced again
without applying anything twice.)

## Authentication

### `PresentedCredentials { authorization, x_api_key, x_goog_api_key, query_key }`

The four places a client may put its key, as `Option<String>`s taken verbatim
from the request. `PresentedCredentials::from_headers(&headers, query_key)`
fills the three header slots. `Debug` shows only which slots are filled.

### `Gateway::authenticate(&self, &PresentedCredentials) -> Result<ClientIdentity, ApiError>`

Candidates in order: `Authorization` (`Bearer x`, else the raw value),
`x-api-key`, `x-goog-api-key`, query `key`. The first candidate that
**matches** an enabled key wins. Nothing presented → 401 `missing API key`;
presented, no match → 401 `invalid API key`. With `auth.required = false`
both cases give an anonymous identity. Comparison is constant-time, on keys
with secret references (`env:NAME`) resolved.

### `ClientIdentity { key_id, key_name, anonymous, internal, .. }`

Who a request runs as. `key_id` is `ConfigStore`'s `client_key_id` of the
resolved key (never the key). The model allow-list and rate limit are
private; `allows_model(&str) -> bool` and `rate_limit_rpm() -> Option<u32>`
read them. The pipeline enforces both (403 naming the model; 429 with
`Retry-After`, sliding one-minute window).

### `Gateway::dashboard_identity(&self) -> ClientIdentity`

The admin playground's identity: `internal`, no restrictions, no rate limit,
`key_name = "dashboard"`.

### `Gateway::error_reply(&self, protocol, &ApiError) -> FullReply`

Renders any `ApiError` in a protocol's error envelope with its status, an
`x-request-id` and, when the error carries a wait, `retry-after`. For errors
the server detects itself (authentication, unknown route or method, body too
large, bad `Content-Encoding`).

## Generation

### `ClientRequest`

| Field | Meaning |
|---|---|
| `protocol` | the client's protocol: how the body is read and the reply rendered |
| `endpoint` | `"POST /v1/messages"`-style label for the request record |
| `body: Bytes` | the body, already content-decoded |
| `path_model`, `path_stream` | Gemini: model and stream flag from the URL |
| `headers: HeaderMap` | the client's request headers, all of them; the gateway forwards an allow-list (`anthropic-beta`, `anthropic-version`, `openai-beta`) and reads `user-agent`, `x-session-id`, `session_id`. `openai-organization` / `openai-project` are **never** forwarded: they name the client's own vendor account, and next to the gateway's key the upstream answers them with a 401 (set them in the provider's `headers` if the upstream key needs them) |
| `identity` | from `authenticate` / `dashboard_identity` |
| `client_ip` | for the record |
| `transport` | `Transport::Http` (becomes `Sse` when the request streams) or `Websocket` |
| `request_id` | used as the record id when `Some`; minted (UUIDv7) otherwise |
| `session` | explicit affinity key (a WebSocket connection id); otherwise derived from headers / `prompt_cache_key` / `metadata.user_id` / a fingerprint of the conversation's opening |
| `cancel: CancellationToken` | cancel it when the client goes away |

`ClientRequest::new(protocol, endpoint, body, identity)` fills the rest with
defaults.

`Debug` of `ClientRequest`, `RawRequest` and `WsOpenRequest` is safe to log:
headers are listed by name, with values only for a short list that cannot
carry a credential (`user-agent`, `content-type`, `accept`, `anthropic-beta`,
`anthropic-version`, `openai-beta`, …) — so not `authorization`, `x-api-key`,
`x-goog-api-key`, `cookie` or `sec-websocket-protocol`; query strings are
shown without their values; bodies (also `FullReply`'s) by size only. Read
the public fields when the real values are needed.

### `Gateway::generate(&self, ClientRequest) -> Reply` (async)

The pipeline. Never fails as a Rust call.

* `Reply::Full(FullReply)` — a complete response, or an error with its
  status. A **streaming request that fails before its first event also gets a
  `Full` reply**, so the client sees a real HTTP status.
* `Reply::Stream(StreamReply)` — the upstream produced its first event.

Inside: JSON parse → model + stream flag → allow-list and rate limit →
resolve (reasoning suffix split off) → attempt loop (`routing.max_attempts`;
streams additionally `streaming.bootstrap_retries` for failures inside the
stream) → passthrough when the upstream speaks the client's protocol and the
body carries no foreign signature, translation otherwise → reply. Failed
attempts are reported to the scheduler; a request fault (upstream 400) ends
the loop at once. The final error is the last upstream error — its own body
and status when the protocols match — else the scheduler's reason.

**The gateway's own credentials stay the operator's business.** The upstream
is called with the gateway's key, so (on every operation, in every protocol,
streaming or not):

* an upstream `401` / `403` is never the client's status and its body is
  never forwarded. A rejected credential (also a service-account file that
  cannot be loaded, or Google's `400 API_KEY_INVALID`) is a 502
  `upstream_auth_error`, "the upstream provider rejected the gateway's
  credential". A `403` the transport files under another class so that the
  scheduler rests the model rather than the key ("project … does not have
  access to model …", `PERMISSION_DENIED … or it may not exist`) is a 502
  `upstream_permission_denied` with an equally generic message. A `403`
  about something the request itself names (a Gemini file of another
  project) is a 400 carrying the upstream's explanation;
* a request that finds every credential resting gets 429 `model_cooldown`
  with `retry-after` and "all credentials for model `m` are cooling down;
  retry in Ns" — **not** the upstream failure that started the rest, which
  happened to an earlier request (possibly another client's).

The operator loses nothing: each attempt on the request record carries the
upstream's status and message, and the record of a request refused during a
rest ends with `(last upstream error: …)`.

**Request faults inside a stream.** A stream that fails with one of the
Responses API's image-input codes (`failed_to_download_image`,
`invalid_image`, `image_too_large`, …) is the request's fault like an
upstream 400: no failover, nothing rests, and — when nothing had been sent
yet — a 400 with the upstream's explanation.

**A failed generation in a complete body.** Without streaming, the Responses
API reports a generation that failed with HTTP `200` and a response object
whose `status` is `"failed"`. Such a body is a failed attempt — in both
modes, as `response.failed` at the head of a stream already is — classified
by the `code` (or `type`) in its `error`: `rate_limit_exceeded` rests the
model on the credential and fails over (the client is told 429 when nothing
else works), `insufficient_quota` and every other code the transport treats
as exhausted quota when it arrives as an HTTP error
(`billing_hard_limit_reached`, `billing_not_active`, `usage_limit_reached`,
`credit_balance_exhausted`, the spend-limit codes, …) rest the key as a 429
— also at the head of a stream and inside one —, `server_error` and unknown
codes fail over as a 502, and a fault of the request (`invalid_prompt`,
`context_length_exceeded`, the image codes above, …) ends the request with a
400 in the client's protocol, the vendor's code and message included,
without resting anything. This holds **whatever the failed generation had
produced** before it failed — a reasoning item, a hosted tool call, the first
words of the answer: without streaming none of it has reached the client, so
it is never delivered as an empty or truncated `200`, and the scheduler is
never told the credential succeeded. A Responses client (passthrough) gets
the error envelope with the failure's status too, not the upstream's `200`
body. Only `"failed"` is a failure: an `incomplete` response is delivered as
the answer it is. A body that carries nothing but an `error` without saying
`failed` is treated the same way.

**Reasoning summaries on Responses upstreams.** A request *translated* for a
Responses upstream asks for `reasoning.summary` when the client wants
reasoning text (a Chat Completions client by turning reasoning on, a Gemini
client with `includeThoughts`, a Messages client with `thinking.display:
"summarized"`). OpenAI refuses that field to organisations it has not verified
(`400`, `param: "reasoning.summary"`, "Your organization must be verified to
generate reasoning summaries"). The gateway then repeats the attempt once on
the same credential without `reasoning.summary` /
`reasoning.generate_summary` — streaming or not — and the client gets its
answer (with the reasoning effort it asked for, without summary text). The
repeat is not one of `routing.max_attempts`, the refusal is not reported to
the scheduler, and the request record lists both calls (the first with
status 400). The refusal is recognised as an HTTP `400`, inside a stream
before its first event, and in a complete body that reports a failed
generation (above). A `400` that names another parameter is never taken for
it, whatever its message quotes from the request.

What is remembered — until the next configuration change, so that later
translated requests leave the field out from the start — depends on what the
refusal is about:

* the organisation is not verified (the message says so): the **provider**,
  by name, for all its models. Only that provider: when the repeat fails for
  another reason and the request fails over to a different provider, that one
  is asked for the summary as usual;
* any other `400` that names `param: "reasoning.summary"` ("'reasoning.summary'
  is not supported with this model"), when what was refused is the plain
  `"auto"`: that **upstream model** of the provider. Its other models are
  still asked;
* a refused detail level the client chose itself (`"concise"`, `"detailed"`
  through a Chat client's `reasoning.summary`): **nothing**. The request is
  healed like the others; the next one — any client, the same model — is
  asked for a summary as usual.

A refusal met by a request that started under an earlier configuration (in
flight across the change, answered for the previous key) is not remembered
either; that request itself is still healed.

A Responses client's own field is never touched —
neither in passthrough nor when its body is re-encoded because it carries
another vendor's signature: it gets the upstream's 400 as it is. Translated
counting requests (`count_tokens`) never carry the field: it has no bearing
on the count.

**Mock providers.** A mock model that fails on purpose is a failed attempt
like a real one (record, failover, reply), but it is reported to the
scheduler only when that rests no more than the failing model itself:
`mock-error-429` / `mock-error-500` rest themselves, `mock-error-401` rests
nothing (an `Auth` failure would rest the provider's only credential and with
it every working mock model). The same holds for `test_provider`.

**Waiting** (`routing.max_wait_secs`, off by default): when no credential can
be picked because all of them are resting — including the ones this very
request just saw fail — and the soonest recovers within the limit, the request
waits for it, once, and tries again (still bounded by `routing.max_attempts`).
So a request whose only credential answers `429, Retry-After: 1` is served a
second later instead of failing. The wait ends at once when the request is
cancelled. A handler therefore may take up to `max_wait_secs` longer to
return; keep-alives are not possible before the reply exists.

**Request records** name the provider, credential and upstream model as soon
as an attempt starts, so a request abandoned mid-call (status 499) still says
who was called; a cancelled call is listed as an attempt with status 499.
Captured bodies (`logging.request_log`) always describe one attempt, the
last: what was sent, and what came back — the upstream's error body included.
They can be read (`telemetry().bodies().read(id)`) from the moment
`request.finished` is published for a record with `has_bodies`: whoever
reacts to the event finds them, whether or not the file is written yet. A
request served by a `mock` provider has no upstream response to capture.

**What the client sees of the upstream.** Passthrough forwards the upstream's
bytes — a complete Responses body that reports a failed generation excepted
(above) — with two exceptions on streams: a Chat Completions client that did not
set `stream_options.include_usage` is not sent the usage-only chunk
(`"choices": []`) that the gateway asks every Chat upstream for; and an
upstream's description of a failure (an in-stream error event, in either
mode) is delivered with the upstream credential replaced by `[redacted]`,
should the upstream have quoted it. The same goes for a complete `200` body
that is no response at all (an error envelope sent with the wrong status):
passthrough hands it on with the credential replaced, and what a translated
request is told about it ("the upstream response could not be understood:
…", a 502) does not contain the credential either. Content is never
rewritten.

### `FullReply { status, headers, content_type, body, request_id }`

`headers` are lower-case `(name, value)` pairs to add to the response:
`x-request-id`; `x-switchyard-provider` and `x-switchyard-model` when an
upstream was involved; `retry-after` for errors that carry a wait; and, when
`upstream.passthrough_headers`, the upstream's `x-ratelimit-*`,
`anthropic-ratelimit-*`, `openai-processing-ms`, `retry-after` and its
request id as `x-upstream-request-id`. `content_type` is not repeated in
`headers`. `header(name)` looks one up. `Debug` shows the body by size.

### `StreamReply { headers, protocol, request_id, events }`

`events` is a bounded `mpsc::Receiver<SseEvent>` (capacity 64) in the client's
protocol. **Dropping it cancels the upstream call.** The channel closing means
the stream is over and the request record has been published; failures after
the first event arrive in-band in the protocol's own error shape. `headers`
as for `FullReply`.

### `Reply::status()`, `Reply::request_id()`

Shortcuts over both variants (`200` for a stream).

## Token counting

### `Gateway::count_tokens(&self, ClientRequest) -> Reply` (async)

Always a `Reply::Full`. Uses the upstream's counting endpoint when its
protocol has one (passthrough when the protocols match, translated
otherwise) and answers in the client protocol's counting shape; falls back to
a local estimate (≈ characters / 4) when the upstream has none, answers
404/405/501, or refuses the call in a way that says nothing about the
credential's ability to generate (401/403: a key that may not count). None of
these rests a credential; rate limits, 5xx and transport failures on the
counting endpoint are failed attempts as usual. A forwarded counting body is
repaired like a generation body (foreign signatures, blocks the vendor
refuses to see again) but gains no generation-only field (`max_tokens`,
`stream`). Chat Completions clients get a 404. `path_stream` is ignored.

## Models

### `Gateway::models(&self, protocol, &ClientIdentity) -> serde_json::Value`

The listing in the protocol's shape: every visible client-facing name the
identity's allow-list admits.

### `Gateway::model(&self, protocol, &ClientIdentity, id) -> Result<Value, ApiError>`

One entry by exact id (Gemini also accepts `models/{id}`); 404
`model_not_found` otherwise.

## Raw side endpoints

### `RawRequest { path, method, body, content_type, query, model, headers, identity, client_ip, endpoint, cancel }`

A request for an OpenAI-style side endpoint. `path` is relative to the
provider's API root (`"embeddings"`, `"images/generations"`,
`"audio/speech"`, `"moderations"`); `model` is the body's `model` as read by
the server; `query` must not contain the client's gateway key.

### `Gateway::raw(&self, RawRequest) -> Reply` (async)

Always a `Reply::Full`. Picks an `openai` / `openai-compat` credential for
the model (404 when the model has no such route), forwards the body with only
the JSON `model` field replaced by the upstream id (bodies that are not JSON
are forwarded byte for byte), and returns the upstream's status, body and
`Content-Type`. Errors are in the OpenAI envelope.

Side endpoints are optional — an upstream may serve chat and nothing else, a
key may be restricted — so a refusal is held against the credential only
when it would hold for generation too: 429, exhausted quota, 5xx, transport
failures. A 401/403/404/405/501 moves on to the next credential and is
answered to the client (404/405 with the upstream's own body, 401/403 as a
generic 502, see Generation) **without** resting the credential or the model: no client can
take a model out of rotation by asking for `/v1/moderations` on an upstream
that has none, or for embeddings from a chat model.

## Upstream WebSockets

### `WsOpenRequest { identity, model, path_and_query, headers, endpoint, client_ip, require_kind }`

`path_and_query` is relative to the provider's API root with `{model}`
standing for the upstream model id: `"realtime?model={model}"` for the
Realtime relay (the one relay the server has). `headers` are the
client's request headers; only an allow-list is offered to the upstream
handshake (`openai-beta`, `openai-safety-identifier`,
`sec-websocket-protocol`, `x-client-request-id`).
`require_kind: Some(ProviderKind::Openai)` restricts the routes.

### `Gateway::open_upstream_ws(&self, WsOpenRequest) -> Result<UpstreamWsSession, ApiError>` (async)

Checks allow-list and rate limit, resolves, and connects with failover across
credentials on handshake failures (an upstream 401/403 is a 502, as
everywhere). 404 when no provider of the required kind serves the model; 429
with `retry_after_secs` — and no upstream wording — when every credential
rests.

As for raw endpoints, a failed handshake rests a credential only when the
upstream's API answered it with something it would answer any call with
(429, exhausted quota, 5xx). An upstream without a WebSocket endpoint (404,
405, 426, 501, a plain HTTP answer to the upgrade), a key that may not use it
(401/403) and a connection that cannot be established over the WebSocket
route (which, unlike HTTP calls, ignores operating-system proxy settings)
yield `Err` and leave the model in rotation, so the server can fall back to
HTTP turns for the same client.

### `UpstreamWsSession { socket, upstream_model, provider, request_id, handshake_headers, .. }`

`socket: UpstreamWebSocket` is a `Stream + Sink` of `WsMessage`; use it
through `&mut` (`session.socket.next()`, `session.socket.send(..)`).
`handshake_headers` are the upstream's `101` headers (echo
`sec-websocket-protocol` to the client). Call `finish(self, WsOutcome)` when
the relay ends: it publishes the session's request record (status `101`,
duration, usage, error) and reports to the scheduler. Dropping the session
without `finish` records it as `aborted`.

`redact(&self, text) -> String` replaces the credential the upstream was
called with by `[redacted]`. Run what the upstream **says about a failure**
through it before showing it to the client or logging it — a close reason,
the message of an error frame (careless upstreams quote the key). Messages
given to `finish` are scrubbed by the gateway. Do not run ordinary content
through it: self-hosted servers use keys such as `ollama`.

### `WsOutcome { end: WsEnd, usage: Usage }`, `WsEnd`

`WsOutcome::closed()` — orderly end; `::upstream_failed(msg)` — the upstream
socket broke (counts as a failure of that credential's upstream);
`::client_failed(msg)` — the client side broke (nobody is blamed).
`.with_usage(usage)` attaches token usage the server observed on the relayed
frames.

## Admin operations

### `Gateway::test_provider(&self, provider, model: Option<&str>) -> ProviderTest` (async)

Sends one tiny request (`"ping"`, 16 output tokens, non-streaming) through the
provider's first usable credential — whatever its cooldown state — in the
provider's first protocol, for `model` (a client-facing name of one of the
provider's models or an upstream id) or the provider's first configured
model. For a provider whose models were discovered, the default is the first
one the built-in catalog knows, else the first whose id does not look like an
embedding / speech / image / legacy-completion model. The outcome is reported
to the scheduler, so a successful test puts a resting model back into
rotation (and a failed one rests the model or the credential like a failed
request would — except a mock model's scripted `401`, which rests nothing).
Never fails as a Rust call.

A `2xx` status alone does not pass the test: the answer is read (up to
`server.body_limit_mb`) and has to be a response a request could be served
with. A Responses body that reports a failed generation (`status: "failed"`,
see Generation) fails the test with the class of its error code — a key that
is out of quota or rate limited rests as it would after a request, and a
rest a request started is not ended by a test that is answered the same way
— and a body that is not a response of the provider's protocol (a web page
behind a mistyped `base_url`, an error envelope sent with a `200`) fails it
as a 502.

### `ProviderTest { ok, status, latency_ms, model, credential, error }`

`Serialize`; the body of `POST /providers/{name}/test`. `status` is the
upstream's HTTP status (`0`: no response) — or, for a `2xx` whose body is not
a usable response, the status that failure amounts to (429 for a failed
generation that was rate limited, 400 for one that blames the request, 502
otherwise). `error` is omitted when `None` and is addressed to the operator:
it says what the upstream said, without key material — the credential that
was used is removed, and anything else the upstream quoted that is shaped
like a key, a token or a password is masked
(`switchyard_telemetry::redact_text`).

### `Gateway::discover(&self, provider) -> Result<Vec<ModelInfo>, ApiError>` (async)

Asks the provider's upstream for its model list now (first usable credential,
short timeouts), feeds it to the scheduler and returns it. For a `mock`
provider returns the built-in list. 404 for an unknown provider, 503 when it
has no usable credential, otherwise the upstream failure converted to an
`ApiError` whose message is masked like a provider test's `error`. The
outcome is recorded as the provider's discovery state, unless that state is
`off`.

### `Gateway::discovery_states(&self) -> HashMap<String, DiscoveryState>`

Where the discovery of each provider's model list stands, by provider name;
every provider of the configuration in effect has an entry.

`DiscoveryState { state, at, error, models }` is `Serialize` with every field
always present:

| Field | Meaning |
|---|---|
| `state: DiscoveryStatus` | `"off"` — the upstream is not asked: the provider is disabled, has `discover = false`, lists its `models` itself, or is a `mock`. `"pending"` — a listing is under way and none has answered since the provider's settings last changed. `"ok"` — the latest listing succeeded. `"failed"` — the latest listing failed. |
| `at: Option<i64>` | Unix ms: when the latest listing succeeded or failed; while `pending`, when it was started. `None` for `off`. |
| `error: Option<String>` | `failed` only: why, as one line of at most 300 characters ("provider `x` has no usable credential" when there was nothing to ask with). Without key material: the transport removes the credential the listing was asked with, and anything else the upstream quotes that is shaped like a key, a token or a password is masked with `switchyard_telemetry::redact_text`, like the log line of the same failure. |
| `models: usize` | Models in the upstream's list that is in use (`Scheduler::discovered_models`): the latest successful listing's — also after a later listing failed, **which keeps the previous list**, and across the provider being switched off and on again — and `0` when there is none (or the provider's `kind` or `base_url` changed, which drops the list). `0` while `off`. |

Discovery runs in the background, never holding up a request or a
configuration edit:

* **at start** for every provider that wants it;
* **after a configuration change** only for the providers whose
  discovery-relevant settings changed: a provider that is new; another
  `kind`, `base_url`, `api_keys`, `credentials`, `proxy`, `headers`,
  `project` or `location`; a provider that wants discovery now and did not
  before (enabled, `discover` switched on, its `models` list emptied); and
  every provider when `upstream.proxy` changed. Editing anything else — an
  alias, a price, a client key, the provider's `priority`, `prefix` or
  `exclude` — asks nobody: the lists are kept;
* **after a reload that changed nothing** (`reload_from_disk` on an unchanged
  file) for every provider that wants it: that is the operator asking again;
* **on request**, through `discover`.

A background listing that finishes after the provider's settings changed
again, or after the provider is gone, is dropped: its list does not reach the
scheduler and its outcome does not replace the newer state. That holds when
a provider is removed and created again under the same name while the first
one's listing is still under way: listings are numbered across all providers
for as long as the gateway runs, so the old listing is never taken for the
new provider's.

---------------------------------------------------------------------------

# How the server crate should call this

## Plain HTTP (JSON) routes

```rust
let presented = PresentedCredentials::from_headers(&headers, query_key);
let identity = match gateway.authenticate(&presented) {
    Ok(identity) => identity,
    Err(error) => return respond_full(gateway.error_reply(protocol, &error)),
};
let cancel = CancellationToken::new();
let _cancel_on_drop = cancel.clone().drop_guard();      // fires when the handler future is dropped
let mut request = ClientRequest::new(protocol, "POST /v1/messages", body, identity);
request.headers = headers;
request.client_ip = Some(peer.ip().to_string());
request.cancel = cancel;
match gateway.generate(request).await {
    Reply::Full(full) => respond_full(full),
    Reply::Stream(stream) => respond_sse(stream),
}
```

`respond_full`: status = `full.status`, `content-type` = `full.content_type`,
add every pair of `full.headers`, body = `full.body`. Do not add another
`x-request-id`: the reply carries the id of the request record.

Gemini: set `request.path_model` from the URL and `request.path_stream =
Some(method == "streamGenerateContent")`; route `countTokens` to
`count_tokens`.

`count_tokens` and `raw` always return `Reply::Full`; treat a `Stream` there
as unreachable (answer 500).

## SSE

For a `Reply::Stream`: status 200, `content-type: text/event-stream`,
`cache-control: no-cache`, `x-accel-buffering: no`, plus `stream.headers`.
Write each `SseEvent` with `event.to_bytes()` and flush per event. When
`events.recv()` returns `None` the stream is complete — the gateway has
already rendered protocol terminators (`data: [DONE]`, `message_stop`, …) and
in-band errors; add nothing.

**Keep-alives** are the server's job: while waiting on `events.recv()`, after
`streaming.keepalive_secs` of silence write `switchyard_core::sse::comment("keep-alive")`
(`: keep-alive\n\n`). Use a timer that is reset by every event. `0` disables.

## Gemini array streaming

`:streamGenerateContent` without `alt=sse` answers with one JSON array instead
of SSE. Call `generate` exactly as for SSE (`path_stream = Some(true)`); then
write `[`, the `data` of each event separated by `,`, and `]` when the channel
closes, with `content-type: application/json`. Send no keep-alive comments in
this mode (they are not valid inside a JSON array; a single space is).

## Cancellation

Two mechanisms, both honoured:

* `ClientRequest::cancel` — cancel the token when the client disconnects. A
  pending upstream call is abandoned, the request is recorded with status 499
  and nothing is held against the credential.
* dropping the `StreamReply::events` receiver — the pump stops and drops the
  upstream connection.

If the handler future itself is dropped while `generate` is still running
(axum does this on disconnect), the request is still recorded (status 499):
records and gauges are released by RAII. Using a `DropGuard` on the token, as
above, covers every case with one line.

## A Responses WebSocket turn

For each `response.create` on a client WebSocket, build the Responses request
body (transcript merging is `switchyard_codecs::responses`' job), force
`stream: true`, and call:

```rust
let mut request = ClientRequest::new(Protocol::OpenaiResponses, "GET /v1/responses", body, identity.clone());
request.transport = Transport::Websocket;
request.session = Some(connection_id.clone());     // keeps the connection on one credential
request.cancel = turn_cancel.clone();              // cancel when the socket closes
match gateway.generate(request).await {
    Reply::Stream(mut stream) => {
        while let Some(event) = stream.events.recv().await {
            if event.is_done_marker() { continue; }          // no [DONE] over WebSocket
            socket.send(Message::Text(event.data.into())).await?;   // one event per text frame
        }
    }
    Reply::Full(full) => { /* an error: send {"type":"error","status":full.status,"error":…} built from full.body */ }
}
```

The error frame is `{"type":"error","status":<n>,"error":{"message","type","code"?,"param"?,"headers"?}}`.
The Responses WebSocket error frame may carry `error.headers`: when
`full.header("retry-after")` is set (a `429` for resting credentials or for
the client key's rate limit), put it there as
`"headers": {"retry-after": "<seconds>"}` — a socket has no response headers,
and this is where the vendor's own protocol puts them.

Turns always take this HTTP path, whatever the upstream: there is no provider
setting that asks for a relay to an upstream's own Responses WebSocket (the
`websocket` option of earlier versions was removed; it never did anything).
`open_upstream_ws` serves the Realtime relay below. Should a server relay
frames of another upstream WebSocket endpoint with it, the same rules apply
as there: call `session.finish(..)` when either side closes, close the client
with 1012 after `WsOutcome::upstream_failed`, and pass close reasons and
upstream error messages you show or log through `session.redact(..)`.

## Realtime relay

```rust
let session = gateway.open_upstream_ws(WsOpenRequest {
    identity, model, path_and_query: "realtime?model={model}".into(),
    headers: handshake_headers, endpoint: "GET /v1/realtime".into(),
    client_ip, require_kind: Some(ProviderKind::Openai),
}).await?;          // Err(ApiError): answer the still-HTTP request with gateway.error_reply(..)
// upgrade the client, echoing session.handshake_headers["sec-websocket-protocol"]
// relay text/binary/ping/pong/close both ways through session.socket
session.finish(WsOutcome::closed().with_usage(usage));
```

Hold `gateway.telemetry().track_ws()` for the lifetime of each client
WebSocket so the dashboard's connection gauge is right.

## Model listings

`GET /v1/models` → `gateway.models(Protocol::OpenaiChat, &identity)` (or
`Protocol::Anthropic` when the request has an `anthropic-version` header);
`GET /v1beta/models` → `Protocol::Gemini`. Single models through
`gateway.model(..)`, rendering `Err` with `error_reply`.

## The admin playground

`POST /admin/api/playground` builds a `ClientRequest` with
`gateway.dashboard_identity()` and the protocol the page selected, then
handles the `Reply` exactly like a client route.
