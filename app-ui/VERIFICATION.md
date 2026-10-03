# Workspace UI verification

Checked on 2026-10-03 against the local Switchya host. The browser checks used the actual app and a controlled local provider fixture. This records UI verification; it does not claim a production release or successful authentication with a live provider account.

## Browser readback

- Opened a real local project and submitted a task through the built-in agent. The recorded sequence read a file, proposed changing its value from 41 to 42, requested approval for that exact edit, and then requested approval for a PowerShell assertion. The command recorded exit code 0 and `fixture check passed`; the resulting file was independently read back as 42.
- Verified the readable conversation, proposed diff, approval controls, recorded command output and exit status. Empty tool-only assistant responses are omitted. Raw request identities, arguments and results remain available in Details.
- Reload retained the saved native session. A follow-up submitted through the CLI appeared in the same browser session.
- Checked 390px mobile and 768px tablet layouts without document overflow. Navigation and review dialogs opened and closed, restored focus, and remained usable. Dark appearance was checked and returned to System.
- The refined desktop screenshot at 1280×720 showed a composer approximately 126px high, two-line session titles, and readable command output.
- In Accounts, created two empty Codex profiles and two empty Claude profiles. Selected a default for each agent, reloaded, and verified all four profiles and both defaults persisted. This flow did not invoke sign-in or CLI account-status probes.
- Immediately after reload, the new-session model picker was temporarily disabled while the catalog loaded. A fresh browser readback then showed the fixture model selected and enabled. No picker defect was reproduced.

## Source checks

The frontend has 56 validated focused checks covering authenticated transport, uncertain request handling, exact retry identities, account-profile binding, event isolation, escaped diffs, transcript construction, truthful usage values, managed-account readiness, review results, and rendering with the shipped Preact/HTM runtime. The renderer regression also removes the static loading fallback and renders real account-card components. Module parsing and whitespace checks passed.

The latest complete suite ran 55 checks before the final account-readiness test was added. The subsequent affected accounts, adapters and rendering suite passed all 15 checks; unchanged checks retain their earlier valid results. Copy-only changes after that passed the rendering regression and parsing checks.

## Remaining boundaries

- Live provider sign-in, live CLI usage-limit/status reads, and launching an authenticated external Claude terminal remain unverified by this UI browser pass.
- Profile creation and list reads are metadata/cache operations. Only the explicit **Refresh account** action requests a CLI status probe. An unknown managed profile must be refreshed before sign-in or a run becomes available.
- Codex runs display the account returned for that run. Changing the default affects future work; uncertain-start retries retain their original profile and request ID.
- Existing-agent runs and external Claude terminals have different persistence and permission boundaries from built-in saved sessions. Those boundaries are stated in the interface.
