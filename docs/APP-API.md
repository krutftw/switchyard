# Local workspace API, first milestone

This document defines the first app/CLI milestone, not the full product roadmap. The gateway remains independently installable. The app uses its own agent engine; existing-agent adapters are a separate integration boundary.

The app host binds only to `127.0.0.1` on an allocated port. Every `/api/` request requires `Authorization: Bearer <random host-session token>`. The launch URL supplies this token in a fragment, which the UI removes before navigation and retains in session storage for reloads. Tokens expire when the host exits. Host and browser Origin checks restrict requests to the exact loopback origin. No remote origin receives CORS access. Tokens and provider keys never appear in logs, event data, or URL query parameters.

All mutations accept JSON. Errors have shape `{ "error": { "code": "conflict", "message": "..." } }` with an appropriate HTTP status. File content, tool output and model output must be rendered as text, never as trusted HTML. The service exposes only explicitly opened projects, never an arbitrary file-serving route.

## Endpoints

| Method and path | Input | Response |
|---|---|---|
| `GET /api/status` | — | `{status: EngineStatus, version, gateway_config_path}` |
| `GET /api/models` | — | `{models: [...model summaries...]}` |
| `GET /api/projects` | — | `{projects: Project[]}` |
| `POST /api/projects` | `{path}` | `{project: Project}` |
| `GET /api/sessions?project_id=...` | Optional exact project id | `{sessions: Session[]}` |
| `POST /api/sessions` | `{project_id, model}` | `{session: Session}` |
| `GET /api/sessions/:id` | — | `SessionView` |
| `POST /api/sessions/:id/turns` | `{command_id, text}`; command id is a client-generated UUID retained across retries | `{run: Run}` |
| `GET /api/sessions/:id/events?after_seq=0&limit=200` | Strict bounded cursor and count | `{events: Event[]}` |
| `POST /api/sessions/:id/operations/:operation_id/decision` | `{expected_hash, decision: "allow_once" or "deny"}` | `{ok: true}` |
| `POST /api/sessions/:id/interrupt` | `{run_id}` | `{ok: true}` |
| `POST /api/sessions/:id/recovery` | `{expected_revision, note}`; a human review note of 1–2000 UTF-8 bytes | `{session: Session}` |

Serialize Rust data using snake_case. `SessionView` contains `session`, `project`, `pending_operation`, and a bounded `events` window. The engine owns the detailed public types in `crates/agent`; their definitions are authoritative. The frontend should use the event `kind` plus its structured `payload`, and tolerate unfamiliar event kinds as neutral activity entries.

The UI needs project names/paths, session titles/model/state, and each event's schema_version, session_id, run_id, seq, at_ms, kind, payload. Models must expose their real IDs; no invented available-provider list. An empty model list or only mock models must clearly lead to provider setup rather than imply a working coding model.

## Execution and approvals

Only `submit_turn` starts a model run. Reading state or reconnecting cannot replay a model call or side effect. One active run is allowed per session. Read/search tools operate inside the opened project; patches and commands require a human decision for the exact persisted operation and argument hash. Approval grants only that operation once. Approved writes and commands are serialized within the native engine across all of this host's projects, including overlapping project folders. Model requests, reads and approval waits remain concurrent. This does not lock out external editors, external agent processes or other programs.

Commands execute on the user's machine with its current OS permissions. A working directory restriction is not an OS sandbox. The UI must show the exact command, working directory, and this permission boundary before approval. File edits show the proposed diff and detect stale file contents before applying.

Interrupted operations with uncertain outcomes are marked `recovery_required`. They are not silently replayed. A model's completion is labeled completed, not verified: checks and exit statuses remain separate evidence.

After inspecting the affected files or command results, a user can acknowledge recovery for the exact session revision. This records the review note and moves the session to `interrupted`, allowing a new turn with a new command ID. The uncertain operation stays `outcome_unknown`; acknowledgement never certifies success or repeats the operation. Stale and duplicate acknowledgements fail with a conflict.

## Product and platform boundaries

The shared Rust engine backs the local HTTP app and terminal CLI. A Tauri desktop shell belongs outside the gateway workspace so headless gateway builds do not require GUI libraries. Cloudflare is the selected platform for the public website and future authenticated coordination; the local execution API is not exposed through a public tunnel by default.

Product brand: Switchya, powered by the independently installable Switchyard Gateway. The selected domain is switchya.com. Authentication and update endpoints must be backed by actual configured services; domain ownership alone does not provide them.
