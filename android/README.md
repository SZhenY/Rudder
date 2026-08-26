# MeatShell Android Beta

This is an experimental Android client kept separate from the desktop binary so
Android-only dependencies and lifecycle rules cannot affect Windows, Linux, or
macOS builds.

Current scope:

- Android ARM64 (`arm64-v8a`), API 23 or newer
- Password authentication
- Interactive PTY shell with command input
- SHA-256 server-key confirmation on every connection

Not yet included: saved sessions, persistent `known_hosts`, private-key login,
SFTP, port forwarding, serial sessions, or full terminal escape-sequence
rendering.

Build with the Android SDK/NDK, Java, Rust's `aarch64-linux-android` target, and
`cargo-apk` installed:

```sh
cargo apk build --manifest-path android/Cargo.toml \
  --locked --release --target aarch64-linux-android --lib
```
