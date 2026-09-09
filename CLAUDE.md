# CLAUDE.md

Operating instructions for Claude Code working in this repository.

---

## 1. What this is

A cross-platform (Linux + Windows) connection manager and terminal, built to replace
MobaXterm with a free, self-hosted, native application. It handles four kinds of session
in a tabbed UI with a saved session tree:

- **SSH** — shell, port forwarding, SFTP browser
- **Serial / RS-232** — for embedded and console work
- **Local PTY** — local shell tabs
- **RDP** — remote desktop to Windows hosts

Written in Rust. Single native binary per platform. No Electron, no webview, no JVM,
no bundled C daemon.

---

## 2. Decisions that are already made

Read `DECISIONS.md` before proposing an alternative to any of these. Each has a recorded
rationale and a list of what was rejected and why.

| Area | Decision |
|---|---|
| Language | Rust, stable toolchain, edition 2024 |
| UI | `eframe` / `egui` on the `wgpu` backend |
| Terminal engine | `alacritty_terminal` |
| SSH | `russh` + `russh-sftp` |
| Serial | `serialport` |
| Local PTY | `portable-pty` |
| RDP | `ironrdp-client`, behind the `RemoteDesktop` trait |
| Async | `tokio` multi-threaded runtime on a background thread |
| Windows target | `x86_64-pc-windows-msvc` only |
| Credentials | OS keyring via the `keyring` crate; never our own vault |
| Layout | Recursive tile tree; input broadcast scoped to a single tile |

**Do not re-litigate these mid-task.** If you believe one is wrong, stop, say so, and
wait. Do not silently substitute a different crate because it was easier to make compile.

---

## 3. Commands

```bash
# Build / check
cargo check --workspace --all-targets
cargo build --workspace
cargo build --release

# Test
cargo test --workspace
cargo test -p polyterm-term            # single crate

# Lint — both must be clean before you call anything done
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings

# Run
cargo run -p polyterm

# Windows target (from a Windows host, not cross-compiled)
cargo build --target x86_64-pc-windows-msvc
```

There is no CI-only step you cannot run locally. If `clippy -D warnings` fails, the
change is not finished.

---

## 4. Workspace map

```
crates/
  polyterm-core/      Session model, IDs, config types, error types,
                      and the Transport + RemoteDesktop traits.
                      Depends on nothing else in the workspace.
  polyterm-term/      VT engine: wraps alacritty_terminal, owns the grid,
                      exposes a renderable snapshot. No I/O.
  polyterm-ssh/       Transport impl over russh. SFTP client. Port forwarding.
  polyterm-serial/    Transport impl over serialport.
  polyterm-pty/       Transport impl over portable-pty.
  polyterm-rdp/       RemoteDesktop impl over ironrdp-client.
  polyterm-store/     Session tree persistence + keyring access.
  polyterm-ui/        egui shell: tabs, session tree, panes, rendering.
apps/
  polyterm/           Binary. Wiring only — no logic lives here.
```

### Dependency rules — these are structural, not stylistic

1. `polyterm-core` depends on no other workspace crate.
2. Transport and backend crates (`-ssh`, `-serial`, `-pty`, `-rdp`) depend on `-core`
   **only**. They never depend on each other or on `-ui`.
3. `polyterm-ui` depends on `-core`, `-term`, and `-store`. It **must not** name
   `russh`, `serialport`, `portable-pty`, or `ironrdp` in its `Cargo.toml`.
4. `apps/polyterm` is the only crate that knows every backend exists. It constructs
   them and hands trait objects to the UI.

If a task seems to require breaking one of these, the design is wrong. Stop and say so.

---

## 5. Hard rules

**Threading**
- `eframe` owns the main thread. Nothing may block it. No `std::thread::sleep`, no
  blocking I/O, no `block_on` in any code reachable from the UI update loop.
- All protocol work runs on the tokio runtime. Crossing the boundary happens only via
  channels defined in `polyterm-core`.
- When data arrives on the async side, wake the UI with
  `egui::Context::request_repaint()`. Do not poll.

**Errors**
- Library crates: `thiserror`, concrete error enums, no `anyhow`.
- The binary: `anyhow` is fine.
- No `unwrap()` or `expect()` in library crates outside of `#[cfg(test)]` and
  statically-provable invariants that carry a `// SAFETY-style` comment explaining why.
- A dropped connection is a normal event, not a panic. Every backend must surface
  disconnection as a `TransportEvent` / `RdpEvent`, never by terminating a task silently.

**Platform parity**
- Every feature must work on Linux and Windows. If a platform difference is unavoidable,
  put the `#[cfg]` inside the lowest-level crate that owns the concern and expose one
  API upward. No `#[cfg(windows)]` in `polyterm-ui`.
- Assume nothing about paths. Use the `directories` crate.
- Test Windows behaviour on Windows. Do not assume a Linux pass implies a Windows pass,
  particularly for the PTY (ConPTY), serial enumeration (COM ports), and SSH agent
  (named pipe, not a Unix socket).

**Security**
- Passwords and private-key passphrases go to the OS keyring. They are never written to
  the session store, never logged, and never included in a `Debug` impl. Derive `Debug`
  manually on any struct holding one.
- Host key verification is on by default and its prompt is a UI concern, not a
  transport-layer auto-accept.

**Dependencies**
- Do not add a crate without saying so and why. New dependencies need a line in
  `DECISIONS.md` if they are load-bearing.
- Prefer pure-Rust crates. Anything pulling in a C build dependency needs explicit
  agreement — that property is the point of the whole stack choice.

---

## 6. How to work a task

1. Requirements are numbered in `REQUIREMENTS.md` (`FR-*`, `NFR-*`). Work against IDs.
   If a task has no ID, ask whether it should be added before writing code.
2. Scope the session to one crate where possible. The crate boundaries exist partly so
   you don't need the whole tree in context.
3. Read `ARCHITECTURE.md` for the trait contracts before implementing against them.
   Do not change a trait in `polyterm-core` as a side effect of a task in another crate —
   that is its own task, and it needs to be called out.
4. Milestones and ordering are in `ROADMAP.md`. Do not skip ahead; earlier milestones
   establish the abstractions later ones depend on.

### Definition of done

- `cargo fmt --all -- --check` clean
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo test --workspace` passing
- New behaviour has a test, or a written explanation of why it isn't testable headless
- Built and manually exercised on both platforms if it touches I/O, rendering, or paths
- No new dependency added silently

### Commit messages

```
<crate>: <imperative summary>

<why, not what>

Refs: FR-12, FR-13
```

---

## 7. Review pass

When acting as a reviewer rather than an implementer, check in this order and report
findings without fixing them unless asked:

1. Dependency rules from §4 — is the crate graph still acyclic and layered?
2. Blocking work on the UI thread.
3. `unwrap`/`expect` in library crates.
4. Secrets in logs, `Debug` output, or the session store.
5. Input routing — can any path deliver a keystroke or paste to a session outside the
   focused tile's subtree? See §9.
6. Windows paths, ConPTY, COM enumeration, and agent-socket assumptions.
7. Unbounded channels and unbounded buffers — backpressure on a fast SSH stream or a
   noisy serial port is a real failure mode here, not a theoretical one.
8. Only then: style and idiom.

---

## 8. Things to ask about rather than decide

- Any change to a trait in `polyterm-core`
- Any new workspace crate
- Any dependency that adds a C toolchain requirement
- Anything that makes a feature Linux-only or Windows-only
- Replacing `ironrdp-client` with FFI bindings to `libfreerdp` — this is a live
  contingency, but it is a decision, not an implementation detail. See `SPIKE-RDP.md`.

---

## 9. Known sharp edges

- **RDP codec coverage.** `ironrdp` supports raw bitmap, interleaved RLE, RDP 6.0 bitmap
  compression, and RemoteFX. It does not cover the full H.264/AVC444 GFX pipeline that
  modern Windows hosts prefer. Sessions negotiate down. If quality or frame rate is
  unacceptable against real targets, the fallback is FFI to `libfreerdp` behind the same
  trait. `SPIKE-RDP.md` defines the go/no-go.
- **Terminal rendering performance.** Naively re-rendering the full grid every frame will
  not hold up under `cat` of a large file. Damage tracking is required, not an
  optimisation to defer.
- **ConPTY resize** semantics differ from Unix `TIOCSWINSZ`. Expect this to need
  platform-specific handling inside `polyterm-pty`.
- **Serial has no resize.** `ControlMsg::Resize` is a no-op there and that is correct;
  do not "fix" it.
- **Input isolation is a safety property, not a UI detail.** Broadcast delivery walks
  *down* from the focused tile and never up (`ARCHITECTURE.md` §10.2). A keystroke reaching
  a session in another tile means a command ran on a host the user was not looking at — the
  worst bug this application can have. Resolve recipients by traversal, never by filtering a
  global list, and test it directly per NFR-15.
- **Broadcast fan-out must not block.** Eight recipients, one wedged, is the normal bad
  case. `try_send` per recipient with a bounded per-session pending queue; drop and say so
  in the UI rather than buffering. Never `send().await` on the UI thread.
