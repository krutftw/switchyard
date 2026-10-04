# Existing agent adapters

Switchya's own durable engine and existing agent CLIs are separate execution backends. `switchyard-agent-adapters` implements a real Codex app-server integration over newline-delimited structured JSON on stdio. It creates a Codex thread, or resumes a saved one when the user explicitly continues a conversation, starts one user turn, streams its structured progress, hands supported approval requests to the human, and interrupts or stops its owned process tree. The caller must explicitly start a run. Discovery, reading a run, event polling and reconnecting never start or replay work.

Claude Code has managed account sign-in/status and an explicit **Open Claude Code** action for a separate terminal. Its tasks are not streamed or controlled by this adapter, and `start` still rejects Claude runs. Oh My Pi is discovery-only.

## Protocol evidence

The initial implementation was checked against the installed **Codex CLI 0.159.3** on 3 October 2026. Its own `app-server --help` describes the stdio transport, and `app-server generate-json-schema --out <temporary-directory>` emitted the authoritative protocol used here. Specifically reviewed:

- `v1/InitializeParams.json` and the initialize/initialized handshake.
- `v2/ThreadStartParams.json`, `ThreadStartResponse.json`, `ThreadResumeParams.json`, `ThreadResumeResponse.json`, `TurnStartParams.json`, `TurnInterruptParams.json`, item and turn notifications.
- `CommandExecutionRequestApprovalParams.json` and its response, `FileChangeRequestApprovalParams.json` and its response, `ServerRequest.json`, `ServerRequestResolvedNotification.json`.

The local CLI labels app-server experimental. Other releases may be compatible but have not been established by this inspection; incompatible response shapes stop the run. The integration uses documented protocol fields, not internal app databases or tokens. CLI help/version inspection also found Claude Code 2.1.286 and an OMP installation.

A fresh, empty managed Codex home was used for a real initialize/thread-start check without a model turn, login or account read. Version 0.159.3 rejected `approval_policy="untrusted"` at startup despite retaining that schema variant. The supported `on-request` policy passed initialize and thread-start, returning `readOnly`, disabled shell network, and `user` approval review. No global configuration was changed.

## Host API

`AdapterManager` is cloneable and shares one registry per host. Add `switchyard-agent-adapters = { path = "../agent-adapters" }` to the host crate. Public types are defined in `crates/agent-adapters/src/types.rs`.

| Method | Behavior |
|---|---|
| `AdapterManager::open(data_dir)` | Opens the durable registry in `data_dir/adapter-runs.sqlite3`; the app host uses this. Fails if another host has it open. |
| `AdapterManager::new()` | Creates an in-memory registry whose runs and Codex threads end with the host. |
| `discover().await` | Checks fixed native executable locations and absolute PATH entries; executes only bounded `--version` probes. |
| `start(StartRequest)` | Validates the request and starts one asynchronous Codex run. |
| `list_runs()` / `run(id)` | Returns retained local run state. |
| `retry_existing(request)` / `retained_profile(command_id)` | Trusted host-only lookup of a retained complete request and immutable binding, without new-start filesystem, CLI or auth checks. The binding must never become an HTTP response. |
| `events(id, after_seq, limit)` | Returns at most 200 structured events, ordered by monotonic sequence. |
| `decide(id, approval_id, expected_hash, decision).await` | Sends the exact pending decision once. The only decisions are `allow_once` and `deny`. |
| `interrupt(id).await` | Requests interruption; later state/event read-back establishes whether Codex confirmed it. |
| `acknowledge_recovery(id)` | Records that a human reviewed a `recovery_required` run and moves it to `interrupted`. It never replays or certifies the uncertain work. |
| `run_profile(id)` | Trusted host-only lookup of a run's binding, used to keep a continuation on the same account. The binding must never become an HTTP response. |
| `shutdown().await` | Cancels active supervisors, stops owned process trees and waits for cleanup. |

`StartRequest` contains `adapter_id`, `project_path`, `prompt`, `command_id`, an optional `continue_run_id`, and an internal optional `ProfileBinding`. The binding cannot be deserialized from HTTP JSON. The HTTP host must resolve both a project ID and profile ID from its trusted registries. It must apply the same loopback, bearer authentication, Origin, JSON body and body-size restrictions as the own-engine routes. A client command UUID is required. Reusing it with the same complete request, including profile identity/home, returns the same run; changing that request causes conflict. The registry retains all command IDs, including those of runs removed by retention, so an evicted ID cannot accidentally replay work. Each run exposes its pinned `profile_id` and `profile_name`; changing a default does not alter existing runs.

Executable, shell arguments, config overrides, model IDs and provider tokens are not HTTP input. Discovery resolves a fixed CLI name to an absolute path, skips relative/empty PATH entries and excludes Windows `.cmd`/`.bat` shims. `Command` launches the native executable directly. Prompt text travels as JSON over stdin; it is never interpolated into a shell command. The selected Codex model comes from the CLI configuration and is reported only after thread creation. Discovery does not test authentication, available models, subscription eligibility or provider billing. Starting a real turn uses that installation's authentication and may consume provider usage.

## Permissions and approvals

The process launch and thread creation both request `sandbox=read-only`, `approvalPolicy=on-request`, and `approvalsReviewer=user`. Before `turn/start`, Switchya checks the returned effective policy is `readOnly`, shell network access is false/default false, the approval policy is `on-request`, and the reviewer is `user`. It fails closed if that read-back differs. The turn also supplies an explicit read-only sandbox with network disabled and the same approval settings. Read-only operations can run without a prompt; writes/elevation need a supported CLI approval. There is no auto-approve flag, automatic risk reviewer, session-wide acceptance, persistent rule amendment or permission escalation API.

These are **Codex-defined shell permission guarantees**. Switchya does not add a filesystem/network security sandbox around the external CLI, and a project working directory is not an OS isolation boundary. The existing CLI configuration, instruction sources and integrations can still apply. Their permissions are separate from the Codex shell sandbox. The UI must display `permission_boundary` when starting runs and deciding approvals. A human-approved operation may run with the user's OS privileges; the adapter must never label that as an OS-sandboxed operation.

Command approvals include the exact protocol parameters and any associated item. Missing command text or working directory makes the request deny-only. File approvals include the preceding `fileChange` item and its exact changes; missing changes or a `grantRoot` request is deny-only because the latter can affect the remainder of the session. An approval's SHA-256 binds its method, wire request ID and complete displayed preview. Stale hashes, duplicate decisions and decisions after completion fail. `allow_once` maps exclusively to Codex `accept`; `deny` maps to `decline`. Requests resolved by Codex are removed from the pending list. Broad permissions, dynamic tools, authentication refresh and other unsupported server requests receive a protocol error, never an inferred approval.

Codex performs its own patch validation and execution. Switchya does not claim its native engine's persisted operation or stale-file checks apply to the external CLI. Completed means the CLI reported completion; it is not a verification result.

## Events, cancellation and bounds

Public events contain `schema_version`, `run_id`, `seq`, `at_ms`, `kind`, and `payload`. Supported event kinds are `state_changed`, `user_task`, `assistant_delta`, `item_started`, `item_completed`, `approval_requested`, `approval_resolved`, `notice`, `adapter_error`, and `turn_completed`. A `user_task` is `{text}` and records the explicitly submitted prompt. An `assistant_delta` is `{item_id,text}`; item events are `{item}`; approval-request events are `{approval}`. UI output, exact approval previews and diffs must be rendered as untrusted text. Do not insert generated HTML into the page. A final agent-message item can repeat text already delivered in deltas; associate them by item ID.

Only selected item fields and message deltas are forwarded. Account/config/environment notifications are ignored. Raw stderr is drained and discarded. Raw RPC error strings are not exposed because CLI diagnostics can contain credentials or local environment values. Terminal/tool output is still user content and can contain information the CLI reads; this filtering is not a general secret-removal claim.

Limits are four active runs, 200 retained runs, a 64 KiB prompt, 256 KiB per JSONL message, 32 MiB each for total stdout and stderr, 16 simultaneous approvals, 512 retained events and 2 MiB retained event bytes per run. Event windows expose `first_retained_seq` and `last_seq` so the UI can identify a truncated history. Handshake requests have 30-second deadlines, a turn has a 30-minute deadline, writes have five-second deadlines, and interruption gets five seconds for a completion notification. When a new run would exceed the retained-run limit, the least recently started inactive conversations are removed whole; the conversation being continued is kept. Their command IDs are retired, not forgotten: retrying one is a conflict and never starts work. If no conversation can be removed, the new run is rejected.

Interruption sends `turn/interrupt` for the exact thread and turn; pending supported approval requests are cancelled. Its response only acknowledges the request. A matching `turn/completed` notification establishes `interrupted`, `completed`, or `failed`. Timeout, malformed transport, unconfirmed process exit or host shutdown after a turn request marks `recovery_required`. An operation may already have acted, so the UI must direct the user to inspect it before retrying. There is no process restart, turn retry, reconnect replay or automatic session resumption. A `recovery_required` run cannot be continued until the user reviews the project and acknowledges recovery.

Windows processes start suspended, join a kill-on-close Job Object, then resume. Unix processes start in their own process group. Cancellation and shutdown terminate the owned job/group and reap the direct child; cleanup failure is reported. This manages process lifecycle, not adversarial OS confinement. A separately daemonized or externally managed service can outlive the owned process group.

## Saved conversations

The app host saves runs, their bounded event windows and each run's trusted profile binding in `adapter-runs.sqlite3` inside its data directory. The file is created private to the user on Unix, and SQLite's exclusive locking keeps a second host from opening it. If saving fails, the run is not started. Streamed assistant fragments are replaced by the completed message once Codex reports it. Codex keeps the model-side thread in the selected profile's own Codex home; Switchya stores only the thread ID, never Codex's internal databases or tokens. A durable host sends `ephemeral=false`; an in-memory manager sends `ephemeral=true`. The external CLI may also maintain its own diagnostics, cache and integration state.

When the host starts, a run left active by a previous host is never resumed. If Codex had created its thread, the run becomes `recovery_required`; otherwise it becomes `failed`. Both receive an `adapter_error` event stating that nothing was replayed.

A conversation is the chain of runs that share `conversation_id`. Continuing one is an explicit start request with `continue_run_id`. Switchya then sends `thread/resume` for the saved thread with the same read-only sandbox, `on-request` approvals and `user` reviewer, plus `excludeTurns=true` so the response omits earlier turns, which could exceed the 256 KiB message limit. It applies the same policy read-back as a new thread and fails before any turn if Codex returns a different thread. Only the new message is sent as a turn.

A continuation is rejected unless the target run exists, used the same adapter, has finished, is not `recovery_required`, was saved durably, has a Codex thread, used the same project folder, and keeps the same account profile, home and agent. It is also rejected while another run in the conversation is active, or when a newer run that reached a Codex thread has already continued from the same run. The host returns `profile_changed` if the original account profile is no longer available unchanged.

Actual resumption against a signed-in Codex installation has not been verified live yet. The behavior above is established by the fixture tests below and by the generated 0.159.3 protocol schema.

## Verification

### Account profiles

`AccountRuntime` provides explicit `inspect`, `start_login`, `login`, `cancel_login`, and `shutdown` operations. Status snapshots expose only typed sign-in, email, plan and usage fields, plus observation time. Raw protocol objects, tokens and stderr never reach the API. A managed Codex child gets its own `CODEX_HOME` and `cli_auth_credentials_store="file"`; a managed Claude child gets its own `CLAUDE_CONFIG_DIR`. Credential/provider/internal routing environment overrides are cleared only in that child. The existing system Codex profile preserves its existing auth/configuration and cannot log in through this runtime. No credential copying, global environment mutation, logout or removal is implemented.

Codex initialize must confirm the canonical selected `codexHome` before account methods or model runs. Inspection uses `account/read` with `refreshToken=false`, then `account/rateLimits/read` with reset-credit details excluded. Named limit snapshots take precedence over the legacy snapshot. Missing limits and `ordinaryUsageAllowed=null` remain unavailable; usage percentages are used percentages, and remaining percentage is clamped `100 - used`. Account metadata is not proof that a token remains valid.

Claude inspection uses the official `auth status --json` command. Recognized `loggedIn`, `email`, and `subscriptionType` fields are filtered; unknown status shapes remain unknown. Claude's documented status-line quota fields do not establish an independent quota-query command, so this adapter reports subscription usage as unavailable. A proposed fresh-empty-home live Claude status probe was blocked by automatic approval review; no retry was made. Claude parsing and lifecycle checks use fixtures and are not a claim of verified live sign-in.

Claude's current official [credential-management documentation](https://code.claude.com/docs/en/authentication#credential-management) explicitly states that `CLAUDE_CONFIG_DIR` also scopes the macOS Keychain entry, in addition to relocating credential files and the macOS file fallback. This supports the profile design on macOS; actual macOS sign-in and simultaneous account operation have not been tested here.

Sign-in requires a managed, confirmed signed-out profile; the worker rechecks the effective account before initiating it. This prevents a stale UI status from replacing existing credentials. Codex uses public `account/login/start` with `type=chatgpt` and matching completion/cancel messages. Claude uses `auth login --claudeai`; Console authentication is excluded because it does not provide the same profile-isolation guarantee. Browser-login jobs are serialized because Codex's documented callback uses localhost port 1455. Multiple completed profiles may remain signed in and be used concurrently. Jobs expose only recorded HTTPS URLs on exact official allowlisted hosts; the host opens a stored job URL only on an explicit action.

At most four inspections/sign-ins run simultaneously. Inspection has a 7.5-second deadline plus bounded cleanup, sign-in a ten-minute deadline, account output a 256 KiB frame / 2 MiB per stream limit, and at most 64 retained login jobs. Owned account processes use the same Windows Job Object / Unix process-group cleanup as model adapters.

`launch_profile_terminal` starts an interactive Claude Code terminal on an explicit user action, with the immutable selected profile and project. Such external sessions are user-owned and are not tracked or stopped by Switchya. A successful return means the terminal launch was requested, not that Claude authenticated or became ready. The account manager must prevent credential replacement while such sessions may exist; this version does so by refusing to log in an already signed-in profile.

Official references: [Codex authentication and credential storage](https://developers.openai.com/codex/auth), [Codex profiles](https://developers.openai.com/codex/config-advanced#profiles), [Claude authentication](https://code.claude.com/docs/en/authentication), and [Claude status-line usage fields](https://code.claude.com/docs/en/statusline).

The test suite compiles a tiny standard-library-only Rust fixture child and runs the actual transport/process supervisor against it. The fixture checks the request policy fields and exercises handshake, delta events, exact single-use decisions, stale hashes, patch preview requirements, unsupported permissions, CLI-resolved approvals, cancellation, wrong policy read-back, durable restart and resume, recovery review before continuation, continuation guards, a resumed-thread mismatch, a locked second host, retention and retired command IDs, malformed and oversized JSONL, unexpected process exit, duplicate requests, request idempotency, event retention, and descendant cleanup. It does not contact any model, sign in, read credentials, or consume provider usage.

Run `cargo test -p switchyard-agent-adapters -j 1`. The fixture requires `rustc`, which is already part of the Rust development toolchain. CLI `--version` discovery can be checked separately through `GET /api/adapters`. Actual authenticated model execution and each operating system's installed Codex sandbox behavior require explicit live verification; fixture results alone do not establish either.
