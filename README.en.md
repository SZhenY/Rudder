# Rudder

[简体中文](./README.md) | **English**

> This project is a fork of [yituorou/meatshell](https://github.com/yituorou/meatshell),
> with the following optimizations and tech-stack migrations:
>
> - **Terminal engine migration**: from `vt100` 0.15 to `alacritty_terminal` 0.26, with native Scrollback, Selection, and Reflow
> - **Scrollback fix**: reverted to `detect_scroll` approach, fixing inability to scroll after `cat` / `history` on large files
> - **Native Selection**: replaced custom selection pipeline with alacritty's `Selection` + `selection_to_string()`
> - **Native Reflow**: replaced raw byte-stream replay with `Term::resize`, preventing output truncation on window resize
> - **Render caching**: row-level `RenderedLine` cache skips regex highlight rebuild on static screens
> - **Code reduction**: net -57 lines, removed `prev` / `prev_display_offset` and other redundant fields

## Screenshots

<p align="center">
  <img src="docs/screenshots/01-welcome-en.png" alt="Welcome / session management" width="800"><br>
  <em>Welcome page: session management + local resource monitor sidebar</em>
</p>

<p align="center">
  <img src="docs/screenshots/02-terminal-htop.png" alt="Terminal + SFTP" width="800"><br>
  <em>Tabbed terminal (full-screen btop) + SFTP file browser + remote resource monitoring</em>
</p>

## Download & install

Every `v*` tag triggers a GitHub Actions build that produces native binaries for
**Windows / Linux / macOS**, published on the
[Releases](https://github.com/jeff141/meatshell/releases) page.

### Windows

Download `meatshell-*-windows-x86_64.zip`, unzip, and run `meatshell.exe`.

### Linux

```bash
tar -xzf meatshell-*-linux-x86_64.tar.gz
cd meatshell-*-linux-x86_64
./meatshell                                  # run it directly
# Optional: install the app icon + launcher entry (shows the icon in the dock /
# app list — no argument needed, it finds the binary next to the script)
chmod +x install-linux.sh && ./install-linux.sh
```

> Requires glibc ≥ 2.35 (Ubuntu 22.04+ / Debian 12+). On Wayland you may need to
> log out/in once after installing the icon.

Building from source with `cargo run` on Linux Mint / Ubuntu / Debian requires
the Slint/winit/rfd system development packages:

```bash
sudo apt update
sudo apt install -y --no-install-recommends \
  build-essential pkg-config cmake \
  libfontconfig1-dev libfreetype6-dev \
  libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
  libgl1-mesa-dev libegl1-mesa-dev libgtk-3-dev \
  libudev-dev
```

### macOS

The download is a `.zip` containing the `meatshell.app` bundle:

```bash
# Unzip (aarch64 = Apple Silicon, x86_64 = Intel)
unzip meatshell-*-macos-*.zip
# Move it to Applications (optional — it also runs in place)
mv meatshell.app /Applications/
# Clear the quarantine flag, otherwise macOS says "meatshell is damaged and can't be opened"
xattr -dr com.apple.quarantine /Applications/meatshell.app
# Open it (or double-click in Finder)
open /Applications/meatshell.app
```

> If you didn't move it to `/Applications`, point both paths above at wherever the `.app` actually is (e.g. `~/Downloads/meatshell.app`).

> To build from source, see [Running](#running) below.

## Features

### Done

- [x] FinalShell-style UI with dark / light / follow-system themes
- [x] Local + remote resource monitoring (CPU / memory / swap / network / disk)
- [x] Remote process monitor (CPU-sorted table with PID copy and permission-aware termination)
- [x] Full VT/ANSI terminal emulation (btop / htop / vim render correctly)
- [x] Color emoji, including skin tones, flags, and ZWJ sequences
- [x] Tabs (welcome page + multiple sessions)
- [x] Session management: create / edit / delete / groups, local JSON, export / import
  - Config location: `%APPDATA%/meatshell/sessions.json` (Windows)
    / `~/.config/meatshell/sessions.json` (Linux)
    / `~/Library/Application Support/meatshell/sessions.json` (macOS)
- [x] SSH (`russh`, pure Rust): password / private key / encrypted key (passphrase)
- [x] SFTP browser + upload / download (drag-and-drop) + in-terminal ZMODEM (`sz`) receive
- [x] SSH port forwarding / tunnels: local -L / remote -R / dynamic -D (SOCKS5)
- [x] Quick commands + command box (broadcast to all sessions) + command history
- [x] Serial / Telnet sessions
- [x] Outbound proxy (SOCKS5 / HTTP)
- [x] Import `~/.ssh/config`
- [x] Session passwords encrypted at rest (ChaCha20-Poly1305)
- [x] Known-hosts (`known_hosts`) verification + first-connect confirmation
- [x] Split panes for tabbed terminals

Color emoji graphics are provided by [Twemoji](https://github.com/jdecked/twemoji)
under [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/). See
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) for the full attribution.

### Planned

- [ ] Store session passwords in the OS keychain

## Tech stack

| Module        | Choice                                                            |
| ------------- | ----------------------------------------------------------------- |
| UI            | [Slint](https://slint.dev) (compiled pure Rust, no GC)            |
| Async runtime | [`tokio`](https://tokio.rs)                                       |
| SSH protocol  | [`russh`](https://crates.io/crates/russh) (no libssh dependency)  |
| System metrics| [`sysinfo`](https://crates.io/crates/sysinfo)                     |
| Serialization | `serde` + `serde_json`                                            |
| Logging       | `tracing` + `tracing-subscriber`                                  |

## Running

```bash
cargo run --release
```

On first launch an empty session store is created at
`%APPDATA%/meatshell/sessions.json`. Click **"＋ New Session"** in the top-right
to add your first server.

## Project layout

```
meatshell/
├── Cargo.toml
├── build.rs                 # Slint compiler entry point
├── ui/
│   ├── app.slint            # top-level window
│   ├── theme.slint          # design tokens
│   ├── widgets.slint        # reusable buttons / inputs / sparkline
│   ├── sidebar.slint        # left-hand system monitor panel
│   ├── tabs.slint           # top tab bar
│   ├── welcome.slint        # welcome page / quick connect
│   ├── session_dialog.slint # new / edit session dialog
│   └── terminal_view.slint  # terminal view (v0.1 line-buffered)
└── src/
    ├── main.rs
    ├── app.rs               # UI ↔ backend bridge
    ├── config.rs            # session JSON persistence
    ├── system.rs            # CPU / memory / network sampling
    └── ssh.rs               # SSH session worker
```

## Development notes

- Slint widgets use a strict layout DSL; after editing a `.slint` file,
  `cargo check` is the fastest feedback loop.
- The application event loop is single-threaded (required by Slint); all
  cross-thread UI updates go through `slint::invoke_from_event_loop` callbacks.
- SSH / SFTP share the `known_hosts` verification path: first contact asks for
  trust and remembers the host key, while later key changes prompt again.

## Release

Do not bump `Cargo.toml` by hand and then create a tag. Use the release helper
so the tag points at a commit that already contains the matching Cargo version:

```powershell
.\scripts\release.ps1 v0.6.0 -Push
```

The script updates `Cargo.toml` / `Cargo.lock`, runs `cargo check --locked`,
verifies `meatshell --version`, commits `Release v0.6.0`, creates an annotated
tag, and pushes the current branch plus the tag. See
[docs/release.md](docs/release.md) for details.

## License

Dual-licensed under MIT OR Apache-2.0.
