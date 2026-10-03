# Agent tools

`prepare(project_root, name, args)` returns an in-memory `PreparedTool`. The engine
must retain that exact value while requesting approval and call
`execute(&prepared, cancellation_token)` only after approval when
`requires_approval` is true. Do not reconstruct an approved operation from model
arguments. The argument hash includes the canonical project root, operation name,
and normalized arguments. Public metadata is checked against the sealed plan.

| Tool | Behavior | Approval |
| --- | --- | --- |
| `read_file` | UTF-8 text, default 64 KiB and maximum 256 KiB; hashes only complete reads | No |
| `search_files` | Literal filename/content search; maximum 100 matches, 2,000 files, 8,000 directory entries and 8 MiB | No |
| `apply_patch` | Up to 16 exact replacements/creations with current SHA-256 preconditions; 512 KiB total before/after content | Always |
| `run_command` | Project-relative cwd; default 30 seconds, maximum 5 minutes; 128 KiB per output stream | Always |

File tools reject absolute/traversing paths, symlinks, Windows reparse points,
repository metadata and common credential filenames. Search also skips dependency
and build directories. An incomplete search or read is marked `truncated`.
This filename policy is a default guard, not a guarantee that arbitrary source
files never contain secrets.
Conventional `.env.example`, `.env.sample` and `.env.template` files remain readable.

Patch previews contain the complete before/after unified diff. A null
`before_sha256` creates a new file only; existing files require the exact hash
returned by `read_file`. Parent directories must already exist. Execution checks
the entire batch before writing and checks each file again before replacing it.
Individual replacements are atomic, while the batch is not a transaction. A late
failure or cancellation reports every successfully changed file in `changes`.
Concurrent editors should be paused during approved edits: filesystem rename does
not offer a portable compare-and-swap transaction with unrelated writers.

Commands run in Windows PowerShell or `/bin/sh` **with the user's ordinary machine
and network permissions**. The cwd is not a sandbox. Windows commands start
suspended, join a kill-on-close job, and then resume. Unix commands use a separate
process group. Cancellation, timeout, normal shell completion, and dropped command
futures terminate the associated job/group, including ordinary background children.
A deliberately detached Unix process can escape its process group; OS isolation
is required for untrusted commands. Inherited output pipes have a bounded drain
period so escaped processes cannot keep an agent run waiting indefinitely.

The crate does not execute external-provider or network-specific tools. Command
access itself remains broad and must be presented as such in the approval UI.
Recognizable credential environment variables (API keys, tokens, passwords and
auth helpers) are removed before spawning commands. Ordinary build paths remain.
This filter does not make credentials stored elsewhere inaccessible.
