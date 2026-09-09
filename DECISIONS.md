# DECISIONS.md

Architecture decision records. Each entry states what was decided, why, what was
rejected, and what would cause us to revisit. The purpose of this file is to stop
settled questions from being reopened in every session.

Format: `ADR-n · Status · Title`. Statuses: `Accepted`, `Superseded by ADR-n`,
`Provisional` (decided, but with an open verification gate).

---

## ADR-1 · Accepted · Rust as the implementation language

**Decision.** Rust, stable channel, edition 2024.

**Why.** The choice was driven almost entirely by two constraints: the availability of an
embeddable terminal emulator, and how RDP pixels get on screen. Rust is the only
ecosystem that answers both without a C dependency. `alacritty_terminal` and
`wezterm-term` are the extracted cores of two production terminals. `ironrdp` is the only
modern RDP protocol implementation outside FreeRDP itself. SSH (`russh`), serial
(`serialport`), and PTY (`portable-pty`) are all mature and all work on Windows.

**Rejected.**

- **Java.** No maintained RDP client exists; everything descends from `properJavaRDP`,
  which implements RDP 4/5 with no TLS or NLA and will not connect to current Windows
  hosts. RDP would have meant embedding an external `xfreerdp` window, which is
  X11-specific and dying with Wayland.
- **C++/Qt.** Technically the strongest option and the only toolkit with first-class
  foreign-window embedding, plus in-process `libfreerdp` linking. Lost on the Windows
  requirement: QTermWidget is Unix-only, so the terminal would have had to be rebuilt
  anyway, which was most of what Qt was buying.
- **Go.** Excellent SSH and serial libraries and a familiar language, but no mature
  terminal widget for any Go GUI toolkit and no serious Go RDP implementation. Would have
  meant writing a VT emulator from scratch and reaching RDP through cgo, surrendering
  Go's advantages.
- **Tauri / Electron.** `xterm.js` is the best terminal emulator available, but WebKitGTK
  is the weak link on Linux, native window embedding is impossible, and Electron is
  already Tabby.
- **C#/Avalonia.** Mature UI, but no terminal widget and no cross-platform RDP.

**Revisit if.** Not foreseen. This is foundational.

---

## ADR-2 · Provisional · `ironrdp` as the RDP implementation

**Decision.** Use `ironrdp-client` behind the `RemoteDesktop` trait in `polyterm-core`.

**Why.** It is a pure-Rust RDP implementation from Devolutions with security as its
stated focus. `ironrdp-client` is factored as a library-only, event-loop-agnostic engine
that emits output on a channel for an embedder to consume — precisely the shape needed
for a tabbed session manager. It keeps the single-binary, no-C-dependency property.

**The open gate.** Codec coverage is raw bitmap, interleaved RLE, RDP 6.0 bitmap
compression, and RemoteFX. It does not cover the full H.264/AVC444 GFX pipeline that
current Windows hosts prefer. Sessions negotiate down, so they connect, but quality and
bandwidth efficiency are worse than FreeRDP. Whether that is acceptable is an empirical
question against real target hosts, not a judgement call.

**Rejected.**

- **Spawning `xfreerdp` and reparenting its window.** Platform-specific, X11-only,
  incompatible with Wayland, and unavailable in every Rust GUI toolkit.
- **`guacd` plus the Guacamole protocol.** Bounded and well-documented work, but adds a C
  daemon as a runtime dependency, which defeats NFR-2.
- **Writing RDP from scratch.** FreeRDP is roughly half a million lines of C. No.

**Revisit if.** `SPIKE-RDP.md` returns a no-go. The fallback is FFI bindings to
`libfreerdp` via `bindgen`, behind the same trait — losing the pure-Rust property and
gaining a C build dependency on both platforms, but changing nothing else in the project.
This is exactly why the trait exists and why it must be defined before any RDP code is
written.

---

## ADR-3 · Accepted · `egui` / `eframe` on `wgpu` for the UI

**Decision.** Immediate-mode UI via `eframe`, rendering through `wgpu`.

**Why.** The two heavy surfaces in this application — the terminal grid and the RDP
framebuffer — are both textures we blit ourselves. GPU-backed rendering matters more than
widget richness. `egui` is mature, cross-platform, and pure Rust.

**Known cost.** Immediate-mode is weakest at exactly what a connection manager needs most:
dense desktop chrome, tree views with context menus, dockable and splittable panes. Expect
to fight the UI layer rather than the protocols. Budget for it.

**Rejected.** Slint (triple-licensing complicates a free-software release), GTK4 (removed
`GtkSocket`/`GtkPlug`, weaker Windows story), Iced (less mature ecosystem at the time of
decision), native-per-platform (doubles all UI work).

**Revisit if.** The pane and tree work in Milestone 4 proves genuinely intractable. Moving
to Slint would touch only `polyterm-ui` if the crate boundaries in `ARCHITECTURE.md` have
been respected.

---

## ADR-4 · Accepted · `alacritty_terminal` for VT emulation

**Decision.** `alacritty_terminal` as the terminal state machine, wrapped by
`polyterm-term`. We render; it parses and holds the grid.

**Why.** Production-proven, actively maintained, ships on Windows, exposes damage
tracking, and is a pure state machine with no I/O — which makes it directly unit-testable
against NFR-10.

**Rejected.** `wezterm-term` (comparable and a reasonable substitute; `alacritty_terminal`
has the leaner API surface), `vte` alone (parser only, no grid model), writing our own
(a VT220-through-xterm emulator is a multi-month project and a solved problem).

**Revisit if.** Damage-tracking granularity proves insufficient for NFR-5. `wezterm-term`
is the drop-in alternative and swapping it touches one crate.

---

## ADR-5 · Accepted · One uniform `Transport` trait for SSH, serial, and PTY

**Decision.** SSH shells, serial ports, and local PTYs all present the same channel-based
handle: bounded output, input, control, events.

**Why.** They are the same thing — a bidirectional byte stream with out-of-band control
and lifecycle events. Unifying them means the terminal pane, session logging, multi-exec
broadcast, and reconnect logic are each written once rather than three times. The
differences (serial has no resize; SSH has host keys; PTY has neither) are expressed as
control messages a backend may ignore and events it may never emit.

**Rejected.** Per-protocol pane types, which is how most connection managers end up with
divergent behaviour between their SSH and serial tabs.

**Consequence to accept.** `ControlMsg::Resize` being a no-op on serial is correct. Do not
"fix" it.

---

## ADR-6 · Accepted · Channels only between the UI thread and the runtime

**Decision.** No shared locks between `eframe`'s main thread and the tokio runtime. All
cross-boundary communication is bounded channels. Wake the UI with `request_repaint()`.

**Why.** This is the property that satisfies NFR-4 and NFR-7. A `Mutex` shared with a task
that is blocked on a dead TCP connection eventually freezes the paint loop; a channel
cannot. Bounding the channels also gives backpressure for free, which is what keeps NFR-5
from becoming unbounded memory growth.

**Rejected.** `Arc<Mutex<TerminalState>>` read by the renderer — simpler to write, and the
direct cause of the freeze-on-dead-connection bug in most naive implementations.

---

## ADR-7 · Accepted · `x86_64-pc-windows-msvc` only; no cross-compilation for development

**Decision.** The MSVC toolchain is the only supported Windows target. Development and
testing happen on a real Windows machine, not via cross-compilation from Linux.

**Why.** `ironrdp`'s CredSSP path uses Windows SSPI, and the `libfreerdp` fallback in
ADR-2 links cleanly only against MSVC. Beyond linking: `cargo-xwin` will produce a Windows
binary from Linux, but a GUI application cannot be debugged that way and RDP negotiation
against a real host cannot be tested that way.

**Rejected.** The `windows-gnu` toolchain (persistent linking friction, worse debugger
support), cross-compilation as the primary workflow (untestable).

---

## ADR-8 · Accepted · OS keyring for credentials; no homegrown vault

**Decision.** Secrets go to the platform credential store via the `keyring` crate —
Windows Credential Manager, Secret Service or KWallet on Linux. The session store holds
only a reference.

**Why.** Writing a credential vault means designing key derivation, at-rest encryption,
and an unlock flow, and getting any of it subtly wrong is worse than not having one. The
platforms already solved this.

**Rejected.** An encrypted local file with a master password. Explicitly rejected as a
fallback too: if the keyring is unavailable, the correct behaviour is to prompt every time,
not to silently write secrets somewhere weaker.

---

## ADR-9 · Accepted · SQLite for the session store

**Decision.** `rusqlite`, in the platform config directory via `directories`.

**Why.** The session tree needs partial updates, will grow, and must survive an unclean
shutdown. A JSON file rewritten wholesale on every change fails the third of those.

**Rejected.** JSON or TOML file (no atomicity, whole-file rewrites), a key-value store
(the tree is relational), a directory of per-session files (poor rename and move
semantics).

**Note.** Export to a portable text format is FR-7 and is a separate concern from the
storage format.

---

## ADR-10 · Accepted · No bundled X server, no bundled Unix userland

**Decision.** Out of scope permanently. See OUT-1 and OUT-2.

**Why.** MobaXterm's Cygwin and X11 layer is the answer to a Windows-specific problem —
the absence of a Unix environment. On Linux it is entirely redundant. On Windows, WSL now
covers it far better than we could. Reimplementing an X server is not a serious proposal.

**Consequence.** This means we are not a MobaXterm clone; we are a connection manager.
X11 forwarding on Linux (FR-30) is a separate and much smaller question.

---

## ADR-11 · Accepted · Crate boundaries follow substitution seams

**Decision.** The workspace is split so that each externally-sourced capability —
terminal engine, SSH stack, RDP stack, UI toolkit — lives behind a trait in
`polyterm-core` and is implemented in exactly one crate.

**Why.** Two reasons. First, ADR-2 is provisional and ADR-3 and ADR-4 have live revisit
conditions; the boundaries are what make those reversible for the cost of one crate rather
than a rewrite. Second, the crate boundaries double as context boundaries for agentic
development — a session can be scoped to one crate instead of the whole tree.

**Consequence.** `polyterm-ui` must not name `russh`, `serialport`, `portable-pty`, or
`ironrdp` in its manifest. This is checkable and should be checked in review.

---

## ADR-12 · Accepted · A recursive tile tree for the content area

**Decision.** The window's content area is a binary split tree. Any tile splits
horizontally or vertically and the result splits again. A leaf holds either one session
view or a tab group with its own tab bar.

**Why.** Two demands that look different are the same structure: "show me four terminals
at once" and "keep a stack of tabs in that corner". A tree covers both, and split ratios,
divider dragging, sibling promotion on close, and directional focus all fall out of it
without special cases. It is also the only layout model that persists meaningfully — FR-95
serialises a tree, not a screenshot.

**Rejected.**

- **A flat grid of panes.** Cannot express nesting, so "split this pane, but only this
  one" has no representation, and resize semantics get arbitrary fast.
- **Floating MDI child windows.** A known usability dead end, and layout restore becomes a
  pile of coordinates that is wrong on the next monitor.
- **A general docking framework.** `egui` has no mature one. ADR-3 already warns that this
  layer is where the fighting happens; adopting a heavyweight docking crate makes the ADR-3
  revisit condition harder to exercise, not easier.

**Open.** `egui_tiles` is the obvious candidate crate and would save real work, but it is a
dependency decision under §5 of `CLAUDE.md` and has not been taken. Evaluate it at M4
against writing the tree ourselves — the deciding question is whether its focus and
drag-drop model can be constrained to satisfy FR-90, not whether it draws splitters.

**Revisit if.** M4 shows the tree cannot be driven from `egui`'s immediate-mode input
handling without per-frame layout thrash.

---

## ADR-13 · Accepted · The tile is the broadcast domain

**Decision.** Multi-exec is a per-tile mode. Input delivered to a tile with multi-exec on
reaches every terminal in that tile's subtree — every tab of its group and every terminal
in its descendant tiles. Input never crosses a tile boundary. There is no global broadcast
mode and no free-floating selection of target tabs. This supersedes FR-75.

**Why.** MobaXterm's multi-exec keeps its target set as per-tab state you cannot see at
the moment you type. That state is easy to leave enabled, and the failure mode is a command
executing on a host you were not looking at — the most expensive bug this class of tool
has. Binding the target set to the layout makes it spatial: the sessions receiving your
keystrokes are exactly the ones inside the highlighted rectangle. Isolation stops being a
flag that is checked and becomes a consequence of how delivery is resolved
(`ARCHITECTURE.md` §10.2).

**Rejected.**

- **Global multi-exec with per-tab enable flags** — the MobaXterm model. Invisible state,
  wrong-host risk, and no way to see the target set at a glance.
- **A "broadcast group" concept orthogonal to layout.** Two containment hierarchies to hold
  in your head, and the UI still cannot show you the target set without drawing it, at
  which point it is the layout again.

**Consequence to accept.** To broadcast to a set of sessions you must first arrange them
into one tile. That is one extra layout action, and it is the feature, not the tax — the
arrangement is the confirmation. It also means the recipient set can include background
tabs, which is why FR-93 requires them to be marked as receiving.

**Revisit if.** Users routinely need to broadcast to sessions they do not want adjacent on
screen. Try FR-97 (named layouts) before reintroducing a selection model.
