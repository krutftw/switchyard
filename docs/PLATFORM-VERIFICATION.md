# Platform verification

Local verification for the Switchya 0.1.0 preview, 3 October 2026. This report distinguishes checks that execute native code from source checks for another platform.

| Platform | Verified result | Remaining boundary |
| --- | --- | --- |
| Windows x64 | App/agent/tools/adapters tests and warnings-denied Clippy passed. CLI and desktop release builds and extracted portable candidate checks passed. The native desktop opened a responding window and completed its local host readiness check. | Native GUI interaction and graceful window close have not been verified. |
| Linux x64 | 91 native tests passed with no failures on Alpine Linux 3.24.2 under WSL2. The native desktop development build and `--version`/`--help` smoke checks passed. | Linux GUI interaction has not been verified. No Linux app release package is provided. |
| macOS Apple Silicon | App/agent/tools/adapters and desktop all-target source checks passed. | No linked application, native tests, GUI runtime, signing, or notarization verified. |
| macOS Intel | App/agent/tools/adapters and desktop all-target source checks passed. | No linked application, native tests, GUI runtime, signing, or notarization verified. |

## Windows release checks

The CLI and native desktop release builds passed. The desktop executable returned successfully for `--version` and `--help`, opened a responding Switchya window, and exposed its local host status with version 0.1.0 and no active runs. All 21 UI assets served by the CLI release matched the current source hashes.

The portable candidate contained 19 allowlisted files with verified manifest hashes. After extraction into a separate directory, the CLI version check, desktop help check, responding native window, local host status, and embedded account-adapter UI asset check passed. The candidate executable hashes are:

- Desktop: `b2fb8a770640c3cad921e0a9b6c7acf572b0c3b47b0d3b148141b2963a20ae6e`
- CLI: `ae1287bcf1b682e67c2c28a7287e8448f38150df51cff16ab09606734ed5a119`

The final Windows preview ZIP contains the same tested executable bytes: 19 files, 20,335,861 bytes, SHA-256 `ea313361bcdf7fa4c1b8a7d9bd17d5870d6301907b7825c6871b55310316c8da`. It was packaged from source commit `cf822e1ca9c088a0f1c2b9fb1096279f39690f0b`; the completed Linux result below was added to this report afterward.

These checks verify startup and readiness. Native webview interaction and graceful closing of the window have not been verified.

## Native Linux tests

Rust/Cargo 1.96.1 ran the following command on the native `x86_64-alpine-linux-musl` host:

```sh
cargo test --locked -p switchyard-app -p switchyard-agent \
  -p switchyard-agent-tools -p switchyard-agent-adapters --all-targets -j 1
```

Results: agent 15, adapters 26, tools 13, app 37; total **91 passed, 0 failed**. This includes the Unix terminal argument/environment guards and account/profile tests. Test fixtures did not exercise a live Claude account, authentication flow, or model request.

The native desktop build passed with GTK 3.24.52 and WebKitGTK 2.48.7:

```sh
cargo build --manifest-path desktop/Cargo.toml --locked -j 1
```

The resulting x86-64 ELF development executable dynamically links to the Linux system libraries. Both `--version` and `--help` returned exit code 0; the version was `switchya-desktop 0.1.0`. Its SHA-256 is `7975a5f30e2cba58ece0bba25f66c263ada093fd0f3e79531d781f15e508a5fc`.

No Linux GUI interaction was performed. This developer build is not a Linux release package.

### Known Linux GUI dependency advisory

The Linux desktop dependency graph includes `glib 0.18.5` through GTK `0.18.2` and WebKitGTK `2.0.2`. It is affected by [RUSTSEC-2024-0429](https://rustsec.org/advisories/RUSTSEC-2024-0429.html), an unsound string-variant iterator implementation. The upstream patched line starts at `glib 0.20.0`; this Tauri/GTK stack constrains the dependency to `0.18`, so adding a separate newer version would not replace the affected dependency.

The Windows desktop target graph excludes glib, and the CLI workspace has no glib dependency. This advisory therefore does not enter either Windows deliverable's dependency graph. The Linux native development build retains the advisory pending a compatible upstream fix or reviewed backport. Dependency presence alone does not establish whether the affected function is reachable in this application; that reachability was not tested. No Linux desktop release package is distributed.

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

Checks ran against immutable, content-hashed source exports, with file integrity verified before and after each command. The UI text refresh used by the later checks changed no Rust source, Cargo manifest, or lockfile. Subsequent trailing-whitespace cleanup changed no Rust source or configuration values. Build outputs and test logs were retained locally. The pre-existing gateway release binary was hashed before and after the Linux tests and desktop build and remained unchanged.

These results do not establish live provider connectivity or account authentication. Those flows require the user's explicitly selected provider or native CLI account.

The jobs in the [GitHub CI run for the release source](https://github.com/krutftw/switchyard/actions/runs/37114468200) did not start. Their failure status does not represent executed tests. The verification in this report was performed locally.
