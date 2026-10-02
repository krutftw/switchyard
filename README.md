# Switchyard

A fast LLM API gateway in a single Rust binary. Point any OpenAI, Anthropic or
Gemini client at it and route requests to any provider you have keys for —
Switchyard translates between the protocols, rotates across your credentials,
fails over when one is rate limited, and shows you what is happening in a
built-in dashboard.

```
 your tools                         Switchyard                         providers
 ─────────────                ┌────────────────────┐
 OpenAI SDK / Codex CLI ────► │ /v1/chat/completions│ ──► OpenAI
 Claude Code / Anthropic SDK ►│ /v1/responses  (+WS)│ ──► Anthropic
 Gemini CLI / google-genai ──►│ /v1/messages        │ ──► Gemini / Vertex AI
 anything OpenAI-compatible ─►│ /v1beta/models/…    │ ──► OpenRouter, Groq, Ollama, vLLM, …
                              └────────────────────┘
```

## What it does

- **Four client protocols**: OpenAI Chat Completions, OpenAI Responses,
  Anthropic Messages and Gemini `generateContent` — streaming and
  non-streaming, tools, images, reasoning, structured output, token counting.
- **Any client, any provider**: a request in one protocol can be served by a
  provider that speaks another. Same-protocol requests are forwarded untouched.
- **WebSockets**: the OpenAI Responses API over WebSocket (`GET /v1/responses`)
  for any model, a relay for the OpenAI Realtime API (`/v1/realtime`), and a
  live event feed for the dashboard.
- **Credential pools**: several keys per provider with round-robin,
  fill-first, weighted or least-latency selection, priorities, per-model
  cooldowns with backoff, `Retry-After` awareness and automatic failover.
- **Model routing**: aliases, provider prefixes (`team-a/gpt-5`), fallback
  chains (`smart → claude-opus, then gpt-5`), model discovery, exclusions.
- **Reasoning control**: `gpt-5(high)`, `claude-sonnet-4-5(16000)`,
  `gemini-2.5-flash(none)` — one suffix syntax, converted to whatever each
  provider expects and clamped to what the model supports.
- **Dashboard** at `/admin/`: live request stream, provider and credential
  health, usage and cost charts, a playground for every protocol, logs, and a
  config editor. Embedded in the binary; no external assets.
- **Operations**: hot-reloaded TOML config (the dashboard edits it in place
  and keeps your comments), client API keys with model allow-lists and rate
  limits, payload rewrite rules, usage history, request/response capture,
  proxies (HTTP, SOCKS5) per provider or credential, optional TLS.

Upstream access is by **API key or service account** only. Switchyard does not
log in to consumer subscriptions or impersonate vendor CLIs.

## Quick start

Download a binary from the [releases page](https://github.com/krutftw/switchyard/releases)
(or build it: `cargo install --git https://github.com/krutftw/switchyard switchyard`), then:

```bash
switchyard
```

On first start it writes `switchyard.toml`, prints an admin secret and a client
API key, and enables a built-in mock provider so you can try everything without
any upstream key. Open <http://127.0.0.1:8317/admin/> and sign in with the
admin secret.

Add a real provider in the dashboard, or in `switchyard.toml`:

```toml
[[providers]]
name = "anthropic"
kind = "anthropic"
api_keys = ["env:ANTHROPIC_API_KEY"]

[[providers]]
name = "openai"
kind = "openai"
api_keys = ["env:OPENAI_API_KEY"]
```

Then call it with whichever SDK you like:

```bash
# OpenAI Chat Completions → served by Anthropic
curl http://127.0.0.1:8317/v1/chat/completions \
  -H "Authorization: Bearer $SWITCHYARD_KEY" -H "Content-Type: application/json" \
  -d '{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"Hello"}]}'

# Anthropic Messages → served by OpenAI
curl http://127.0.0.1:8317/v1/messages \
  -H "x-api-key: $SWITCHYARD_KEY" -H "anthropic-version: 2023-06-01" -H "Content-Type: application/json" \
  -d '{"model":"gpt-5","max_tokens":1024,"messages":[{"role":"user","content":"Hello"}]}'

# Gemini → served by anything
curl "http://127.0.0.1:8317/v1beta/models/gpt-5:generateContent" \
  -H "x-goog-api-key: $SWITCHYARD_KEY" -H "Content-Type: application/json" \
  -d '{"contents":[{"parts":[{"text":"Hello"}]}]}'
```

### Using it from coding tools

| Tool | Setting |
|---|---|
| Claude Code | `ANTHROPIC_BASE_URL=http://127.0.0.1:8317` and `ANTHROPIC_API_KEY=<client key>` |
| Codex CLI | a `model_providers` entry with `base_url = "http://127.0.0.1:8317/v1"`, `wire_api = "responses"` |
| Gemini CLI | `GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:8317` and `GEMINI_API_KEY=<client key>` |
| OpenAI SDKs | `base_url="http://127.0.0.1:8317/v1"` |

## Endpoints

| | |
|---|---|
| `POST /v1/chat/completions` | OpenAI Chat Completions |
| `POST /v1/completions` | legacy completions (served through chat) |
| `POST /v1/responses` | OpenAI Responses |
| `GET  /v1/responses` | Responses over WebSocket |
| `POST /v1/responses/input_tokens` | token count |
| `POST /v1/messages`, `/v1/messages/count_tokens` | Anthropic Messages |
| `POST /v1beta/models/{model}:generateContent`, `:streamGenerateContent`, `:countTokens` | Gemini |
| `GET  /v1/models`, `/v1beta/models` | model lists in each vendor's shape |
| `GET  /v1/realtime` | WebSocket relay to an OpenAI Realtime upstream |
| `POST /v1/embeddings`, `/v1/images/generations`, `/v1/moderations`, `/v1/audio/speech` | forwarded to an OpenAI-compatible provider |
| `GET  /healthz` | liveness |
| `/admin/`, `/admin/api/…` | dashboard and admin API |

Clients authenticate with a key from `[[auth.keys]]`, sent as
`Authorization: Bearer`, `x-api-key`, `x-goog-api-key` or `?key=`.

## Configuration

Everything lives in one TOML file; [`switchyard.example.toml`](switchyard.example.toml)
documents every setting with its default. Highlights:

```toml
[routing]
strategy = "round-robin"     # round-robin | fill-first | weighted | least-latency
max_attempts = 3

[[providers]]
name = "openrouter"
kind = "openai-compat"       # openai | anthropic | gemini | vertex | openai-compat | mock
base_url = "https://openrouter.ai/api/v1"
api_keys = ["env:OPENROUTER_API_KEY"]
prefix = "or"                # models are served as "or/<model>"

[[aliases]]
name = "smart"               # a virtual model with a fallback chain
targets = ["claude-opus-4-5", "gpt-5(high)"]

[[payload.override]]         # patch upstream request bodies
models = ["gpt-*"]
set = { "reasoning.summary" = "auto" }
```

Secrets may be literal or `env:NAME`. The file is reloaded when it changes; an
invalid edit is rejected and the running config kept.

## Command line

```
switchyard [serve] [--config <path>] [--host <h>] [--port <p>]
switchyard init [--config <path>] [--force]         write a starter config with fresh secrets
switchyard check [--config <path>]                  validate a config file
switchyard import-cliproxy <config.yaml> [-o <path>]  convert the API-key sections of a CLIProxyAPI config
switchyard version
```

Environment: `SWITCHYARD_CONFIG`, `SWITCHYARD_ADMIN_SECRET`,
`SWITCHYARD_ADMIN_ALLOW_REMOTE`.

## Docker

```bash
docker run -d --name switchyard -p 8317:8317 \
  -v switchyard-data:/data \
  -e SWITCHYARD_ADMIN_SECRET=change-me \
  ghcr.io/krutftw/switchyard:latest
```

The config is created at `/data/switchyard.toml` on first start.

## How translation works

Each protocol has a codec that maps it to and from one canonical request and
stream model, so supporting four protocols takes four codecs rather than
twelve pairwise translators. When the client and the provider speak the same
protocol the body is forwarded as-is (only the model name, and reasoning
settings when you use a suffix, are touched). Details that do not survive a
protocol boundary — provider-specific blocks, signed reasoning from another
vendor — are dropped rather than sent somewhere they would be rejected.
See [docs/DESIGN.md](docs/DESIGN.md).

## Building

```bash
cargo build --release -p switchyard     # Rust 1.88+
cargo test --workspace
```

The dashboard in `ui/` has no build step and is embedded at compile time.
`node tools/ui-dev.mjs --api http://127.0.0.1:8317` serves it from disk for
development.

## Acknowledgements

Switchyard is an independent implementation inspired by
[CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI); see [NOTICE](NOTICE).

## Licence

MIT
