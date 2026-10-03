# Native workspace shell

This independent Tauri workspace embeds the Rust app host in the desktop process. It loads only the host's authenticated loopback page, grants the page no Tauri IPC capabilities, and shuts down the host and active agent work when the desktop closes. The standalone gateway does not depend on this crate or on GUI libraries.

The product is Switchya, powered by the independently installable Switchyard Gateway.

Build with Rust 1.90 or newer, plus the [Tauri platform prerequisites](https://v2.tauri.app/start/prerequisites/):

```sh
cargo build --manifest-path desktop/Cargo.toml --release --locked
```

Run the resulting `switchya-desktop` binary. Optional `--config PATH`, `--data-dir PATH`, and `--client-key-name NAME` select a separate app/gateway configuration. The CLI host accepts the same config and data directory for sequential use; while the desktop is running, the CLI must connect through an explicitly written launch-info file instead of opening a second database writer.

`--write-launch-info PATH` writes that private connection file without replacing an existing file. Its bearer token grants access to the local workspace for the lifetime of the running host; do not share it.

Native operating system webviews are required. Cross-compilation or a successful Rust source check does not establish that a desktop build launches on another operating system. Platform verification is recorded separately from gateway verification.
