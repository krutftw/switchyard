# Platform verification

Local verification for the Switchya 0.1.0 preview, 3 October 2026. This report distinguishes checks that execute native code from source checks for another platform.

| Platform | Verified result | Remaining boundary |
| --- | --- | --- |
| Windows x64 | App/agent/tools/adapters tests and warnings-denied Clippy passed. CLI and desktop release builds passed. The native desktop opened a responding window and completed its local host readiness check. | Packaged preview verification is in progress. Native GUI interaction and graceful window close have not been verified. |
| Linux x64 | 91 native tests passed with no failures on Alpine Linux 3.24.2 under WSL2. | Native desktop build and command-line smoke checks are in progress. |
| macOS Apple Silicon | App/agent/tools/adapters and desktop all-target source checks passed. | No linked application, native tests, GUI runtime, signing, or notarization verified. |
| macOS Intel | App/agent/tools/adapters and desktop all-target source checks passed. | No linked application, native tests, GUI runtime, signing, or notarization verified. |

## Windows release checks

The CLI and native desktop release builds passed. The desktop executable returned successfully for `--version` and `--help`, opened a responding Switchya window, and exposed its local host status with version 0.1.0 and no active sessions. All 21 UI assets served by the CLI release matched the current source hashes.

These checks verify startup and readiness. Native webview interaction and graceful closing of the window have not been verified.

## Native Linux tests

Rust/Cargo 1.96.1 ran the following command on the native `x86_64-alpine-linux-musl` host:

```sh
cargo test --locked -p switchyard-app -p switchyard-agent \
  -p switchyard-agent-tools -p switchyard-agent-adapters --all-targets -j 1
```

Results: agent 15, adapters 26, tools 13, app 37; total **91 passed, 0 failed**. This includes the Unix terminal argument/environment guards and account/profile tests. Test fixtures did not exercise a live Claude account, authentication flow, or model request.

The native desktop build uses GTK 3.24.52 and WebKitGTK 2.48.7. Its result will be recorded when the build and checks complete. No Linux GUI interaction has been verified.

## macOS source checks

Rust/Cargo 1.95.0, cargo-zigbuild 0.23.4 and Zig 0.16.0 checked both `aarch64-apple-darwin` and `x86_64-apple-darwin`, with a deployment target of macOS 11.0:

```sh
cargo-zigbuild check -p switchyard-app -p switchyard-agent \
  -p switchyard-agent-tools -p switchyard-agent-adapters \
  --all-targets --locked --target TARGET -j 1
cargo-zigbuild check --manifest-path desktop/Cargo.toml \
  --all-targets --locked --target TARGET -j 1
```

All four commands returned exit code 0. They checked the platform-specific Rust implementation and compiled the required native helper code. They did not link or run a macOS application.

No Apple SDK, header stubs, replacement framework definitions, or documentation-only build bypass was used. The check found and corrected a Darwin C variadic argument promotion issue in the file-opening code before the complete checks passed.

## Evidence integrity

Checks ran against immutable, content-hashed source exports, with file integrity verified before and after each command. The UI text refresh used by the later checks changed no Rust source, Cargo manifest, or lockfile. Build outputs and test logs were retained locally. The pre-existing gateway release binary was hashed before and after the Linux test run and remained unchanged.

These results do not establish live provider connectivity or account authentication. Those flows require the user's explicitly selected provider or native CLI account.
