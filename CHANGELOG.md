# Changelog

## 0.1.0

First release.

### Gateway

- Client APIs: OpenAI Chat Completions, OpenAI Responses, Anthropic Messages
  and Gemini `generateContent`, streaming and non-streaming, with token
  counting and model listings in each vendor's shape. Legacy `/v1/completions`
  is served through chat.
- Any client protocol can be served by a provider that speaks another one.
  Requests in the provider's own protocol preserve provider fields, with JSON
  normalization, model routing and configured reasoning or payload changes.
- Providers: OpenAI, Anthropic, Gemini, Vertex AI (service account or API
  key), any OpenAI-compatible server, and a built-in mock provider for trying
  things out.
- Several credentials per provider with round-robin, fill-first, weighted or
  least-latency selection, priorities, session affinity, cooldowns with
  backoff and automatic failover. Streaming requests are retried on another
  credential until the first event has been sent.
- Model aliases with fallback chains, provider prefixes, exclusions, model
  discovery and a built-in catalog of model capabilities.
- Reasoning suffixes — `model(high)`, `model(8192)`, `model(none)`,
  `model(auto)` — converted to each provider's own settings and fitted to what
  the model accepts.
- Payload rules that set default values, override values or remove fields in
  upstream request bodies.
- Client API keys with model allow-lists and per-minute rate limits.
- Outbound proxies (HTTP, HTTPS, SOCKS5) globally, per provider or per
  credential. Optional TLS for the listener.

### WebSockets

- The OpenAI Responses API over WebSocket (`GET /v1/responses`) for any model,
  with per-connection conversation state so follow-up turns can send only
  their new input.
- A relay for the OpenAI Realtime API (`GET /v1/realtime`).
- A live event feed for the dashboard.
- Single-use browser connection tickets, with current key permissions checked
  during long-lived client sessions.

### Dashboard

- Embedded in the binary at `/admin/`: live overview, request log with attempt
  timelines and captured bodies, provider and credential management, model
  routes and aliases, client keys, usage and cost charts, a playground for all
  four protocols and the WebSocket endpoint, live logs, settings forms and a
  raw configuration editor. Light and dark themes; works on phones.

### Operations

- One TOML configuration file, reloaded when it changes. Edits made from the
  dashboard keep the file's comments and formatting.
- Usage history kept on disk, with estimated cost from a configurable price
  table. Optional capture of request and response bodies.
- `switchyard init`, `check` and `import-cliproxy` (converts the API-key
  sections of a CLIProxyAPI configuration).

### Release hardening

- Enforced current key policy and resolved-model permissions; anonymous browser
  requests check their Origin and Host independently of CORS configuration.
- Bounded schema conversion, tool bookkeeping, discovery responses and JSON
  display work; expanded secret redaction and private Unix file permissions.
- Completed dashboard forms, focus handling, exact usage/request drill-down,
  WebSocket tickets, configuration error mapping and credential preservation.
- Scoped release job permissions and resolved release tags to an immutable
  commit. Documented shared-account, trusted-browser and Windows ACL limits in
  [the security model](docs/SECURITY-MODEL.md).

### Artifact availability

- Windows x64 archive, built and verified locally. Linux/macOS archives and
  container publication remain pending while GitHub Actions is unavailable.

### Not included

- Signing in to consumer subscriptions or imitating vendor command-line
  clients. Providers are reached with API keys or service accounts.
