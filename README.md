# PolyTerm

A cross-platform connection manager and terminal for Linux and Windows — a free,
self-hosted, native replacement for MobaXterm.

One tabbed, tiling UI over a saved session tree, handling four kinds of session:

- **SSH** — shell, port forwarding (local, remote, SOCKS), SFTP browser
- **Serial / RS-232** — for embedded and console work, with DTR/RTS control and BREAK
- **Local shell** — PTY tabs (ConPTY on Windows, forkpty on Linux)
- **RDP** — remote desktop to Windows hosts, with NLA

Written in Rust. One native binary per platform. No Electron, no webview, no JVM, no
bundled C daemon, no bundled X server.

---

## Status

**Specification stage. There is no implementation yet.**

The design is settled and written down; the code is not started. Specifically:

- `Cargo.toml` declares a workspace whose member crates **do not exist yet**. It will not
  build. Creating them is Milestone 1.
- The dependency versions in `[workspace.dependencies]` are starting points, not
  researched pins. They need verifying against crates.io before the first build.
- **Milestone 0 — the RDP go/no-go spike — has not run, and it blocks everything else.**
  See `SPIKE-RDP.md`.

## Why the RDP spike comes first

The one real technical risk in this project is whether [`ironrdp`][ironrdp] is good enough
to be the RDP backend. It implements raw bitmap, interleaved RLE, RDP 6.0 bitmap
compression, and RemoteFX, but not the full H.264/AVC444 graphics pipeline that current
Windows hosts prefer. Modern hosts negotiate down, so sessions connect — the open question
is whether the result is pleasant to work in for a full day, over the network path you
actually use, against hosts with your actual GPO settings.

That is an empirical question, it is one day of work, and the fallback (FFI bindings to
`libfreerdp`) changes the toolchain and the packaging story. Discovering that on day one
costs nothing; discovering it at Milestone 8 is expensive. Hence M0.

## Design in one paragraph

The UI thread and the async runtime share **no locks**. `eframe`/`egui` owns the main
thread and never blocks; all protocol work runs on a `tokio` runtime on background
threads; everything between them crosses on *bounded* channels. That single property is
what keeps a hung SSH connection or a dead RDP host from freezing the application, and
bounding the channels gives backpressure for free. Every byte-stream session — SSH, serial,
local PTY — presents the same `Transport` trait, so the terminal pane, session logging,
broadcast, and reconnect logic are each written once instead of three times.

## Documentation

| File | What it is |
|---|---|
| [`REQUIREMENTS.md`](REQUIREMENTS.md) | Numbered, stable requirements (`FR-*`, `NFR-*`) and the v1.0 acceptance targets |
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | Threading model, crate graph, trait contracts, tiling and input routing |
| [`DECISIONS.md`](DECISIONS.md) | Architecture decision records — what was chosen, what was rejected, and what would reopen it |
| [`ROADMAP.md`](ROADMAP.md) | Milestones in dependency order, each with a demonstrable exit criterion |
| [`SPIKE-RDP.md`](SPIKE-RDP.md) | The M0 spike procedure and its decision rule |
| [`CLAUDE.md`](CLAUDE.md) | Operating instructions for agentic work in this repository |

## Stack

| Area | Choice |
|---|---|
| Language | Rust, stable, edition 2024 |
| UI | [`eframe`][eframe] / [`egui`][egui] on the `wgpu` backend |
| Terminal engine | [`alacritty_terminal`][alacritty] |
| SSH / SFTP | [`russh`][russh] + `russh-sftp` |
| Serial | [`serialport`][serialport] |
| Local PTY | [`portable-pty`][portable-pty] |
| RDP | [`ironrdp-client`][ironrdp], behind a `RemoteDesktop` trait *(provisional — see M0)* |
| Storage | `rusqlite`, plus the OS keyring for credentials |

Each externally-sourced capability sits behind a trait in `polyterm-core` and is
implemented in exactly one crate, so that swapping any of them costs one crate rather than
a rewrite. That is not incidental — `ironrdp` is explicitly provisional, and the terminal
engine and UI toolkit both have recorded revisit conditions.

## Building

Nothing to build yet. Once Milestone 1 lands:

```sh
cargo check --workspace --all-targets
cargo test  --workspace
cargo clippy --workspace --all-targets -- -D warnings   # must be clean
cargo run -p polyterm
```

Windows is built on Windows, targeting `x86_64-pc-windows-msvc`. Cross-compiling from
Linux produces a binary you cannot debug and cannot test RDP negotiation with, so it is
not the development workflow.

## Scope

Deliberately **not** in scope: a bundled X server, a bundled Unix userland, Telnet and
other unencrypted remote shells, cloud sync, telemetry, or any account system. PolyTerm is
a connection manager, not a MobaXterm clone — MobaXterm's Cygwin and X11 layers answer a
Windows-specific problem that WSL now covers better.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE))
- MIT License ([`LICENSE-MIT`](LICENSE-MIT))

at your option.

[ironrdp]: https://github.com/Devolutions/IronRDP
[eframe]: https://github.com/emilk/egui/tree/master/crates/eframe
[egui]: https://github.com/emilk/egui
[alacritty]: https://github.com/alacritty/alacritty
[russh]: https://github.com/Eugeny/russh
[serialport]: https://github.com/serialport/serialport-rs
[portable-pty]: https://github.com/wravery/portable-pty
