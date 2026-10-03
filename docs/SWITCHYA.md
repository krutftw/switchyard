# Switchya user guide

Switchya is a local coding workspace. Its browser interface, terminal CLI and
desktop shell use the same built-in agent and saved project sessions. The
independently installable [Switchyard Gateway](../README.md#switchyard-gateway)
provides model access and routing.

The [**Windows x64 portable preview**](https://github.com/krutftw/switchyard/releases/tag/switchya-v0.1.0-preview.1)
is tagged `switchya-v0.1.0-preview.1`. Its archive contains the native desktop
shell and `switchya` CLI, targeting Windows 10/11 x64. It is unsigned and has
no installer or bundled runtime DLLs. Install the
[Microsoft Visual C++ v14 x64 Redistributable](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist?view=msvc-170)
and Microsoft Edge WebView2 Runtime; WebView2 is required for the desktop
shell. Extract the ZIP and open `switchya-desktop.exe`, or run
`switchya.exe serve` for the browser workspace. Configuration and saved sessions
remain in your application-data directory; they are not stored inside the ZIP.

Gateway downloads remain a separate product. The gateway's Windows and Linux
release does not establish Switchya desktop availability on those platforms.
Separate Linux checks are not a Linux desktop release. There is no macOS app
binary; ARM and Intel source checks are not linked applications or macOS
runtime tests. Source-build instructions remain below.

## Preview verification status

The verification snapshot is from 3 October 2026. The [platform report](PLATFORM-VERIFICATION.md)
separates builds, source checks and runtime observations.

| Area | Evidence and limits |
|---|---|
| Windows automated checks | 88 tests passed across the four new packages: agent engine 15, agent tools 13, adapters 22 and app host 38. The 38 host tests passed again after the final lint refactor. All four packages passed all-targets Clippy with warnings denied. This count is not a whole-gateway-workspace test total. |
| UI checks | The final complete `app-ui/tests/*.test.mjs` run passed all 56 tests. |
| Browser workflow | A controlled local provider exercised file reads, proposed patches, edit approval/denial and command approval, recorded command results, saved-session reload and CLI handoff. These are fixture checks, not live-model quality results. |
| Native Windows shell | Release version/help and startup checks passed, including a responding window and healthy local host. Interaction inside the native GUI has not been verified; the browser workflow above is a separate check. |
| Portable release package | The release includes a SHA-256 file and a manifest of every packaged file. Verify the downloaded archive against those files. |
| Accounts and model quality | Actual account sign-ins and live AI coding quality remain unverified. Account tests use controlled fixtures. Claude subscription quota remains unavailable because its documented status command does not expose it. |
| Other platforms | 91 native Linux tests passed. Both macOS architectures passed app and desktop source checks. Linux and macOS desktop downloads are not included in this preview; see the platform report for the separate build/runtime limits. |

See [Accounts](ACCOUNTS.md) for the current sign-in, usage and native-terminal
boundaries. A source check, fixture run or successful build does not establish
an end-to-end live account workflow.

## Build and open the workspace

Build the app host and CLI from the repository root:

```powershell
cargo build --release --locked -p switchyard-app
.\target\release\switchya.exe serve
```

The default command is also `serve`. It starts a local host on an allocated
`127.0.0.1` port; open the launch URL printed in the interactive terminal. Keep
that terminal running. Ctrl+C stops the host. The URL grants access to this
host session, so keep it private. A plain URL without the launch token is not
a sign-in link. Redirected output deliberately omits the private launch URL.

The Windows desktop shell has its own Cargo workspace and needs Rust 1.90+
and the [Tauri platform prerequisites](../desktop/README.md):

```powershell
cargo build --manifest-path desktop/Cargo.toml --release --locked
.\desktop\target\release\switchya-desktop.exe
```

These paths assume the default Cargo target directories. The desktop embeds
the local host, opens its authenticated page in an operating-system webview,
and stops the host and its managed agent work when the app closes. It does not
need a separately running gateway server. Building the headless gateway does
not require the desktop manifest or GUI dependencies. See
[desktop/README.md](../desktop/README.md) for the shell's current platform boundary.

## Connect a coding model

On first start, Switchya creates a gateway configuration only if one does not
already exist. Its default Windows location is
`%LOCALAPPDATA%\Switchya\gateway.toml`; saved session data is under
`%LOCALAPPDATA%\Switchya\sessions`. It falls back to `%APPDATA%` if local
application data is unavailable. This configuration is separate from a
standalone gateway's default `switchyard.toml`.

1. Choose **Connect a provider** in the initial prompt, or **Providers** in
   the project rail to reopen setup. If only starter mock models are
   available, the workspace prompts for a real provider.
2. Choose **OpenAI**, **Anthropic**, **Gemini**, or **Ollama — local**, and give
   the connection a unique configuration name.
3. For a cloud provider, choose **Save an API key** or **Use an environment
   variable**. Enter the variable's name, not its value, for the latter. The
   app process must inherit it; set a missing variable and restart the app.
   The setup dialog uses the cloud provider's official endpoint.
4. For Ollama, start the local server separately and enter its loopback `/v1`
   endpoint, normally `http://127.0.0.1:11434/v1`. This setup path uses no key.
5. Choose **Save provider**, read its configuration and discovery status,
   then **Discover models** or **Refresh status** as needed. If the main
   workspace still prompts for a coding model, choose **Refresh models** there.

A saved provider or fetched model list does not prove generation access,
billing readiness or successful coding. Saving and model discovery do not
send a generation request. Starting a task with a cloud model can consume
provider usage. API keys are stored through the local gateway configuration
and are not displayed back in the setup dialog; keep that file private.

The model selector lists real IDs available to the app's selected gateway
client key. It respects that key's model permissions. With an existing
configuration containing several keys, use `--client-key-name NAME` if the
app cannot select exactly one enabled key. An admin secret is not an app
model credential. Other gateway provider types and advanced routing remain
configuration features; the app's setup dialog exposes only the four choices
above.

## Work in a project and session

Choose **Open a project** and enter an existing local folder path. Switchya
does not clone a repository or create a new project folder through this
dialog. You can keep editing the same files in your usual editor.

Select a model, describe the task and choose **Send task**. Ctrl+Enter, or
Command+Enter where applicable, sends the task from the composer. The first
task creates a saved session. Choose an existing session in the project rail
to continue it, or **New session** for separate work. A session keeps its
selected model; choose a new session to use another model.

Conversation text and actual tool activity appear together. Read and search
tools are bounded to the opened project. File writes and local commands wait
in **Actions & results** for your decision. Only one run can be active in a
session. Opening or refreshing a session does not start another model call.

Saved built-in sessions persist locally across app restarts. Keep the session
data directory on local storage. Only one host may own it at a time; connect
the CLI to that host for concurrent app/terminal access, as described below.

## Review edits, commands and results

For an edit, review the proposed diff and file paths. **Exact request details**
shows the operation and argument hash; full diffs can be copied or downloaded.
Choose **Allow once** to apply that proposal or **Deny** to refuse it. If you
or another tool changed a file after the preview was prepared, the built-in
engine refuses the stale edit. Ask for a fresh proposal against the current
file instead of assuming the earlier approval still applies.

For a command, review its exact text, working directory and displayed
execution details. Approval applies only to that operation once. The command
runs with your account's operating-system permissions and network access;
its project working directory is not an OS sandbox. Commands may affect files
or services outside the project if their normal permissions allow it.

After an operation, inspect the recorded result, output, file changes and
command exit code. **Completed** means the model finished the run. It does
not mean the project passed tests, the change was reviewed or a release was
published. Read the observed checks separately and request missing checks
when needed.

## Interrupt and recover

**Interrupt** requests cancellation of the active model call or tool. It does
not undo completed file edits or other side effects. An interrupted session
keeps its conversation; submit an explicit follow-up with **Resume with task**.

If an operation's outcome cannot be established after an interruption or
restart, the session shows **Recovery required**. Inspect the affected files,
command output and any external effects. Record what you checked in **What
did you check?**, then acknowledge the review. This permits a new follow-up
task; it does not certify success, undo effects or replay the uncertain action.

When a connection fails during submission, use **Refresh state** first.
**Retry same request** retains the original request ID so the host can return
the existing run instead of creating another one. Do not reconstruct an
uncertain submission as a new task until its state is clear.

## Continue from the CLI

The commands below assume `switchya` is on your PATH. Otherwise use the full
path to the built `switchya.exe`. For terminal-only work, with no app host
using the same session directory:

```powershell
switchya models
switchya chat --project 'C:\projects\my-app' --model 'MODEL_ID'
switchya run --project 'C:\projects\my-app' --model 'MODEL_ID' --prompt 'Explain how this project is tested.'
switchya sessions
switchya chat --session 'SESSION_ID'
```

Use an actual model ID from `models` and an actual session ID from `sessions`
or the `Session:` line printed when the CLI creates one. `chat` is interactive;
enter `/exit` to leave. `run` submits one prompt. Neither command replaces
human approval with automatic permission: a noninteractive run that needs
an edit or command decision stops instead of approving it.

In an interactive terminal, review the displayed operation and type
`allow_once OPERATION_ID` or `deny OPERATION_ID` exactly. Attaching `chat` to
a session with active work shows that run and its pending decisions before
asking for a new task.

### Use the CLI while the app stays open

Start the host with an explicit, new connection file in an existing private
directory. For the browser host:

```powershell
switchya serve --write-launch-info "$env:LOCALAPPDATA\Switchya\cli-connection.json"
```

For the desktop, pass the same `--write-launch-info` option to
`switchya-desktop.exe`. Then, in another terminal:

```powershell
switchya --launch-info "$env:LOCALAPPDATA\Switchya\cli-connection.json" sessions
switchya --launch-info "$env:LOCALAPPDATA\Switchya\cli-connection.json" chat --session 'SESSION_ID'
```

The file contains the host's bearer token. Keep it private, do not commit or
share it, and use a new filename for a new host: export never overwrites an
existing file and the old credentials stop working when their host exits.
`--launch-info` uses that host's configuration; do not combine it with
`--config`, `--data-dir` or `--client-key-name`. Without a launch file, close
the app before opening the same state directory directly in the CLI.

Both surfaces accept explicit `--config PATH`, `--data-dir PATH` and
`--client-key-name NAME` for a separately configured workspace. For recovery
from the CLI, after inspecting the uncertain effects, use
`switchya recover --session SESSION_ID --expected-revision REVISION --note 'What I checked'`.
Use the current `revision` from `sessions`; recovery never reruns an action.

## Existing agents and accounts

**Existing agents** is a separate execution view. The implemented structured
adapter runs an installed Codex CLI with that CLI's model, authentication and
configuration. **Start new run** explicitly starts one task and may consume
that account's usage. Review its exact approval requests in **Agent review**.

These runs are kept only for the current local host's lifetime; they are not
saved built-in Switchya sessions. Codex applies its own patch validation and
permission rules. Switchya requests and checks Codex's read-only shell policy
with network disabled, but does not wrap the external CLI or its integrations
in an additional OS sandbox. Read the permission boundary shown for the run
and for each decision. Unsupported or incomplete approval requests can be
deny-only. See [the adapter contract](AGENT-ADAPTERS.md).

### Manage CLI sign-ins

Codex and Claude CLI account profiles belong to external-agent workflows.
They do not become provider API keys for the built-in agent or Switchyard
Gateway. The gateway continues to use configured API keys or service accounts.
Install the relevant official CLI separately before using its account profile.

1. Open **Accounts** in the project rail. Under **Add an account profile**,
   choose Codex or Claude Code, enter a label and choose **Create profile**.
2. A new profile starts with **Sign-in status unknown**. Choose **Refresh
   account** to request a status check from the official CLI. Starting
   Switchya, opening Accounts, refreshing the list and creating a profile
   only read or create cached metadata; they do not inspect CLI sign-ins.
3. After a check confirms **Signed out**, choose **Sign in** for that managed
   profile and complete the official provider flow. Use **Continue in browser** when offered, then
   return to Switchya and refresh the account. **Cancel sign-in** stops the
   pending login; it is not a logout action.
4. Choose **Use as default** for future work, or choose an account in the
   existing-agent run form. A running Codex task keeps its original profile.

Managed profiles keep separate CLI homes and sign-ins. They do not copy or
replace the existing CLI's authentication. The existing Codex CLI entry keeps
its current sign-in; Switchya can inspect it and select it, but cannot replace
that sign-in through the account controls. To add another identity, create
another managed profile. This version has no managed-profile logout, removal
or credential-replacement controls.

Codex usage windows and reset times come from its documented account API when
available. Missing limits mean **unavailable**, never zero usage or unlimited
capacity. Claude's documented sign-in status does not provide subscription
quota, so Switchya leaves Claude usage unavailable. Checking account status
does not send a model generation request. A signed-in status or available
quota still does not prove a task will succeed.

For a signed-in managed Claude profile and an opened project, **Open Claude
Code** requests a separate native terminal in that project. Its work is outside
Switchya's recorded runs: review, approval and lifecycle belong to the CLI and
terminal. No Switchya usage status line is installed in that terminal. See
[Accounts](ACCOUNTS.md) for account storage, supported operations and platform
limitations.

Claude Code and Oh My Pi are not structured Switchya execution adapters.
Detecting an installed CLI or opening an external terminal does not add its
conversation, approvals or results to a saved Switchya session. An uncertain
external run must be inspected before explicitly starting another run; there
is no automatic retry or resumption.

## Current boundaries

Switchya's local host is available only on loopback. It has no published
remote-access service, cloud session sync, Switchya cloud account service or
automatic update service. The selected `switchya.com` domain does not itself provide
those services. Do not treat the product roadmap as a list of shipped features.

This guide describes current source behavior and the limited checks recorded
in [Preview verification status](#preview-verification-status). It does not
establish live-model coding quality or that a detected CLI is authenticated.
See [APP-API.md](APP-API.md) for the
local host contract and [APP-ROADMAP.md](APP-ROADMAP.md) for proposed work and
verification gates.
