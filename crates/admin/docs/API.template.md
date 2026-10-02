# Switchyard admin API

The API behind the dashboard: everything under `/admin/api`, plus the
dashboard's own files under `/admin/`. This is the document dashboard pages
are written against.

Every example below is a real exchange with a gateway running the built-in
mock provider, recorded by `tests/api_doc.rs` (which also regenerates this
file: `SWITCHYARD_WRITE_API_DOC=1 cargo test -p switchyard-admin --test api_doc`).
Three things are cosmetic: the configuration directory is shown as
`/etc/switchyard`, the port as `8317`, and long lists are cut where the text
says so. Edit `docs/API.template.md`, not this file.

Contents: [Conventions](#conventions) · [Access](#access) ·
[Session and status](#session-and-status) · [Configuration](#configuration) ·
[Providers](#providers) · [Credentials](#credentials) ·
[Models, aliases, payload rules, prices](#models-aliases-payload-rules-prices) ·
[Client keys](#client-keys) · [Usage, requests, logs](#usage-requests-logs) ·
[Playground](#playground) · [Live events](#live-events-websocket) ·
[Dashboard files](#dashboard-files) · [Route index](#route-index)

---------------------------------------------------------------------------

## Conventions

* **Base path** `/admin/api`. The dashboard derives it from its own location
  (`<directory of the page>/api`), so a reverse proxy may add a prefix.
* **Bodies are JSON**, both ways. A request's `Content-Type` is not checked.
  Request bodies are limited to 4 MiB (the playground: `server.body_limit_mb`);
  larger ones get `413`.
* **Field names** are `snake_case`. **Timestamps** are unix milliseconds
  (`*_at`, `at`, `t`, `from`, `to`), **durations** milliseconds (`*_ms`) or
  seconds (`*_secs`, `expires_in`, `Retry-After`).
* **Stable shapes.** Views built by this API (`/status`, `/providers`,
  `/keys`, `/aliases`, `/payload`, `/pricing`) always carry every field;
  a missing value is `null`, `""` or `[]`. Types passed through from the
  engine keep its conventions, noted where they apply: the configuration in
  `GET /config` and `ModelInfo` omit fields that are at their default.
* **Lists are bare arrays** (`GET /providers` answers `[ … ]`, not
  `{"providers": [ … ]}`).
* **Unknown fields in request bodies are errors** (`400`), so a typo is not
  silently ignored. Unknown *query* parameters are ignored, and query values
  that do not parse fall back to their default.
* **Responses are never to be cached**: every API response carries
  `cache-control: no-store` (the playground's event stream: `no-cache`) and
  `x-content-type-options: nosniff`.

### Errors

Every error of the admin API has this body, whatever the status:

```json
{"error": {"message": "…", "issues": [{"path": "…", "message": "…"}]}}
```

`message` is a sentence for the operator and is always present. `issues` is
present only for validation failures; each names a field (`path`) and what
is wrong with it. For configuration issues the path is the field's place in
the whole configuration (`providers[1].base_url`, `server.port`), for
request-shape issues the field's place in the request body
(`routing.cooldown.auth_secs`, `name`), and for TOML syntax errors
`line L, column C`.

| Status | Meaning |
|---|---|
| `400` | The request is malformed: not JSON, a missing or unknown field, a wrong type, a field that may not be changed here. `issues` names the field when one can be named. |
| `401` | No secret, a wrong secret, or (on `/ws`) a missing, used or expired ticket. Carries `WWW-Authenticate: Bearer`. |
| `403` | The peer is remote and remote access is off. |
| `404` | No such route, provider, credential, key or request — or the admin interface is off (see [Access](#access)). |
| `405` | The route exists but not with this method. |
| `409` | A name or key that already exists. |
| `413` | The request body is too large. |
| `422` | The request was understood, but the configuration it would produce is not valid. Always with `issues`. Nothing was changed. |
| `429` | Locked out after repeated wrong secrets. Carries `Retry-After` (seconds). |
| `500` | The configuration file could not be read or written, or an internal failure. |
| `502`, `504` | `POST /providers/{name}/discover` only: the provider's upstream failed (`502`) or did not answer in time (`504`). |
| `503` | `POST /providers/{name}/discover`: the provider has no usable credential. `GET /ws`: the gateway is shutting down. |

These statuses are the admin API's own and mean nothing else. In particular
a failure of an *upstream* is never reported with the upstream's status: an
upstream that rate-limits, has no such endpoint or rejects a request yields
`502` with its explanation in `message`, not `429`, `404`, `400` or `422`.

The one exception is `POST /playground`, whose answer — errors included — is
the chosen protocol's own (see [Playground](#playground)).

### Secrets

A response never contains a literal secret, with three exceptions that exist
to show one: `GET /config/raw` (the operator's own file), `POST /keys` (the
key just created) and `POST /keys/{id}/reveal`.

Everywhere else a literal is **masked**: a short prefix and suffix around
`…` (`sk-liv…6b5c`), and secrets of up to 11 characters as bullets
(`••••••••`). A secret **reference** (`env:NAME`, `${NAME}`) names a
variable, not a secret, and is shown as written. Masked too: the password in
a proxy URL or a `base_url` (`http://user:prox…word@host`), and the values of
provider headers whose name looks like a credential (`Authorization`,
`X-Api-Key`, …).

In an update, a secret field that is **empty** or **equal to the mask it was
shown as** means "keep the stored value"; anything else replaces it. A mask
that matches no stored secret is refused with `422` (it would otherwise be
saved as the key). The dashboard can therefore send back what it was shown.
Exception: an empty row in a provider's `api_keys` list is a *removed* key,
not a kept one.

### Configuration edits

Every mutation is written to `switchyard.toml` through the configuration
store: comments, order and formatting of the file are preserved and only the
values that changed are touched; concurrent edits are applied one after the
other, each on the result of the previous one; a result that does not
validate is refused with `422` and changes neither the file nor the running
gateway. The same goes for a value a TOML file cannot hold, wherever it is
sent: JSON `null` inside free-form values (the `set` of a payload rule) and
integers above 9223372036854775807 are refused with `422`, each named in
`issues` by its place in the configuration
(`payload.override[0].set.response_format`, `routing.max_wait_secs`).
A mutation answers once the gateway runs on the new configuration,
so what it returns — and what the next `GET` returns — is already in effect.
Each applied change is also announced as a `config.reloaded` live event.

---------------------------------------------------------------------------

## Access

In this order, for every route under `/admin/api`:

1. **Availability.** With `admin.enabled = false`, or with no admin secret
   (neither `admin.secret` nor `SWITCHYARD_ADMIN_SECRET`; a reference to an
   unset variable counts as none), every route answers `404`
   `{"error":{"message":"not found"}}` and the dashboard's files are `404`
   too: the admin interface does not exist.
2. **Loopback rule.** The address of the TCP peer must be loopback
   (`127.0.0.0/8`, `::1`, IPv4-mapped included) unless `admin.allow_remote`
   is on (or `SWITCHYARD_ADMIN_ALLOW_REMOTE=1`). Headers are never trusted to
   make a peer local. They are used the other way round: a loopback peer
   that sends `X-Forwarded-For`, `Forwarded` or `X-Real-IP` is a reverse
   proxy on this machine relaying someone else, and counts as **remote**.
   Refused requests get `403` and are not counted as sign-in failures.
   A reverse proxy in front of the gateway must therefore add one of these
   headers (nginx: `proxy_set_header X-Forwarded-For
   $proxy_add_x_forwarded_for;`): a proxy on this machine that adds none
   makes every client it relays look local.
3. **Lockout.** Five wrong secrets in a row from one address (IPv6: one /64)
   lock that address out for 30 minutes: the fifth and everything after it
   gets `429` with `Retry-After`, the right secret included. A successful
   request resets the count. A request without any secret is `401` but is
   not counted. The table is in memory (a restart clears it), holds at most
   10 000 addresses and forgets an address two hours after its last failure.
   The address is that of the TCP peer. What a reverse proxy on this
   machine relays (see 2) is counted apart from what connects to the
   gateway directly on loopback: wrong secrets sent through the proxy never
   lock out someone signing in on the machine itself, nor the other way
   round. All clients of that proxy share one count, though — the address
   the proxy reports in its header is not used, since a client can send
   that header too — so remote sign-in through a proxy can be locked for
   everybody by one client's wrong guesses; signing in directly on the
   machine keeps working. The same holds for any other arrangement in which
   many clients arrive from one address (NAT, a container's port mapping).
4. **The secret.** `Authorization: Bearer <secret>` (the bare secret without
   `Bearer` is accepted too), or `x-admin-secret: <secret>`. The comparison
   is constant-time over the raw bytes; a secret that is not ASCII is sent
   as its UTF-8 bytes. `GET /ws` alone takes a ticket instead (see
   [Live events](#live-events-websocket)).

The effective secret is `SWITCHYARD_ADMIN_SECRET` when set, else
`admin.secret`. Changing `admin.secret` takes effect at once.

A wrong secret:

{{example:error_401}}

A relayed request (here: `X-Forwarded-For` on a loopback connection) while
remote access is off:

{{example:error_403}}

The fifth wrong secret in a row, and every request for the next 30 minutes:

{{example:error_429}}

An unknown route (with the secret; without it the answer is `401`, so
routes cannot be probed):

{{example:error_404}}

---------------------------------------------------------------------------

## Session and status

### `POST /login`

Checks the secret and nothing else. The body is ignored. This is what the
sign-in page calls; `401`, `403`, `404` and `429` mean what [Access](#access)
says.

{{example:login}}

### `GET /status`

{{example:status}}

| Field | Meaning |
|---|---|
| `version` | The gateway's version. |
| `started_at`, `uptime_ms` | When the process started, and for how long it has run. |
| `config_path`, `data_dir` | Absolute paths of the configuration file and the data directory (`null` when there is none). |
| `listen` | The address the server is bound to, `null` when unknown. |
| `restart_required` | Settings that were changed since start and only take effect after a restart: any of `server.host`, `server.port`, `server.tls`, `server.data_dir`. |
| `warnings` | Problems in the configuration that do not make it invalid: credentials whose secret cannot be resolved, alias targets that match no model, shadowed names. Plain sentences. |
| `counts.providers`, `counts.client_keys` | Entries in the configuration. |
| `counts.credentials`, `counts.credentials_ready` | Upstream credentials in total, and those with status `ready`. |
| `counts.models` | Client-facing model names that are listed to clients (aliases included, hidden names excluded). |
| `live` | Gauges: requests in flight, open streams, open client WebSockets (the client API's, not dashboard connections), and `totals` since start (see [Totals](#totals)). |
| `admin.allow_remote` | Whether remote peers are admitted (configuration or environment). |
| `admin.remote` | Whether *this* request counted as remote. |
| `auth_required` | `auth.required`: whether the client API demands a key. |

### `POST /ws-ticket`

Sells a ticket for `GET /ws`: 32 random bytes, URL-safe, valid once and for
`expires_in` seconds. The body is ignored. At most 1024 tickets are
outstanding; beyond that the one closest to expiry is dropped.

{{example:ws_ticket}}

---------------------------------------------------------------------------

## Configuration

### `GET /config`

The live configuration with every secret masked, the file's path and the
settings waiting for a restart. `config` is the configuration schema of
`switchyard.toml` as JSON; sections and fields at their default *are*
present for the scalar sections (`server` … `usage`), while empty lists
(`providers`, `aliases`, `pricing`, `auth.keys`), an empty `payload` and
per-entry defaults are omitted, as in the file.

{{example:config_get}}

### `GET /config/raw`

The file exactly as it is on disk — comments, secrets and all. `modified_at`
is the file's modification time (`null` if the file system cannot tell).
`500` when the file cannot be read.

{{example:config_raw_get}}

### `PUT /config/raw`

Request: `{"text": "<the whole file>"}`. The text is validated, written
verbatim and applied. Response: the same shape as `GET /config`
(`providers` is cut to its first entry here).

{{example:config_raw_put}}

An invalid text is refused with `422`; nothing is written:

{{example:config_raw_put_invalid}}

### `POST /config/validate`

Request: `{"text": "<a whole file>"}`. Always `200`; the verdict is in the
body. Nothing is written or applied. Issue messages never quote values from
the text (a key pasted into the wrong place is not echoed).

{{example:config_validate_ok}}

{{example:config_validate_bad}}

### `PATCH /settings`

A JSON merge patch (RFC 7396) over the scalar sections of the configuration:
`server`, `admin`, `routing` (with `routing.cooldown`), `streaming`,
`upstream`, `logging`, `usage`, and of `auth` only `auth.required`. Objects
merge key by key; `null` resets a field to its default. Response: the same
shape as `GET /config` (`providers` is cut to its first entry here).

`admin.secret` and a password inside `upstream.proxy` follow the
[mask rule](#secrets): send the mask or `""` to keep them. `admin.secret`
therefore cannot be emptied here; a new value replaces the secret at once
(the next request must use it).

{{example:settings_patch}}

Errors:

* `400` — the patch is not an object, touches another section (`providers`,
  `auth.keys`, `aliases`, …: they have their own endpoints), names a field
  that does not exist, or gives a value of the wrong type. Nothing of a
  refused patch is applied.
* `422` — the values have the right types but break a rule, or are integers
  too large for the file (see [Configuration edits](#configuration-edits)).

{{example:settings_patch_refused}}

{{example:settings_patch_invalid}}

### `POST /reload`

Reads the file again and applies it, whether or not it changed (the file is
normally watched; this is the manual way). The body is ignored. Response:
the same shape as `GET /config` (`providers` is cut to its first entry
here). `422` when the file is not a valid configuration — the previous one
stays in effect — and `500` when it cannot be read.

{{example:reload}}

---------------------------------------------------------------------------

## Providers

### The provider view

`GET /providers`, `GET /providers/{name}` and every provider or credential
mutation return this object:

{{example:provider_get}}

It is the provider's configuration entry with every field present and
secrets masked, except that two keys are replaced and four are added:

| Field | Meaning |
|---|---|
| `name` … `location` | The entry as configured (`kind`: `openai`, `anthropic`, `gemini`, `vertex`, `openai-compat`, `mock`; `wire_api`: `auto`, `chat`, `responses`; `legacy_max_tokens`, `stream_usage`: `true`, `false` or `null` for the kind's default). |
| `credentials` | **Replaced:** one entry per runtime credential, configuration merged with runtime state (below). The configured `credentials` list is in `config.credentials`. |
| `models` | **Replaced:** the client-facing names this provider serves right now, sorted. The configured model list is in `config.models`. |
| `model_count` | Upstream models the provider serves, after `exclude`. `models` can be longer: with a `prefix`, each model is listed as `prefix/name` and under its bare name. |
| `effective_base_url` | `base_url`, or the kind's default when that is empty. |
| `protocols` | Wire protocols the provider can be spoken to in, most preferred first: `openai-chat`, `openai-responses`, `anthropic`, `gemini`. |
| `config` | The entry itself, in exactly the shape `POST /providers` and `PUT /providers/{name}` accept: edit this object and send it back. |

A credential entry:

| Field | Meaning |
|---|---|
| `id` | Stable id (`<provider>:<12 hex>`, sometimes with a `-N` suffix): a hash of provider, kind, key and endpoint, never the key. It survives edits of label, weight, priority and proxy, and changes when the key, the endpoint or the provider's name changes. Use it with `/credentials/{id}/…`. |
| `label` | The configured label, else the masked key, else the provider's name. |
| `masked_key` | The masked key; the reference as written when it cannot be resolved; a service-account file's name; or `""` for a keyless credential. |
| `source`, `index` | Where the credential is configured: `"api_keys"` (`config.api_keys[index]`), `"credentials"` (`config.credentials[index]`), or `"implicit"` with `index: null` — the keyless credential that `mock` and `openai-compat` providers get when they list none. |
| `disabled` | Switched off, in the configuration or at runtime. |
| `weight`, `priority` | Effective values (`priority` falls back to the provider's). |
| `proxy`, `service_account_file` | As configured for a `credentials` entry (proxy password masked), else `""`. |
| `status` | `ready`, `cooling` (the whole credential rests, or every model on it does), `disabled`, `unusable` — or `unknown` in the rare moment the scheduler has not seen the credential yet. |
| `cooldown_until`, `cooldown_reason` | End and cause of the cooldown that makes it `cooling`, else `null`. Causes: `rate_limit`, `quota`, `auth`, `server`, `transport`, `model_not_found`, `request`. |
| `model_cooldowns` | Models resting on this credential: `[{model, until, reason}]` (upstream model ids). |
| `requests`, `successes`, `failures`, `consecutive_failures` | Upstream attempts since start (failures exclude request faults). |
| `latency_ms` | Moving average of the response latency, `null` before the first success. |
| `last_used_at` | Unix ms of the last attempt, or `null`. |
| `last_error` | `{status, class, message, at, model}` of the latest upstream failure, or `null`. `status` is `0` when no response arrived. |
| `usable`, `unusable_reason` | `false` with a reason when the credential can never be used as configured (its variable is not set, it has no key). |

The same view of a provider whose only credential is implicit and has seen a
failure:

{{example:provider_get_mock}}

### `GET /providers`

Every provider, in configuration order (the list is cut to two entries per
array here).

{{example:providers_list}}

### `GET /providers/{name}`

One provider view. `404` when there is no provider of that name.

### `POST /providers`

Request: a provider entry (the `config` shape above; only `name` and `kind`
are required). Response: `201` and the new provider's view (lists cut to
three entries here).

* The name is trimmed; blank `api_keys` rows are dropped.
* `409` when a provider of that name exists.
* `400` when the body is not a provider entry (unknown field, unknown
  `kind`, missing `name`).
* `422` when the entry is one but is not valid — including a *masked* secret
  in a new provider, which has no stored value to stand for.

{{example:provider_create}}

{{example:provider_create_conflict}}

{{example:provider_create_invalid}}

### `PUT /providers/{name}`

Request: the whole provider entry (`config`, edited). Response: the updated
view. The entry replaces the stored one: a field that is left out goes back
to its default.

* **Secrets** follow the [mask rule](#secrets): unchanged masks keep their
  secrets — also when entries are reordered, deleted, or moved between
  `api_keys` and `credentials` — new values replace, references stay as
  written. An emptied `credentials[].api_key` keeps the stored key; a blank
  `api_keys` row is dropped.
* **Rename:** a `name` in the body that differs from the path renames the
  provider (`409` if that name is taken). Its secrets are kept as long as
  `kind` and `base_url` stay the same; payload rules that name the provider
  follow the rename. Credential ids change with the name, so their runtime
  counters start afresh.
* `404` unknown provider, `400` not a provider entry, `422` not valid.

Here the first key comes back as its mask, a second is added, and the
priority changes:

{{example:provider_put}}

### `DELETE /providers/{name}`

`404` when there is no such provider.

{{example:provider_delete}}

### `POST /providers/{name}/test`

Sends one tiny request ("ping", 16 output tokens, not streamed) through the
provider's first usable credential, whatever its cooldown state. Request:
`{"model": "<client-facing name or upstream id>"}`, `{}`, or no body at all
(then the provider's first model is used). Takes as long as the upstream
does, at most 60 seconds.

Always `200` for a provider that exists: the outcome is in the body. A
success puts a resting model back into rotation; a failure rests it like a
failed request would. `404` unknown provider.

| Field | Meaning |
|---|---|
| `ok` | Whether the upstream answered successfully. |
| `status` | The upstream's HTTP status; `0` when there was no response. |
| `latency_ms` | How long the call took. |
| `model` | The upstream model id that was used, `null` if none could be chosen. |
| `credential` | Label of the credential that was used, `null` if there is none. |
| `error` | Present only on failure: what the upstream said, credentials removed. |

{{example:provider_test}}

{{example:provider_test_failed}}

### `POST /providers/{name}/discover`

Asks the upstream for its model list now, hands it to the scheduler and
returns it (cut to two entries here). The body is ignored. `ModelInfo`
omits fields it has no value for.

{{example:provider_discover}}

Errors:

* `404` — unknown provider.
* `503` — the provider has no usable credential to ask its upstream with
  (none configured, all disabled, or a reference to an unset variable).
* `502` — the upstream did not deliver the list, whatever its reason: it
  could not be reached, rejected the credential, rate-limited the request,
  has no model listing at that address, or answered with an error of its
  own. `message` starts with what happened and ends with what the upstream
  said (credentials removed); a wait the upstream asked for is mentioned in
  the sentence. There is no `Retry-After` header and the upstream's status
  is not passed on: `429` and `404` from this route would read as the
  sign-in lockout and an unknown provider.
* `504` — the upstream did not answer in time.

An upstream that rate-limits its model listing (`limited` is a provider
whose upstream answers `429` with `Retry-After: 17`):

{{example:provider_discover_failed}}

---------------------------------------------------------------------------

## Credentials

`{id}` is a credential's `id` from the provider view. It contains a `:`;
percent-encoding it is fine but not required. All three calls ignore the
body.

### `POST /credentials/{id}/reset`

Clears every cooldown and failure streak of the credential (counters stay).
`404` unknown id.

{{example:credential_reset}}

### `POST /credentials/{id}/disable`, `POST /credentials/{id}/enable`

Switches a credential off or on **in the configuration**, so the choice
survives a restart, and returns the updated provider view. Idempotent.
`404` unknown id.

* A `credentials[]` entry gets `disabled = true` (or loses it).
* An `api_keys[]` entry is a bare string and has nowhere to keep the flag:
  disabling one turns it into a `credentials[]` entry
  (`{api_key, disabled: true}`) appended to that list. Its `id` stays the
  same — and with it the runtime counters — but its `source` and `index`
  change, as do the `index` of the `api_keys` rows after it. Enabling it
  later clears the flag; it stays a `credentials[]` entry.
* The implicit credential of a keyless provider becomes
  `credentials = [{disabled: true}]` the same way.
* `enable` also lifts a switch-off that was only made at runtime.

Disabling the shorthand key of `vendor`: it leaves `api_keys` and reappears
as `credentials[1]` with the same id.

{{example:credential_disable}}

Enabling it again (lists cut to one entry here):

{{example:credential_enable}}

{{example:credential_unknown}}

---------------------------------------------------------------------------

## Models, aliases, payload rules, prices

### `GET /models`

The client-facing model table, sorted by name (cut to three entries here):
every model name and alias, including names hidden from client listings.

| Field | Meaning |
|---|---|
| `name` | The name clients use. |
| `info` | Model metadata; `id` equals `name`. Fields without a value are omitted (`display_name`, `description`, `owned_by`, `created`, `context_window`, `max_output_tokens`, `thinking`). `known: false` means the gateway has no metadata for the model and passes requests through unfitted. |
| `hidden` | Hidden from listings by an alias with `hide_targets`; still routable. |
| `alias_targets` | Present only for aliases: the targets as configured. |
| `routes` | The providers behind the name: `provider`, `upstream_model`, `credentials_total`, and `credentials_available` — how many of them could serve the model right now (enabled, usable, not resting). `0` available means requests would currently fail. An alias with no routable target has `routes: []`. |

{{example:models}}

### `GET /catalog`

The built-in catalog of well-known models (cut to two entries here):
`ModelInfo` keyed by the vendor's id, plus `family` (`openai`, `anthropic`,
`google`) and, when not every provider kind of the family serves the model,
`kinds`. `thinking` describes reasoning support: a token budget range
(`min`, `max`, `zero_allowed`, `dynamic_allowed`) and/or named `levels`.

{{example:catalog}}

### `GET /aliases`, `PUT /aliases`

An alias is a virtual model: `name` routes to `targets`, tried in order; a
target may pin a reasoning depth with a suffix (`"mock-think(high)"`).
`hide_targets` hides the targets' own names from client listings.

`PUT` replaces the whole list with the body — an array of
`{name, targets, hide_targets?}` — and returns the new list. `[]` clears it.

{{example:aliases_put}}

{{example:aliases_get}}

`400` when the body is not such an array; `422` when a rule is broken
(empty or duplicate name, no target, an alias that targets itself):

{{example:aliases_put_invalid}}

### `GET /payload`, `PUT /payload`

Rules that patch the JSON body sent upstream, applied in the order
`default` (set a field only when the client did not), `override` (always
set), `filter` (remove). A rule:

| Field | Meaning |
|---|---|
| `models` | Wildcard patterns matched against the upstream model id and the client-requested name. Required, at least one. |
| `protocol` | Only when the upstream request uses this protocol; `null` for any. |
| `provider` | Only for this provider; `""` for any. |
| `set` | `default` / `override` rules: dotted path → JSON value (`messages.0.role`; `\.` for a literal dot). Required there. |
| `remove` | `filter` rules: dotted paths to delete. Required there. |

`PUT` replaces all three lists with the body, an object
`{default?, override?, filter?}` whose rules may leave out `protocol`,
`provider`, and whichever of `set` / `remove` does not apply; a list that is
left out is emptied. It returns the new rules in the full shape.

{{example:payload_put}}

{{example:payload_get}}

`400` when the body is not that object (unknown key, unknown `protocol`);
`422` when a rule has no model pattern, lacks its `set` / `remove`, or sets
a value the configuration file cannot hold — `null` anywhere in a `set`
value, or an integer above 9223372036854775807. To make the upstream body
lose a field, use a `filter` rule; a rule cannot set a field to `null`.

{{example:payload_put_invalid}}

### `GET /pricing`, `PUT /pricing`

Prices in USD per million tokens, for cost estimates. The first entry whose
`model` pattern (wildcards, matched against the upstream model id) fits
wins. `cache_read` and `cache_write` default to `input` when `null`.

`PUT` replaces the whole list with the body — an array of
`{model, input, output, cache_read?, cache_write?}` — and returns the new
list. Prices apply to requests from then on; recorded costs do not change.

{{example:pricing_put}}

{{example:pricing_get}}

`400` when the body is not such an array; `422` for an empty `model` or a
negative price.

---------------------------------------------------------------------------

## Client keys

The keys applications present to use the gateway (`auth.keys`).

`{id}` is `key_` plus 12 hex digits: a hash of the key's value (for a
reference, of the value it resolves to — or of the reference text while its
variable is unset). It is the `client.key_id` of request records.

### `GET /keys`

{{example:keys_list}}

| Field | Meaning |
|---|---|
| `id` | See above. |
| `name` | The label. The usage statistics (`by_key`, `group_by=key`) and the request list show requests under the name the key had when they were made. |
| `masked` | The masked key, or the reference as written. |
| `is_reference` | The key is `env:NAME` / `${NAME}`. |
| `resolved` | `false` for a reference whose variable is not set: the key cannot be used until it is. |
| `enabled` | Disabled keys are refused by the client API. |
| `models` | Wildcard patterns of models the key may use; `[]` means all. |
| `rate_limit_rpm` | Requests per minute, `null` for unlimited. |
| `usage` | What this key did over the last 30 days (the `30d` range of `/usage/summary`, cut to `usage.retention_days`): `requests`, `errors`, `tokens` (prompt + output), `cost` (USD), and `last_used_at`, the start (unix ms) of its most recent request on record, else `null`. |

`usage` belongs to the key, not to its name: it is counted from the request
records by their `client.key_id`. Renaming a key keeps its numbers, and a
key that is given a name another key had before starts at zero. (The
per-name breakdowns — `by_key` in `/usage/summary`, `group_by=key` in
`/usage/timeseries` — keep showing past requests under the name of the
time.) The records counted are those of the usage files; with
`usage.persist = false` only the requests still in the in-memory request
list (the most recent 2000) can be counted, and nothing survives a restart.
`DELETE /usage` resets these numbers along with all other statistics.

### `POST /keys`

Request: `{"name", "models"?, "rate_limit_rpm"?, "key"?}`. Without `key` the
gateway generates one: `sy-` and 40 random letters and digits. A `key` of
your own may be a literal (no spaces) or a reference (`env:NAME`).

Response: `201` with `id`, the full `key` — shown here and by `reveal`,
nowhere else — and `is_reference`. The key works as soon as the response
arrives.

{{example:key_create}}

* `400` — `name` missing, empty, longer than 100 characters; `key` with
  spaces; a wrong type; an unknown field.
* `409` — the name is taken (compared without regard to case), or the key
  already exists.

{{example:key_create_conflict}}

### `PATCH /keys/{id}`

Request: any of `{"name", "enabled", "models", "rate_limit_rpm"}`; what is
left out stays. `"rate_limit_rpm": null` removes the limit; `"models": []`
allows every model. The key's value cannot be changed (create another key).
Response: the updated entry, as in `GET /keys`.

`404` unknown id; `409` the new name is taken; `400` unknown field or wrong
type.

{{example:key_patch}}

### `POST /keys/{id}/reveal`

The key as the configuration holds it. For a reference that is the
reference text (`"env:NAME"`, `is_reference: true`), not the variable's
value. The body is ignored. `404` unknown id.

{{example:key_reveal}}

### `DELETE /keys/{id}`

The key stops working at once. `404` unknown id.

{{example:key_delete}}

---------------------------------------------------------------------------

## Usage, requests, logs

### Totals

The counter set that appears in `/status`, `/usage/summary`, time-series
points and the `stats` live frame:

`requests`, `errors` (requests that did not end ok), `input_tokens`
(uncached prompt tokens), `cache_read_tokens`, `cache_write_tokens`,
`output_tokens` (includes `reasoning_tokens`), `reasoning_tokens`, `cost`
(USD, estimated from `pricing`), `duration_ms_sum` (divide by `requests` for
the mean), `ttfb_ms_sum` over the `ttfb_count` requests that had a first
byte.

### `GET /usage/summary?range=`

`range`: `1h`, `24h` (default), `7d`, `30d`.

{{example:usage_summary}}

`latency` holds percentiles in milliseconds (`p50`, `p90`, `p95`, `p99`,
time to first byte `ttfb_p50`, `ttfb_p95`) over `window_ms`: for `7d` and
`30d` the percentiles describe the last 24 hours. `by_model`, `by_provider`
and `by_key` are totals per client-facing model, provider and client key
name, most requests first. Names that stand for "none": `unknown` (a request
that failed before it had a model or provider), `anonymous` (no client key),
`dashboard` (the playground).

### `GET /usage/timeseries?range=&bucket=&group_by=`

`range` as above. `bucket`: `auto` (default: minutes for `1h`, hours for
`24h` and `7d`, days for `30d`), `minute`, `hour`, `day` — never finer than
the stored data (minutes exist for the last 24 hours); the response says
which width was used. `group_by`: `model`, `provider`, `key`, or absent for
no breakdown (`"none"`).

`points` are contiguous buckets, oldest first; a bucket without traffic is
present with zero counters; the first and last may be partial. Each point is
the totals of its bucket plus `t` (start of the bucket) and `groups`: per
series `{requests, errors, tokens, cost}`, only for series with traffic in
that bucket. `series` lists every series name, most requests first; beyond
the 20 busiest the rest is folded into `"other"`.

The last two of the 61 points of an hour:

{{example:usage_timeseries}}

### `GET /requests?limit=&before=&model=&provider=&key=&status=&q=`

The most recent requests, newest first (by start time), from the in-memory
list.

| Parameter | Meaning |
|---|---|
| `limit` | Page size, default 50, at most 500. |
| `before` | Cursor: the `next_before` of the previous page. (A bare request id or unix-ms timestamp works too.) |
| `model` | Exact model name — requested, client-facing or upstream — ignoring case. |
| `provider` | Exact provider name, ignoring case. |
| `key` | Client key name or id, ignoring case; `anonymous` for requests without a key. |
| `status` | `ok`, `error`, an HTTP status (`429`) or a class (`5xx`). |
| `q` | Substring, ignoring case, searched in id, model names, provider, credential label, key name, endpoint and error. |

`total` counts the requests in memory that match the filters, ignoring
paging. `next_before` is `null` on the last page.

{{example:requests_list}}

A request record:

| Field | Meaning |
|---|---|
| `id` | UUIDv7; also the `x-request-id` the client received. |
| `started_at`, `finished_at`, `duration_ms`, `ttfb_ms` | Timing; `ttfb_ms` is `null` when nothing was sent before the end. |
| `client` | `key_id`, `key_name` (never the key), `ip`, `user_agent`; each `null` when unknown. |
| `client_protocol`, `endpoint`, `transport`, `stream` | How the client asked: protocol, `"POST /v1/messages"`-style label, `http` / `sse` / `websocket`, and whether it asked for a stream. |
| `requested_model`, `client_model`, `upstream_model` | The name as sent; the client-facing model it resolved to; the id sent upstream. |
| `provider`, `credential_id`, `credential_label`, `upstream_protocol` | Who served it (the last attempt). `null` when the request failed before routing. |
| `mode` | `passthrough`, `translated`, `mock`, `raw`, or `null`. |
| `status`, `ok` | The HTTP status sent to the client (`499`: the client went away). |
| `error` | `null`, or `{kind, message, upstream_status}`; `kind` is a class such as `invalid_request`, `not_found`, `rate_limit`, `upstream`, `unavailable`, `timeout`. |
| `usage` | Token counts (see [Totals](#totals)). |
| `cost` | Estimated USD, `null` without a matching price or for failures. |
| `reasoning` | The reasoning depth that was applied, as a label, or `null`. |
| `attempts` | Every upstream call, in order: `provider`, `credential_id`, `credential_label`, `upstream_model`, `upstream_protocol`, `status` (`0`: no response), `ok`, `error`, `duration_ms`. |
| `has_bodies` | Whether bodies were captured (`logging.request_log`). |

### `GET /requests/{id}`

The record — from memory, else from the usage files — and its captured
bodies, or `"bodies": null` when none were captured. Bodies are text,
truncated to `logging.request_log_max_body_kb` and redacted by the gateway;
they describe the last attempt. `client_headers` and `upstream_headers` are
redacted header maps (empty when the pipeline was given none). `404` unknown
id. (Bodies are cut to 400 characters here.)

{{example:request_detail}}

### `DELETE /usage`

Forgets all statistics: the request list, every time bucket, the usage
files on disk, and with them the `usage` of every client key (`GET /keys`).
Totals since start (`/status`, `stats`) and captured bodies are not
affected.

{{example:usage_delete}}

### `GET /logs?limit=&level=&q=&before=`

The application log's in-memory tail (the last 2000 lines), oldest first.

| Parameter | Meaning |
|---|---|
| `limit` | Lines per page, default 200. |
| `level` | Least severe level to include: `trace`, `debug`, `info`, `warn`, `error`. |
| `q` | Substring, ignoring case, searched in message, target and fields. |
| `before` | Cursor: only lines with a smaller `seq` (the `next_before` of the previous page). |

A page holds the *newest* matching lines; `next_before` leads to older ones
and is `null` when there are none. `seq` increases by one per line and is
what to de-duplicate on when combining a page with `log` live events.

{{example:logs}}

---------------------------------------------------------------------------

## Playground

### `POST /playground`

Runs one request through the real pipeline as the built-in `dashboard`
client (no model restrictions, no rate limit). It is recorded like any
request, with endpoint `POST /admin/api/playground` and key name
`dashboard`.

Request:

| Field | Meaning |
|---|---|
| `protocol` | `openai-chat`, `openai-responses`, `anthropic` or `gemini`: how `body` is read and how the answer is rendered. |
| `body` | The request body of that protocol, an object. |
| `model` | Optional for the first three: when given, it is written into `body.model`. **Required for `gemini`**, which names the model in the URL (`models/` prefix accepted). |
| `stream` | Optional. For the first three: when given, it is written into `body.stream`; otherwise `body.stream` decides. For `gemini`: `true` is `streamGenerateContent`, absent or `false` is `generateContent`. |

Response — **the protocol's own**, not the admin envelope:

* not streamed: the status, `Content-Type` and JSON body a client of that
  protocol would get, with `x-request-id` and, when an upstream was
  involved, `x-switchyard-provider` and `x-switchyard-model`;
* streamed: `200`, `content-type: text/event-stream`,
  `cache-control: no-cache`, `x-accel-buffering: no`, and the protocol's
  server-sent events exactly as the client API sends them — including its
  terminator (`data: [DONE]`, `message_stop`, …) and `: keep-alive` comment
  lines after `streaming.keepalive_secs` of silence. Gemini streams are
  always SSE here. Closing the connection cancels the upstream call;
* errors of the pipeline (unknown model, upstream failure, every credential
  resting): the protocol's error body with its status. A streaming request
  that fails before its first event gets such a JSON error, not a stream;
  a failure after the first event arrives inside the stream, in the
  protocol's in-stream error shape.

So a `2xx` is JSON or SSE depending on `Content-Type`, and a non-`2xx` body
is the protocol's error — *except* for the admin API's own refusals, which
keep the admin shape: `400` for a bad envelope (unknown `protocol`, `body`
not an object, `model` missing for `gemini`, unknown field), `413`, and the
[access](#access) errors. `ui/js/lib/api.js` reads `error.message` from
either shape.

{{example:playground_chat}}

{{example:playground_gemini}}

A stream:

{{example:playground_stream}}

A pipeline error, in the protocol's shape:

{{example:playground_error}}

A bad envelope, in the admin shape:

{{example:playground_bad_envelope}}

---------------------------------------------------------------------------

## Live events (WebSocket)

### `GET /ws?ticket=<ticket>`

Browsers cannot set headers on a WebSocket, so this one route is
authenticated by a ticket from `POST /ws-ticket` instead of the secret.
The loopback rule applies as everywhere. Before the upgrade:

* `401` — the ticket is missing, unknown, expired or already used (each
  connection needs a fresh one);
* `403` — remote peer while remote access is off;
* `404` — admin interface off;
* `400` — a good ticket on a request that is not a WebSocket upgrade (the
  ticket is used up anyway);
* `503` — the gateway is shutting down.

Every message, both ways, is a JSON text frame.

**Server → client:** `{"type": <string>, "data": <object>}`.

| `type` | When | `data` |
|---|---|---|
| `hello` | Once, first. | `version`, `topics` (every type that can be subscribed to), `server_time`. |
| `stats` | Right after `hello`, then once a second. | See below. |
| `request.started` | A client request began. | The first part of a request record: `id`, `started_at`, `client`, `client_protocol`, `endpoint`, `transport`, `stream`, `requested_model`. |
| `request.finished` | It ended. One per `request.started`, same `id`. | The full [request record](#get-requestslimitbeforemodelproviderkeystatusq). |
| `log` | An application log line. | `seq`, `at`, `level`, `target`, `message`, `fields` — as in `GET /logs`. |
| `credential` | A credential may have changed state: a failed upstream attempt, a provider test. | `provider` and `credential`: the runtime part of a [credential entry](#the-provider-view) (no `source`, `index`, `proxy`, `service_account_file`; absent values are omitted rather than `null`). |
| `config.reloaded` | A configuration was applied (`ok: true`) or refused (`ok: false`, e.g. a broken edit of the file or a failed reload). | `at`, `ok`, `message`. Reload whatever depends on the configuration. |
| `subscribed` | Answer to `subscribe`. | `topics`: the set now in force. |
| `lagged` | This connection could not keep up. | `missed`: events dropped for it. The stream continues with newer events; refetch lists if completeness matters. |
| `pong` | Answer to `ping`. | None: the frame is `{"type":"pong"}`. |

**Client → server:**

* `{"type": "subscribe", "topics": ["log", "stats"]}` — replaces the set of
  frame types this connection receives. Initially: everything. `hello` and
  `stats` are topics like the others; `subscribed`, `lagged` and `pong` are
  always delivered. Unknown topic names are accepted (and match nothing);
  `[]` silences the stream.
* `{"type": "ping"}` — answered with `{"type": "pong"}`.
* Anything else is ignored.

The gateway also sends a WebSocket ping every 20 seconds and drops a client
that has been silent for 50 (browsers answer pings on their own). It closes
the socket with code `1001` when it shuts down, and with `1008` when the
admin interface is switched off, the secret changes, or remote access is
withdrawn from a remote peer — within a second of the change. A slow
client never slows the gateway: events it does not take in time are dropped
for it and reported as `lagged`.

{{example:ws_hello}}

`stats`: `in_flight`, `active_streams` and `ws_connections` are gauges
(`ws_connections` counts the client API's WebSockets, not dashboard
connections); `rpm` and `tpm` are requests and tokens finished in the last
60 seconds, `error_rate_1m` the share of those that failed; `p50_ms` and
`p95_ms` are request durations over the last hour; `totals` are the
[totals](#totals) since start.

{{example:ws_stats}}

{{example:ws_request_started}}

{{example:ws_request_finished}}

{{example:ws_log}}

{{example:ws_credential}}

{{example:ws_config_reloaded}}

{{example:ws_subscribed}}

{{example:ws_lagged}}

{{example:ws_pong}}

---------------------------------------------------------------------------

## Dashboard files

| Request | Answer |
|---|---|
| `GET /admin` | `308` to `admin/` (relative, so a path prefix in front of the gateway survives). |
| `GET /admin/` | `index.html`. |
| `GET /admin/<path>` | That file of `ui/`, or `404`. There is no fallback to `index.html`: the dashboard routes by URL hash. |

* **No authentication, no loopback rule**: the sign-in page must load, and
  the files hold no data. They are served to whoever can reach the
  listener, as long as the admin interface is enabled, has a secret and
  `admin.ui` is on; otherwise every path is `404` (the API is unaffected by
  `admin.ui`).
* `GET` and `HEAD` only.
* `Content-Type` by extension (`text/javascript`, `text/css`, `text/html`
  with `charset=utf-8`; `image/svg+xml`; `font/woff2`; `application/json`).
* `ETag` (from the file's SHA-256) and `Cache-Control: no-cache`:
  browsers revalidate on every load and get `304` while the binary is the
  same.
* Every answer carries
  `Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self' ws: wss:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'`,
  `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer` and
  `X-Frame-Options: DENY`. Pages therefore cannot use inline scripts,
  `eval`, or resources from other origins.
* Not embedded and never served: `tests/`, `*.md`, `package.json`,
  dotfiles. Paths may only consist of letters, digits, `-`, `_`, `.` and
  `/`.
* Release builds serve the files embedded in the binary; debug builds read
  `ui/` from disk on every request.

---------------------------------------------------------------------------

## Route index

All under `/admin/api`.

| Method | Path | Request body | Success |
|---|---|---|---|
| `GET` | `/status` | – | status object |
| `POST` | `/login` | ignored | `{"ok": true}` |
| `POST` | `/ws-ticket` | ignored | `{"ticket", "expires_in"}` |
| `GET` | `/ws?ticket=` | – | WebSocket |
| `GET` | `/config` | – | `{"config", "path", "restart_required"}` |
| `GET` | `/config/raw` | – | `{"text", "path", "modified_at"}` |
| `PUT` | `/config/raw` | `{"text"}` | as `GET /config` |
| `POST` | `/config/validate` | `{"text"}` | `{"ok", "issues"}` |
| `PATCH` | `/settings` | merge patch | as `GET /config` |
| `POST` | `/reload` | ignored | as `GET /config` |
| `GET` | `/providers` | – | `[provider view]` |
| `POST` | `/providers` | provider entry | `201` provider view |
| `GET` | `/providers/{name}` | – | provider view |
| `PUT` | `/providers/{name}` | provider entry | provider view |
| `DELETE` | `/providers/{name}` | – | `{"ok": true}` |
| `POST` | `/providers/{name}/test` | `{"model"?}` or none | test result |
| `POST` | `/providers/{name}/discover` | ignored | `{"models": [ModelInfo]}` |
| `POST` | `/credentials/{id}/reset` | ignored | `{"ok": true}` |
| `POST` | `/credentials/{id}/enable` | ignored | provider view |
| `POST` | `/credentials/{id}/disable` | ignored | provider view |
| `GET` | `/models` | – | `[model entry]` |
| `GET` | `/catalog` | – | `[catalog entry]` |
| `GET` | `/aliases` | – | `[alias]` |
| `PUT` | `/aliases` | `[alias]` | `[alias]` |
| `GET` | `/payload` | – | `{"default", "override", "filter"}` |
| `PUT` | `/payload` | `{"default"?, "override"?, "filter"?}` | as `GET` |
| `GET` | `/pricing` | – | `[price]` |
| `PUT` | `/pricing` | `[price]` | `[price]` |
| `GET` | `/keys` | – | `[key entry]` |
| `POST` | `/keys` | `{"name", "models"?, "rate_limit_rpm"?, "key"?}` | `201` `{"id", "key", "is_reference"}` |
| `PATCH` | `/keys/{id}` | `{"name"?, "enabled"?, "models"?, "rate_limit_rpm"?}` | key entry |
| `DELETE` | `/keys/{id}` | – | `{"ok": true}` |
| `POST` | `/keys/{id}/reveal` | ignored | `{"key", "is_reference"}` |
| `GET` | `/usage/summary` | – | summary |
| `GET` | `/usage/timeseries` | – | time series |
| `DELETE` | `/usage` | – | `{"ok": true}` |
| `GET` | `/requests` | – | `{"items", "next_before", "has_more", "total"}` |
| `GET` | `/requests/{id}` | – | `{"record", "bodies"}` |
| `GET` | `/logs` | – | `{"lines", "next_before", "has_more"}` |
| `POST` | `/playground` | `{"protocol", "body", "model"?, "stream"?}` | the protocol's JSON or SSE |
