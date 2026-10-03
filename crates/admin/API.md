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
  silently ignored. Unknown *query* parameters are ignored when given once.
  On every API route, a repeated parameter (including an unknown one),
  malformed percent encoding or invalid UTF-8 is answered `400`. A known
  parameter with a value that cannot be read is also `400`, naming the
  parameter in `message` and as the `path` of the one issue
  (`{"path": "since", "message": "must be a whole number of unix milliseconds"}`).
  Names are decoded before checking for repeats (`x=1&%78=2` repeats `x`).
  Omitted parameters use the defaults below; an empty number, cursor or
  enum value is invalid. Empty text filters mean no filter. Valid numeric
  values retain the documented clamps. Availability and remote-access
  checks run first. REST queries are checked after secret authentication;
  WebSocket queries are checked before redeeming their ticket, so a
  malformed query does not consume it.
* **Responses are never to be cached**: every API response carries
  `cache-control: no-store` (the playground's event stream: `no-cache`) and
  `x-content-type-options: nosniff`.

### Errors

Every error of the admin API has this body, whatever the status:

```json
{"error": {"message": "…", "issues": [{"path": "…", "message": "…"}]}}
```

`message` is a sentence for the operator and is always present; it quotes
the first issues, so it says what is wrong on its own. `issues` is present
whenever a field can be named; each names a field (`path`) and what is wrong
with it (`message`, a fragment that reads after the path: `must not be
empty`, `expected a list, got a string`).

**Issue paths: one rule.** A path is the field's place **in the request
body**, whatever the status (`400`, `409`, `422`) and whoever found the
problem:

| The body is | Paths look like |
|---|---|
| a provider entry (`POST /providers`, `PUT /providers/{name}`) | `name`, `base_url`, `headers.X-Team`, `api_keys[1]`, `credentials[0].api_key`, `models[2].thinking` |
| a client key (`POST /keys`, `PATCH /keys/{id}`) | `name`, `key`, `rate_limit_rpm` |
| a list (`PUT /aliases`, `PUT /pricing`) | `[0].name`, `[0].targets[1]`, `[2]` |
| the payload rules (`PUT /payload`) | `override[0].set.response_format`, `default[1].models` |
| a settings patch (`PATCH /settings`) | `server.port`, `routing.cooldown.auth_secs` |
| a whole file (`PUT /config/raw`, `POST /config/validate`, `POST /reload`) | the place in the configuration: `providers[1].base_url`, `aliases[0].targets[1]`; for TOML syntax errors `line L, column C` |

An issue about the body as a whole has the path `""`. Two kinds of issue are
not about the body and say so by their path: the issues of the `409` for an
invalid file on disk are the *file's* (as in the last row), and an issue
about a part of the configuration the request did not send — which a request
cannot normally cause — keeps its place in the whole configuration
(`server.port` in the answer to a `PUT /aliases`).

**Shape errors (`400`)** say which shape is expected, in plain words:
`models[0]: expected an object such as {"id": "model-name"}, got a string`,
`rate_limit_rpm: must be a whole number from 1 to 4294967295`,
`websocket: unknown field `websocket``, `name: is required`,
`kind: must be one of `openai`, `anthropic`, …`. They never repeat the value
that was sent. A body that is not JSON at all is answered with
`the request body is not valid JSON: line L, column C: …`.

**Shape errors in a file** — the `422` of `PUT /config/raw`, the verdict of
`POST /config/validate`, a refused `POST /reload`, the file's issues in the
`409` above and the message of a `config.reloaded` event with `ok: false` —
are reported at their `line L, column C` and say what is expected in TOML's
words: `invalid type: string "•", expected a table such as { id =
"model-name" }` (model names where model tables go), ``invalid value:
integer `99999`, expected a whole number from 0 to 65535``, `invalid type:
string "sk-pro…cdef", expected an array`, `expected a string holding one of
the accepted names`. What was found is shown masked (strings always, numbers
of more than five digits, unknown names of more than twenty characters), so
a key on the broken line is not repeated.

| Status | Meaning |
|---|---|
| `400` | The request is malformed: not JSON, a missing or unknown field, a wrong type, a field that may not be changed here; or a query parameter given twice or with a value the route refuses (see [Conventions](#conventions)). `issues` names the field — or the query parameter — when one can be named. |
| `401` | No secret, a wrong secret, or (on `/ws`) a missing, used, expired or revoked ticket. Carries `WWW-Authenticate: Bearer`. |
| `403` | The peer is remote and remote access is off. |
| `404` | No such route, provider, credential, key or request — or the admin interface is off (see [Access](#access)). |
| `405` | The route exists but not with this method. |
| `409` | A name or key that already exists: `issues` names the field (`name`, `key`). Or the configuration file on disk is not valid and the edit would overwrite it (see [Configuration edits](#configuration-edits)): `issues` are then the file's. (Also, without `issues`: a credential that another edit changed while it was being switched on or off.) |
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
saved as the key), and so is one that could stand for several stored
secrets which cannot be told apart (see
[`PUT /providers/{name}`](#put-providersname)). The dashboard can therefore
send back what it was shown.
Two exceptions, both about removing a provider's key:

* an empty row in a provider's `api_keys` list is a *removed* key, not a
  kept one;
* `"api_key": null` in a provider's `credentials[]` entry means **this
  credential has no key** — where `""` (or leaving the field out) would
  keep the key stored for the credential at that place or under that label.
  See [`PUT /providers/{name}`](#put-providersname).

### Configuration edits

Every mutation is written to `switchyard.toml` through the configuration
store: comments, order and formatting of the file are preserved and only the
values that changed are touched; concurrent edits are applied one after the
other, each on the result of the previous one; a result that does not
validate is refused with `422` and changes neither the file nor the running
gateway. The same goes for a value a TOML file cannot hold, wherever it is
sent: JSON `null` inside free-form values (the `set` of a payload rule) and
integers from 9223372036854775808 to 18446744073709551615 are refused with
`422`, each named in `issues` by its place in the request body
(`override[0].set.response_format` for `PUT /payload`,
`routing.max_wait_secs` for `PATCH /settings`). A whole number larger still
is not an integer to a JSON reader but a floating-point number, and is
treated as one: in a free-form value it is accepted and stored as a float
(`99999999999999999999` comes back as `1e20`), and a setting that takes a
whole number refuses it with `400` (`expected a whole number from 0 to
18446744073709551615, got a number`).
A mutation answers once the gateway runs on the new configuration,
so what it returns — and what the next `GET` returns — is already in effect.
Each applied change is also announced as a `config.reloaded` live event.

**A broken file is never overwritten.** When the file on disk holds
something other than the configuration in effect and that something is not
valid — a manual edit in progress, or one the gateway refused — every
mutation that would change the file is refused with `409`: writing it would
rebuild the file from the last valid configuration and discard what was
typed. The `message` says so and what to do (fix or restore the file, or
replace it as a whole with `PUT /config/raw`, which is the one mutation that
is not refused); `issues` lists what is wrong with the file, by its place in
the file. A mutation that changes nothing is not refused. Once the file is
valid again — equal to the configuration in effect, or a new valid one —
this is announced with `config.reloaded` (`ok: true`) and edits work again.
A file the gateway has refused (the watcher's verdict, announced with
`config.reloaded` `ok: false`, or a refused `POST /reload`) is also
reported by `config_rejected` and `warnings` of [`GET /status`](#get-status)
and by `config_rejected` of `GET /config`, for as long as it lasts, so a
page loaded later knows too.

A settings patch while the file ends in a half-typed provider entry:

```http
PATCH /admin/api/settings

{
  "logging": {
    "level": "info"
  }
}
```

```http
HTTP/1.1 409 Conflict

{
  "error": {
    "message": "the configuration file on disk is not valid, so the change was not saved (the file was left as it is); fix or restore the file, or replace it as a whole on the raw tab (PUT /config/raw). What is wrong with the file: line 50, column 8: string values must be quoted, expected literal string",
    "issues": [
      {
        "path": "line 50, column 8",
        "message": "string values must be quoted, expected literal string"
      }
    ]
  }
}
```

**Cost.** Applying a change costs the same per provider however many
providers there are: an edit of one provider does not ask the other
providers' upstreams for their model lists (see `discovery` in
[the provider view](#the-provider-view)) and does not rebuild anything once
per provider. With 300 providers every mutation answers in well under half a
second on a debug build.

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
   10 000 addresses and forgets an unlocked address two hours after its last
   failure. Only unlocked entries can be evicted to make room. When all
   10 000 entries are actively locked, requests from unknown addresses also
   get `429`, with `Retry-After` until the earliest lockout expires; active
   lockouts are never discarded to admit new addresses.
   The address is that of the TCP peer. What a reverse proxy on this
   machine relays (see 2) is counted apart from what connects to the
   gateway directly on loopback: wrong secrets sent through the proxy never
   lock out someone signing in on the machine itself, nor the other way
   round. All clients of that proxy share one count, though — the address
   the proxy reports in its header is not used, since a client can send
   that header too — so remote sign-in through a proxy can be locked for
   everybody by one client's wrong guesses; signing in directly on the
   machine keeps working unless the capacity throttle above applies. The
   same holds for any other arrangement in which
   many clients arrive from one address (NAT, a container's port mapping).
4. **The secret.** `Authorization: Bearer <secret>` (the bare secret without
   `Bearer` is accepted too), or `x-admin-secret: <secret>`. The comparison
   is constant-time over the raw bytes; a secret that is not ASCII is sent
   as its UTF-8 bytes. `GET /ws` alone takes a ticket instead (see
   [Live events](#live-events-websocket)).

The effective secret is `SWITCHYARD_ADMIN_SECRET` when set, else
`admin.secret`. Changing `admin.secret` takes effect at once.

A wrong secret:

```http
GET /admin/api/status
```

```http
HTTP/1.1 401 Unauthorized
www-authenticate: Bearer

{
  "error": {
    "message": "the admin secret is not correct"
  }
}
```

A relayed request (here: `X-Forwarded-For` on a loopback connection) while
remote access is off:

```http
GET /admin/api/status
```

```http
HTTP/1.1 403 Forbidden

{
  "error": {
    "message": "remote admin access is disabled"
  }
}
```

The fifth wrong secret in a row, and every request for the next 30 minutes:

```http
GET /admin/api/status
```

```http
HTTP/1.1 429 Too Many Requests
retry-after: 1800

{
  "error": {
    "message": "too many failed sign-in attempts from this address; try again in 30 minutes"
  }
}
```

An unknown route (with the secret; without it the answer is `401`, so
routes cannot be probed):

```http
GET /admin/api/nothing-here
```

```http
HTTP/1.1 404 Not Found

{
  "error": {
    "message": "no such admin API route"
  }
}
```

---------------------------------------------------------------------------

## Session and status

### `POST /login`

Checks the secret and nothing else. The body is ignored. This is what the
sign-in page calls; `401`, `403`, `404` and `429` mean what [Access](#access)
says.

```http
POST /admin/api/login

{}
```

```http
HTTP/1.1 200 OK

{
  "ok": true
}
```

### `GET /status`

```http
GET /admin/api/status
```

```http
HTTP/1.1 200 OK

{
  "version": "0.1.0",
  "started_at": 1791004027826,
  "uptime_ms": 144,
  "config_path": "/etc/switchyard/switchyard.toml",
  "data_dir": "/etc/switchyard/data",
  "listen": "127.0.0.1:8317",
  "tls": false,
  "restart_required": [],
  "command_line_overrides": [],
  "config_rejected": null,
  "warnings": [],
  "counts": {
    "providers": 2,
    "credentials": 3,
    "credentials_ready": 3,
    "models": 9,
    "client_keys": 1
  },
  "live": {
    "started_at": 1791004027826,
    "uptime_ms": 144,
    "in_flight": 0,
    "active_streams": 0,
    "ws_connections": 0,
    "totals": {
      "requests": 3,
      "errors": 1,
      "input_tokens": 6,
      "cache_read_tokens": 0,
      "cache_write_tokens": 0,
      "output_tokens": 68,
      "reasoning_tokens": 44,
      "cost": 0.00010499999999999999,
      "duration_ms_sum": 64,
      "ttfb_ms_sum": 63,
      "ttfb_count": 2
    }
  },
  "admin": {
    "allow_remote": false,
    "remote": false
  },
  "auth_required": true
}
```

| Field | Meaning |
|---|---|
| `version` | The gateway's version. |
| `started_at`, `uptime_ms` | When the process started, and for how long it has run. |
| `config_path`, `data_dir` | Absolute paths of the configuration file and the data directory (`null` when there is none). |
| `listen` | The address the server is really bound to — also when `--host` / `--port` on the command line override the file — or `null` when unknown. It may be a wildcard (`0.0.0.0:8317`). |
| `tls` | Whether that listener serves HTTPS (`server.tls` was set when it was bound): the scheme to put in front of `listen`. |
| `restart_required` | Settings that were changed since start and only take effect after a restart: any of `server.host`, `server.port`, `server.tls`, `server.data_dir` — except those in `command_line_overrides`, which a restart with the same command line does not apply. |
| `command_line_overrides` | Settings the command line fixes whatever the file says: `server.host` when the gateway was started with `--host`, `server.port` with `--port`. The file's value of such a setting can be changed and is saved, but neither now nor after a restart with the same command line does it take effect; `listen` says what is in use. `[]` without such flags. |
| `config_rejected` | `null` while the file on disk is in effect. When the gateway refused the file — a hand edit that does not validate, picked up by the file watcher or by `POST /reload` — `{"at", "message", "issues"}`: when (Unix ms), a sentence that says so and quotes the first issues, and every issue of the file (by its place in the file). It stays set for as long as the file holds that content, also across page loads, and goes back to `null` once the file is valid again (fixed, restored, or replaced with `PUT /config/raw`). Meanwhile the previous configuration stays in effect and every edit that would change the file is refused with `409` (see [Configuration edits](#configuration-edits)). |
| `warnings` | Problems that need the operator's attention, as plain sentences: first, while `config_rejected` is set, its `message` (it starts with `configuration file:`); then problems that do not make the configuration invalid — credentials whose secret cannot be resolved or whose service-account file cannot be used, alias targets that match no model, shadowed names. |
| `counts.providers`, `counts.client_keys` | Entries in the configuration (disabled providers included). |
| `counts.credentials`, `counts.credentials_ready` | Upstream credentials in service, and those of them with status `ready`. A credential with status `disabled` is in neither number, whatever switched it off (`disabled_by`): its provider (`enabled = false`), its own entry, or a switch at runtime. A credential that is `unusable` or `cooling` is in service, and counted in the first. |
| `counts.models` | Client-facing names a request can be routed by: the entries of [`GET /models`](#get-models) that are not `ignored`. Names hidden from client listings count (they are routable); an alias without a routable target does not. |
| `live` | Gauges: requests in flight, open streams, open client WebSockets (the client API's, not dashboard connections), and `totals` since start (see [Totals](#totals)). |
| `admin.allow_remote` | Whether remote peers are admitted (configuration or environment). |
| `admin.remote` | Whether *this* request counted as remote. |
| `auth_required` | `auth.required`: whether the client API demands a key. |

The same while the file on disk ends in a provider entry that does not
validate (refused by `POST /reload` here):

```http
GET /admin/api/status
```

```http
HTTP/1.1 200 OK

{
  "version": "0.1.0",
  "started_at": 1791004027826,
  "uptime_ms": 193,
  "config_path": "/etc/switchyard/switchyard.toml",
  "data_dir": "/etc/switchyard/data",
  "listen": "127.0.0.1:8317",
  "tls": false,
  "restart_required": [
    "server.port"
  ],
  "command_line_overrides": [],
  "config_rejected": {
    "at": 1791004028017,
    "message": "configuration file: the file on disk was refused and is not in effect; the gateway keeps running on the last valid configuration until the file is fixed: line 50, column 8: string values must be quoted, expected literal string",
    "issues": [
      {
        "path": "line 50, column 8",
        "message": "string values must be quoted, expected literal string"
      }
    ]
  },
  "warnings": [
    "configuration file: the file on disk was refused and is not in effect; the gateway keeps running on the last valid configuration until the file is fixed: line 50, column 8: string values must be quoted, expected literal string"
  ],
  "counts": {
    "providers": 2,
    "credentials": 3,
    "credentials_ready": 3,
    "models": 9,
    "client_keys": 1
  },
  "live": {
    "started_at": 1791004027826,
    "uptime_ms": 193,
    "in_flight": 0,
    "active_streams": 0,
    "ws_connections": 0,
    "totals": {
      "requests": 3,
      "errors": 1,
      "input_tokens": 6,
      "cache_read_tokens": 0,
      "cache_write_tokens": 0,
      "output_tokens": 68,
      "reasoning_tokens": 44,
      "cost": 0.00010499999999999999,
      "duration_ms_sum": 64,
      "ttfb_ms_sum": 63,
      "ttfb_count": 2
    }
  },
  "admin": {
    "allow_remote": false,
    "remote": false
  },
  "auth_required": true
}
```

### `POST /ws-ticket`

Sells a ticket for `GET /ws`: 32 random bytes, URL-safe, valid once and for
`expires_in` seconds. The body is ignored. At most 1024 tickets are
outstanding; beyond that the one closest to expiry is dropped. A ticket is
bound to the effective admin secret that issued it: after that secret
changes, an unused ticket from the old secret is refused. Existing live
sessions also close when they observe the changed secret.

```http
POST /admin/api/ws-ticket

{}
```

```http
HTTP/1.1 200 OK

{
  "ticket": "gTIJDmjLVhqHPpxAJiRjVC3APvlguE2mBGQFGKkkceM",
  "expires_in": 30
}
```

---------------------------------------------------------------------------

## Configuration

### `GET /config`

The live configuration with every secret masked, the file's path, the
settings waiting for a restart, and — as in [`GET /status`](#get-status) —
`command_line_overrides` and `config_rejected` (the file on disk, while the
gateway refuses it). Every mutation that returns the whole configuration
answers in this shape. `config` is the configuration schema of
`switchyard.toml` as JSON; sections and fields at their default *are*
present for the scalar sections (`server` … `usage`), while empty lists
(`providers`, `aliases`, `pricing`, `auth.keys`), an empty `payload` and
per-entry defaults are omitted, as in the file.

It is the *file's* configuration: `server.host` and `server.port` are what
the file says, also while `--host` / `--port` on the command line override
them. The address in use is `listen` in [`GET /status`](#get-status). Such
an overridden setting is named in `command_line_overrides`; changing it in
the file is saved but is not listed in `restart_required`, since a restart
with the same command line keeps the command line's value.

```http
GET /admin/api/config
```

```http
HTTP/1.1 200 OK

{
  "config": {
    "server": {
      "host": "127.0.0.1",
      "port": 8317,
      "body_limit_mb": 64,
      "cors": true,
      "data_dir": "data"
    },
    "admin": {
      "enabled": true,
      "secret": "s3cr3t…0001",
      "allow_remote": false,
      "ui": true
    },
    "auth": {
      "required": true,
      "keys": [
        {
          "key": "sy-Zk3…tQwE",
          "name": "laptop"
        }
      ]
    },
    "routing": {
      "strategy": "round-robin",
      "session_affinity": true,
      "session_affinity_ttl_secs": 3600,
      "force_model_prefix": false,
      "max_attempts": 3,
      "max_wait_secs": 0,
      "cooldown": {
        "enabled": true,
        "rate_limit_base_secs": 1,
        "rate_limit_max_secs": 1800,
        "transient_secs": 60,
        "auth_secs": 1800,
        "quota_secs": 3600,
        "model_not_found_secs": 43200
      }
    },
    "streaming": {
      "keepalive_secs": 15,
      "bootstrap_retries": 2,
      "idle_timeout_secs": 300
    },
    "upstream": {
      "proxy": "direct",
      "connect_timeout_secs": 30,
      "request_timeout_secs": 600,
      "passthrough_headers": true
    },
    "logging": {
      "level": "info",
      "file": false,
      "max_total_size_mb": 200,
      "request_log": "all",
      "request_log_max_body_kb": 256
    },
    "usage": {
      "enabled": true,
      "persist": true,
      "retention_days": 30
    },
    "providers": [
      {
        "name": "mock",
        "kind": "mock"
      },
      {
        "name": "vendor",
        "kind": "openai-compat",
        "base_url": "http://127.0.0.1:9/v1",
        "api_keys": [
          "sk-liv…6b5c"
        ],
        "credentials": [
          {
            "api_key": "sk-liv…4c5d",
            "label": "team",
            "weight": 2
          }
        ],
        "models": [
          {
            "id": "vendor-large",
            "alias": "large"
          }
        ],
        "discover": false
      }
    ],
    "pricing": [
      {
        "model": "mock-*",
        "input": 0.5,
        "output": 1.5
      }
    ]
  },
  "path": "/etc/switchyard/switchyard.toml",
  "restart_required": [],
  "command_line_overrides": [],
  "config_rejected": null
}
```

### `GET /config/raw`

The file exactly as it is on disk — comments, secrets and all. `modified_at`
is the file's modification time (`null` if the file system cannot tell).
`500` when the file cannot be read.

```http
GET /admin/api/config/raw
```

```http
HTTP/1.1 200 OK

{
  "text": "# Switchyard configuration.\n\n[admin]\nsecret = \"s3cr3t-admin-secret-change-me-0001\"\n\n[upstream]\nproxy = \"direct\"\n\n[logging]\nrequest_log = \"all\"\n\n[[auth.keys]]\nkey = \"sy-Zk3vTq8LmW2xYb7NcR5dHs9JfP4gAe6Uo1iKtQwE\"\nname = \"laptop\"\n\n# The built-in fake models.\n[[providers]]\nname = \"mock\"\nkind = \"mock\"\n\n[[providers]]\nname = \"vendor\"\nkind = \"openai-compat\"\nbase_url = \"http://127.0.0.1:9/v1\"\ndiscover = false\napi_keys = [\"sk-live-4f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c\"]\n\n[[providers.credentials]]\napi_key = \"sk-live-0a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d\"\nlabel = \"team\"\nweight = 2\n\n[[providers.models]]\nid = \"vendor-large\"\nalias = \"large\"\n\n[[pricing]]\nmodel = \"mock-*\"\ninput = 0.5\noutput = 1.5\n",
  "path": "/etc/switchyard/switchyard.toml",
  "modified_at": 1791004027820
}
```

### `PUT /config/raw`

Request: `{"text": "<the whole file>"}`. The text is validated, written
verbatim and applied. Response: the same shape as `GET /config`
(`providers` is cut to its first entry here).

```http
PUT /admin/api/config/raw

{
  "text": "# Switchyard configuration.\n\n[admin]\nsecret = \"s3cr3t-admin-secret-change-me-0001\"\n\n[upstream]\nproxy = \"direct\"\n\n[logging]\nrequest_log = \"all\"\n\n[[auth.keys]]\nkey = \"sy-Zk3vTq8LmW2xYb7NcR5dHs9JfP4gAe6Uo1iKtQwE\"\nname = \"laptop\"\n\n# The built-in fake models.\n[[providers]]\nname = \"mock\"\nkind = \"mock\"\n\n[[providers]]\nname = \"vendor\"\nkind = \"openai-compat\"\nbase_url = \"http://127.0.0.1:9/v1\"\ndiscover = false\napi_keys = [\"sk-live-4f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c\"]\n\n[[providers.credentials]]\napi_key = \"sk-live-0a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d\"\nlabel = \"team\"\nweight = 2\n\n[[providers.models]]\nid = \"vendor-large\"\nalias = \"large\"\n\n[[pricing]]\nmodel = \"mock-*\"\ninput = 0.5\noutput = 1.5\n\n[routing]\nstrategy = \"fill-first\"\n"
}
```

```http
HTTP/1.1 200 OK

{
  "config": {
    "server": {
      "host": "127.0.0.1",
      "port": 8317,
      "body_limit_mb": 64,
      "cors": true,
      "data_dir": "data"
    },
    "admin": {
      "enabled": true,
      "secret": "s3cr3t…0001",
      "allow_remote": false,
      "ui": true
    },
    "auth": {
      "required": true,
      "keys": [
        {
          "key": "sy-Zk3…tQwE",
          "name": "laptop"
        }
      ]
    },
    "routing": {
      "strategy": "fill-first",
      "session_affinity": true,
      "session_affinity_ttl_secs": 3600,
      "force_model_prefix": false,
      "max_attempts": 3,
      "max_wait_secs": 0,
      "cooldown": {
        "enabled": true,
        "rate_limit_base_secs": 1,
        "rate_limit_max_secs": 1800,
        "transient_secs": 60,
        "auth_secs": 1800,
        "quota_secs": 3600,
        "model_not_found_secs": 43200
      }
    },
    "streaming": {
      "keepalive_secs": 15,
      "bootstrap_retries": 2,
      "idle_timeout_secs": 300
    },
    "upstream": {
      "proxy": "direct",
      "connect_timeout_secs": 30,
      "request_timeout_secs": 600,
      "passthrough_headers": true
    },
    "logging": {
      "level": "info",
      "file": false,
      "max_total_size_mb": 200,
      "request_log": "all",
      "request_log_max_body_kb": 256
    },
    "usage": {
      "enabled": true,
      "persist": true,
      "retention_days": 30
    },
    "providers": [
      {
        "name": "mock",
        "kind": "mock"
      }
    ],
    "pricing": [
      {
        "model": "mock-*",
        "input": 0.5,
        "output": 1.5
      }
    ]
  },
  "path": "/etc/switchyard/switchyard.toml",
  "restart_required": [],
  "command_line_overrides": [],
  "config_rejected": null
}
```

An invalid text is refused with `422`; nothing is written:

```http
PUT /admin/api/config/raw

{
  "text": "[routing]\nmax_attempts = 0\n"
}
```

```http
HTTP/1.1 422 Unprocessable Entity

{
  "error": {
    "message": "the configuration is not valid: routing.max_attempts: must be at least 1",
    "issues": [
      {
        "path": "routing.max_attempts",
        "message": "must be at least 1"
      }
    ]
  }
}
```

The text replaces **everything**, and nothing asks twice: a client that
offers a raw editor should show what will change before sending. One kind of
text is refused with `422`, because it would lock out the dashboard that sent
it: a text that switches the admin interface off — `admin.enabled = false`,
or no usable `admin.secret` while none comes from `SWITCHYARD_ADMIN_SECRET`:
none at all, or a reference to a variable that is not set (or empty) for the
gateway, which the issue names. An empty text is such a text (every setting
at its default, no secret). The issue names `admin.enabled` or
`admin.secret`; `PATCH /settings` refuses the same changes with the same
issues, and `POST /config/validate` reports them. These are checked once the
text is otherwise valid: a text with other issues is refused with those. To
switch the admin interface off, edit the file itself.

### `POST /config/validate`

Request: `{"text": "<a whole file>"}`. Always `200`; the verdict is in the
body. Nothing is written or applied. Issue messages never quote values from
the text (a key pasted into the wrong place is not echoed).

The verdict is the one [`PUT /config/raw`](#put-configraw) would give, issue
for issue and word for word: the text's issues, or — for a text that is
otherwise valid — what saving it would lock out (`admin.enabled = false`, no
usable admin secret; see above). `ok: true` therefore means Save would
succeed.

```http
POST /admin/api/config/validate

{
  "text": "[server]\nport = 9000\n"
}
```

```http
HTTP/1.1 200 OK

{
  "ok": false,
  "issues": [
    {
      "path": "admin.secret",
      "message": "is missing: saving this would leave the admin interface without a secret and lock the dashboard out; keep an admin secret, or edit the configuration file itself to switch the admin interface off"
    }
  ]
}
```

```http
POST /admin/api/config/validate

{
  "text": "[server]\nport = 0\n\n[[providers]]\nname = \"My Provider\"\nkind = \"openai-compat\"\n"
}
```

```http
HTTP/1.1 200 OK

{
  "ok": false,
  "issues": [
    {
      "path": "server.port",
      "message": "must be between 1 and 65535"
    },
    {
      "path": "providers[0].name",
      "message": "may only contain lowercase letters, digits, `-` and `_`"
    },
    {
      "path": "providers[0].base_url",
      "message": "is required for this provider kind"
    }
  ]
}
```

A valid text whose admin secret names a variable the gateway does not have:

```http
POST /admin/api/config/validate

{
  "text": "[admin]\nsecret = \"env:DASHBOARD_SECRET\"\n"
}
```

```http
HTTP/1.1 200 OK

{
  "ok": false,
  "issues": [
    {
      "path": "admin.secret",
      "message": "names the environment variable `DASHBOARD_SECRET`, which is not set (or empty) for the gateway: saving this would leave the admin interface without a secret and lock the dashboard out; set the variable and restart first, or keep an admin secret, or edit the configuration file itself to switch the admin interface off"
    }
  ]
}
```

### `PATCH /settings`

A JSON merge patch (RFC 7396) over the scalar sections of the configuration:
`server`, `admin`, `routing` (with `routing.cooldown`), `streaming`,
`upstream`, `logging`, `usage`, and of `auth` only `auth.required`. Objects
merge key by key; `null` resets a field — or a whole table such as
`routing.cooldown` — to its default by **taking it out of the file**, so the
default applies, and a later release's default would too; nothing is written
in its place. The key goes with the comment on its line and the comment
lines directly above it (a paragraph set apart by a blank line stays, as for
any key an edit removes), and a table left with no keys loses its header.
A default spelled out in the file (`port = 8317`) is taken out too, although
the configuration does not change. Response: the same shape as `GET /config`
(`providers` is cut to its first entry here).

`admin.secret` and a password inside `upstream.proxy` follow the
[mask rule](#secrets): send the mask or `""` to keep them. `admin.secret`
therefore cannot be emptied here — `null` keeps it too, the one field where
`null` does not mean the default; a new value replaces the secret at once
(the next request must use it).

```http
PATCH /admin/api/settings

{
  "routing": {
    "strategy": "round-robin",
    "cooldown": {
      "transient_secs": 30
    }
  },
  "logging": {
    "level": "debug"
  },
  "server": {
    "port": 9000
  },
  "auth": {
    "required": true
  }
}
```

```http
HTTP/1.1 200 OK

{
  "config": {
    "server": {
      "host": "127.0.0.1",
      "port": 9000,
      "body_limit_mb": 64,
      "cors": true,
      "data_dir": "data"
    },
    "admin": {
      "enabled": true,
      "secret": "s3cr3t…0001",
      "allow_remote": false,
      "ui": true
    },
    "auth": {
      "required": true,
      "keys": [
        {
          "key": "sy-Zk3…tQwE",
          "name": "laptop"
        }
      ]
    },
    "routing": {
      "strategy": "round-robin",
      "session_affinity": true,
      "session_affinity_ttl_secs": 3600,
      "force_model_prefix": false,
      "max_attempts": 3,
      "max_wait_secs": 0,
      "cooldown": {
        "enabled": true,
        "rate_limit_base_secs": 1,
        "rate_limit_max_secs": 1800,
        "transient_secs": 30,
        "auth_secs": 1800,
        "quota_secs": 3600,
        "model_not_found_secs": 43200
      }
    },
    "streaming": {
      "keepalive_secs": 15,
      "bootstrap_retries": 2,
      "idle_timeout_secs": 300
    },
    "upstream": {
      "proxy": "direct",
      "connect_timeout_secs": 30,
      "request_timeout_secs": 600,
      "passthrough_headers": true
    },
    "logging": {
      "level": "debug",
      "file": false,
      "max_total_size_mb": 200,
      "request_log": "all",
      "request_log_max_body_kb": 256
    },
    "usage": {
      "enabled": true,
      "persist": true,
      "retention_days": 30
    },
    "providers": [
      {
        "name": "mock",
        "kind": "mock"
      }
    ],
    "pricing": [
      {
        "model": "mock-*",
        "input": 0.5,
        "output": 1.5
      }
    ]
  },
  "path": "/etc/switchyard/switchyard.toml",
  "restart_required": [
    "server.port"
  ],
  "command_line_overrides": [],
  "config_rejected": null
}
```

`null` puts the transient cooldown back to its default (60) and takes it
out of the file, where the patch above had written `transient_secs = 30`:

```http
PATCH /admin/api/settings

{
  "routing": {
    "cooldown": {
      "transient_secs": null
    }
  }
}
```

```http
HTTP/1.1 200 OK

{
  "config": {
    "server": {
      "host": "127.0.0.1",
      "port": 9000,
      "body_limit_mb": 64,
      "cors": true,
      "data_dir": "data"
    },
    "admin": {
      "enabled": true,
      "secret": "s3cr3t…0001",
      "allow_remote": false,
      "ui": true
    },
    "auth": {
      "required": true,
      "keys": [
        {
          "key": "sy-Zk3…tQwE",
          "name": "laptop"
        }
      ]
    },
    "routing": {
      "strategy": "round-robin",
      "session_affinity": true,
      "session_affinity_ttl_secs": 3600,
      "force_model_prefix": false,
      "max_attempts": 3,
      "max_wait_secs": 0,
      "cooldown": {
        "enabled": true,
        "rate_limit_base_secs": 1,
        "rate_limit_max_secs": 1800,
        "transient_secs": 60,
        "auth_secs": 1800,
        "quota_secs": 3600,
        "model_not_found_secs": 43200
      }
    },
    "streaming": {
      "keepalive_secs": 15,
      "bootstrap_retries": 2,
      "idle_timeout_secs": 300
    },
    "upstream": {
      "proxy": "direct",
      "connect_timeout_secs": 30,
      "request_timeout_secs": 600,
      "passthrough_headers": true
    },
    "logging": {
      "level": "debug",
      "file": false,
      "max_total_size_mb": 200,
      "request_log": "all",
      "request_log_max_body_kb": 256
    },
    "usage": {
      "enabled": true,
      "persist": true,
      "retention_days": 30
    },
    "providers": [
      {
        "name": "mock",
        "kind": "mock"
      }
    ],
    "pricing": [
      {
        "model": "mock-*",
        "input": 0.5,
        "output": 1.5
      }
    ]
  },
  "path": "/etc/switchyard/switchyard.toml",
  "restart_required": [
    "server.port"
  ],
  "command_line_overrides": [],
  "config_rejected": null
}
```

Errors:

* `400` — the patch is not an object, touches another section (`providers`,
  `auth.keys`, `aliases`, …: they have their own endpoints), names a field
  that does not exist, or gives a value of the wrong type. Nothing of a
  refused patch is applied.
* `422` — the values have the right types but break a rule, or are integers
  too large for the file (see [Configuration edits](#configuration-edits)).
  Among the rules: `server.host` must be an IP address or a host name and
  `server.data_dir` must not be empty (a gateway could not start with
  either); `routing.cooldown.rate_limit_max_secs` must not be below
  `rate_limit_base_secs`; `admin.secret` must not be a reference without a
  variable name (`env:`, `${}`), must not start or end with a space and must
  not contain control characters such as tabs or line breaks (an HTTP header
  cannot carry it). And, as for [`PUT /config/raw`](#put-configraw), a patch
  that would lock the dashboard out — `admin.enabled = false`, or an
  `admin.secret` naming a variable that is not set while
  `SWITCHYARD_ADMIN_SECRET` is not either. Each secret problem reads the same
  here, from the raw editor and from `POST /config/validate`.
* `409` — the file on disk is not valid (see
  [Configuration edits](#configuration-edits)).

```http
PATCH /admin/api/settings

{
  "providers": [],
  "routing": {
    "stratgy": "fill-first"
  }
}
```

```http
HTTP/1.1 400 Bad Request

{
  "error": {
    "message": "the settings patch touches fields it may not change",
    "issues": [
      {
        "path": "providers",
        "message": "cannot be changed through /settings (server, admin, auth.required, routing, streaming, upstream, logging and usage can)"
      }
    ]
  }
}
```

```http
PATCH /admin/api/settings

{
  "logging": {
    "level": "loud"
  }
}
```

```http
HTTP/1.1 422 Unprocessable Entity

{
  "error": {
    "message": "the configuration is not valid: logging.level: must be one of trace, debug, info, warn, error",
    "issues": [
      {
        "path": "logging.level",
        "message": "must be one of trace, debug, info, warn, error"
      }
    ]
  }
}
```

### `POST /reload`

Reads the file again and applies it, whether or not it changed (the file is
normally watched; this is the manual way). The body is ignored. Response:
the same shape as `GET /config` (`providers` is cut to its first entry
here). `422` when the file is not a valid configuration — the previous one
stays in effect — and `500` when it cannot be read.

Applying the file again also repeats what is done when a configuration is
applied: service-account files are read again (a credential whose file was
repaired becomes usable), and — when the file did not change — every
provider that wants discovery is asked for its model list again.

```http
POST /admin/api/reload

{}
```

```http
HTTP/1.1 200 OK

{
  "config": {
    "server": {
      "host": "127.0.0.1",
      "port": 9000,
      "body_limit_mb": 64,
      "cors": true,
      "data_dir": "data"
    },
    "admin": {
      "enabled": true,
      "secret": "s3cr3t…0001",
      "allow_remote": false,
      "ui": true
    },
    "auth": {
      "required": true,
      "keys": [
        {
          "key": "sy-Zk3…tQwE",
          "name": "laptop"
        }
      ]
    },
    "routing": {
      "strategy": "round-robin",
      "session_affinity": true,
      "session_affinity_ttl_secs": 3600,
      "force_model_prefix": false,
      "max_attempts": 3,
      "max_wait_secs": 0,
      "cooldown": {
        "enabled": true,
        "rate_limit_base_secs": 1,
        "rate_limit_max_secs": 1800,
        "transient_secs": 60,
        "auth_secs": 1800,
        "quota_secs": 3600,
        "model_not_found_secs": 43200
      }
    },
    "streaming": {
      "keepalive_secs": 15,
      "bootstrap_retries": 2,
      "idle_timeout_secs": 300
    },
    "upstream": {
      "proxy": "direct",
      "connect_timeout_secs": 30,
      "request_timeout_secs": 600,
      "passthrough_headers": true
    },
    "logging": {
      "level": "debug",
      "file": false,
      "max_total_size_mb": 200,
      "request_log": "all",
      "request_log_max_body_kb": 256
    },
    "usage": {
      "enabled": true,
      "persist": true,
      "retention_days": 30
    },
    "providers": [
      {
        "name": "mock",
        "kind": "mock"
      }
    ],
    "pricing": [
      {
        "model": "mock-*",
        "input": 0.5,
        "output": 1.5
      }
    ]
  },
  "path": "/etc/switchyard/switchyard.toml",
  "restart_required": [
    "server.port"
  ],
  "command_line_overrides": [],
  "config_rejected": null
}
```

---------------------------------------------------------------------------

## Providers

### The provider view

`GET /providers`, `GET /providers/{name}` and every provider or credential
mutation return this object:

```http
GET /admin/api/providers/vendor
```

```http
HTTP/1.1 200 OK

{
  "name": "vendor",
  "kind": "openai-compat",
  "enabled": true,
  "base_url": "http://127.0.0.1:9/v1",
  "api_keys": [
    "sk-liv…6b5c"
  ],
  "credentials": [
    {
      "id": "vendor:1ceceddcdbe7",
      "label": "sk-liv…6b5c",
      "masked_key": "sk-liv…6b5c",
      "source": "api_keys",
      "index": 0,
      "disabled": false,
      "disabled_by": null,
      "weight": 1,
      "priority": 0,
      "proxy": "",
      "service_account_file": "",
      "status": "ready",
      "cooldown_until": null,
      "cooldown_reason": null,
      "model_cooldowns": [],
      "requests": 0,
      "successes": 0,
      "failures": 0,
      "consecutive_failures": 0,
      "latency_ms": null,
      "last_used_at": null,
      "last_error": null,
      "usable": true,
      "unusable_reason": null
    },
    {
      "id": "vendor:3653bbd415fc",
      "label": "team",
      "masked_key": "sk-liv…4c5d",
      "source": "credentials",
      "index": 0,
      "disabled": false,
      "disabled_by": null,
      "weight": 2,
      "priority": 0,
      "proxy": "",
      "service_account_file": "",
      "status": "ready",
      "cooldown_until": null,
      "cooldown_reason": null,
      "model_cooldowns": [],
      "requests": 0,
      "successes": 0,
      "failures": 0,
      "consecutive_failures": 0,
      "latency_ms": null,
      "last_used_at": null,
      "last_error": null,
      "usable": true,
      "unusable_reason": null
    }
  ],
  "prefix": "",
  "priority": 0,
  "proxy": "",
  "headers": {},
  "models": [
    "large"
  ],
  "exclude": [],
  "discover": false,
  "wire_api": "auto",
  "legacy_max_tokens": null,
  "stream_usage": null,
  "project": "",
  "location": "",
  "model_count": 1,
  "effective_base_url": "http://127.0.0.1:9/v1",
  "protocols": [
    "openai-chat"
  ],
  "discovery": {
    "state": "off",
    "at": null,
    "error": null,
    "models": 0
  },
  "config": {
    "name": "vendor",
    "kind": "openai-compat",
    "enabled": true,
    "base_url": "http://127.0.0.1:9/v1",
    "api_keys": [
      "sk-liv…6b5c"
    ],
    "credentials": [
      {
        "api_key": "sk-liv…4c5d",
        "label": "team",
        "weight": 2
      }
    ],
    "prefix": "",
    "priority": 0,
    "proxy": "",
    "headers": {},
    "models": [
      {
        "id": "vendor-large",
        "alias": "large"
      }
    ],
    "exclude": [],
    "discover": false,
    "wire_api": "auto",
    "legacy_max_tokens": null,
    "stream_usage": null,
    "project": "",
    "location": ""
  }
}
```

It is the provider's configuration entry with every field present and
secrets masked, except that two keys are replaced and five are added:

| Field | Meaning |
|---|---|
| `name` … `location` | The entry as configured (`kind`: `openai`, `anthropic`, `gemini`, `vertex`, `openai-compat`, `mock`; `wire_api`: `auto`, `chat`, `responses`; `legacy_max_tokens`, `stream_usage`: `true`, `false` or `null` for the kind's default). |
| `credentials` | **Replaced:** one entry per runtime credential, configuration merged with runtime state (below). The configured `credentials` list is in `config.credentials`. |
| `models` | **Replaced:** the client-facing names this provider serves right now, sorted — **strings**. The configured model list, a list of **objects**, is in `config.models` (see below). |
| `model_count` | Upstream models the provider serves, after `exclude`. `models` can be longer: with a `prefix`, each model is listed as `prefix/name` and under its bare name. A disabled provider serves nothing: `0`, like its `models` (`[]`). |
| `effective_base_url` | `base_url`, or the kind's default when that is empty. |
| `protocols` | Wire protocols the provider can be spoken to in, most preferred first: `openai-chat`, `openai-responses`, `anthropic`, `gemini`. |
| `discovery` | Where the discovery of the provider's model list stands (below). |
| `config` | The entry itself, in exactly the shape `POST /providers` and `PUT /providers/{name}` accept: edit this object and send it back. A `credentials[]` entry without a key carries `"api_key": null` (see [`PUT /providers/{name}`](#put-providersname)), so the object round-trips exactly: sent back as it is — also with its entries reordered or relabelled — a keyless credential stays keyless. |

**`config.models` — the shape requests take.** In an entry sent to
`POST /providers` or `PUT /providers/{name}`, `models` is a list of objects
`{"id": "<upstream model id>", "alias"?, "display_name"?, "context_window"?,
"max_output_tokens"?, "thinking"?}`: only `id` is needed, `alias` is the name
clients use when it differs, `thinking` is
`{min, max, zero_allowed, dynamic_allowed, levels?}`. A list of names
(`"models": ["large"]`, the shape of the view's own top-level `models`) is
refused with `400` at `models[0]`.

**`discovery`** — `{state, at, error, models}`, always all four:

| Field | Meaning |
|---|---|
| `state` | `off`: the upstream is not asked for its model list — the provider is disabled, has `discover: false`, lists its `models` itself, or is a `mock`. `pending`: a listing is under way and none has answered since the provider's settings last changed. `ok`: the latest listing succeeded. `failed`: the latest listing failed. |
| `at` | Unix ms: when the latest listing succeeded or failed; while `pending`, when it was started. `null` for `off`. |
| `error` | `failed` only, else `null`: why, in one line of at most 300 characters (`provider `x` has no usable credential` when there was nothing to ask with). No key material: the credential the listing was asked with is removed, and anything else the upstream quoted that is shaped like a key, a token or a password is masked, as in the log. |
| `models` | How many models the upstream's list in use holds: the latest successful listing's. **A failed listing keeps the previous list**, so this can be above `0` while `state` is `failed` — also when the provider was switched off and on again in between, which keeps the list too. `0` while `off`, and after a change of `kind` or `base_url`, which drops the list. |

So an empty `models` with `discovery.state` `pending` means "wait", with
`failed` "look at `error`", and with `ok` "the upstream lists nothing (or
`exclude` hides it all)". Discovery runs in the background: at start for
every provider that wants it; after a configuration change only for the
providers whose `kind`, `base_url`, `api_keys`, `credentials`, `proxy`,
`headers`, `project` or `location` changed, for new providers, and for
providers that want discovery now and did not before (and for all of them
when `upstream.proxy` changed); after `POST /reload` of an unchanged file
for every provider that wants it; and on
[`POST /providers/{name}/discover`](#post-providersnamediscover). No live
event announces the end of a listing: read the provider again when
`at` of a `pending` state is a few seconds old.

A credential entry:

| Field | Meaning |
|---|---|
| `id` | Stable id (`<provider>:<12 hex>`, sometimes with a `-N` suffix): a hash of provider, kind, key and endpoint, never the key. It survives edits of label, weight, priority and proxy, and changes when the key, the endpoint or the provider's name changes. Use it with `/credentials/{id}/…`. |
| `label` | The configured label, else the masked key, else the provider's name. |
| `masked_key` | The masked key; the reference as written when it cannot be resolved; a service-account file's name; or `""` for a keyless credential. |
| `source`, `index` | Where the credential is configured: `"api_keys"` (`config.api_keys[index]`), `"credentials"` (`config.credentials[index]`), or `"implicit"` with `index: null` — the keyless credential that `mock` and `openai-compat` providers get when they list none. |
| `disabled` | The credential's own switch: off in the configuration or at runtime. (A provider that is switched off does not set it.) |
| `disabled_by` | What makes the status `disabled`, else `null`: `provider` (the provider has `enabled: false` — all its credentials are out of rotation, whatever their own state), `credential` (the entry's `disabled`), `runtime` (switched off at runtime only). |
| `weight`, `priority` | Effective values (`priority` falls back to the provider's). |
| `proxy`, `service_account_file` | As configured for a `credentials` entry (proxy password masked), else `""`. |
| `status` | `ready`, `cooling` (the whole credential rests, or every model on it does), `disabled` (see `disabled_by`; this wins over everything else), `unusable` — or `unknown` in the rare moment the scheduler has not seen the credential yet. |
| `cooldown_until`, `cooldown_reason` | End and cause of the cooldown that makes it `cooling`, else `null`. Causes: `rate_limit`, `quota`, `auth`, `server`, `transport`, `model_not_found`, `request`. |
| `model_cooldowns` | Models resting on this credential: `[{model, until, reason}]` (upstream model ids). |
| `requests`, `successes`, `failures`, `consecutive_failures` | Upstream attempts since start (failures exclude request faults). |
| `latency_ms` | Moving average of the response latency, `null` before the first success. |
| `last_used_at` | Unix ms of the last attempt, or `null`. |
| `last_error` | `{status, class, message, at, model}` of the latest upstream failure, or `null`. `status` is `0` when no response arrived. |
| `usable`, `unusable_reason` | `false` with a reason when the credential cannot be used as it is: its variable is not set, it has no key, or its `service_account_file` is missing, unreadable or not a usable key file (the reason names the file and the problem, never what the file holds). Such a credential is never picked and is listed in `warnings` of `GET /status`. The file is looked at when a configuration is applied (`POST /reload` is enough) and before a provider test or a model listing on request, so repairing it and pressing "Test" brings the credential back — and so does taking the `service_account_file` out of a credential that also has an `api_key`: it is `ready` again as soon as the edit is applied. |

The same view of a provider whose only credential is implicit and has seen a
failure:

```http
GET /admin/api/providers/mock
```

```http
HTTP/1.1 200 OK

{
  "name": "mock",
  "kind": "mock",
  "enabled": true,
  "base_url": "",
  "api_keys": [],
  "credentials": [
    {
      "id": "mock:8da14ba21598",
      "label": "mock",
      "masked_key": "",
      "source": "implicit",
      "index": null,
      "disabled": false,
      "disabled_by": null,
      "weight": 1,
      "priority": 0,
      "proxy": "",
      "service_account_file": "",
      "status": "ready",
      "cooldown_until": null,
      "cooldown_reason": null,
      "model_cooldowns": [
        {
          "model": "mock-error-500",
          "until": 1791004087956,
          "reason": "server"
        }
      ],
      "requests": 3,
      "successes": 2,
      "failures": 1,
      "consecutive_failures": 1,
      "latency_ms": 30,
      "last_used_at": 1791004027956,
      "last_error": {
        "status": 500,
        "class": "server",
        "message": "Mock upstream failure.",
        "at": 1791004027956,
        "model": "mock-error-500"
      },
      "usable": true,
      "unusable_reason": null
    }
  ],
  "prefix": "",
  "priority": 0,
  "proxy": "",
  "headers": {},
  "models": [
    "mock-echo",
    "mock-error-401",
    "mock-error-429",
    "mock-error-500",
    "mock-lorem",
    "mock-slow",
    "mock-think",
    "mock-tools"
  ],
  "exclude": [],
  "discover": true,
  "wire_api": "auto",
  "legacy_max_tokens": null,
  "stream_usage": null,
  "project": "",
  "location": "",
  "model_count": 8,
  "effective_base_url": "mock://local",
  "protocols": [
    "openai-chat",
    "openai-responses",
    "anthropic",
    "gemini"
  ],
  "discovery": {
    "state": "off",
    "at": null,
    "error": null,
    "models": 0
  },
  "config": {
    "name": "mock",
    "kind": "mock",
    "enabled": true,
    "base_url": "",
    "api_keys": [],
    "credentials": [],
    "prefix": "",
    "priority": 0,
    "proxy": "",
    "headers": {},
    "models": [],
    "exclude": [],
    "discover": true,
    "wire_api": "auto",
    "legacy_max_tokens": null,
    "stream_usage": null,
    "project": "",
    "location": ""
  }
}
```

### `GET /providers`

Every provider, in configuration order (the list is cut to two entries per
array here).

```http
GET /admin/api/providers
```

```http
HTTP/1.1 200 OK

[
  {
    "name": "mock",
    "kind": "mock",
    "enabled": true,
    "base_url": "",
    "api_keys": [],
    "credentials": [
      {
        "id": "mock:8da14ba21598",
        "label": "mock",
        "masked_key": "",
        "source": "implicit",
        "index": null,
        "disabled": false,
        "disabled_by": null,
        "weight": 1,
        "priority": 0,
        "proxy": "",
        "service_account_file": "",
        "status": "ready",
        "cooldown_until": null,
        "cooldown_reason": null,
        "model_cooldowns": [
          {
            "model": "mock-error-500",
            "until": 1791004087956,
            "reason": "server"
          }
        ],
        "requests": 3,
        "successes": 2,
        "failures": 1,
        "consecutive_failures": 1,
        "latency_ms": 30,
        "last_used_at": 1791004027956,
        "last_error": {
          "status": 500,
          "class": "server",
          "message": "Mock upstream failure.",
          "at": 1791004027956,
          "model": "mock-error-500"
        },
        "usable": true,
        "unusable_reason": null
      }
    ],
    "prefix": "",
    "priority": 0,
    "proxy": "",
    "headers": {},
    "models": [
      "mock-echo",
      "mock-error-401"
    ],
    "exclude": [],
    "discover": true,
    "wire_api": "auto",
    "legacy_max_tokens": null,
    "stream_usage": null,
    "project": "",
    "location": "",
    "model_count": 8,
    "effective_base_url": "mock://local",
    "protocols": [
      "openai-chat",
      "openai-responses"
    ],
    "discovery": {
      "state": "off",
      "at": null,
      "error": null,
      "models": 0
    },
    "config": {
      "name": "mock",
      "kind": "mock",
      "enabled": true,
      "base_url": "",
      "api_keys": [],
      "credentials": [],
      "prefix": "",
      "priority": 0,
      "proxy": "",
      "headers": {},
      "models": [],
      "exclude": [],
      "discover": true,
      "wire_api": "auto",
      "legacy_max_tokens": null,
      "stream_usage": null,
      "project": "",
      "location": ""
    }
  },
  {
    "name": "vendor",
    "kind": "openai-compat",
    "enabled": true,
    "base_url": "http://127.0.0.1:9/v1",
    "api_keys": [
      "sk-liv…6b5c"
    ],
    "credentials": [
      {
        "id": "vendor:1ceceddcdbe7",
        "label": "sk-liv…6b5c",
        "masked_key": "sk-liv…6b5c",
        "source": "api_keys",
        "index": 0,
        "disabled": false,
        "disabled_by": null,
        "weight": 1,
        "priority": 0,
        "proxy": "",
        "service_account_file": "",
        "status": "ready",
        "cooldown_until": null,
        "cooldown_reason": null,
        "model_cooldowns": [],
        "requests": 0,
        "successes": 0,
        "failures": 0,
        "consecutive_failures": 0,
        "latency_ms": null,
        "last_used_at": null,
        "last_error": null,
        "usable": true,
        "unusable_reason": null
      },
      {
        "id": "vendor:3653bbd415fc",
        "label": "team",
        "masked_key": "sk-liv…4c5d",
        "source": "credentials",
        "index": 0,
        "disabled": false,
        "disabled_by": null,
        "weight": 2,
        "priority": 0,
        "proxy": "",
        "service_account_file": "",
        "status": "ready",
        "cooldown_until": null,
        "cooldown_reason": null,
        "model_cooldowns": [],
        "requests": 0,
        "successes": 0,
        "failures": 0,
        "consecutive_failures": 0,
        "latency_ms": null,
        "last_used_at": null,
        "last_error": null,
        "usable": true,
        "unusable_reason": null
      }
    ],
    "prefix": "",
    "priority": 0,
    "proxy": "",
    "headers": {},
    "models": [
      "large"
    ],
    "exclude": [],
    "discover": false,
    "wire_api": "auto",
    "legacy_max_tokens": null,
    "stream_usage": null,
    "project": "",
    "location": "",
    "model_count": 1,
    "effective_base_url": "http://127.0.0.1:9/v1",
    "protocols": [
      "openai-chat"
    ],
    "discovery": {
      "state": "off",
      "at": null,
      "error": null,
      "models": 0
    },
    "config": {
      "name": "vendor",
      "kind": "openai-compat",
      "enabled": true,
      "base_url": "http://127.0.0.1:9/v1",
      "api_keys": [
        "sk-liv…6b5c"
      ],
      "credentials": [
        {
          "api_key": "sk-liv…4c5d",
          "label": "team",
          "weight": 2
        }
      ],
      "prefix": "",
      "priority": 0,
      "proxy": "",
      "headers": {},
      "models": [
        {
          "id": "vendor-large",
          "alias": "large"
        }
      ],
      "exclude": [],
      "discover": false,
      "wire_api": "auto",
      "legacy_max_tokens": null,
      "stream_usage": null,
      "project": "",
      "location": ""
    }
  }
]
```

### `GET /providers/{name}`

One provider view. `404` when there is no provider of that name.

### `POST /providers`

Request: a provider entry (the `config` shape above; only `name` and `kind`
are required). Response: `201` and the new provider's view (lists cut to
three entries here).

* The name is trimmed; blank `api_keys` rows are dropped.
* `409` when a provider of that name exists, with the issue on `name`.
* `400` when the body is not a provider entry (unknown field, unknown
  `kind`, missing `name`, `models` given as names instead of objects).
* `422` when the entry is one but is not valid — including a *masked* secret
  in a new provider, which has no stored value to stand for. Among the
  rules: `name` of lower-case letters, digits, `-` and `_`; a `base_url`
  where the kind has no default; header names that are valid header names
  and header values that are neither empty nor contain control characters
  (`headers.<name>`); no secret reference without a variable name (`env:`,
  `${}`) and no key listed twice (`api_keys[j]`, `credentials[j].api_key`);
  a `thinking` range with `min` not above `max` (`models[i].thinking`). All
  problems of the entry are listed in one answer — except unresolvable
  masks, which are reported first, on their own.
* A `vertex` credential's `service_account_file` is not a rule of the
  configuration: a file that is missing or unusable does not refuse the
  entry, it makes that credential `unusable` (with the reason) in the view
  that comes back.

```http
POST /admin/api/providers

{
  "name": "second-mock",
  "kind": "mock",
  "prefix": "lab"
}
```

```http
HTTP/1.1 201 Created

{
  "name": "second-mock",
  "kind": "mock",
  "enabled": true,
  "base_url": "",
  "api_keys": [],
  "credentials": [
    {
      "id": "second-mock:b126c75572c8",
      "label": "second-mock",
      "masked_key": "",
      "source": "implicit",
      "index": null,
      "disabled": false,
      "disabled_by": null,
      "weight": 1,
      "priority": 0,
      "proxy": "",
      "service_account_file": "",
      "status": "ready",
      "cooldown_until": null,
      "cooldown_reason": null,
      "model_cooldowns": [],
      "requests": 0,
      "successes": 0,
      "failures": 0,
      "consecutive_failures": 0,
      "latency_ms": null,
      "last_used_at": null,
      "last_error": null,
      "usable": true,
      "unusable_reason": null
    }
  ],
  "prefix": "lab",
  "priority": 0,
  "proxy": "",
  "headers": {},
  "models": [
    "lab/mock-echo",
    "lab/mock-error-401",
    "lab/mock-error-429"
  ],
  "exclude": [],
  "discover": true,
  "wire_api": "auto",
  "legacy_max_tokens": null,
  "stream_usage": null,
  "project": "",
  "location": "",
  "model_count": 8,
  "effective_base_url": "mock://local",
  "protocols": [
    "openai-chat",
    "openai-responses",
    "anthropic"
  ],
  "discovery": {
    "state": "off",
    "at": null,
    "error": null,
    "models": 0
  },
  "config": {
    "name": "second-mock",
    "kind": "mock",
    "enabled": true,
    "base_url": "",
    "api_keys": [],
    "credentials": [],
    "prefix": "lab",
    "priority": 0,
    "proxy": "",
    "headers": {},
    "models": [],
    "exclude": [],
    "discover": true,
    "wire_api": "auto",
    "legacy_max_tokens": null,
    "stream_usage": null,
    "project": "",
    "location": ""
  }
}
```

```http
POST /admin/api/providers

{
  "name": "mock",
  "kind": "mock"
}
```

```http
HTTP/1.1 409 Conflict

{
  "error": {
    "message": "a provider named `mock` already exists",
    "issues": [
      {
        "path": "name",
        "message": "is the name of another provider"
      }
    ]
  }
}
```

```http
POST /admin/api/providers

{
  "name": "Another One",
  "kind": "openai-compat"
}
```

```http
HTTP/1.1 422 Unprocessable Entity

{
  "error": {
    "message": "the configuration is not valid: name: may only contain lowercase letters, digits, `-` and `_`; base_url: is required for this provider kind",
    "issues": [
      {
        "path": "name",
        "message": "may only contain lowercase letters, digits, `-` and `_`"
      },
      {
        "path": "base_url",
        "message": "is required for this provider kind"
      }
    ]
  }
}
```

Models given as names instead of objects:

```http
POST /admin/api/providers

{
  "name": "third",
  "kind": "mock",
  "models": [
    "mock-echo"
  ]
}
```

```http
HTTP/1.1 400 Bad Request

{
  "error": {
    "message": "invalid request: models[0]: expected an object such as {\"id\": \"model-name\"}, got a string",
    "issues": [
      {
        "path": "models[0]",
        "message": "expected an object such as {\"id\": \"model-name\"}, got a string"
      }
    ]
  }
}
```

### `PUT /providers/{name}`

Request: the whole provider entry (`config`, edited). Response: the updated
view. The entry replaces the stored one: a field that is left out goes back
to its default.

* **Secrets** follow the [mask rule](#secrets): unchanged masks keep their
  secrets — also when entries are reordered, deleted, or moved between
  `api_keys` and `credentials` — new values replace, references stay as
  written. An emptied `credentials[].api_key` keeps the stored key; a blank
  `api_keys` row is dropped.
* **Keys that mask alike.** Keys of up to 11 characters all show as
  `••••••••` (and longer ones can share a mask too), so a mask alone does
  not say which of them it stands for; a `label` does. Without labels:
  sending back as many such masks as there are stored keys keeps them all,
  in their stored order — their order cannot be changed by moving masks
  around, and moving one of them to `credentials` (to give it a label)
  keeps each key where its row was. Sending back **fewer** (one was
  deleted) is refused with `422` on those fields ("is masked like several
  stored secrets that cannot be told apart …"), because which key was
  deleted cannot be known and guessing could keep the very key that was
  removed. Send the keys to keep in full (or give the keys labels first).
* **A credential without a key:** `"api_key": null` in a `credentials[]`
  entry. An empty or absent `api_key` keeps the key stored for the
  credential with the same `service_account_file`, else the same `label`,
  else at the same position — so a keyless credential that takes the place
  of one with a key would inherit that key. `null` says that it has none:
  with `[{"api_key": "sk-…", "label": "old"}, {"label": "keyless via
  proxy", "proxy": "…"}]` stored, sending `[{"label": "local", "api_key":
  null, "proxy": "…"}]` leaves one credential and no key anywhere. (Where
  the provider's kind requires a key, such a credential is refused with
  `422` at `credentials[j]`.) `POST /providers` accepts `null` too. The
  view shows a keyless credential with `masked_key: ""`, and `config`
  with `"api_key": null` — so what was shown can be sent back as it is.
* **Rename:** a `name` in the body that differs from the path renames the
  provider (`409` if that name is taken, with the issue on `name`). Its secrets are kept as long as
  `kind` and `base_url` stay the same; payload rules that name the provider
  follow the rename. Credential ids change with the name, so their runtime
  counters start afresh.
* `404` unknown provider, `400` not a provider entry, `422` not valid.

Here the first key comes back as its mask, a second is added, and the
priority changes:

```http
PUT /admin/api/providers/vendor

{
  "name": "vendor",
  "kind": "openai-compat",
  "enabled": true,
  "base_url": "http://127.0.0.1:9/v1",
  "api_keys": [
    "sk-liv…6b5c",
    "sk-live-9e8d7c6b5a4f3e2d1c0b9a8f7e6d5c4b"
  ],
  "credentials": [
    {
      "api_key": "sk-liv…4c5d",
      "label": "team",
      "weight": 2
    }
  ],
  "prefix": "",
  "priority": 10,
  "proxy": "",
  "headers": {},
  "models": [
    {
      "id": "vendor-large",
      "alias": "large"
    }
  ],
  "exclude": [],
  "discover": false,
  "wire_api": "auto",
  "legacy_max_tokens": null,
  "stream_usage": null,
  "project": "",
  "location": ""
}
```

```http
HTTP/1.1 200 OK

{
  "name": "vendor",
  "kind": "openai-compat",
  "enabled": true,
  "base_url": "http://127.0.0.1:9/v1",
  "api_keys": [
    "sk-liv…6b5c",
    "sk-liv…5c4b"
  ],
  "credentials": [
    {
      "id": "vendor:1ceceddcdbe7",
      "label": "sk-liv…6b5c",
      "masked_key": "sk-liv…6b5c",
      "source": "api_keys",
      "index": 0,
      "disabled": false,
      "disabled_by": null,
      "weight": 1,
      "priority": 10,
      "proxy": "",
      "service_account_file": "",
      "status": "ready",
      "cooldown_until": null,
      "cooldown_reason": null,
      "model_cooldowns": [],
      "requests": 0,
      "successes": 0,
      "failures": 0,
      "consecutive_failures": 0,
      "latency_ms": null,
      "last_used_at": null,
      "last_error": null,
      "usable": true,
      "unusable_reason": null
    },
    {
      "id": "vendor:4f5c5cd78c5d",
      "label": "sk-liv…5c4b",
      "masked_key": "sk-liv…5c4b",
      "source": "api_keys",
      "index": 1,
      "disabled": false,
      "disabled_by": null,
      "weight": 1,
      "priority": 10,
      "proxy": "",
      "service_account_file": "",
      "status": "ready",
      "cooldown_until": null,
      "cooldown_reason": null,
      "model_cooldowns": [],
      "requests": 0,
      "successes": 0,
      "failures": 0,
      "consecutive_failures": 0,
      "latency_ms": null,
      "last_used_at": null,
      "last_error": null,
      "usable": true,
      "unusable_reason": null
    },
    {
      "id": "vendor:3653bbd415fc",
      "label": "team",
      "masked_key": "sk-liv…4c5d",
      "source": "credentials",
      "index": 0,
      "disabled": false,
      "disabled_by": null,
      "weight": 2,
      "priority": 10,
      "proxy": "",
      "service_account_file": "",
      "status": "ready",
      "cooldown_until": null,
      "cooldown_reason": null,
      "model_cooldowns": [],
      "requests": 0,
      "successes": 0,
      "failures": 0,
      "consecutive_failures": 0,
      "latency_ms": null,
      "last_used_at": null,
      "last_error": null,
      "usable": true,
      "unusable_reason": null
    }
  ],
  "prefix": "",
  "priority": 10,
  "proxy": "",
  "headers": {},
  "models": [
    "large"
  ],
  "exclude": [],
  "discover": false,
  "wire_api": "auto",
  "legacy_max_tokens": null,
  "stream_usage": null,
  "project": "",
  "location": "",
  "model_count": 1,
  "effective_base_url": "http://127.0.0.1:9/v1",
  "protocols": [
    "openai-chat"
  ],
  "discovery": {
    "state": "off",
    "at": null,
    "error": null,
    "models": 0
  },
  "config": {
    "name": "vendor",
    "kind": "openai-compat",
    "enabled": true,
    "base_url": "http://127.0.0.1:9/v1",
    "api_keys": [
      "sk-liv…6b5c",
      "sk-liv…5c4b"
    ],
    "credentials": [
      {
        "api_key": "sk-liv…4c5d",
        "label": "team",
        "weight": 2
      }
    ],
    "prefix": "",
    "priority": 10,
    "proxy": "",
    "headers": {},
    "models": [
      {
        "id": "vendor-large",
        "alias": "large"
      }
    ],
    "exclude": [],
    "discover": false,
    "wire_api": "auto",
    "legacy_max_tokens": null,
    "stream_usage": null,
    "project": "",
    "location": ""
  }
}
```

### `DELETE /providers/{name}`

`404` when there is no such provider.

```http
DELETE /admin/api/providers/second-mock
```

```http
HTTP/1.1 200 OK

{
  "ok": true
}
```

### `POST /providers/{name}/test`

Sends one tiny request ("ping", 16 output tokens, not streamed) through the
provider's first usable credential, whatever its cooldown state. Request:
`{"model": "<client-facing name or upstream id>"}`, `{}`, or no body at all
(then the provider's first model is used). Takes as long as the upstream
does, at most 60 seconds.

Always `200` for a provider that exists: the outcome is in the body. A
success puts a resting model back into rotation; a failure rests it like a
failed request would. `404` unknown provider.

One failure rests nothing, on purpose: the mock model `mock-error-401`.
A rejected key rests the *whole credential*, and a mock provider has one
credential that all its models share — so a scripted `401` that counted
would switch off the mock models that work. The test (and a client request
for that model) is answered `ok: false`, `status: 401`, while the
credential's counters, `last_error` and cooldowns stay as they were.
`mock-error-429` and `mock-error-500` do count, and rest only themselves
(a model cooldown in `model_cooldowns`). A whole-credential cooldown
(`cooldown_until` / `cooldown_reason: "auth"`) can therefore not be
produced with the mock provider; it takes a real upstream that answers
`401`.

| Field | Meaning |
|---|---|
| `ok` | Whether the upstream answered successfully. |
| `status` | The upstream's HTTP status; `0` when there was no response. |
| `latency_ms` | How long the call took. |
| `model` | The upstream model id that was used, `null` if none could be chosen. |
| `credential` | Label of the credential that was used, `null` if there is none. |
| `error` | Present only on failure: what the upstream said, credentials removed (the one that was used, and anything else key-shaped the upstream quoted is masked). |

```http
POST /admin/api/providers/mock/test

{
  "model": "mock-echo"
}
```

```http
HTTP/1.1 200 OK

{
  "ok": true,
  "status": 200,
  "latency_ms": 21,
  "model": "mock-echo",
  "credential": "mock"
}
```

```http
POST /admin/api/providers/mock/test

{
  "model": "mock-error-401"
}
```

```http
HTTP/1.1 200 OK

{
  "ok": false,
  "status": 401,
  "latency_ms": 30,
  "model": "mock-error-401",
  "credential": "mock",
  "error": "Mock credential rejected."
}
```

### `POST /providers/{name}/discover`

Asks the upstream for its model list now, hands it to the scheduler and
returns it (cut to two entries here). The body is ignored. `ModelInfo`
omits fields it has no value for. The outcome becomes the provider's
`discovery` state (unless that is `off`): `ok` with the number of models,
or `failed` with the reason.

```http
POST /admin/api/providers/mock/discover

{}
```

```http
HTTP/1.1 200 OK

{
  "models": [
    {
      "id": "mock-echo",
      "display_name": "Mock Echo",
      "description": "Repeats the last user message.",
      "owned_by": "switchyard",
      "context_window": 128000,
      "max_output_tokens": 8192,
      "known": true
    },
    {
      "id": "mock-lorem",
      "display_name": "Mock Lorem",
      "description": "Streams three paragraphs of filler text.",
      "owned_by": "switchyard",
      "context_window": 128000,
      "max_output_tokens": 8192,
      "known": true
    }
  ]
}
```

Errors:

* `404` — unknown provider.
* `503` — the provider has no usable credential to ask its upstream with
  (none configured, all disabled, or a reference to an unset variable).
* `502` — the upstream did not deliver the list, whatever its reason: it
  could not be reached, rejected the credential, rate-limited the request,
  has no model listing at that address, or answered with an error of its
  own. `message` starts with what happened and ends with what the upstream
  said (credentials removed, other key-shaped text masked); a wait the upstream asked for is mentioned in
  the sentence. There is no `Retry-After` header and the upstream's status
  is not passed on: `429` and `404` from this route would read as the
  sign-in lockout and an unknown provider.
* `504` — the upstream did not answer in time.

An upstream that rate-limits its model listing (`limited` is a provider
whose upstream answers `429` with `Retry-After: 17`):

```http
POST /admin/api/providers/limited/discover

{}
```

```http
HTTP/1.1 502 Bad Gateway

{
  "error": {
    "message": "the upstream refused to list its models: it is rate limiting this credential or the credential is out of quota (it asks to wait 17 seconds): Rate limit reached for requests"
  }
}
```

---------------------------------------------------------------------------

## Credentials

`{id}` is a credential's `id` from the provider view. It contains a `:`;
percent-encoding it is fine but not required. All three calls ignore the
body.

### `POST /credentials/{id}/reset`

Clears every cooldown and failure streak of the credential (counters stay).
`404` unknown id.

```http
POST /admin/api/credentials/mock:8da14ba21598/reset

{}
```

```http
HTTP/1.1 200 OK

{
  "ok": true
}
```

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

```http
POST /admin/api/credentials/vendor:1ceceddcdbe7/disable

{}
```

```http
HTTP/1.1 200 OK

{
  "name": "vendor",
  "kind": "openai-compat",
  "enabled": true,
  "base_url": "http://127.0.0.1:9/v1",
  "api_keys": [
    "sk-liv…5c4b"
  ],
  "credentials": [
    {
      "id": "vendor:4f5c5cd78c5d",
      "label": "sk-liv…5c4b",
      "masked_key": "sk-liv…5c4b",
      "source": "api_keys",
      "index": 0,
      "disabled": false,
      "disabled_by": null,
      "weight": 1,
      "priority": 10,
      "proxy": "",
      "service_account_file": "",
      "status": "ready",
      "cooldown_until": null,
      "cooldown_reason": null,
      "model_cooldowns": [],
      "requests": 0,
      "successes": 0,
      "failures": 0,
      "consecutive_failures": 0,
      "latency_ms": null,
      "last_used_at": null,
      "last_error": null,
      "usable": true,
      "unusable_reason": null
    },
    {
      "id": "vendor:3653bbd415fc",
      "label": "team",
      "masked_key": "sk-liv…4c5d",
      "source": "credentials",
      "index": 0,
      "disabled": false,
      "disabled_by": null,
      "weight": 2,
      "priority": 10,
      "proxy": "",
      "service_account_file": "",
      "status": "ready",
      "cooldown_until": null,
      "cooldown_reason": null,
      "model_cooldowns": [],
      "requests": 0,
      "successes": 0,
      "failures": 0,
      "consecutive_failures": 0,
      "latency_ms": null,
      "last_used_at": null,
      "last_error": null,
      "usable": true,
      "unusable_reason": null
    },
    {
      "id": "vendor:1ceceddcdbe7",
      "label": "sk-liv…6b5c",
      "masked_key": "sk-liv…6b5c",
      "source": "credentials",
      "index": 1,
      "disabled": true,
      "disabled_by": "credential",
      "weight": 1,
      "priority": 10,
      "proxy": "",
      "service_account_file": "",
      "status": "disabled",
      "cooldown_until": null,
      "cooldown_reason": null,
      "model_cooldowns": [],
      "requests": 0,
      "successes": 0,
      "failures": 0,
      "consecutive_failures": 0,
      "latency_ms": null,
      "last_used_at": null,
      "last_error": null,
      "usable": true,
      "unusable_reason": null
    }
  ],
  "prefix": "",
  "priority": 10,
  "proxy": "",
  "headers": {},
  "models": [
    "large"
  ],
  "exclude": [],
  "discover": false,
  "wire_api": "auto",
  "legacy_max_tokens": null,
  "stream_usage": null,
  "project": "",
  "location": "",
  "model_count": 1,
  "effective_base_url": "http://127.0.0.1:9/v1",
  "protocols": [
    "openai-chat"
  ],
  "discovery": {
    "state": "off",
    "at": null,
    "error": null,
    "models": 0
  },
  "config": {
    "name": "vendor",
    "kind": "openai-compat",
    "enabled": true,
    "base_url": "http://127.0.0.1:9/v1",
    "api_keys": [
      "sk-liv…5c4b"
    ],
    "credentials": [
      {
        "api_key": "sk-liv…4c5d",
        "label": "team",
        "weight": 2
      },
      {
        "api_key": "sk-liv…6b5c",
        "disabled": true
      }
    ],
    "prefix": "",
    "priority": 10,
    "proxy": "",
    "headers": {},
    "models": [
      {
        "id": "vendor-large",
        "alias": "large"
      }
    ],
    "exclude": [],
    "discover": false,
    "wire_api": "auto",
    "legacy_max_tokens": null,
    "stream_usage": null,
    "project": "",
    "location": ""
  }
}
```

Enabling it again (lists cut to one entry here):

```http
POST /admin/api/credentials/vendor:1ceceddcdbe7/enable

{}
```

```http
HTTP/1.1 200 OK

{
  "name": "vendor",
  "kind": "openai-compat",
  "enabled": true,
  "base_url": "http://127.0.0.1:9/v1",
  "api_keys": [
    "sk-liv…5c4b"
  ],
  "credentials": [
    {
      "id": "vendor:4f5c5cd78c5d",
      "label": "sk-liv…5c4b",
      "masked_key": "sk-liv…5c4b",
      "source": "api_keys",
      "index": 0,
      "disabled": false,
      "disabled_by": null,
      "weight": 1,
      "priority": 10,
      "proxy": "",
      "service_account_file": "",
      "status": "ready",
      "cooldown_until": null,
      "cooldown_reason": null,
      "model_cooldowns": [],
      "requests": 0,
      "successes": 0,
      "failures": 0,
      "consecutive_failures": 0,
      "latency_ms": null,
      "last_used_at": null,
      "last_error": null,
      "usable": true,
      "unusable_reason": null
    }
  ],
  "prefix": "",
  "priority": 10,
  "proxy": "",
  "headers": {},
  "models": [
    "large"
  ],
  "exclude": [],
  "discover": false,
  "wire_api": "auto",
  "legacy_max_tokens": null,
  "stream_usage": null,
  "project": "",
  "location": "",
  "model_count": 1,
  "effective_base_url": "http://127.0.0.1:9/v1",
  "protocols": [
    "openai-chat"
  ],
  "discovery": {
    "state": "off",
    "at": null,
    "error": null,
    "models": 0
  },
  "config": {
    "name": "vendor",
    "kind": "openai-compat",
    "enabled": true,
    "base_url": "http://127.0.0.1:9/v1",
    "api_keys": [
      "sk-liv…5c4b"
    ],
    "credentials": [
      {
        "api_key": "sk-liv…4c5d",
        "label": "team",
        "weight": 2
      }
    ],
    "prefix": "",
    "priority": 10,
    "proxy": "",
    "headers": {},
    "models": [
      {
        "id": "vendor-large",
        "alias": "large"
      }
    ],
    "exclude": [],
    "discover": false,
    "wire_api": "auto",
    "legacy_max_tokens": null,
    "stream_usage": null,
    "project": "",
    "location": ""
  }
}
```

```http
POST /admin/api/credentials/mock:000000000000/reset

{}
```

```http
HTTP/1.1 404 Not Found

{
  "error": {
    "message": "there is no credential with the id `mock:000000000000`"
  }
}
```

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
| `ignored` | `true` for an alias without any routable target: no request can be served under the name (its `routes` is `[]`, and `GET /status` warns about it). `false` for everything else. `counts.models` of `GET /status` is the number of entries that are not ignored. |
| `shadows_model` | `true` for an alias whose name equals, ignoring case, a model a provider serves — a model the alias therefore hides — also when the alias targets that very model (to pin a reasoning depth on it, say). Spelled exactly alike, the alias takes the name over: requests for it reach the alias, and the model has no entry of its own in this table or in client listings. Spelled differently (`Fast`, `fast`), both keep their entries and each exact spelling reaches its own; any other spelling (`FAST`) reaches the alias. `false` for every other entry, models and ignored aliases (which hide nothing) included. Use this rather than the "hides the model of the same name" sentence in `warnings` of `GET /status`, which is only given for an alias spelled exactly like the model that does not target it. |
| `alias_targets` | Present only for aliases: the targets as configured. |
| `routes` | The providers behind the name, one entry per provider and upstream model — for an alias, per configured target, in target order (the same provider and model reached through two targets is listed under each). An alias with no routable target has `routes: []`. |

A route:

| Field | Meaning |
|---|---|
| `provider`, `upstream_model` | Who serves it, and under which id. |
| `target` | Present only in an alias's routes: the configured target the route belongs to, as written — with its reasoning suffix when it pins a depth (`"mock-think(high)"`). |
| `priority` | The tier the route competes in: the highest effective priority among the provider's credentials that could serve the model right now; if none could, among those that are usable at all; if none is, among all of them (the provider's own priority when it has no credential). Requests go to the highest tier that has an available credential. |
| `credentials_total`, `credentials_available` | The provider's credentials, and how many of them could serve the model right now (enabled, usable, not resting). `0` available means requests would currently fail on this route. |

```http
GET /admin/api/models
```

```http
HTTP/1.1 200 OK

[
  {
    "name": "fast",
    "info": {
      "id": "fast",
      "display_name": "Mock Echo",
      "description": "Repeats the last user message.",
      "owned_by": "switchyard",
      "context_window": 128000,
      "max_output_tokens": 8192,
      "known": true
    },
    "hidden": false,
    "ignored": false,
    "shadows_model": false,
    "alias_targets": [
      "mock-echo"
    ],
    "routes": [
      {
        "provider": "mock",
        "upstream_model": "mock-echo",
        "target": "mock-echo",
        "priority": 0,
        "credentials_total": 1,
        "credentials_available": 1
      }
    ]
  },
  {
    "name": "large",
    "info": {
      "id": "large",
      "owned_by": "vendor",
      "known": false
    },
    "hidden": false,
    "ignored": false,
    "shadows_model": false,
    "routes": [
      {
        "provider": "vendor",
        "upstream_model": "vendor-large",
        "priority": 10,
        "credentials_total": 3,
        "credentials_available": 3
      }
    ]
  },
  {
    "name": "mock-echo",
    "info": {
      "id": "mock-echo",
      "display_name": "Mock Echo",
      "description": "Repeats the last user message.",
      "owned_by": "switchyard",
      "context_window": 128000,
      "max_output_tokens": 8192,
      "known": true
    },
    "hidden": false,
    "ignored": false,
    "shadows_model": false,
    "routes": [
      {
        "provider": "mock",
        "upstream_model": "mock-echo",
        "priority": 0,
        "credentials_total": 1,
        "credentials_available": 1
      }
    ]
  }
]
```

### `GET /catalog`

The built-in catalog of well-known models (cut to two entries here):
`ModelInfo` keyed by the vendor's id, plus `family` (`openai`, `anthropic`,
`google`) and, when not every provider kind of the family serves the model,
`kinds`. `thinking` describes reasoning support: a token budget range
(`min`, `max`, `zero_allowed`, `dynamic_allowed`) and/or named `levels`.

```http
GET /admin/api/catalog
```

```http
HTTP/1.1 200 OK

[
  {
    "id": "gpt-5.5",
    "display_name": "GPT 5.5",
    "owned_by": "openai",
    "created": 1776902400,
    "context_window": 272000,
    "max_output_tokens": 128000,
    "thinking": {
      "min": 0,
      "max": 0,
      "zero_allowed": false,
      "dynamic_allowed": false,
      "levels": [
        "low",
        "medium",
        "high",
        "xhigh"
      ]
    },
    "known": true,
    "family": "openai",
    "kinds": [
      "openai"
    ]
  },
  {
    "id": "gpt-6-astra",
    "display_name": "GPT 6.0 Astra",
    "owned_by": "openai",
    "created": 1783616400,
    "context_window": 272000,
    "max_output_tokens": 128000,
    "thinking": {
      "min": 0,
      "max": 0,
      "zero_allowed": false,
      "dynamic_allowed": false,
      "levels": [
        "low",
        "medium",
        "high",
        "xhigh",
        "max"
      ]
    },
    "known": true,
    "family": "openai",
    "kinds": [
      "openai"
    ]
  }
]
```

### `GET /aliases`, `PUT /aliases`

An alias is a virtual model: `name` routes to `targets`, tried in order; a
target may pin a reasoning depth with a suffix (`"mock-think(high)"`).
`hide_targets` hides the targets' own names from client listings.

`PUT` replaces the whole list with the body — an array of
`{name, targets, hide_targets?}` — and returns the new list. `[]` clears it.

```http
PUT /admin/api/aliases

[
  {
    "name": "fast",
    "targets": [
      "mock-echo"
    ]
  },
  {
    "name": "smart",
    "targets": [
      "mock-think(high)",
      "large"
    ],
    "hide_targets": false
  }
]
```

```http
HTTP/1.1 200 OK

[
  {
    "name": "fast",
    "targets": [
      "mock-echo"
    ],
    "hide_targets": false
  },
  {
    "name": "smart",
    "targets": [
      "mock-think(high)",
      "large"
    ],
    "hide_targets": false
  }
]
```

```http
GET /admin/api/aliases
```

```http
HTTP/1.1 200 OK

[
  {
    "name": "fast",
    "targets": [
      "mock-echo"
    ],
    "hide_targets": false
  },
  {
    "name": "smart",
    "targets": [
      "mock-think(high)",
      "large"
    ],
    "hide_targets": false
  }
]
```

`400` when the body is not such an array; `422` when a rule is broken, each
issue at `[i].name`, `[i].targets` or `[i].targets[j]`:

* a name must not be empty, start or end with a space, contain white space
  (clients send it as a model name), repeat another alias's name (ignoring
  case), or end with a reasoning suffix such as `(high)` — clients add that
  themselves, and a request for `name(high)` would never reach the alias;
* an alias needs at least one target (`[i].targets`);
* a target must not be empty, start or end with a space, or be the alias
  itself (`[i].targets[j]`).

A target that matches no model, or carries a suffix the gateway does not
know, is *not* refused — the model may appear later: it shows as a warning
in `GET /status`, and an alias none of whose targets is routable is
`ignored` in `GET /models`.

```http
PUT /admin/api/aliases

[
  {
    "name": "loop",
    "targets": [
      "loop"
    ]
  }
]
```

```http
HTTP/1.1 422 Unprocessable Entity

{
  "error": {
    "message": "the configuration is not valid: [0].targets[0]: an alias cannot target itself",
    "issues": [
      {
        "path": "[0].targets[0]",
        "message": "an alias cannot target itself"
      }
    ]
  }
}
```

### `GET /payload`, `PUT /payload`

Rules that patch the JSON body sent upstream, applied in the order
`default` (set a field only when the client did not), `override` (always
set), `filter` (remove). A rule:

| Field | Meaning |
|---|---|
| `models` | Wildcard patterns matched against the upstream model id and the client-requested name. Required, at least one. |
| `protocol` | Only when the upstream request uses this protocol; `null` for any. |
| `provider` | Only for this provider; `""` for any. |
| `set` | `default` / `override` rules: path → JSON value. Required there. Its fields are listed in the order of the file. |
| `remove` | `filter` rules: paths to delete. Required there. |

A **path** names a field of the upstream request body: object keys
separated by `.` (`generationConfig.thinkingConfig.thinkingBudget`); a part
made of digits indexes an array when the value there is one
(`messages.0.role`) and is a key otherwise; `\.` is a dot inside a key
(`metadata.trace\.id`) and `\\` a backslash. There are no wildcards: `*` is
a key like any other. A path that can never address a field is refused
(`422`): an empty one, one with an empty part (`a..b`, a dot at the start
or end), and one with spaces or control characters (around it or inside a
part). Only the paths of the field a rule's list reads are checked — `set`
of `default` / `override` rules, `remove` of `filter` rules.

Rules apply to the built-in `mock` provider too, so a rule can be tried out
before a real provider exists: the mock speaks whatever protocol the client
speaks, so its "upstream request" is in the client's layout, and the
captured `upstream_request` of the [request record](#get-requestsid) shows
the request the mock answered, rules applied.

`PUT` replaces all three lists with the body, an object
`{default?, override?, filter?}` whose rules may leave out `protocol`,
`provider`, and whichever of `set` / `remove` does not apply; a list that is
left out is emptied. It returns the new rules in the full shape. A rule the
`PUT` leaves unchanged keeps its text in the file, and the order of its
`set` fields with it, even when the body lists them in another order; a
field added to a rule goes at its end.

```http
PUT /admin/api/payload

{
  "default": [
    {
      "models": [
        "mock-*"
      ],
      "set": {
        "temperature": 0.2
      }
    }
  ],
  "override": [
    {
      "models": [
        "large"
      ],
      "protocol": "openai-chat",
      "provider": "vendor",
      "set": {
        "reasoning_effort": "high"
      }
    }
  ],
  "filter": [
    {
      "models": [
        "*"
      ],
      "remove": [
        "metadata.trace_id"
      ]
    }
  ]
}
```

```http
HTTP/1.1 200 OK

{
  "default": [
    {
      "models": [
        "mock-*"
      ],
      "protocol": null,
      "provider": "",
      "set": {
        "temperature": 0.2
      },
      "remove": []
    }
  ],
  "override": [
    {
      "models": [
        "large"
      ],
      "protocol": "openai-chat",
      "provider": "vendor",
      "set": {
        "reasoning_effort": "high"
      },
      "remove": []
    }
  ],
  "filter": [
    {
      "models": [
        "*"
      ],
      "protocol": null,
      "provider": "",
      "set": {},
      "remove": [
        "metadata.trace_id"
      ]
    }
  ]
}
```

```http
GET /admin/api/payload
```

```http
HTTP/1.1 200 OK

{
  "default": [
    {
      "models": [
        "mock-*"
      ],
      "protocol": null,
      "provider": "",
      "set": {
        "temperature": 0.2
      },
      "remove": []
    }
  ],
  "override": [
    {
      "models": [
        "large"
      ],
      "protocol": "openai-chat",
      "provider": "vendor",
      "set": {
        "reasoning_effort": "high"
      },
      "remove": []
    }
  ],
  "filter": [
    {
      "models": [
        "*"
      ],
      "protocol": null,
      "provider": "",
      "set": {},
      "remove": [
        "metadata.trace_id"
      ]
    }
  ]
}
```

`400` when the body is not that object (unknown key, unknown `protocol`);
`422` when a rule has no model pattern, lacks its `set` / `remove`, has a
path that can never address a field (see above), or sets a value the
configuration file cannot hold — `null` anywhere in a `set` value, or an
integer from 9223372036854775808 to 18446744073709551615 (a whole number
beyond that is a floating-point number and is stored as one, see
[Configuration edits](#configuration-edits)). To make the upstream body lose
a field, use a `filter` rule; a rule cannot set a field to `null`. Both
statuses name fields from the same root, the body: `default[0].protocol`,
`override[0].set.response_format`, `filter[0].remove[1]`. An issue about a
path of `set` is named `…set.` followed by the path exactly as sent, dots
and all (`default[0].set.reasoning..effort`; `default[0].set.` for an empty
one).

```http
PUT /admin/api/payload

{
  "override": [
    {
      "models": [
        "large"
      ],
      "set": {
        "response_format": null
      }
    }
  ]
}
```

```http
HTTP/1.1 422 Unprocessable Entity

{
  "error": {
    "message": "the configuration is not valid: override[0].set.response_format: is null, which a TOML file cannot hold; leave the field out instead",
    "issues": [
      {
        "path": "override[0].set.response_format",
        "message": "is null, which a TOML file cannot hold; leave the field out instead"
      }
    ]
  }
}
```

```http
PUT /admin/api/payload

{
  "default": [
    {
      "models": [
        "mock-*"
      ],
      "set": {
        "reasoning..effort": "low"
      }
    }
  ],
  "filter": [
    {
      "models": [
        "*"
      ],
      "remove": [
        "metadata.trace_id",
        "user "
      ]
    }
  ]
}
```

```http
HTTP/1.1 422 Unprocessable Entity

{
  "error": {
    "message": "the configuration is not valid: default[0].set.reasoning..effort: has an empty part (two dots in a row, or a dot at the start or end); write a dot inside a field name as `\\.`; filter[0].remove[1]: must not start or end with a space",
    "issues": [
      {
        "path": "default[0].set.reasoning..effort",
        "message": "has an empty part (two dots in a row, or a dot at the start or end); write a dot inside a field name as `\\.`"
      },
      {
        "path": "filter[0].remove[1]",
        "message": "must not start or end with a space"
      }
    ]
  }
}
```

### `GET /pricing`, `PUT /pricing`

Prices in USD per million tokens, for cost estimates. The first entry whose
`model` pattern (wildcards, matched against the upstream model id) fits
wins. `cache_read` and `cache_write` default to `input` when `null`.

`PUT` replaces the whole list with the body — an array of
`{model, input, output, cache_read?, cache_write?}` — and returns the new
list. `input` and `output` are `0` when left out. Prices apply to requests
from then on; recorded costs do not change.

```http
PUT /admin/api/pricing

[
  {
    "model": "mock-*",
    "input": 0.5,
    "output": 1.5
  },
  {
    "model": "vendor-large",
    "input": 3,
    "output": 15,
    "cache_read": 0.3,
    "cache_write": 3.75
  }
]
```

```http
HTTP/1.1 200 OK

[
  {
    "model": "mock-*",
    "input": 0.5,
    "output": 1.5,
    "cache_read": null,
    "cache_write": null
  },
  {
    "model": "vendor-large",
    "input": 3.0,
    "output": 15.0,
    "cache_read": 0.3,
    "cache_write": 3.75
  }
]
```

```http
GET /admin/api/pricing
```

```http
HTTP/1.1 200 OK

[
  {
    "model": "mock-*",
    "input": 0.5,
    "output": 1.5,
    "cache_read": null,
    "cache_write": null
  },
  {
    "model": "vendor-large",
    "input": 3.0,
    "output": 15.0,
    "cache_read": 0.3,
    "cache_write": 3.75
  }
]
```

`400` when the body is not such an array; `422` for an empty `model`
(`[i].model`) or a price that is negative or not a finite number, named by
its own field (`[i].input`, `[i].output`, `[i].cache_read`,
`[i].cache_write`).

```http
PUT /admin/api/pricing

[
  {
    "model": "mock-*",
    "input": 0.5,
    "output": 1.5
  },
  {
    "model": "vendor-large",
    "input": 3,
    "output": -15
  }
]
```

```http
HTTP/1.1 422 Unprocessable Entity

{
  "error": {
    "message": "the configuration is not valid: [1].output: must be a number of 0 or more (USD per million tokens)",
    "issues": [
      {
        "path": "[1].output",
        "message": "must be a number of 0 or more (USD per million tokens)"
      }
    ]
  }
}
```

---------------------------------------------------------------------------

## Client keys

The keys applications present to use the gateway (`auth.keys`).

`{id}` is `key_` plus 12 hex digits: a hash of the key's value (for a
reference, of the value it resolves to — or of the reference text while its
variable is unset). It is the `client.key_id` of request records.

### `GET /keys`

```http
GET /admin/api/keys
```

```http
HTTP/1.1 200 OK

[
  {
    "id": "key_df58088f8694",
    "name": "laptop",
    "masked": "sy-Zk3…tQwE",
    "is_reference": false,
    "resolved": true,
    "enabled": true,
    "models": [],
    "rate_limit_rpm": null,
    "usage": {
      "requests": 3,
      "errors": 1,
      "tokens": 74,
      "cost": 0.00010499999999999999,
      "last_used_at": 1791004027955
    }
  },
  {
    "id": "key_570ef9a5569d",
    "name": "ci",
    "masked": "sy-ABn…AVTH",
    "is_reference": false,
    "resolved": true,
    "enabled": true,
    "models": [
      "mock-*"
    ],
    "rate_limit_rpm": 120,
    "usage": {
      "requests": 0,
      "errors": 0,
      "tokens": 0,
      "cost": 0.0,
      "last_used_at": null
    }
  }
]
```

| Field | Meaning |
|---|---|
| `id` | See above. |
| `name` | The label, trimmed. The usage statistics (`by_key`, `group_by=key`) and the request list show requests under the name the key had when they were made. |
| `masked` | The masked key, or the reference as written. |
| `is_reference` | The key is `env:NAME` / `${NAME}`. |
| `resolved` | `false` for a reference whose variable is not set: the key cannot be used until it is. |
| `enabled` | Disabled keys are refused by the client API (`401`) — while `auth.required` is on. With `auth.required = false` a disabled key, like an unknown one, is served as an anonymous client. |
| `models` | Wildcard patterns of models the key may use; `[]` means all. Patterns match model names ignoring case. Shown as `POST` and `PATCH` store them: trimmed, without empty entries, each pattern once — compared ignoring case, the first spelling kept (`["GPT-*", "gpt-*"]` is stored as `["GPT-*"]`) — also when the file was written by hand. |
| `rate_limit_rpm` | Requests per minute, `null` for unlimited. Never `0`. |
| `usage` | What this key did over the last 30 days (the `30d` range of `/usage/summary`, cut to `usage.retention_days`): `requests`, `errors`, `tokens` (prompt + output), `cost` (USD), and `last_used_at`, the start (unix ms) of its most recent request **within that same window**, else `null` — `null` means "no request in the last 30 days" (or since the cut), not "never used". |

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

```http
POST /admin/api/keys

{
  "name": "ci",
  "models": [
    "mock-*"
  ],
  "rate_limit_rpm": 120
}
```

```http
HTTP/1.1 201 Created

{
  "id": "key_570ef9a5569d",
  "key": "sy-ABnPoFQkP3RwtkPb8YQ1ywTQm7KPawgSWYGEAVTH",
  "is_reference": false
}
```

* `400` — `name` missing, empty, longer than 100 characters; `key` with
  spaces; a wrong type (`rate_limit_rpm: must be a whole number from 1 to
  4294967295`); an unknown field.
* `409` — the name is taken (compared without regard to case), or the key
  already exists. `issues` names the field: `name` or `key`.
* `422` — `rate_limit_rpm` is `0` (a limit of nothing would refuse every
  request; leave the field out for no limit), or `key` is a reference that
  names no variable (`env:`, `${}`). The issue is on `rate_limit_rpm` or
  `key`.

A literal `key` of your own is accepted at any length: choosing one that is
long and random is up to you.

```http
POST /admin/api/keys

{
  "name": "ci"
}
```

```http
HTTP/1.1 409 Conflict

{
  "error": {
    "message": "a client key named `ci` already exists",
    "issues": [
      {
        "path": "name",
        "message": "is the name of another client key"
      }
    ]
  }
}
```

```http
POST /admin/api/keys

{
  "name": "nightly",
  "rate_limit_rpm": 0
}
```

```http
HTTP/1.1 422 Unprocessable Entity

{
  "error": {
    "message": "the configuration is not valid: rate_limit_rpm: must be at least 1; leave it out for no limit",
    "issues": [
      {
        "path": "rate_limit_rpm",
        "message": "must be at least 1; leave it out for no limit"
      }
    ]
  }
}
```

### `PATCH /keys/{id}`

Request: any of `{"name", "enabled", "models", "rate_limit_rpm"}`; what is
left out stays. `"rate_limit_rpm": null` removes the limit; `"models": []`
allows every model. The key's value cannot be changed (create another key).
Response: the updated entry, as in `GET /keys`.

`404` unknown id; `409` the new name is taken (issue on `name`); `400`
unknown field or wrong type; `422` `rate_limit_rpm` of `0` (issue on
`rate_limit_rpm`).

```http
PATCH /admin/api/keys/key_570ef9a5569d

{
  "name": "ci-runner",
  "rate_limit_rpm": null,
  "enabled": false
}
```

```http
HTTP/1.1 200 OK

{
  "id": "key_570ef9a5569d",
  "name": "ci-runner",
  "masked": "sy-ABn…AVTH",
  "is_reference": false,
  "resolved": true,
  "enabled": false,
  "models": [
    "mock-*"
  ],
  "rate_limit_rpm": null,
  "usage": {
    "requests": 0,
    "errors": 0,
    "tokens": 0,
    "cost": 0.0,
    "last_used_at": null
  }
}
```

### `POST /keys/{id}/reveal`

The key as the configuration holds it. For a reference that is the
reference text (`"env:NAME"`, `is_reference: true`), not the variable's
value. The body is ignored. `404` unknown id.

```http
POST /admin/api/keys/key_570ef9a5569d/reveal

{}
```

```http
HTTP/1.1 200 OK

{
  "key": "sy-ABnPoFQkP3RwtkPb8YQ1ywTQm7KPawgSWYGEAVTH",
  "is_reference": false
}
```

### `DELETE /keys/{id}`

The key stops working at once. `404` unknown id.

```http
DELETE /admin/api/keys/key_570ef9a5569d
```

```http
HTTP/1.1 200 OK

{
  "ok": true
}
```

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

`cost` in totals — here, in `by_*` rows, in time-series points and groups —
is a plain sum and is `0` both for usage that no price matched and for usage
priced at zero: the two cannot be told apart in aggregates. A single
[request record](#get-requestslimitbeforesincemodelclient_modelproviderkeystatusq) can:
its `cost` is `null` when no price matched.

### `GET /usage/summary?range=`

`range`: `1h`, `24h` (default), `7d`, `30d`. An invalid value is `400`,
with the issue on `range`.

```http
GET /admin/api/usage/summary?range=24h
```

```http
HTTP/1.1 200 OK

{
  "range": "24h",
  "from": 1790917680000,
  "to": 1791004028358,
  "totals": {
    "requests": 7,
    "errors": 2,
    "input_tokens": 12,
    "cache_read_tokens": 0,
    "cache_write_tokens": 0,
    "output_tokens": 74,
    "reasoning_tokens": 44,
    "cost": 0.000117,
    "duration_ms_sum": 157,
    "ttfb_ms_sum": 127,
    "ttfb_count": 5
  },
  "latency": {
    "window_ms": 86400000,
    "p50": 30,
    "p90": 34,
    "p95": 34,
    "p99": 34,
    "ttfb_p50": 31,
    "ttfb_p95": 34,
    "samples": 7,
    "ttfb_samples": 5
  },
  "error_rate": 0.2857142857142857,
  "requests_per_minute": 7,
  "tokens_per_minute": 86,
  "by_model": [
    {
      "name": "mock-echo",
      "requests": 4,
      "errors": 0,
      "input_tokens": 9,
      "cache_read_tokens": 0,
      "cache_write_tokens": 0,
      "output_tokens": 9,
      "reasoning_tokens": 0,
      "cost": 0.000017999999999999997,
      "duration_ms_sum": 124,
      "ttfb_ms_sum": 95,
      "ttfb_count": 4
    },
    {
      "name": "mock-error-500",
      "requests": 1,
      "errors": 1,
      "input_tokens": 0,
      "cache_read_tokens": 0,
      "cache_write_tokens": 0,
      "output_tokens": 0,
      "reasoning_tokens": 0,
      "cost": 0.0,
      "duration_ms_sum": 1,
      "ttfb_ms_sum": 0,
      "ttfb_count": 0
    },
    {
      "name": "mock-think",
      "requests": 1,
      "errors": 0,
      "input_tokens": 3,
      "cache_read_tokens": 0,
      "cache_write_tokens": 0,
      "output_tokens": 65,
      "reasoning_tokens": 44,
      "cost": 0.000099,
      "duration_ms_sum": 32,
      "ttfb_ms_sum": 32,
      "ttfb_count": 1
    },
    {
      "name": "no-such-model",
      "requests": 1,
      "errors": 1,
      "input_tokens": 0,
      "cache_read_tokens": 0,
      "cache_write_tokens": 0,
      "output_tokens": 0,
      "reasoning_tokens": 0,
      "cost": 0.0,
      "duration_ms_sum": 0,
      "ttfb_ms_sum": 0,
      "ttfb_count": 0
    }
  ],
  "by_provider": [
    {
      "name": "mock",
      "requests": 6,
      "errors": 1,
      "input_tokens": 12,
      "cache_read_tokens": 0,
      "cache_write_tokens": 0,
      "output_tokens": 74,
      "reasoning_tokens": 44,
      "cost": 0.000117,
      "duration_ms_sum": 157,
      "ttfb_ms_sum": 127,
      "ttfb_count": 5
    },
    {
      "name": "unknown",
      "requests": 1,
      "errors": 1,
      "input_tokens": 0,
      "cache_read_tokens": 0,
      "cache_write_tokens": 0,
      "output_tokens": 0,
      "reasoning_tokens": 0,
      "cost": 0.0,
      "duration_ms_sum": 0,
      "ttfb_ms_sum": 0,
      "ttfb_count": 0
    }
  ],
  "by_key": [
    {
      "name": "dashboard",
      "requests": 4,
      "errors": 1,
      "input_tokens": 6,
      "cache_read_tokens": 0,
      "cache_write_tokens": 0,
      "output_tokens": 6,
      "reasoning_tokens": 0,
      "cost": 0.000012,
      "duration_ms_sum": 93,
      "ttfb_ms_sum": 64,
      "ttfb_count": 3
    },
    {
      "name": "laptop",
      "requests": 3,
      "errors": 1,
      "input_tokens": 6,
      "cache_read_tokens": 0,
      "cache_write_tokens": 0,
      "output_tokens": 68,
      "reasoning_tokens": 44,
      "cost": 0.00010499999999999999,
      "duration_ms_sum": 64,
      "ttfb_ms_sum": 63,
      "ttfb_count": 2
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `from`, `to` | The range: `to` is now, `from` the start of its first bucket. |
| `totals` | The [totals](#totals) of the range. |
| `error_rate` | `totals.errors / totals.requests` — **over the range** (`0` without requests). |
| `requests_per_minute`, `tokens_per_minute` | Requests finished and tokens used (prompt + output) in the **last 60 seconds, whatever the range**: a `30d` summary of a gateway that is idle right now says `0` for both. They are the `rpm` and `tpm` of the `stats` live frame; the error rate of the same 60 seconds is only in that frame (`error_rate_1m`). |
| `latency` | Percentiles in milliseconds (`p50`, `p90`, `p95`, `p99`, time to first byte `ttfb_p50`, `ttfb_p95`) over `window_ms`, with the numbers of requests they are made of (`samples`, `ttfb_samples`). |
| `by_model`, `by_provider`, `by_key` | Totals per client-facing model, provider and client key name, most requests first. |

**Latency.** The percentiles of `1h` describe the last 60 minutes; those of
`24h`, `7d` and `30d` all describe the last 24 hours (`window_ms` says
which). **When `samples` is `0` the percentiles are `0` and mean "no
data"**, not "0 ms": check `samples` (`ttfb_samples` for the two `ttfb_*`
values) before showing them. `samples` is not `totals.requests`: the
latency window is cut in whole clock hours (the current hour and the 23
before it) while the totals of `24h` cover the last 1440 minutes, so
`samples` is the smaller number by the requests of the oldest partial hour.

Names that stand for "none" in the breakdowns: `unknown` — as a model, a
request refused before a model could be read; as a provider, the requests
no provider served: they failed before routing, or every credential of the
model was cooling down — `anonymous` (no client key), `dashboard` (the
playground).

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

**How many points.** A range is a fixed number of stored buckets ending
with the current one — 60 minutes for `1h`, 1440 minutes for `24h`, 168
hours for `7d`, 720 hours for `30d` — regrouped into slices of the bucket
width that are cut on clock boundaries:

| `range` | default bucket | points |
|---|---|---|
| `1h` | `minute` | 60 |
| `24h` | `hour` | 25 (24 in the last minute of a clock hour) |
| `7d` | `hour` | 168 |
| `30d` | `day` | 31 (30 in the last hour of a UTC day) |

With another `bucket` the count follows the same rule (`24h` by `minute`:
1440; `7d` by `day`: 8, or 7 in the last hour of a UTC day).

**Buckets are cut in UTC.** Hours and minutes are the same everywhere; a
**day bucket runs from 00:00 UTC to 00:00 UTC** (`t` is a UTC midnight).
There is no time-zone parameter: label day buckets with their UTC date.

The last two of the 60 points of an hour:

```http
GET /admin/api/usage/timeseries?range=1h&bucket=minute&group_by=model
```

```http
HTTP/1.1 200 OK

{
  "range": "1h",
  "bucket": "minute",
  "bucket_ms": 60000,
  "group_by": "model",
  "from": 1791000480000,
  "to": 1791004028359,
  "series": [
    "mock-echo",
    "mock-error-500",
    "mock-think",
    "no-such-model"
  ],
  "points": [
    {
      "t": 1791003960000,
      "requests": 0,
      "errors": 0,
      "input_tokens": 0,
      "cache_read_tokens": 0,
      "cache_write_tokens": 0,
      "output_tokens": 0,
      "reasoning_tokens": 0,
      "cost": 0.0,
      "duration_ms_sum": 0,
      "ttfb_ms_sum": 0,
      "ttfb_count": 0,
      "groups": {}
    },
    {
      "t": 1791004020000,
      "requests": 7,
      "errors": 2,
      "input_tokens": 12,
      "cache_read_tokens": 0,
      "cache_write_tokens": 0,
      "output_tokens": 74,
      "reasoning_tokens": 44,
      "cost": 0.000117,
      "duration_ms_sum": 157,
      "ttfb_ms_sum": 127,
      "ttfb_count": 5,
      "groups": {
        "mock-echo": {
          "requests": 4,
          "errors": 0,
          "tokens": 18,
          "cost": 0.000017999999999999997
        },
        "mock-error-500": {
          "requests": 1,
          "errors": 1,
          "tokens": 0,
          "cost": 0.0
        },
        "mock-think": {
          "requests": 1,
          "errors": 0,
          "tokens": 68,
          "cost": 0.000099
        },
        "no-such-model": {
          "requests": 1,
          "errors": 1,
          "tokens": 0,
          "cost": 0.0
        }
      }
    }
  ]
}
```

### `GET /requests?limit=&before=&since=&model=&client_model=&provider=&key=&status=&q=`

The most recent requests, newest first (by start time), from the in-memory
list.

**What is in the list.**

* **Finished requests only.** A record exists from the moment a request
  ends. A request in flight is in no list and `GET /requests/{id}` answers
  `404` for it until it finishes; it is visible as a `request.started` live
  frame and in the `in_flight` gauge of `GET /status`.
* **At most `capacity` of them** (2000): the list is a ring, the oldest
  record leaves when a new one arrives. `total` therefore stops at
  `capacity`, however many requests the gateway has served (that number is
  `live.totals.requests` in `GET /status`). Older records are still found
  by id (`GET /requests/{id}`) while the usage files keep them.
* **Not the requests refused for their client key.** A request with a
  missing or wrong key is answered `401` before it reaches the pipeline and
  leaves no record — and nothing in the statistics. Its line in the log
  carries a request id all the same, which resolves to nothing: `404` from
  `GET /requests/{id}` is the expected answer for it. (So `key=anonymous`
  only finds requests admitted without a key, with `auth.required = false`.)
  The same goes for the other requests the server answers by itself: a
  body it cannot read or that is too large, an unknown route, and the model
  listings (`GET /v1/models`), which are no generation requests.

| Parameter | Meaning |
|---|---|
| `limit` | Page size, default 50, clamped to 1–500 (`0` returns one item). Not a nonnegative whole number within the supported integer range: `400`, issue on `limit`. |
| `before` | Cursor: the `next_before` of the previous page. (A request id or a unix-ms timestamp works too: the requests that started before it.) Anything else is refused with `400` and the issue on `before` — not answered with an empty page. |
| `since` | Unix ms: only requests that started at or after it (`started_at >= since`). A filter like the others, so `total` counts only those. Not a whole number: `400`, issue on `since`. |
| `model` | Exact model name — requested, client-facing or upstream — ignoring case. `unknown` selects the requests that have no model (refused before one could be read), the row the summaries list them under. |
| `client_model` | Exact client-facing model name, ignoring case, compared with the name alone that `by_model` of [`/usage/summary`](#get-usagesummaryrange) and `group_by=model` of `/usage/timeseries` count a request under: the resolved model (`client_model` of the record), else the name the client asked for. So a row of those breakdowns opens exactly its requests — which `model` does not promise, as it also matches the upstream name (an alias's requests under its target's row). `unknown` selects the requests without a model. |
| `provider` | Exact provider name, ignoring case; `unknown` for the requests no provider served: they failed before routing, or every credential of the model was cooling down. |
| `key` | Client key name or id, ignoring case; `anonymous` for requests without a key. |
| `status` | `ok`, `error`, an HTTP status from `100` to `599` (`429`) or a class from `1xx` to `5xx`. Anything else: `400`, issue on `status`. |
| `q` | Substring, ignoring case, searched in id, model names, provider, credential label, key name, endpoint, and in the error's `kind` and `message` (`q=rate_limit`, `q=client_disconnect`). |

The filters combine (a request is listed when it passes all of them).
`total` counts the requests in memory that match the filters, ignoring
paging (so at most `capacity`). `next_before` is `null` on the last page.
`capacity` is how many records the list can hold. A parameter given twice
is refused with `400`, the issue on its name.

```http
GET /admin/api/requests?limit=2&status=ok
```

```http
HTTP/1.1 200 OK

{
  "items": [
    {
      "id": "01a10028-ada1-77d3-bb63-b01260746ef1",
      "started_at": 1791004028322,
      "finished_at": 1791004028351,
      "duration_ms": 29,
      "ttfb_ms": 0,
      "client": {
        "key_id": null,
        "key_name": "dashboard",
        "ip": "127.0.0.1",
        "user_agent": null
      },
      "client_protocol": "anthropic",
      "endpoint": "POST /admin/api/playground",
      "transport": "sse",
      "stream": true,
      "requested_model": "mock-echo",
      "client_model": "mock-echo",
      "upstream_model": "mock-echo",
      "provider": "mock",
      "credential_id": "mock:8da14ba21598",
      "credential_label": "mock",
      "upstream_protocol": "anthropic",
      "mode": "mock",
      "status": 200,
      "ok": true,
      "error": null,
      "usage": {
        "input_tokens": 2,
        "cache_read_tokens": 0,
        "cache_write_tokens": 0,
        "output_tokens": 2,
        "reasoning_tokens": 0
      },
      "cost": 4e-6,
      "reasoning": null,
      "attempts": [
        {
          "provider": "mock",
          "credential_id": "mock:8da14ba21598",
          "credential_label": "mock",
          "upstream_model": "mock-echo",
          "upstream_protocol": "anthropic",
          "status": 200,
          "ok": true,
          "error": null,
          "duration_ms": 0
        }
      ],
      "has_bodies": true
    },
    {
      "id": "01a10028-ad82-75e6-b429-e72d90a3b077",
      "started_at": 1791004028290,
      "finished_at": 1791004028320,
      "duration_ms": 30,
      "ttfb_ms": 30,
      "client": {
        "key_id": null,
        "key_name": "dashboard",
        "ip": "127.0.0.1",
        "user_agent": null
      },
      "client_protocol": "gemini",
      "endpoint": "POST /admin/api/playground",
      "transport": "http",
      "stream": false,
      "requested_model": "mock-echo",
      "client_model": "mock-echo",
      "upstream_model": "mock-echo",
      "provider": "mock",
      "credential_id": "mock:8da14ba21598",
      "credential_label": "mock",
      "upstream_protocol": "gemini",
      "mode": "mock",
      "status": 200,
      "ok": true,
      "error": null,
      "usage": {
        "input_tokens": 2,
        "cache_read_tokens": 0,
        "cache_write_tokens": 0,
        "output_tokens": 2,
        "reasoning_tokens": 0
      },
      "cost": 4e-6,
      "reasoning": null,
      "attempts": [
        {
          "provider": "mock",
          "credential_id": "mock:8da14ba21598",
          "credential_label": "mock",
          "upstream_model": "mock-echo",
          "upstream_protocol": "gemini",
          "status": 200,
          "ok": true,
          "error": null,
          "duration_ms": 29
        }
      ],
      "has_bodies": true
    }
  ],
  "next_before": "1791004028290:01a10028-ad82-75e6-b429-e72d90a3b077",
  "has_more": true,
  "total": 5,
  "capacity": 2000
}
```

The requests of one summary row since a moment — and a `since` that is not
a number:

```http
GET /admin/api/requests?client_model=mock-echo&since=1790917628402&limit=1
```

```http
HTTP/1.1 200 OK

{
  "items": [
    {
      "id": "01a10028-ada1-77d3-bb63-b01260746ef1",
      "started_at": 1791004028322,
      "finished_at": 1791004028351,
      "duration_ms": 29,
      "ttfb_ms": 0,
      "client": {
        "key_id": null,
        "key_name": "dashboard",
        "ip": "127.0.0.1",
        "user_agent": null
      },
      "client_protocol": "anthropic",
      "endpoint": "POST /admin/api/playground",
      "transport": "sse",
      "stream": true,
      "requested_model": "mock-echo",
      "client_model": "mock-echo",
      "upstream_model": "mock-echo",
      "provider": "mock",
      "credential_id": "mock:8da14ba21598",
      "credential_label": "mock",
      "upstream_protocol": "anthropic",
      "mode": "mock",
      "status": 200,
      "ok": true,
      "error": null,
      "usage": {
        "input_tokens": 2,
        "cache_read_tokens": 0,
        "cache_write_tokens": 0,
        "output_tokens": 2,
        "reasoning_tokens": 0
      },
      "cost": 4e-6,
      "reasoning": null,
      "attempts": [
        {
          "provider": "mock",
          "credential_id": "mock:8da14ba21598",
          "credential_label": "mock",
          "upstream_model": "mock-echo",
          "upstream_protocol": "anthropic",
          "status": 200,
          "ok": true,
          "error": null,
          "duration_ms": 0
        }
      ],
      "has_bodies": true
    }
  ],
  "next_before": "1791004028322:01a10028-ada1-77d3-bb63-b01260746ef1",
  "has_more": true,
  "total": 4,
  "capacity": 2000
}
```

```http
GET /admin/api/requests?since=yesterday
```

```http
HTTP/1.1 400 Bad Request

{
  "error": {
    "message": "invalid query parameter `since`: must be a whole number of unix milliseconds",
    "issues": [
      {
        "path": "since",
        "message": "must be a whole number of unix milliseconds"
      }
    ]
  }
}
```

A request record:

| Field | Meaning |
|---|---|
| `id` | UUIDv7; also the `x-request-id` the client received. |
| `started_at`, `finished_at`, `duration_ms`, `ttfb_ms` | Timing; `ttfb_ms` is `null` when nothing was sent before the end. |
| `client` | `key_id`, `key_name` (never the key), `ip`, `user_agent`; each `null` when unknown. |
| `client_protocol`, `endpoint`, `transport`, `stream` | How the client asked: protocol, `"POST /v1/messages"`-style label, `http` / `sse` / `websocket`, and whether it asked for a stream. |
| `requested_model` | The model name **as the client wrote it**, reasoning suffix included (`"mock-echo(high)"`); `""` when the request was refused before a model could be read. This is the field to rebuild the request from (a Gemini URL, say). |
| `client_model` | The client-facing model the name resolved to, **without the reasoning suffix** (`"mock-echo"`); `null` before routing: when the model is unknown or the request was refused earlier. |
| `upstream_model` | The id sent upstream (the last attempt); `null` when nothing was sent. |
| `provider`, `credential_id`, `credential_label`, `upstream_protocol` | Who served it (the last attempt). `null` for the requests no provider served: they failed before routing, or every credential of the model was cooling down. The breakdowns and the `provider` filter call these `unknown`. |
| `mode` | `passthrough`, `translated`, `mock`, `raw`, or `null`. |
| `status`, `ok` | The HTTP status sent to the client (`499`: the client went away; `101`: a WebSocket session relayed to the upstream, recorded when it ends). |
| `error` | `null`, or `{kind, message, upstream_status}`. Every `kind` there is: `invalid_request` (400), `permission` (403: the key may not use the model), `not_found` (404: unknown model), `too_large` (413: an answer or a request beyond what the gateway or the upstream takes), `rate_limit` (429: the key's own limit, an upstream limit, or every credential resting), `upstream` (502; also a relayed WebSocket session whose upstream side failed — status `101`, with `upstream_status: null`), `unavailable` (503), `timeout` (504), `internal` (500), `client_disconnect` (499: the client went away before the answer was complete; also a relayed WebSocket session whose client side failed, status `101`), `aborted` (status `101`: a relayed WebSocket session that ended without being closed in order — the task relaying it was dropped) — and `authentication`, which exists but is not recorded, since requests refused for their key leave no record. |
| `usage` | Token counts (see [Totals](#totals)). |
| `cost` | Estimated USD, `null` without a matching price or for failures. |
| `reasoning` | The reasoning depth that was applied, as a label, or `null`. |
| `attempts` | Every upstream call, in order: `provider`, `credential_id`, `credential_label`, `upstream_model`, `upstream_protocol`, `status` (`0`: no response), `ok`, `error`, `duration_ms`. |
| `has_bodies` | Whether bodies were captured (`logging.request_log`). Always `false` for a relayed WebSocket session (status `101`): its frames are not captured. |

### `GET /requests/{id}`

The record — from memory, else from the usage files — and its captured
bodies, or `"bodies": null` when none were captured. Bodies are text,
truncated to `logging.request_log_max_body_kb` and redacted by the gateway;
they describe the last attempt. `client_headers` and `upstream_headers` are
redacted header maps (empty when the pipeline was given none). `404` unknown
id — and for a request still in flight. (Bodies are cut to 400 characters
here.)

When each part is `null`:

* `bodies` as a whole — nothing was captured: `logging.request_log` is
  `off`, or it is `errors` and the request succeeded.
* `bodies.upstream_request` — nothing was sent upstream (the request was
  refused before an attempt).
* `bodies.upstream_response` — there was no answer to capture: no attempt
  was made, the upstream never answered, or **the request was served by a
  `mock` provider**, which has no upstream (a mock model's scripted
  *failure* does leave the error body it made up, as in the example).
* `bodies.client_response` — the client was sent nothing.

The bodies of a record with `has_bodies: true` can be read as soon as its
`request.finished` live frame has arrived.

```http
GET /admin/api/requests/01a10028-ac33-756e-9d2e-f81ec74a387d
```

```http
HTTP/1.1 200 OK

{
  "record": {
    "id": "01a10028-ac33-756e-9d2e-f81ec74a387d",
    "started_at": 1791004027955,
    "finished_at": 1791004027956,
    "duration_ms": 1,
    "ttfb_ms": null,
    "client": {
      "key_id": "key_df58088f8694",
      "key_name": "laptop",
      "ip": null,
      "user_agent": null
    },
    "client_protocol": "openai-chat",
    "endpoint": "POST /v1/chat/completions",
    "transport": "http",
    "stream": false,
    "requested_model": "mock-error-500",
    "client_model": "mock-error-500",
    "upstream_model": "mock-error-500",
    "provider": "mock",
    "credential_id": "mock:8da14ba21598",
    "credential_label": "mock",
    "upstream_protocol": "openai-chat",
    "mode": "mock",
    "status": 502,
    "ok": false,
    "error": {
      "kind": "upstream",
      "message": "Mock upstream failure.",
      "upstream_status": 500
    },
    "usage": {
      "input_tokens": 0,
      "cache_read_tokens": 0,
      "cache_write_tokens": 0,
      "output_tokens": 0,
      "reasoning_tokens": 0
    },
    "cost": null,
    "reasoning": null,
    "attempts": [
      {
        "provider": "mock",
        "credential_id": "mock:8da14ba21598",
        "credential_label": "mock",
        "upstream_model": "mock-error-500",
        "upstream_protocol": "openai-chat",
        "status": 500,
        "ok": false,
        "error": "Mock upstream failure.",
        "duration_ms": 0
      }
    ],
    "has_bodies": true
  },
  "bodies": {
    "client_request": "{\"model\":\"mock-error-500\",\"messages\":[{\"role\":\"user\",\"content\":\"hello there\"}]}",
    "upstream_request": "{\"model\":\"mock-error-500\",\"stream\":false,\"messages\":[{\"role\":\"user\",\"parts\":[{\"type\":\"text\",\"text\":\"hello there\"}]}],\"source\":\"openai-chat\"}",
    "upstream_response": "{\"error\":{\"message\":\"Mock upstream failure.\",\"type\":\"server_error\",\"code\":\"internal_error\"}}",
    "client_response": "{\"error\":{\"message\":\"Mock upstream failure.\",\"type\":\"server_error\",\"param\":null,\"code\":\"internal_error\"}}",
    "client_headers": {},
    "upstream_headers": {}
  }
}
```

### `DELETE /usage`

Forgets all statistics: the request list, every time bucket, the usage
files on disk, and with them the `usage` of every client key (`GET /keys`).
Totals since start (`/status`, `stats`) and captured bodies are not
affected.

```http
DELETE /admin/api/usage
```

```http
HTTP/1.1 200 OK

{
  "ok": true
}
```

### `GET /logs?limit=&level=&q=&target=&before=`

The application log's in-memory tail (the last 2000 lines), oldest first.
The filters combine: a line is returned when it passes all of them.

| Parameter | Meaning | Defaults and validation |
|---|---|---|
| `limit` | Lines per page. | Missing: 200. `0` returns one line; more than the buffer holds returns the whole buffer (2000). Not a nonnegative whole number within the supported integer range: `400`, issue on `limit`. |
| `level` | Least severe level to include: `trace`, `debug`, `info`, `warn`, `error`. | Missing: every level. Not a level name: `400`, issue on `level`. |
| `q` | Substring, ignoring case, searched in message, target and fields. | Missing or empty: no text filter. |
| `target` | The module a line comes from, ignoring case: `switchyard_gateway::pipeline` is exactly that target; `switchyard_gateway::` (ending in `::`) that module and everything below it; `switchyard_*` (ending in `*`) every target that starts so. | Missing or empty: every target. |
| `before` | Cursor: only lines with a smaller `seq` (the `next_before` of the previous page). | Missing: the newest lines. Not a whole number from `0` to `18446744073709551615`: `400`, issue on `before`. `0` and `1` return no line (`seq` starts at 1). |

A malformed value or any parameter given twice is refused (`400`, the
issue on its name); values are not echoed in the error.

A page holds the *newest* matching lines; `next_before` leads to older ones
and is `null` when there are none. `seq` increases by one per line and is
what to de-duplicate on when combining a page with `log` live events.

| Field | Meaning |
|---|---|
| `lines` | The page, oldest first: `seq`, `at`, `level`, `target`, `message`, `fields`. |
| `next_before`, `has_more` | The cursor to the next older page; `null` / `false` when nothing older matches. |
| `last_seq` | `seq` of the newest line in the buffer when the page was taken, **whatever the filters** (`0` for an empty buffer). Every `log` live event with a larger `seq` is news, and none in between was missed — also when the page itself is filtered. |
| `started_at` | When this process started (unix ms; the `started_at` of `GET /status` and of the `hello` frame). `seq` begins at 1 again after a restart: a different `started_at` means the numbers belong to another run and what was kept from before cannot be joined with them. |

```http
GET /admin/api/logs?limit=3
```

```http
HTTP/1.1 200 OK

{
  "lines": [
    {
      "seq": 1,
      "at": 1791004027969,
      "level": "info",
      "target": "switchyard_gateway::gateway",
      "message": "configuration applied",
      "fields": {
        "provider": "mock"
      }
    },
    {
      "seq": 2,
      "at": 1791004027969,
      "level": "warn",
      "target": "switchyard_gateway::failover",
      "message": "upstream attempt failed",
      "fields": {
        "provider": "mock"
      }
    },
    {
      "seq": 3,
      "at": 1791004027969,
      "level": "info",
      "target": "switchyard_gateway::pipeline",
      "message": "request finished",
      "fields": {
        "provider": "mock"
      }
    }
  ],
  "next_before": null,
  "has_more": false,
  "last_seq": 3,
  "started_at": 1791004027826
}
```

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
  protocol's in-stream error shape. A wait the gateway asks for (every
  credential resting, a rate limit) is the **`retry-after` response
  header** of that answer — streamed requests included — and is not
  repeated in the body. (The client API's Responses WebSocket, which has no
  headers per turn, puts it in its error frame's `error.headers` instead.)

So a `2xx` is JSON or SSE depending on `Content-Type`, and a non-`2xx` body
is the protocol's error — *except* for the admin API's own refusals, which
keep the admin shape: `400` for a bad envelope (unknown `protocol`, `body`
not an object, `model` missing for `gemini`, unknown field), `413`, and the
[access](#access) errors. `ui/js/lib/api.js` reads `error.message` from
either shape.

**A `body` that is not JSON.** The dashboard writes the body into the
envelope as text, so a typing mistake in it makes the whole envelope
invalid. Such an error is reported at its place **in the body**, not in the
envelope: `400` with the issue on `body` — `{"path": "body", "message": "is
not valid JSON: line 1, column 37: expected value"}` — where line and
column count from the start of the body's value (columns in characters). A
syntax error before the body is the envelope's own and reads `the request
body is not valid JSON: line L, column C: …` without an issue.

```http
POST /admin/api/playground

{
  "protocol": "openai-chat",
  "model": "mock-echo",
  "body": {
    "messages": [
      {
        "role": "user",
        "content": "Hello"
      }
    ]
  }
}
```

```http
HTTP/1.1 200 OK

{
  "id": "chatcmpl-mock-4ca8dcc6e4868724",
  "object": "chat.completion",
  "created": 1791004028,
  "model": "mock-echo",
  "choices": [
    {
      "index": 0,
      "message": {
        "role": "assistant",
        "content": "Hello",
        "refusal": null
      },
      "logprobs": null,
      "finish_reason": "stop"
    }
  ],
  "usage": {
    "prompt_tokens": 2,
    "completion_tokens": 2,
    "total_tokens": 4,
    "prompt_tokens_details": {
      "cached_tokens": 0
    },
    "completion_tokens_details": {
      "reasoning_tokens": 0
    }
  }
}
```

```http
POST /admin/api/playground

{
  "protocol": "gemini",
  "model": "mock-echo",
  "body": {
    "contents": [
      {
        "role": "user",
        "parts": [
          {
            "text": "Hello"
          }
        ]
      }
    ]
  }
}
```

```http
HTTP/1.1 200 OK

{
  "candidates": [
    {
      "content": {
        "parts": [
          {
            "text": "Hello"
          }
        ],
        "role": "model"
      },
      "finishReason": "STOP",
      "index": 0
    }
  ],
  "usageMetadata": {
    "promptTokenCount": 2,
    "candidatesTokenCount": 2,
    "totalTokenCount": 4
  },
  "modelVersion": "mock-echo",
  "responseId": "mock-4ca8dcc6e4868724"
}
```

A stream:

```http
POST /admin/api/playground

{
  "protocol": "anthropic",
  "model": "mock-echo",
  "stream": true,
  "body": {
    "max_tokens": 64,
    "messages": [
      {
        "role": "user",
        "content": "Hello"
      }
    ]
  }
}
```

```http
HTTP/1.1 200 OK
content-type: text/event-stream

event: message_start
data: {"type":"message_start","message":{"id":"msg_mock-4ca8dcc6e4868724","type":"message","role":"assistant","model":"mock-echo","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":2}}

event: message_stop
data: {"type":"message_stop"}
```

A pipeline error, in the protocol's shape:

```http
POST /admin/api/playground

{
  "protocol": "anthropic",
  "model": "no-such-model",
  "body": {
    "max_tokens": 16,
    "messages": [
      {
        "role": "user",
        "content": "Hello"
      }
    ]
  }
}
```

```http
HTTP/1.1 404 Not Found

{
  "type": "error",
  "error": {
    "type": "not_found_error",
    "message": "unknown model `no-such-model`: no configured provider serves it"
  }
}
```

A bad envelope, in the admin shape:

```http
POST /admin/api/playground

{
  "protocol": "gemini",
  "body": {
    "contents": []
  }
}
```

```http
HTTP/1.1 400 Bad Request

{
  "error": {
    "message": "invalid request: model: is required for the gemini protocol, which names the model in the URL",
    "issues": [
      {
        "path": "model",
        "message": "is required for the gemini protocol, which names the model in the URL"
      }
    ]
  }
}
```

---------------------------------------------------------------------------

## Live events (WebSocket)

### `GET /ws?ticket=<ticket>`

Browsers cannot set headers on a WebSocket, so this one route is
authenticated by a ticket from `POST /ws-ticket` instead of the secret.
The loopback rule applies as everywhere. Before the upgrade:

* `401` — the ticket is missing, unknown, expired, already used, or was
  issued under a different admin secret (each
  connection needs a fresh one);
* `403` — remote peer while remote access is off;
* `404` — admin interface off;
* `400` — a repeated parameter or malformed query encoding (the ticket is
  not consumed); or a good ticket on a request that is not a WebSocket
  upgrade (the ticket is used up anyway);
* `503` — the gateway is shutting down.

Every message, both ways, is a JSON text frame.

**Server → client:** `{"type": <string>, "data": <object>}`.

| `type` | When | `data` |
|---|---|---|
| `hello` | Once, first. | `version`, `topics` (every type that can be subscribed to), `server_time`, `started_at` (when the process started, as in `GET /status`: a client that reconnects and finds another value is talking to a restarted gateway, whose log `seq` and totals since start begin again). |
| `stats` | Right after `hello`, then once a second. | See below. |
| `request.started` | A client request began. | The first part of a request record: `id`, `started_at`, `client`, `client_protocol`, `endpoint`, `transport`, `stream`, `requested_model`. |
| `request.finished` | It ended. One per `request.started`, same `id`. | The full [request record](#get-requestslimitbeforesincemodelclient_modelproviderkeystatusq). |
| `log` | An application log line. | `seq`, `at`, `level`, `target`, `message`, `fields` — as in `GET /logs`. |
| `credential` | A **failed** upstream attempt was held against a credential, or a provider test ran (passed, or failed in a way that counts). Nothing else: see below. | `provider` and `credential`: the runtime part of a [credential entry](#the-provider-view) (no `source`, `index`, `proxy`, `service_account_file`; absent values are omitted rather than `null`). |
| `config.reloaded` | A configuration was applied (`ok: true`) or refused (`ok: false`, e.g. a broken edit of the file or a failed reload). Also sent with `ok: true` when a file that was refused is valid again. The frames come in the order things happened, so the latest one says how the file stands: `ok: false` is never the last word for a file that has been put right, nor `ok: true` for one that was refused afterwards. | `at`, `ok`, `message`. Reload whatever depends on the configuration. |
| `subscribed` | Answer to `subscribe`. | `topics`: the set now in force. |
| `lagged` | This connection could not keep up. | `missed`: how many events were dropped for it. See below for what to do. |
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
for it and reported as `lagged`. A client that takes **no frame at all for
10 seconds** (its socket stops accepting data) is not sent `lagged`: the
connection is closed, without a close frame. Reconnect with a new ticket
and refetch, as after `lagged`.

**`credential` frames do not follow a credential's whole life.** One is
sent when a failed attempt is reported (a cooldown may have started) and
after a provider test. None is sent

* **when a cooldown ends.** The end is not an event: time it from
  `cooldown_until` (and the `until` of each entry of `model_cooldowns`) of
  the last frame or of `GET /providers`, and read the provider again when
  that moment has passed. A client that only trusts frames shows `cooling`
  for ever;
* for successful requests: `requests`, `successes`, `latency_ms` and
  `last_used_at` of a healthy credential move without a frame;
* when a credential is reset, switched on or off, or becomes (un)usable:
  those come with the answer of the request that did it, or with
  `config.reloaded`.

**`lagged`.** Whenever an event is dropped for a connection, that
connection gets a `lagged` frame before the next event it does receive: no
event is lost silently. `missed` counts events of every type, also of types
the connection is not subscribed to, so the frame says "something may be
missing", not what. The stream continues with newer events; nothing is sent
again. A client that keeps state from events has to refetch it:

* the request list (`GET /requests`) — and drop the rows it shows as in
  flight unless a later `request.finished` or the list confirms them: the
  `request.finished` of a `request.started` it has seen may be among the
  dropped;
* the log (`GET /logs`; `last_seq` says where the page ends);
* provider and credential state (`GET /providers`);
* whatever depends on the configuration, if it relies on `config.reloaded`.

```json
{
  "type": "hello",
  "data": {
    "version": "0.1.0",
    "topics": [
      "hello",
      "request.started",
      "request.finished",
      "log",
      "credential",
      "config.reloaded",
      "stats"
    ],
    "server_time": 1791004028469,
    "started_at": 1791004027826
  }
}
```

`stats`: `in_flight`, `active_streams` and `ws_connections` are gauges
(`ws_connections` counts the client API's WebSockets, not dashboard
connections); `rpm` and `tpm` are requests and tokens finished in the last
60 seconds, `error_rate_1m` the share of those that failed; `p50_ms` and
`p95_ms` are request durations over the last hour, made of
`latency_samples` requests — **`0` samples means no request finished in
that hour and the two percentiles, then `0`, mean "no data"**; `totals` are
the [totals](#totals) since start.

**Across a restart** the two kinds of number behave differently. The
percentiles (and `latency_samples`) are computed from the request records,
which are read back from the usage files at start (with `usage.persist`
on): they **survive a restart** and still describe the last hour. `totals`
(and `uptime_ms`) count since *this* process started and **begin again at
zero** — as `started_at` of `hello` says.

```json
{
  "type": "stats",
  "data": {
    "at": 1791004028475,
    "in_flight": 0,
    "active_streams": 0,
    "ws_connections": 0,
    "rpm": 9,
    "tpm": 92,
    "error_rate_1m": 0.3333333333333333,
    "p50_ms": 29,
    "p95_ms": 34,
    "latency_samples": 9,
    "uptime_ms": 649,
    "totals": {
      "requests": 9,
      "errors": 3,
      "input_tokens": 15,
      "cache_read_tokens": 0,
      "cache_write_tokens": 0,
      "output_tokens": 77,
      "reasoning_tokens": 44,
      "cost": 0.000123,
      "duration_ms_sum": 183,
      "ttfb_ms_sum": 153,
      "ttfb_count": 6
    }
  }
}
```

```json
{
  "type": "request.started",
  "data": {
    "id": "01a10028-ae3c-734e-89d0-d48d1bc209a2",
    "started_at": 1791004028476,
    "client": {
      "key_id": "key_df58088f8694",
      "key_name": "laptop",
      "ip": null,
      "user_agent": null
    },
    "client_protocol": "openai-chat",
    "endpoint": "POST /v1/chat/completions",
    "transport": "http",
    "stream": false,
    "requested_model": "mock-echo"
  }
}
```

```json
{
  "type": "request.finished",
  "data": {
    "id": "01a10028-ae3c-734e-89d0-d48d1bc209a2",
    "started_at": 1791004028476,
    "finished_at": 1791004028506,
    "duration_ms": 30,
    "ttfb_ms": 30,
    "client": {
      "key_id": "key_df58088f8694",
      "key_name": "laptop",
      "ip": null,
      "user_agent": null
    },
    "client_protocol": "openai-chat",
    "endpoint": "POST /v1/chat/completions",
    "transport": "http",
    "stream": false,
    "requested_model": "mock-echo",
    "client_model": "mock-echo",
    "upstream_model": "mock-echo",
    "provider": "mock",
    "credential_id": "mock:8da14ba21598",
    "credential_label": "mock",
    "upstream_protocol": "openai-chat",
    "mode": "mock",
    "status": 200,
    "ok": true,
    "error": null,
    "usage": {
      "input_tokens": 3,
      "cache_read_tokens": 0,
      "cache_write_tokens": 0,
      "output_tokens": 3,
      "reasoning_tokens": 0
    },
    "cost": 6e-6,
    "reasoning": null,
    "attempts": [
      {
        "provider": "mock",
        "credential_id": "mock:8da14ba21598",
        "credential_label": "mock",
        "upstream_model": "mock-echo",
        "upstream_protocol": "openai-chat",
        "status": 200,
        "ok": true,
        "error": null,
        "duration_ms": 30
      }
    ],
    "has_bodies": true
  }
}
```

```json
{
  "type": "log",
  "data": {
    "seq": 4,
    "at": 1791004028507,
    "level": "info",
    "target": "switchyard_gateway::pipeline",
    "message": "request finished",
    "fields": {
      "model": "mock-echo"
    }
  }
}
```

```json
{
  "type": "credential",
  "data": {
    "provider": "mock",
    "credential": {
      "id": "mock:8da14ba21598",
      "label": "mock",
      "masked_key": "",
      "disabled": false,
      "usable": true,
      "status": "ready",
      "model_cooldowns": [
        {
          "model": "mock-error-429",
          "until": 1791004030507,
          "reason": "rate_limit"
        }
      ],
      "requests": 10,
      "successes": 8,
      "failures": 2,
      "consecutive_failures": 1,
      "latency_ms": 24,
      "last_used_at": 1791004028507,
      "last_error": {
        "status": 429,
        "class": "rate_limit",
        "message": "Mock rate limit reached. Please try again in 2s.",
        "at": 1791004028507,
        "model": "mock-error-429"
      },
      "weight": 1,
      "priority": 0
    }
  }
}
```

```json
{
  "type": "config.reloaded",
  "data": {
    "at": 1791004028516,
    "ok": true,
    "message": "configuration applied"
  }
}
```

```json
{
  "type": "subscribed",
  "data": {
    "topics": [
      "request.started",
      "request.finished",
      "log",
      "credential",
      "config.reloaded"
    ]
  }
}
```

```json
{
  "type": "lagged",
  "data": {
    "missed": 476
  }
}
```

```json
{
  "type": "pong"
}
```

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
| `GET` | `/config` | – | `{"config", "path", "restart_required", "command_line_overrides", "config_rejected"}` |
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
| `GET` | `/requests` | – | `{"items", "next_before", "has_more", "total", "capacity"}` |
| `GET` | `/requests/{id}` | – | `{"record", "bodies"}` |
| `GET` | `/logs` | – | `{"lines", "next_before", "has_more", "last_seq", "started_at"}` |
| `POST` | `/playground` | `{"protocol", "body", "model"?, "stream"?}` | the protocol's JSON or SSE |
