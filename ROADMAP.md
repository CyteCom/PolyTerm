# ROADMAP.md

Milestones in dependency order. Each has an exit criterion that is demonstrable, not a
checklist of files written. Do not start a milestone before its predecessor's exit
criterion is met — the earlier ones establish abstractions the later ones consume.

---

## M0 — RDP go/no-go spike — ✅ DONE (2026-09-09)

**Verdict: go with `ironrdp`, pure Rust.** ADR-2 is `Accepted`. Details in `SPIKE-RDP.md`;
M8 is unblocked. The residual gaps (FR-61 cert prompt, FR-62 non-US layout, FR-64
clipboard, a Linux runtime auth test, and the viewer's dynamic-resize reconnect) are M8
verification items, not spike blockers.


**Nothing that depends on the verdict starts until this finishes.** It is the entire
technical risk in the project and it is one day of work. M1 is the one milestone that does
not depend on it — the `RemoteDesktop` trait is identical under either answer, which is
the point of having the trait — and may run alongside. M8 is what waits.

Follow `SPIKE-RDP.md`. Build `ironrdp-viewer` from the upstream repository, point it at
your real Windows targets with your real GPO security settings, and answer three
questions: does NLA negotiate, does it fall back cleanly when the host wants the GFX
pipeline, and is the resulting frame rate and text legibility acceptable to a human doing
actual work.

**Exit (met):** a written verdict in `SPIKE-RDP.md` — `ironrdp`, with the evidence behind
it — and ADR-2 moved to `Accepted`.

**Deliberately not built here:** anything reusable. This is throwaway.

---

## M1 — Workspace skeleton and core contracts

Create the workspace per `ARCHITECTURE.md` §2. Define `Transport`, `RemoteDesktop`,
`SessionSpec`, and the error types in `polyterm-core`. Every other crate exists as a stub
that compiles.

Set up `rustfmt.toml`, `clippy.toml`, a CI workflow running fmt/clippy/test on both
Linux and Windows, and `tracing` with a subscriber that has a redaction filter for secrets
from day one — retrofitting NFR-8 later means auditing every log site.

**Exit:** `cargo clippy --workspace --all-targets -- -D warnings` clean on both platforms
in CI. The traits are written and reviewed. No behaviour yet.

---

## M2 — Terminal widget over a local shell

The hardest UI work in the project. Everything downstream plugs into it, so it comes
before any network code.

`polyterm-term` wraps `alacritty_terminal` and exposes a renderable snapshot with damage
information. `polyterm-pty` implements `Transport` over `portable-pty`. `polyterm-ui`
renders the grid into an egui texture with a glyph atlas, handles keyboard input,
selection, scrollback, and resize.

The widget renders into a caller-supplied rect and must not assume it owns the window.
From M4 onward, many live instances at different sizes is the normal case, so the glyph
atlas is shared per font configuration rather than owned per widget.

Get the performance right here rather than deferring it. Damage-tracked rendering and
rect-relative layout are structural properties of the renderer, not optimisations you bolt
on.

**Exit:** open a local shell on both Linux and Windows. Run `vim`, `htop`, and `tmux`
correctly. `cat` a 10 MB file and hold 30 fps (NFR-5). Select and copy text. Resize the
window and have everything stay correct. `polyterm-term` has headless unit tests
(NFR-10).

**Requirements:** FR-10 through FR-17, FR-55, FR-56, NFR-5, NFR-10.

---

## M3 — Serial

Small, and it exists at this position for a reason: it is the cheapest possible test of
whether the `Transport` abstraction from M1 is actually generic. If adding serial requires
changing the trait, the trait was wrong, and it is far better to learn that now than after
SSH is built on it.

Port enumeration, line settings, DTR/RTS control, BREAK, modem line status display,
disconnect and reconnect handling, session logging.

**Exit:** connect to a board at 115200 8N1, toggle DTR to reset it, watch the boot log,
unplug the USB adapter mid-session, plug it back in, reconnect without restarting the
application. Works on both platforms.

**Requirements:** FR-45 through FR-50.

---

## M4 — Session tree, persistence, and tabs

The application becomes usable as an application rather than a demo.

`polyterm-store` with SQLite and keyring. The session tree panel with folders, CRUD,
and search. The tile tree from `ARCHITECTURE.md` §10 — splits, dividers, sibling promotion,
tab groups, dragging tabs between groups, directional focus — plus tab lifecycle and layout
restore. Settings UI.

Decide `egui_tiles` versus a hand-written tree here, per the open question in ADR-12.

This is where the `egui` cost identified in ADR-3 gets paid, and the tile tree is the
largest line on that bill. If it proves intractable, this is the milestone at which to
invoke that ADR's revisit condition — not later.

Focus routing lands here even though broadcast does not. Build the downward subtree walk
from `ARCHITECTURE.md` §10.2 now, with the single-recipient case as the only caller;
M6 turns multi-exec on and must not have to restructure delivery to do it.

**Exit:** create, organise, and search saved sessions across restarts. Split the window
four ways with a live local shell in each, drag a tab from one tile into another, restart,
and get the same layout back. Store and retrieve a credential from the OS keyring on both
platforms.

**Requirements:** FR-1 through FR-6, FR-85 through FR-89, FR-95, FR-76, FR-77, FR-78.

---

## M5 — SSH

Now that the terminal, the transport abstraction, and the session store all exist, SSH is
mostly configuration surface rather than new architecture.

`polyterm-ssh` implementing `Transport` over `russh`. Password, public key, encrypted
key, keyboard-interactive, and agent authentication — including the Windows agent, which
is a named pipe and Pageant rather than a Unix socket, and will need its own transport
code. Known-hosts store and the host key prompt flow from `ARCHITECTURE.md` §6. Keepalive
and reconnect. Jump host chaining.

**Exit:** connect with each authentication method on both platforms. Reject a changed host
key with a blocking warning. Survive a network interruption and reconnect. Connect through
a jump host.

**Requirements:** FR-20 through FR-23, FR-28, FR-29.

---

## M6 — Port forwarding and multi-exec

Two features that are cheap now and are core to why the tool exists.

Local, remote, and dynamic forwards owned by the session, with live status and the
teardown confirmation from `ARCHITECTURE.md` §9. Tile-scoped multi-exec per ADR-13,
including the recipient marking of FR-93, the single paste confirmation of FR-94, and the
per-recipient backpressure rule in `ARCHITECTURE.md` §10.3.

The isolation test is not a nice-to-have on this milestone. NFR-15 wants it covered
directly: assert that a session outside the focused tile's subtree receives zero bytes.

**Exit:** split a tile four ways, open an SSH session in each, enable multi-exec on the
parent, run one command, and see four results — with a fifth session in an adjacent tile
confirmed to have received nothing. Broadcast to a group whose background tabs are also
recipients and confirm they ran it too. Establish a SOCKS proxy and route a browser
through it. Close the last shell tab of a session with an active tunnel and be correctly
prompted.

**Requirements:** FR-24 through FR-27, FR-90 through FR-94, NFR-15, NFR-16.

---

## M7 — SFTP

The browser pane sharing the authenticated connection with the shell. Navigation,
transfers with progress, remote file operations, drag and drop.

**Exit:** browse a remote filesystem in the same connection as an open shell. Drag a file
to the local desktop and a directory back up. Watch a large transfer report progress
without the UI stalling.

**Requirements:** FR-35 through FR-38.

---

## M8 — RDP

Last, because M0 already told us whether it works and because it depends on the tab and
pane infrastructure from M4.

`polyterm-rdp` implementing `RemoteDesktop` with whichever backend M0 selected. Partial
texture updates into a persistent `TextureHandle`. Raw scancode keyboard input — not
egui's translated character events, which will break non-US layouts. Certificate prompt
flow. Clipboard.

**Exit:** connect to a Windows 11 host and a Server session host with NLA, log in, work in
the session, copy text out to the local clipboard. Acceptable to a human, per the same
subjective bar used in M0.

**Requirements:** FR-60 through FR-65.

---

## M9 — Hardening and release

Cross-platform packaging: a `.deb` and an AppImage or Flatpak for Linux, an MSI or a
signed portable executable for Windows. Crash handling that respects NFR-8. Documentation.
The v1.0 acceptance targets at the end of `REQUIREMENTS.md`, run end to end on both
platforms from the same session store.

**Exit:** all six acceptance targets pass.

---

## After v1.0

Deferred items are marked `v1.1` in `REQUIREMENTS.md` and are not scheduled here. The two
worth flagging as likely first:

- **FR-8, import from PuTTY and `~/.ssh/config`.** The single largest reduction in
  adoption friction for anyone migrating.
- **FR-66/FR-67, RDP dynamic resize and clipboard file transfer.** The two omissions most
  likely to be noticed daily.

`LATER` items — Mosh, scripting, multi-monitor RDP — are acknowledged, not committed.
