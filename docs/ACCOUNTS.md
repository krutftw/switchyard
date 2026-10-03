# CLI account profiles

Switchya can keep multiple Codex and Claude Code sign-ins in separate managed
profiles. Profiles belong to the official CLIs; they do not turn subscription
credentials into gateway API keys. Install the relevant CLI separately.

## Use accounts

1. Open **Accounts**, choose Codex or Claude Code, and create a named profile.
2. Choose **Refresh account**. New profiles show unknown status until this
   explicit check completes. Starting Switchya, listing accounts and creating
   profiles do not inspect existing CLI sign-ins.
3. Once the profile is confirmed signed out, choose **Sign in** and complete
   the official provider flow. **Continue in browser** opens the recorded
   official login URL. The app also shows that URL for manual use.
4. Select **Use as default** for future runs, or choose a profile in the
   existing-agent run form. Running tasks retain their original account.

**Cancel sign-in** stops the pending login. It does not log out an account or
erase credentials. To add another identity, create another profile. This
version has no logout, profile removal or credential replacement controls.

The **Existing Codex CLI** entry uses the current CLI configuration. Account
management treats it as read-only: it can be selected and explicitly inspected,
but Switchya cannot start a replacement login for it. It may be used for a
Codex run without first requesting account inspection.

Managed profiles require a successful signed-in status check before starting
work. A status check is a snapshot, not a guarantee that the next request will
succeed. After restarting the host, refresh managed profiles again.

## Concurrent work and switching

The structured Codex adapter accepts up to four active runs across all
profiles. Each accepted run pins its account identity and private CLI home.
Changing the selected default affects later runs only. Retrying the same
command keeps the original binding, including after a default change; changing
that command's profile, agent or request content returns a conflict.

Sign-in is rejected while the profile has an active tracked run, refresh or
login. It also requires a confirmed signed-out snapshot and an additional
official CLI status check before login starts. Already signed-in credentials
are not replaced by the sign-in action.

For Claude, **Open Claude Code** starts the official CLI in a separate native
terminal for the currently opened project, using the selected managed profile.
This is an external CLI session: its conversation, approvals, termination and
results are not tracked by Switchya. Closing Switchya does not close it.
Switchya cannot detect all activity in separately opened terminals.

## Usage information

Codex status uses `account/read` with `refreshToken: false`; subscription usage
uses `account/rateLimits/read`. When available, `rateLimitsByLimitId` takes
precedence over the legacy `rateLimits` response. Remaining percentage is
`100 - usedPercent`, clamped to 0–100. Missing values remain unavailable;
reset timestamps and window durations are shown only when supplied.

Claude status uses `claude auth status --json`. That status response does not
provide subscription quota, so Claude usage remains unavailable in Switchya.
No Switchya usage status line is installed in external Claude terminals.

Status and usage checks do not request model generation. Cached account views
include the check time and distinguish unknown, unavailable and error states
from a measured zero. Refreshing the account list only rereads this cache;
**Refresh account** is the operation that requests a new CLI check.

## Storage and process isolation

The local app data directory contains:

```text
account-profiles.json       # schema version, profile labels/IDs and defaults
accounts/<profile UUID>/    # one private CLI home per managed profile
```

Profile metadata contains no credentials, account email, quota history or
caller-supplied filesystem paths. Profile directories derive from generated
UUIDs, and symlinks or Windows reparse points within this managed layout are
rejected. The host establishes its exclusive app-state owner before opening
the account manager. Metadata writes use private temporary files and atomic
replacement; unexpected external metadata edits are rejected without being
overwritten.

Directories are protected for the current user (Unix mode `0700`, or a private
Windows ACL); metadata files use mode `0600` or the corresponding private ACL.
Official CLIs own their credential storage. Managed Codex uses a separate
`CODEX_HOME` with file credential storage; managed Claude uses a separate
`CLAUDE_CONFIG_DIR`. These settings apply only to owned child processes.
Switchya does not copy or swap the user's global CLI authentication files.
Managed children also remove inherited provider credential and endpoint
overrides so an unrelated environment setting cannot select another account.

The authenticated local API returns account status, labels and sanitized usage
summaries. It does not return CLI home paths or credentials. Login URLs and
codes are sign-in data exposed through the dedicated login state.

## Local API

All routes require the app host's authenticated local client and its normal
mutation protections. JSON bodies reject unknown fields; paths and query
parameters cannot select an arbitrary credential directory.

| Method | Route | Body or effect |
| --- | --- | --- |
| GET | `/api/account-profiles` | Cached profiles and selected defaults |
| POST | `/api/account-profiles` | `{ "agent_id": "codex", "name": "Work" }`; `claude` also supported |
| POST | `/api/account-profiles/{id}/refresh` | `{}`; queues a status check, returns 202 |
| POST | `/api/account-profiles/{id}/login` | `{}`; queues sign-in, returns 202 |
| GET | `/api/account-profiles/{id}/login` | Cached login state; does not start a process |
| POST | `/api/account-profiles/{id}/login/cancel` | `{}`; cancels the pending login |
| POST | `/api/account-profiles/{id}/login/open` | `{}`; opens its recorded official login URL |
| POST | `/api/account-profiles/{id}/select` | `{}`; saves the default for that agent |
| POST | `/api/account-profiles/{id}/terminal` | `{ "project_id": "opened-project-id" }`; managed Claude only |

At most 32 managed profiles are supported. Names contain 1–64 characters after
trimming, cannot contain control characters, and must be unique per agent
under ASCII case-insensitive comparison. Account reads are limited to two
concurrent checks. Login work runs outside the HTTP request deadline; clients
poll the cached login state. Host shutdown cancels owned account work.

## Platform and validation limits

The account manager and scoped CLI environment support Windows, macOS and
Linux. Browser continuation uses the platform browser launcher. External
Claude terminals require a supported installed terminal and native CLI;
headless systems may need the displayed login URL opened manually elsewhere.

Automated checks use controlled CLI fixtures and local metadata. They cover
isolation, usage parsing, login state, cancellation, request retries and
negative gates without performing a real provider sign-in. A successful build
or fixture test does not establish that real OAuth, the user's subscriptions,
macOS Keychain or the platform's interactive terminal has been tested. See
[the user guide](SWITCHYA.md) and [the adapter contract](AGENT-ADAPTERS.md) for
the current execution and platform boundaries.
