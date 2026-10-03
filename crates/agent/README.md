# Switchya agent engine

The local coding engine for Switchya, powered by the independently usable Switchyard gateway. The app host, terminal client and desktop shell use this same Rust library. This is the first app milestone, not a claim of parity with every agent product.

`AppEngine` binds sessions to explicitly opened projects and an authenticated gateway client identity. It sends real requests through the gateway's existing routing, codecs, cancellation and accounting. It persists session events and Responses input/output items in a private SQLite database. Foreign provider reasoning signatures survive replay but are not included in public UI events.

Read/search tools are bounded and project-scoped. File edits and local shell commands wait for an exact, single-use user decision. File previews show original hashes and diffs; stale content is refused. Commands execute with normal user permissions. The project working directory is not an operating-system sandbox. Recognizable inherited credential environment variables are removed from command subprocesses; arbitrary local secrets are not made inaccessible by this filter.

Only one run may be active in a session. A stable caller command ID deduplicates retries. Interrupt cancels the gateway request or current tool and expires approvals. Restart never repeats an incomplete operation: a side effect whose result was not recorded produces `recovery_required`. Review its actual effects and acknowledge recovery with a note before submitting a new turn. The old operation's outcome remains unknown. A model completing a run does not mean that its proposed result was verified; the event ledger separately records tool results, file changes and command exits.

The first storage schema is versioned. SQLite uses WAL and an exclusive owner lock, so another host cannot recover a live host's runs. Keep the state directory on local storage. Provider keys remain managed by the gateway's existing configuration/environment; do not place real credentials in prompts or project files that the model is asked to read.

Verification command: `cargo test -p switchyard-agent -p switchyard-agent-tools`. The integration tests use the real gateway with local scripted provider fixtures and never require paid model calls.
